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

use crate::activator::{PowerOffError, PowerOnError};
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
    /// A model to load once the machine is up (`provider/model` or a
    /// public name with one target). Operator and admin keys only.
    #[serde(default)]
    load: Option<String>,
    /// End the hold no later than the next quiet window's start
    /// (`server.optimize.quiet_windows`). `sb ai tune` asks for it, so a run
    /// cut off before it took its lock leaves nothing that reaches into the
    /// window.
    #[serde(default)]
    until_quiet_window: bool,
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
    let mut ttl = match ttl_ms(&state, body.ttl_secs) {
        Ok(ttl) => ttl,
        Err(response) => return response,
    };
    if body.until_quiet_window {
        let now = sb_store::now_millis();
        if let Some(start) = sb_runtime::optimize::next_quiet_window_start(
            &state.snapshot().config.server.optimize,
            now,
        ) {
            let left_ms = u64::try_from(start.saturating_sub(now))
                .unwrap_or(0)
                .max(1_000);
            ttl = ttl.min(left_ms);
        }
    }
    if body.reason.chars().count() > 200 {
        return error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "reason is at most 200 characters".to_string(),
        );
    }
    // A preload is engine control: operator business, checked before the
    // hold is taken.
    let preload = match &body.load {
        None => None,
        Some(_) if !principal.is_operator_or_admin() => return tenancy::forbidden(),
        Some(name) => {
            match crate::handlers::model_settings::resolve_one(&state, &principal, name) {
                Ok(named) => Some(named.target),
                Err(response) => return response,
            }
        }
    };
    match state.capacity.take_hold(
        &host,
        &body.reason,
        ttl,
        (principal.tenant.clone(), principal.project.clone()),
        body.wake,
    ) {
        Ok(Some(hold)) => {
            let mut answer = hold_json(&hold);
            if let Some(target) = preload {
                crate::engine_control::preload_after_hold(&state, target.clone(), body.wake);
                answer["load"] = json!({
                    "target": target,
                    "status": if body.wake { "loading after the wake" } else { "loading if the machine is up" },
                });
            }
            (StatusCode::CREATED, Json(answer)).into_response()
        }
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

/// `POST /v1/hosts/{host}/power` with `{"action": "off" | "on"}`.
///
/// Operator and admin keys only: a machine's power is the operator's
/// business, never a client's.
///
/// `off` powers the machine off now, through the same `poweroff_command`
/// and the same `poweroff_guard_command` the idle timer uses, under the
/// same locks and re-checks. It is refused with 409 and a typed `reason`
/// while a hold is active (unless `"release_holds": true`, which ends the
/// holds that were active when the request came, once every check has
/// passed, and names them), while a lane of the machine has work in
/// flight or queued, while the guard says the machine is busy, while the
/// machine has not been up long enough, while a wake, a power-off or the
/// idle check runs on it, and when the machine has no power-off command
/// at all. A machine none of whose lanes answers is already off: 200 with
/// `"already_off": true`, and no command runs. Any HTTP status is an
/// answer (an engine that answers 503 while it loads runs on a machine
/// that is up); only no answer at all is not. A machine the gateway
/// knows is off is asked too (one probe per lane): someone may have
/// started it by hand. A failed command is 502.
///
/// `on` wakes the machine through the same wake path a hold with
/// `wake: true` uses and answers at once (202, `"status": "waking"`): the
/// wake runs in the background. It takes no hold, so the idle timer still
/// applies. A machine one of whose lanes answers is up: 200 with
/// `"status": "up"`, and no wake is sent.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PowerRequest {
    action: String,
    /// End the active holds first (with `off`).
    #[serde(default)]
    release_holds: bool,
}

/// A refusal: 409 with a machine-readable `reason` beside the message.
fn refused(status: StatusCode, reason: &str, message: String) -> Response {
    (
        status,
        Json(json!({
            "error": {
                "message": message,
                "type": "power_refused",
                "reason": reason,
            }
        })),
    )
        .into_response()
}

pub(crate) async fn power(
    State(state): State<AppState>,
    Path(host): Path<String>,
    Json(body): Json<PowerRequest>,
) -> Response {
    match body.action.as_str() {
        "off" => power_off(&state, &host, body.release_holds).await,
        "on" => power_on(&state, &host).await,
        other => error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            format!("action must be `on` or `off` (got `{other}`)"),
        ),
    }
}

/// The machine is powered off now.
async fn power_off(state: &AppState, host: &str, release_holds: bool) -> Response {
    // The power-off runs in its own task: a caller that goes away
    // mid-command (Ctrl-C, a client timeout) drops this handler, and a
    // power-off cancelled there would leave every lane "powering off" and
    // stop the idle power-off. The task always finishes the lanes or puts
    // them back.
    let capacity = state.capacity.clone();
    let machine = host.to_string();
    let task = tokio::spawn(async move { capacity.power_off(&machine, release_holds).await });
    let result = match task.await {
        Ok(result) => result,
        Err(join) => Err(PowerOffError::Failed(format!(
            "the power-off task ended early: {join}"
        ))),
    };
    match result {
        Ok(outcome) => Json(json!({
            "host": host,
            "action": "off",
            "powered_off": true,
            "already_off": outcome.already_off,
            "lanes": outcome.lanes,
            "released_holds": outcome.released_holds,
        }))
        .into_response(),
        Err(PowerOffError::UnknownHost) => no_host(host),
        Err(PowerOffError::Refused(refusal)) => {
            refused(StatusCode::CONFLICT, refusal.reason(), refusal.message())
        }
        Err(PowerOffError::Failed(message)) => {
            refused(StatusCode::BAD_GATEWAY, "poweroff_failed", message)
        }
    }
}

/// The machine is woken now (the wake runs in the background).
async fn power_on(state: &AppState, host: &str) -> Response {
    match state.capacity.power_on(host).await {
        // A lane of the machine answers: it is up, and no wake was sent.
        Ok(outcome) if outcome.already_up => Json(json!({
            "host": host,
            "action": "on",
            "status": "up",
            "lanes": outcome.lanes,
        }))
        .into_response(),
        Ok(outcome) => (
            StatusCode::ACCEPTED,
            Json(json!({
                "host": host,
                "action": "on",
                "status": "waking",
                "lanes": outcome.lanes,
            })),
        )
            .into_response(),
        Err(PowerOnError::UnknownHost) => no_host(host),
        Err(PowerOnError::Refused(refusal)) => {
            refused(StatusCode::CONFLICT, refusal.reason(), refusal.message())
        }
        Err(PowerOnError::Failed(message)) => {
            refused(StatusCode::BAD_GATEWAY, "wake_failed", message)
        }
    }
}

/// `POST /v1/admin/alerts/test` (admin keys only): send one `kind: "test"`
/// alert through `server.alert_command` now and answer with its exit code,
/// to prove the delivery path on a live gateway.
pub(crate) async fn alerts_test(State(state): State<AppState>) -> Response {
    match state.capacity.alerts().send_test().await {
        Ok(code) => Json(json!({"sent": code == 0, "exit_code": code})).into_response(),
        Err(message) if message.contains("not set") => {
            error(StatusCode::CONFLICT, "invalid_request_error", message)
        }
        Err(message) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({"sent": false, "exit_code": null, "error": {"message": message, "type": "alert_error"}})),
        )
            .into_response(),
    }
}
