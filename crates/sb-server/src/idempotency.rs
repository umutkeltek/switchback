//! Gateway-boundary idempotency (Oracle #2 / #7, first version).
//!
//! A client sends `Idempotency-Key: <key>`. Behaviour:
//!   - **Non-streaming**: the first request executes and its EXACT rendered
//!     response is stored (keyed by the key + a fingerprint of the request body).
//!     A duplicate with the same key + same body replays the stored bytes
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
use sb_store::IdempotencyRecord;

use crate::AppState;

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

/// A stable fingerprint of the request body — a reused key with a different body
/// is a client error, not a replay.
pub fn fingerprint(body: &serde_json::Value) -> String {
    use std::hash::{Hash, Hasher};
    let json = serde_json::to_string(body).unwrap_or_default();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    json.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn idem_error(status: StatusCode, message: &str) -> Response {
    (
        status,
        Json(serde_json::json!({"error": {"message": message, "type": "idempotency_error"}})),
    )
        .into_response()
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

/// Pre-execution check for a non-streaming replay. `Some` short-circuits the
/// handler (a replay or a 422 mismatch); `None` means proceed to execute.
pub fn precheck(state: &AppState, key: &str, fp: &str) -> Option<Response> {
    let store = state.engine.store()?;
    match store.idempotency_get(key) {
        Ok(Some(rec)) if rec.fingerprint != fp => Some(mismatch_response()),
        Ok(Some(rec)) => Some(replay_response(&rec)),
        _ => None,
    }
}

/// Store a rendered JSON response for future replay (first writer wins).
pub fn store_json(state: &AppState, key: &str, fp: &str, value: &serde_json::Value) {
    let Some(store) = state.engine.store() else {
        return;
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
        tracing::warn!(error = %e, "idempotency store write failed");
    }
}

/// In-memory registry of in-flight idempotency keys (per-process single-flight).
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
