//! The `/cp/v1` declarative control plane (Oracle's "control-plane surface").
//!
//! A k8s-style envelope (`apiVersion` / `kind` / `metadata{name,revision,etag}` /
//! `spec`) over the live config, plus a draft → validate → publish lifecycle and
//! a `route-preview` that turns the explainable `RouteDecision` into a product
//! surface — all without touching the YAML file (the API is authoritative; YAML
//! stays bootstrap). The dashboard and the AI-facing CLI are meant to be thin
//! clients over THIS, not second config parsers.

use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use futures::Stream;
use sb_core::Config;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::controlplane::{
    account_visible_to_principal, audit_context, provider_type_name, provider_visible_to_principal,
    redact_config_for_principal, tenant_scope,
};
use crate::handlers::common::attach_session_metadata;
use crate::AppState;

const API_VERSION: &str = "cp.switchback.dev/v1";

/// `(url segment, envelope kind, config array key, name field)` for each
/// declarative resource projected from the config.
const KINDS: &[(&str, &str, &str, &str)] = &[
    ("providers", "ProviderEndpoint", "providers", "id"),
    ("combos", "ComboProfile", "combos", "$key"),
    ("routes", "RouteProfile", "routes", "name"),
    ("harnesses", "HarnessAdapter", "harnesses", "name"),
    ("tenants", "Tenant", "tenants", "id"),
    ("egress", "EgressProfile", "egress", "id"),
    ("plugins", "Plugin", "plugins", "type"),
];

fn kind_for(segment: &str) -> Option<(&'static str, &'static str, &'static str)> {
    KINDS
        .iter()
        .find(|(seg, ..)| *seg == segment)
        .map(|(_, kind, key, name)| (*kind, *key, *name))
}

fn envelope(kind: &str, name: &str, revision: u64, spec: Value) -> Value {
    json!({
        "apiVersion": API_VERSION,
        "kind": kind,
        "metadata": {
            "name": name,
            "revision": revision,
            "etag": format!("W/\"rev-{revision}\""),
        },
        "spec": spec,
    })
}

fn cp_error(status: StatusCode, message: impl Into<String>) -> Response {
    (
        status,
        Json(json!({"error": {"message": message.into(), "type": "cp_error"}})),
    )
        .into_response()
}

/// `GET /cp/v1` — discovery: the resource kinds + the lifecycle/preview verbs.
pub async fn root(State(state): State<AppState>) -> Json<Value> {
    Json(json!({
        "apiVersion": API_VERSION,
        "revision": state.revision(),
        "kinds": KINDS.iter().map(|(seg, kind, ..)| json!({"name": kind, "path": seg})).collect::<Vec<_>>(),
        "verbs": [
            "GET /cp/v1/resources/{kind}", "GET /cp/v1/resources/{kind}/{name}",
            "GET /cp/v1/runtime-state",
            "POST /cp/v1/drafts", "GET /cp/v1/drafts", "GET /cp/v1/drafts/{id}",
            "POST /cp/v1/drafts/{id}/validate", "POST /cp/v1/drafts/{id}/publish",
            "POST /cp/v1/route-preview", "POST /cp/v1/admission-preview",
            "POST /cp/v1/runtime-state/reset-lockout",
            "GET /cp/v1/watch (SSE)",
        ],
    }))
}

/// `GET /cp/v1/resources/{kind}` — list the declarative resources of a kind.
pub async fn list_resources(
    State(state): State<AppState>,
    Extension(principal): Extension<crate::tenancy::Principal>,
    Path(kind_seg): Path<String>,
) -> Response {
    let Some((kind, key, name_field)) = kind_for(&kind_seg) else {
        return cp_error(StatusCode::NOT_FOUND, format!("unknown kind `{kind_seg}`"));
    };
    let snap = state.snapshot();
    let redacted = redact_config_for_principal(&snap.config, &principal);
    let resources: Vec<Value> = match redacted.get(key) {
        Some(Value::Array(items)) => items
            .iter()
            .enumerate()
            .map(|(i, spec)| {
                let name = spec
                    .get(name_field)
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| format!("{kind_seg}-{i}"));
                envelope(kind, &name, snap.revision, spec.clone())
            })
            .collect(),
        Some(Value::Object(items)) if name_field == "$key" => items
            .iter()
            .map(|(name, spec)| envelope(kind, name, snap.revision, spec.clone()))
            .collect(),
        _ => Vec::new(),
    };
    Json(json!({ "apiVersion": API_VERSION, "kind": kind, "items": resources })).into_response()
}

/// `GET /cp/v1/resources/{kind}/{name}` — one declarative resource.
pub async fn get_resource(
    State(state): State<AppState>,
    Extension(principal): Extension<crate::tenancy::Principal>,
    Path((kind_seg, name)): Path<(String, String)>,
) -> Response {
    let Some((kind, key, name_field)) = kind_for(&kind_seg) else {
        return cp_error(StatusCode::NOT_FOUND, format!("unknown kind `{kind_seg}`"));
    };
    let snap = state.snapshot();
    let redacted = redact_config_for_principal(&snap.config, &principal);
    let found = match redacted.get(key) {
        Some(Value::Array(items)) => items
            .iter()
            .find(|spec| spec.get(name_field).and_then(|v| v.as_str()) == Some(name.as_str())),
        Some(Value::Object(items)) if name_field == "$key" => items.get(&name),
        _ => None,
    };
    match found {
        Some(spec) => Json(envelope(kind, &name, snap.revision, spec.clone())).into_response(),
        None => cp_error(StatusCode::NOT_FOUND, format!("no {kind} `{name}`")),
    }
}

/// `GET /cp/v1/runtime-state` — live, non-secret operator state as a
/// declarative control-plane resource. This is the machine-consumable companion
/// to `/v1/health`: runtime knobs, provider circuit/account availability, and
/// admission headroom in the same envelope shape as config resources.
pub async fn runtime_state(
    State(state): State<AppState>,
    Extension(principal): Extension<crate::tenancy::Principal>,
) -> Json<Value> {
    let snap = state.snapshot();
    let providers: Vec<Value> = snap
        .config
        .providers
        .iter()
        .filter(|p| provider_visible_to_principal(&snap.config, &principal, p))
        .map(|p| {
            let ph = snap.resolver.pool_health(&p.id, "");
            let accounts = snap
                .resolver
                .account_health(&p.id, "")
                .into_iter()
                .filter(|account| {
                    account_visible_to_principal(&snap.config, &principal, &p.id, &account.id)
                })
                .collect::<Vec<_>>();
            let accounts_total = accounts.len();
            let accounts_healthy = accounts.iter().filter(|account| account.healthy).count();
            json!({
                "id": p.id,
                "type": provider_type_name(&p.kind),
                "accounts_total": accounts_total,
                "accounts_healthy": accounts_healthy,
                "accounts": accounts,
                "circuit_open": ph.circuit_open,
                "status": if ph.circuit_open || accounts_healthy == 0 { "degraded" } else { "healthy" },
            })
        })
        .collect();
    let healthy = providers
        .iter()
        .filter(|p| p["status"] == "healthy")
        .count();
    Json(envelope(
        "RuntimeState",
        "current",
        snap.revision,
        json!({
            "runtime": &snap.runtime,
            "providers": providers,
            "summary": {
                "providers": providers.len(),
                "healthy": healthy,
            },
            "admission": {
                "max_concurrency": state.admission.limit(),
                "available": crate::admission::available(&state),
            },
        }),
    ))
}

#[derive(Debug, Deserialize)]
pub struct ResetLockoutRequest {
    provider: String,
    account: String,
    #[serde(default)]
    model: Option<String>,
}

/// `POST /cp/v1/runtime-state/reset-lockout` — operator override for a
/// provider/account or provider/account/model lockout. This intentionally goes
/// through the resolver boundary: the control plane can clear availability
/// state, but never touches leases or adapter auth.
pub async fn reset_lockout(
    State(state): State<AppState>,
    Json(body): Json<ResetLockoutRequest>,
) -> Response {
    let snap = state.snapshot();
    let model = body
        .model
        .as_deref()
        .filter(|m| !m.is_empty())
        .map(str::to_string);
    match snap
        .resolver
        .reset_lockout(&body.provider, &body.account, model.as_deref())
    {
        Some(cleared) => Json(json!({
            "ok": true,
            "cleared": cleared,
            "provider": body.provider,
            "account": body.account,
            "model": model,
            "revision": snap.revision,
        }))
        .into_response(),
        None => cp_error(
            StatusCode::NOT_FOUND,
            format!(
                "unknown provider/account `{}/{}`",
                body.provider, body.account
            ),
        ),
    }
}

/// `POST /cp/v1/route-preview` — the explainable decision for a request, computed
/// without executing it. Body is an OpenAI-shaped chat request.
pub async fn route_preview(
    State(state): State<AppState>,
    Extension(principal): Extension<crate::tenancy::Principal>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    route_preview_inner(state, principal, headers, body).await
}

async fn route_preview_inner(
    state: AppState,
    principal: crate::tenancy::Principal,
    headers: HeaderMap,
    body: Value,
) -> Response {
    let mut req = match sb_protocols::openai::request_from_openai_chat(&body) {
        Ok(req) => req,
        Err(msg) => return cp_error(StatusCode::BAD_REQUEST, msg),
    };
    req.tenant = principal.tenant.clone();
    req.project = principal.project.clone();
    attach_session_metadata(&mut req, &headers);
    let preview_tenant = req.tenant.clone();
    let preview_project = req.project.clone();
    let preview_session_id = req.metadata.get("session_id").cloned();
    match state.engine.preview_route(&req) {
        Ok((revision, plan)) => {
            let harness_candidates = state.engine.harness_candidates_for_plan(&plan);
            Json(json!({
                "revision": revision,
                "principal": {
                    "tenant": preview_tenant,
                    "project": preview_project,
                    "session_id": preview_session_id,
                },
                "decision": plan.decision,
                "candidates": plan.candidates.iter().map(|c| &c.id).collect::<Vec<_>>(),
                "harness_candidates": harness_candidates,
            }))
            .into_response()
        }
        Err(e) => (
            StatusCode::from_u16(e.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            Json(json!({"error": {"message": e.message, "type": e.error_type}})),
        )
            .into_response(),
    }
}

/// `POST /cp/v1/admission-preview` — would a request from this caller be admitted
/// right now? Reports the global in-flight headroom and the caller's tenant
/// concurrency + budget status (resolved from the API key). A point-in-time
/// prediction (not a reservation) — the companion to `route-preview`.
pub async fn admission_preview(
    State(state): State<AppState>,
    Extension(principal): Extension<crate::tenancy::Principal>,
) -> Response {
    let snap = state.snapshot();

    let global_available = crate::admission::available(&state);
    let global_ok = global_available.map(|a| a > 0).unwrap_or(true);

    let mut tenant_json = Value::Null;
    let mut tenant_ok = true;
    if let Some(tenant) = principal.tenant.as_deref() {
        let tc = snap.config.tenant(tenant);
        let in_flight = crate::tenancy::in_flight(&state, tenant);
        let concurrency_ok = tc
            .and_then(|t| t.max_concurrency)
            .map(|max| in_flight < max)
            .unwrap_or(true);
        let spent_usd = state.ledger.tenant_spend_usd(tenant);
        let budget_ok = tc
            .and_then(|t| t.budget_usd)
            .map(|b| spent_usd < b)
            .unwrap_or(true);
        tenant_ok = concurrency_ok && budget_ok;
        tenant_json = json!({
            "tenant": tenant,
            "max_concurrency": tc.and_then(|t| t.max_concurrency),
            "in_flight": in_flight,
            "concurrency_ok": concurrency_ok,
            "budget_usd": tc.and_then(|t| t.budget_usd),
            "spent_usd": spent_usd,
            "budget_ok": budget_ok,
        });
    }

    Json(json!({
        "admitted": global_ok && tenant_ok,
        "global": {
            "max_concurrency": state.admission.limit(),
            "available": global_available,
            "ok": global_ok,
        },
        "tenant": tenant_json,
    }))
    .into_response()
}

/// `GET /cp/v1/watch` — an SSE stream of control-plane changes. Emits the current
/// config `revision` immediately, then a `revision` event whenever it changes (a
/// publish / reload / runtime-patch), with keep-alive heartbeats in between. The
/// dashboard and CLI subscribe here instead of polling. (First slice watches the
/// revision; richer health/usage watch is a follow-up.)
pub async fn watch(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = futures::stream::unfold((state, None::<u64>), |(state, last)| async move {
        loop {
            let current = state.revision();
            if last != Some(current) {
                let event = Event::default()
                    .event("revision")
                    .json_data(json!({ "revision": current }))
                    .unwrap_or_else(|_| Event::default().data("{}"));
                return Some((Ok(event), (state, Some(current))));
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

// --- Drafts -----------------------------------------------------------------

#[derive(Clone)]
struct Draft {
    config: Config,
    base_revision: u64,
    created_at_ms: i64,
}

/// Staged `/cp/v1` drafts. Durable in the SQLite state store when one is
/// configured, else process-lifetime in memory. Durable drafts can include the
/// full proposed config body, so inline secret-bearing drafts are blocked unless
/// the live server config explicitly opts in.
#[derive(Clone, Default)]
pub struct DraftStore {
    mem: Arc<Mutex<HashMap<String, Draft>>>,
    store: Option<Arc<dyn sb_store::StateStore>>,
    required: bool,
}

impl DraftStore {
    pub fn new(store: Option<Arc<dyn sb_store::StateStore>>, required: bool) -> Self {
        Self {
            mem: Arc::default(),
            store,
            required,
        }
    }

    fn mem(&self) -> std::sync::MutexGuard<'_, HashMap<String, Draft>> {
        self.mem.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn is_durable(&self) -> bool {
        self.store.is_some()
    }

    fn put(&self, id: &str, config: &Config, base_revision: u64) -> Result<(), String> {
        let created_at_ms = sb_store::now_millis();
        if let Some(store) = &self.store {
            let config_json = serde_json::to_string(config).unwrap_or_default();
            if let Err(e) = store.put_draft(&sb_store::DraftRecord {
                id: id.to_string(),
                config_json,
                base_revision,
                created_at_ms,
            }) {
                if self.required {
                    return Err(format!("draft store write failed: {e}"));
                }
                tracing::warn!(error = %e, id, "draft store write failed");
                self.mem().insert(
                    id.to_string(),
                    Draft {
                        config: config.clone(),
                        base_revision,
                        created_at_ms,
                    },
                );
            }
        } else {
            self.mem().insert(
                id.to_string(),
                Draft {
                    config: config.clone(),
                    base_revision,
                    created_at_ms,
                },
            );
        }
        Ok(())
    }

    fn get(&self, id: &str) -> Option<Draft> {
        if let Some(draft) = self.mem().get(id).cloned() {
            return Some(draft);
        }
        if let Some(store) = &self.store {
            let rec = store.get_draft(id).ok().flatten()?;
            let config = serde_json::from_str::<Config>(&rec.config_json).ok()?;
            Some(Draft {
                config,
                base_revision: rec.base_revision,
                created_at_ms: rec.created_at_ms,
            })
        } else {
            self.mem().get(id).cloned()
        }
    }

    /// `(id, base_revision, created_at_ms)` for every staged draft.
    fn list(&self) -> Vec<(String, u64, i64)> {
        let mut items: Vec<(String, u64, i64)> = self
            .mem()
            .iter()
            .map(|(id, d)| (id.clone(), d.base_revision, d.created_at_ms))
            .collect();
        if let Some(store) = &self.store {
            let mem_ids: HashSet<String> = items.iter().map(|(id, ..)| id.clone()).collect();
            items.extend(
                store
                    .list_drafts()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|r| (r.id, r.base_revision, r.created_at_ms))
                    .filter(|(id, ..)| !mem_ids.contains(id)),
            );
        }
        items
    }

    fn remove(&self, id: &str) {
        if let Some(store) = &self.store {
            if let Err(e) = store.delete_draft(id) {
                tracing::warn!(error = %e, id, "draft store delete failed");
            }
        }
        self.mem().remove(id);
    }
}

/// `POST /cp/v1/drafts` — stage a proposed config (full `Config` as JSON). The
/// draft is validated for shape on create; semantic validation is `/validate`.
pub async fn create_draft(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    let config: Config = match serde_json::from_value(body) {
        Ok(cfg) => cfg,
        Err(e) => return cp_error(StatusCode::BAD_REQUEST, format!("invalid config: {e}")),
    };
    let live = state.snapshot();
    if state.drafts.is_durable()
        && config.has_inline_secret_material()
        && !live.config.server.persist_secret_bearing_drafts
    {
        return cp_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "durable drafts containing inline secrets are disabled; use env/vault references or set server.persist_secret_bearing_drafts=true",
        );
    }
    let id = sb_core::new_id("draft");
    let base_revision = state.revision();
    if let Err(e) = state.drafts.put(&id, &config, base_revision) {
        return cp_error(StatusCode::INTERNAL_SERVER_ERROR, e);
    }
    (
        StatusCode::CREATED,
        Json(json!({ "id": id, "base_revision": base_revision })),
    )
        .into_response()
}

/// `GET /cp/v1/drafts` — list staged drafts (metadata only).
pub async fn list_drafts(
    State(state): State<AppState>,
    Extension(principal): Extension<crate::tenancy::Principal>,
) -> Response {
    if tenant_scope(&principal).is_some() {
        return cp_error(
            StatusCode::FORBIDDEN,
            "tenant operators cannot read global drafts",
        );
    }
    let items: Vec<Value> = state
        .drafts
        .list()
        .into_iter()
        .map(|(id, base_revision, created_at_ms)| {
            json!({ "id": id, "base_revision": base_revision, "created_at_ms": created_at_ms })
        })
        .collect();
    Json(json!({ "drafts": items })).into_response()
}

/// `GET /cp/v1/drafts/{id}` — a draft's proposed config, redacted.
pub async fn get_draft(
    State(state): State<AppState>,
    Extension(principal): Extension<crate::tenancy::Principal>,
    Path(id): Path<String>,
) -> Response {
    if tenant_scope(&principal).is_some() {
        return cp_error(
            StatusCode::FORBIDDEN,
            "tenant operators cannot read global drafts",
        );
    }
    match state.drafts.get(&id) {
        Some(d) => Json(json!({
            "id": id,
            "base_revision": d.base_revision,
            "config": redact_config_for_principal(&d.config, &principal),
        }))
        .into_response(),
        None => cp_error(StatusCode::NOT_FOUND, format!("no draft `{id}`")),
    }
}

/// `POST /cp/v1/drafts/{id}/validate` — compile-check the draft (registry +
/// resolver) without publishing.
pub async fn validate_draft(
    State(state): State<AppState>,
    Extension(principal): Extension<crate::tenancy::Principal>,
    Path(id): Path<String>,
) -> Response {
    if tenant_scope(&principal).is_some() {
        return cp_error(
            StatusCode::FORBIDDEN,
            "tenant operators cannot validate global drafts",
        );
    }
    let config = match state.drafts.get(&id) {
        Some(d) => d.config,
        None => return cp_error(StatusCode::NOT_FOUND, format!("no draft `{id}`")),
    };
    match sb_runtime::Engine::validate_config(&config) {
        Ok(()) => Json(json!({ "valid": true })).into_response(),
        Err(e) => Json(json!({ "valid": false, "errors": [e] })).into_response(),
    }
}

/// `POST /cp/v1/drafts/{id}/publish` — validate + atomically hot-swap the draft
/// config (a new revision). Optimistic concurrency: if an `If-Match: <revision>`
/// header is present it must equal the current revision, else 409 (someone else
/// published since this draft was based). On success the draft is consumed.
pub async fn publish_draft(
    State(state): State<AppState>,
    Extension(principal): Extension<crate::tenancy::Principal>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let config = match state.drafts.get(&id) {
        Some(d) => d.config,
        None => return cp_error(StatusCode::NOT_FOUND, format!("no draft `{id}`")),
    };

    // Optimistic concurrency via If-Match (the current revision). Parse the
    // expected revision and hand it to the engine so the check is enforced
    // atomically with the swap — a check here would be a TOCTOU (two concurrent
    // publishers could both pass it, then both swap, losing one update).
    let expected_revision = match headers.get("if-match").and_then(|v| v.to_str().ok()) {
        Some(raw) => {
            let trimmed = raw
                .trim_matches('"')
                .trim_start_matches("W/")
                .trim_matches('"')
                .trim_start_matches("rev-");
            match trimmed.parse::<u64>() {
                Ok(rev) => Some(rev),
                Err(_) => {
                    return cp_error(
                        StatusCode::BAD_REQUEST,
                        format!("malformed If-Match `{raw}` (expected a revision number)"),
                    )
                }
            }
        }
        None => None,
    };

    if let Err(e) = sb_runtime::Engine::validate_config(&config) {
        return cp_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("draft invalid: {e}"),
        );
    }
    match state.engine.publish_with_audit(
        config,
        audit_context("draft_publish", "control-plane draft publish", &principal)
            .with_object_id(id.clone()),
        expected_revision,
    ) {
        Ok(revision) => {
            state.drafts.remove(&id);
            Json(json!({ "ok": true, "revision": revision })).into_response()
        }
        Err(sb_runtime::PublishError::Conflict { expected, current }) => cp_error(
            StatusCode::CONFLICT,
            format!("revision changed (If-Match `{expected}` != current `{current}`)"),
        ),
        Err(sb_runtime::PublishError::Failed(e)) => cp_error(
            if e.contains("state store") {
                StatusCode::INTERNAL_SERVER_ERROR
            } else {
                StatusCode::UNPROCESSABLE_ENTITY
            },
            format!("publish failed: {e}"),
        ),
    }
}
