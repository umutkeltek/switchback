//! Host holds over HTTP: `/v1/hosts/{host}/holds`.
//!
//! A client key takes, lists, extends and deletes only the holds its own
//! tenant and project took; another's answers 404, like jobs and artifacts.
//! Operator and admin keys see every hold.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::handlers::workloads::may_read;
use crate::holds::{Hold, HoldError, MAX_ACTIVE_HOLDS_PER_TENANT};
use crate::{tenancy, AppState};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HoldRequest {
    ttl_secs: u64,
    #[serde(default)]
    reason: String,
    #[serde(default)]
    wake: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExtendRequest {
    ttl_secs: u64,
}

fn error(status: StatusCode, kind: &str, message: String) -> Response {
    (
        status,
        Json(json!({"error": {"message": message, "type": kind}})),
    )
        .into_response()
}

fn no_host(host: &str) -> Response {
    error(
        StatusCode::NOT_FOUND,
        "not_found",
        format!("no managed machine `{host}` (a lane's `host`, or the name of a lane with a machine of its own)"),
    )
}

fn no_hold() -> Response {
    error(
        StatusCode::NOT_FOUND,
        "not_found",
        "hold not found".to_string(),
    )
}

/// The hold as the API shows it.
pub(crate) fn hold_json(hold: &Hold) -> Value {
    let view = hold.view();
    json!({
        "id": hold.id,
        "host": hold.host,
        "reason": hold.reason,
        "expires_at": view.expires_at,
        "expires_at_ms": hold.expires_at_ms,
        "state": hold.state,
        "owner": hold.tenant,
        "wake_detail": hold.wake_detail,
    })
}

/// The hold time asked for, checked against `server.max_hold_secs`.
#[allow(clippy::result_large_err)]
fn ttl_ms(state: &AppState, ttl_secs: u64) -> Result<u64, Response> {
    let max = state.snapshot().config.server.max_hold_secs;
    if ttl_secs == 0 || ttl_secs > max {
        return Err(error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            format!("ttl_secs must be from 1 to {max} (server.max_hold_secs)"),
        ));
    }
    Ok(ttl_secs.saturating_mul(1_000))
}

/// The hold `id` on `host`, if the caller may see it.
fn readable(
    state: &AppState,
    principal: &tenancy::Principal,
    host: &str,
    id: &str,
) -> Option<Hold> {
    state
        .capacity
        .holds()
        .get(host, id, state.capacity.now_ms())
        .filter(|hold| may_read(principal, &hold.tenant, &hold.project))
}

/// `POST /v1/hosts/{host}/holds`: keep the machine on for `ttl_secs`.
/// Answers 201 with the hold at once; with `wake: true` the wake runs in the
/// background and the hold's `state` goes from `waking` to `active` (or
/// `wake_failed`).
pub(crate) async fn create(
    State(state): State<AppState>,
    Extension(principal): Extension<tenancy::Principal>,
    Path(host): Path<String>,
    Json(body): Json<HoldRequest>,
) -> Response {
    let ttl = match ttl_ms(&state, body.ttl_secs) {
        Ok(ttl) => ttl,
        Err(response) => return response,
    };
    if body.reason.chars().count() > 200 {
        return error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "reason is at most 200 characters".to_string(),
        );
    }
    match state.capacity.take_hold(
        &host,
        &body.reason,
        ttl,
        (principal.tenant.clone(), principal.project.clone()),
        body.wake,
    ) {
        Ok(Some(hold)) => (StatusCode::CREATED, Json(hold_json(&hold))).into_response(),
        Ok(None) => no_host(&host),
        Err(HoldError::TooMany) => error(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limit_error",
            format!("at most {MAX_ACTIVE_HOLDS_PER_TENANT} active holds per tenant"),
        ),
    }
}

/// `GET /v1/hosts/{host}/holds`: the active holds the caller may see.
pub(crate) async fn list(
    State(state): State<AppState>,
    Extension(principal): Extension<tenancy::Principal>,
    Path(host): Path<String>,
) -> Response {
    if !state.capacity.has_host(&host) {
        return no_host(&host);
    }
    let holds = state.capacity.holds();
    let data: Vec<Value> = holds
        .list(Some(&host), state.capacity.now_ms())
        .iter()
        .filter(|hold| may_read(&principal, &hold.tenant, &hold.project))
        .map(hold_json)
        .collect();
    Json(json!({
        "object": "list",
        "host": host,
        "data": data,
        "durable": holds.durable(),
    }))
    .into_response()
}

/// `PATCH /v1/hosts/{host}/holds/{id}` with `{"ttl_secs": N}`: the hold
/// now ends `N` seconds from now (at most `server.max_hold_secs`).
pub(crate) async fn extend(
    State(state): State<AppState>,
    Extension(principal): Extension<tenancy::Principal>,
    Path((host, id)): Path<(String, String)>,
    Json(body): Json<ExtendRequest>,
) -> Response {
    let ttl = match ttl_ms(&state, body.ttl_secs) {
        Ok(ttl) => ttl,
        Err(response) => return response,
    };
    if readable(&state, &principal, &host, &id).is_none() {
        return no_hold();
    }
    match state
        .capacity
        .holds()
        .extend(&host, &id, ttl, state.capacity.now_ms())
    {
        Some(hold) => Json(hold_json(&hold)).into_response(),
        None => no_hold(),
    }
}

/// `DELETE /v1/hosts/{host}/holds/{id}`: end the hold now; the machine's
/// idle countdown runs from this moment once no other hold is left.
pub(crate) async fn delete(
    State(state): State<AppState>,
    Extension(principal): Extension<tenancy::Principal>,
    Path((host, id)): Path<(String, String)>,
) -> Response {
    if readable(&state, &principal, &host, &id).is_none() {
        return no_hold();
    }
    match state
        .capacity
        .holds()
        .delete(&host, &id, state.capacity.now_ms())
    {
        Some(hold) => {
            Json(json!({"id": hold.id, "host": hold.host, "deleted": true})).into_response()
        }
        None => no_hold(),
    }
}
