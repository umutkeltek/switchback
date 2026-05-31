//! Multi-tenancy at the gateway edge (Oracle #4).
//!
//! An inbound API key resolves to a [`Principal`] (tenant + optional project).
//! The tenant is the unit of quota and usage attribution. This module handles
//! authentication (key → principal) and concurrency admission (reserve a slot
//! before dispatch, release on completion). The per-tenant SPEND cap is enforced
//! in `sb-runtime` (it needs the ledger); concurrency lives here because it is an
//! HTTP-connection concern tied to the request's lifetime.
//!
//! When no `api_keys` are configured, behaviour is unchanged: `server.api_key`
//! governs (single-tenant, unattributed), or the gateway is open.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::http::{header::AUTHORIZATION, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use sb_core::ApiKeyRole;

use crate::AppState;

/// The authenticated caller. `tenant`/`project` are `None` in the single-key or
/// open configurations (no attribution, no quota).
#[derive(Clone)]
pub struct Principal {
    pub tenant: Option<String>,
    pub project: Option<String>,
    pub role: ApiKeyRole,
}

impl Principal {
    pub fn admin() -> Self {
        Self {
            tenant: None,
            project: None,
            role: ApiKeyRole::Admin,
        }
    }

    pub fn is_admin(&self) -> bool {
        matches!(self.role, ApiKeyRole::Admin)
    }

    pub fn is_operator_or_admin(&self) -> bool {
        matches!(self.role, ApiKeyRole::Operator | ApiKeyRole::Admin)
    }
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({
            "error": {"message": "missing or invalid api key", "type": "invalid_request_error"}
        })),
    )
        .into_response()
}

pub fn forbidden() -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(serde_json::json!({
            "error": {"message": "api key is not authorized for this endpoint", "type": "permission_error"}
        })),
    )
        .into_response()
}

/// 429 — the tenant is at its `max_concurrency`.
pub fn over_capacity_response(tenant: &str) -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        Json(serde_json::json!({
            "error": {
                "message": format!("tenant `{tenant}` is at its concurrency limit"),
                "type": "rate_limit_error"
            }
        })),
    )
        .into_response()
}

/// Authenticate a request and resolve its principal. `Err` is the rejection
/// response to return as-is.
#[allow(clippy::result_large_err)] // Err is a ready-to-return HTTP Response, by design.
pub fn authenticate(state: &AppState, headers: &HeaderMap) -> Result<Principal, Response> {
    let snap = state.snapshot();
    let bearer = headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|s| s.trim());

    if !snap.config.api_keys.is_empty() {
        // Multi-tenant: the key must be in the list, and maps to a tenant.
        match bearer.and_then(|b| snap.config.principal_for_key(b)) {
            Some((tenant, project, role)) => Ok(Principal {
                tenant: Some(tenant.to_string()),
                project: project.map(str::to_string),
                role,
            }),
            None => Err(unauthorized()),
        }
    } else if let Some(expected) = snap.config.server.api_key.as_deref() {
        // Back-compat single key — authenticated but unattributed.
        if bearer == Some(expected) {
            Ok(Principal::admin())
        } else {
            Err(unauthorized())
        }
    } else {
        // Open gateway.
        Ok(Principal::admin())
    }
}

/// Reserve a concurrency slot for the principal's tenant (if it has a
/// `max_concurrency` limit). `Ok(None)` = no limit / no tenant; `Ok(Some(guard))`
/// = reserved (released on drop); `Err` = 429, already at capacity.
#[allow(clippy::result_large_err)] // Err is a ready-to-return HTTP Response, by design.
pub fn admit_concurrency(
    state: &AppState,
    principal: &Principal,
) -> Result<Option<ConcurrencyGuard>, Response> {
    let Some(tenant) = principal.tenant.as_deref() else {
        return Ok(None);
    };
    let snap = state.snapshot();
    let Some(max) = snap.config.tenant(tenant).and_then(|t| t.max_concurrency) else {
        return Ok(None);
    };
    match state.concurrency.reserve(tenant, max) {
        Some(guard) => Ok(Some(guard)),
        None => Err(over_capacity_response(tenant)),
    }
}

/// Per-tenant in-flight request counters for concurrency admission. Per-process
/// (in-memory) — multi-node would back this with the shared store.
#[derive(Clone, Default)]
pub struct Concurrency(Arc<Mutex<HashMap<String, u32>>>);

impl Concurrency {
    /// Reserve a slot for `tenant` if it is below `max`. Returns a guard that
    /// releases the slot on drop, or `None` if already at the cap.
    pub fn reserve(&self, tenant: &str, max: u32) -> Option<ConcurrencyGuard> {
        let mut map = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let count = map.entry(tenant.to_string()).or_insert(0);
        if *count >= max {
            return None;
        }
        *count += 1;
        Some(ConcurrencyGuard {
            map: self.0.clone(),
            tenant: tenant.to_string(),
        })
    }

    /// Current in-flight count for a tenant (for the `/v1/tenants` view).
    pub fn in_flight(&self, tenant: &str) -> u32 {
        self.0
            .lock()
            .map(|m| m.get(tenant).copied().unwrap_or(0))
            .unwrap_or(0)
    }
}

/// Releases a tenant's reserved concurrency slot on drop. For a streamed
/// response it is moved into the SSE encoder closure, so the slot stays held
/// until the stream is fully consumed.
pub struct ConcurrencyGuard {
    map: Arc<Mutex<HashMap<String, u32>>>,
    tenant: String,
}

impl Drop for ConcurrencyGuard {
    fn drop(&mut self) {
        if let Ok(mut map) = self.map.lock() {
            if let Some(count) = map.get_mut(&self.tenant) {
                *count = count.saturating_sub(1);
            }
        }
    }
}
