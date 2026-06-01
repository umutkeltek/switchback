//! Gateway-boundary idempotency (Oracle #2 / #7, first version).
//!
//! A client sends `Idempotency-Key: <key>`. Behaviour:
//!   - **Non-streaming**: with `server.idempotency.persist_response_bodies=true`,
//!     the first request executes and its EXACT rendered response is stored
//!     (keyed by tenant/project/endpoint/key + a fingerprint of the request
//!     body). A duplicate with the same key + same body replays the stored bytes
//!     (`Idempotent-Replayed: true`); a duplicate with the same key + a different
//!     body is a 422 (Stripe's rule). Replay is durable — it needs `state_store`.
//!   - **Streaming**: single-flight only. A concurrent duplicate (same key, still
//!     in flight) gets 409; the stream is NOT stored (post-completion replay would
//!     require retained event logs — we don't pretend it's free). The in-flight
//!     registry is in-memory, so single-flight is per-process.
//!
//! No key = today's behaviour, unchanged.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use sb_store::{IdempotencyBegin, IdempotencyRecord, StateStore};
use sha2::{Digest, Sha256};

use crate::{tenancy::Principal, AppState};

pub const KEY_HEADER: &str = "idempotency-key";
pub const REPLAY_HEADER: &str = "idempotent-replayed";

/// The idempotency key from the request headers, if present and non-empty.
pub fn key_from(headers: &HeaderMap) -> Option<String> {
    headers
        .get(KEY_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

fn hex_sha256(material: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(material.as_bytes());
    let mut out = String::with_capacity(3 + digest.len() * 2);
    out.push_str("v2:");
    for byte in digest {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Storage/single-flight key scoped to the authenticated caller and endpoint.
/// The raw client key is hashed before persistence so state stores do not retain
/// caller-provided replay keys in plaintext.
pub fn scoped_key(raw_key: &str, principal: &Principal, endpoint: &str) -> String {
    let tenant = principal.tenant.as_deref().unwrap_or("-");
    let project = principal.project.as_deref().unwrap_or("-");
    hex_sha256(&format!("{endpoint}\0{tenant}\0{project}\0{raw_key}"))
}

/// A stable fingerprint of the request body — a reused key with a different body
/// is a client error, not a replay.
pub fn fingerprint(body: &serde_json::Value) -> String {
    let json = serde_json::to_string(body).unwrap_or_default();
    hex_sha256(&json)
}

fn idem_error(status: StatusCode, message: &str) -> Response {
    (
        status,
        Json(serde_json::json!({"error": {"message": message, "type": "idempotency_error"}})),
    )
        .into_response()
}

fn store_error_response(message: impl std::fmt::Display) -> Response {
    idem_error(
        StatusCode::SERVICE_UNAVAILABLE,
        &format!("idempotency store unavailable: {message}"),
    )
}

/// 422 — the key was reused with a different request body.
pub fn mismatch_response() -> Response {
    idem_error(
        StatusCode::UNPROCESSABLE_ENTITY,
        "idempotency key reused with different request parameters",
    )
}

/// 409 — a request with this key is already in flight (single-flight).
pub fn in_progress_response() -> Response {
    idem_error(
        StatusCode::CONFLICT,
        "a request with this idempotency key is already in progress",
    )
}

/// Replay a stored response verbatim, flagged with `Idempotent-Replayed: true`.
pub fn replay_response(rec: &IdempotencyRecord) -> Response {
    let status = StatusCode::from_u16(rec.status).unwrap_or(StatusCode::OK);
    let mut resp = (status, rec.body.clone()).into_response();
    let ct = HeaderValue::from_str(&rec.content_type)
        .unwrap_or_else(|_| HeaderValue::from_static("application/json"));
    resp.headers_mut().insert("content-type", ct);
    resp.headers_mut()
        .insert(REPLAY_HEADER, HeaderValue::from_static("true"));
    resp
}

pub enum Begin {
    Proceed(ClaimGuard),
    Replay(Response),
}

/// Pre-execution durable replay + single-flight claim. If a state store is
/// configured, the in-flight claim is stored there so multiple gateway processes
/// coordinate on the same key. Otherwise it falls back to the in-process guard.
#[allow(clippy::result_large_err)] // Err is a ready-to-return HTTP Response, by design.
pub fn begin(state: &AppState, key: &str, fp: &str) -> Result<Begin, Response> {
    if let Some(store) = state.engine.store() {
        let ttl_ms = state.snapshot().config.server.idempotency.inflight_ttl_ms;
        match store.idempotency_begin(key, fp, ttl_ms) {
            Ok(IdempotencyBegin::Claimed) => {
                return Ok(Begin::Proceed(ClaimGuard::durable(store, key)));
            }
            Ok(IdempotencyBegin::InProgress) => return Err(in_progress_response()),
            Ok(IdempotencyBegin::Mismatch) => return Err(mismatch_response()),
            Ok(IdempotencyBegin::Replay(rec)) => return Ok(Begin::Replay(replay_response(&rec))),
            Err(e) if state.engine.store_required() => return Err(store_error_response(e)),
            Err(e) => {
                tracing::warn!(error = %e, "durable idempotency claim failed; falling back to in-process guard");
            }
        }
    }

    match state.inflight.try_claim(key) {
        Some(guard) => Ok(Begin::Proceed(ClaimGuard::local(guard))),
        None => Err(in_progress_response()),
    }
}

/// Store a rendered JSON response for future replay (first writer wins). Returns
/// an error when response persistence is enabled but the required state store
/// cannot accept the response.
pub fn store_json(
    state: &AppState,
    key: &str,
    fp: &str,
    value: &serde_json::Value,
) -> Result<(), String> {
    if !state
        .snapshot()
        .config
        .server
        .idempotency
        .persist_response_bodies
    {
        return Ok(());
    }
    let Some(store) = state.engine.store() else {
        return if state.engine.store_required() {
            Err("idempotency store is required but not configured".to_string())
        } else {
            Ok(())
        };
    };
    let body = serde_json::to_string(value).unwrap_or_default();
    if let Err(e) = store.idempotency_put(&IdempotencyRecord {
        key: key.to_string(),
        fingerprint: fp.to_string(),
        status: 200,
        content_type: "application/json".to_string(),
        body,
        created_at_ms: sb_store::now_millis(),
    }) {
        if state.engine.store_required() {
            return Err(format!("idempotency store write failed: {e}"));
        }
        tracing::warn!(error = %e, "idempotency store write failed");
    }
    Ok(())
}

/// In-memory registry of scoped in-flight idempotency keys (per-process
/// single-flight).
#[derive(Clone, Default)]
pub struct InFlight(Arc<Mutex<HashSet<String>>>);

impl InFlight {
    /// Claim a key for the duration of a request. Returns a guard that releases
    /// the key on drop, or `None` if the key is already in flight.
    pub fn try_claim(&self, key: &str) -> Option<InFlightGuard> {
        let mut set = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if !set.insert(key.to_string()) {
            return None;
        }
        Some(InFlightGuard {
            set: self.0.clone(),
            key: key.to_string(),
        })
    }
}

/// Releases its idempotency key from the in-flight set when dropped. For a
/// streamed response it is moved into the SSE encoder closure, so the key stays
/// claimed until the stream is fully consumed or dropped.
pub struct InFlightGuard {
    set: Arc<Mutex<HashSet<String>>>,
    key: String,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        if let Ok(mut set) = self.set.lock() {
            set.remove(&self.key);
        }
    }
}

pub struct ClaimGuard {
    local: Option<InFlightGuard>,
    durable: Option<DurableClaimGuard>,
}

impl ClaimGuard {
    fn local(guard: InFlightGuard) -> Self {
        Self {
            local: Some(guard),
            durable: None,
        }
    }

    fn durable(store: Arc<dyn StateStore>, key: &str) -> Self {
        Self {
            local: None,
            durable: Some(DurableClaimGuard {
                store,
                key: key.to_string(),
            }),
        }
    }
}

impl Drop for ClaimGuard {
    fn drop(&mut self) {
        let _ = self.local.take();
        let _ = self.durable.take();
    }
}

struct DurableClaimGuard {
    store: Arc<dyn StateStore>,
    key: String,
}

impl Drop for DurableClaimGuard {
    fn drop(&mut self) {
        if let Err(e) = self.store.idempotency_release(&self.key) {
            tracing::warn!(error = %e, "durable idempotency release failed");
        }
    }
}
