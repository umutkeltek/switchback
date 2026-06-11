use std::time::Instant;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use sb_runtime::ExecOutcome;

use crate::handlers::common::{attach_native_client_metadata, attach_session_metadata};
use crate::http_response::{
    openai_error, render_exec_error, sse_response, with_client_profile_header, with_queue_header,
    with_request_id, with_revision_header, with_route_header,
};
use crate::{admission, idempotency, sse, tenancy, AppState};

/// Anthropic `/v1/messages` ingress: an Anthropic-shaped client (Claude Code,
/// the Anthropic SDK) parsed into the canonical IR, routed across ANY provider
/// by the same `execute_request` core, then rendered back as Anthropic SSE or
/// JSON. This is the "never rewrite client code" promise for Anthropic clients.
pub(crate) async fn messages(
    State(state): State<AppState>,
    Extension(principal): Extension<tenancy::Principal>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let started = Instant::now();
    let idem = idempotency::key_from(&headers);
    let idem_scope = idem
        .as_deref()
        .map(|key| idempotency::scoped_key(key, &principal, "/v1/messages"));
    let idem_fp = idem_scope.as_ref().map(|_| idempotency::fingerprint(&body));
    let _guard = match (idem_scope.as_deref(), idem_fp.as_deref()) {
        (Some(key), Some(fp)) => match idempotency::begin(&state, key, fp) {
            Ok(idempotency::Begin::Proceed(guard)) => Some(guard),
            Ok(idempotency::Begin::Replay(resp)) => return resp,
            Err(resp) => return resp,
        },
        _ => None,
    };
    // Global admission (bounded backpressure): wait for an in-flight slot, or 503.
    let (_admit, queue_ms) = match admission::acquire(&state).await {
        Ok(slot) => slot,
        Err(resp) => return resp,
    };
    let _conc = match tenancy::admit_concurrency(&state, &principal) {
        Ok(guard) => guard,
        Err(resp) => return resp,
    };
    let mut req = match sb_protocols::anthropic::request_from_anthropic(&body) {
        Ok(request) => request,
        Err(message) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(openai_error(&message, "invalid_request_error")),
            )
                .into_response();
        }
    };
    req.tenant = principal.tenant.clone();
    req.project = principal.project.clone();
    attach_session_metadata(&mut req, &headers);
    let client_profile =
        attach_native_client_metadata(&mut req, &headers, "claude-code", "anthropic_messages");
    let (req_id, req_model) = (req.id.clone(), req.model.clone());
    let trace_id = req.id.clone();
    let (revision, outcome) = state.engine.execute(req, started).await;
    let response = match outcome {
        ExecOutcome::Stream { stream, summary } => {
            let mut encoder =
                sb_protocols::anthropic::AnthropicStreamEncoder::new(req_id, req_model);
            let body = sse::body(
                stream,
                move |event| {
                    let _hold = (&_guard, &_conc, &_admit);
                    encoder.encode(event)
                },
                sse::anthropic_error_frame,
                None,
            );
            sse_response(body, &summary)
        }
        ExecOutcome::Collected { response, summary } => {
            let value = sb_protocols::anthropic::response_to_anthropic(&response);
            if let (Some(key), Some(fp)) = (idem_scope.as_deref(), idem_fp.as_deref()) {
                if let Err(e) = idempotency::store_json(&state, key, fp, &value) {
                    return (
                        StatusCode::SERVICE_UNAVAILABLE,
                        Json(openai_error(&e, "idempotency_store_unavailable")),
                    )
                        .into_response();
                }
            }
            with_route_header((StatusCode::OK, Json(value)).into_response(), &summary)
        }
        ExecOutcome::Error(e) => render_exec_error(&e),
    };
    with_client_profile_header(
        with_queue_header(
            with_revision_header(with_request_id(response, &trace_id), revision),
            queue_ms,
        ),
        &client_profile,
        "anthropic_messages",
    )
}

/// Anthropic `/v1/messages/count_tokens`. Returns an approximate `input_tokens`
/// (chars/4 heuristic) — the shape Claude Code expects for context budgeting.
pub(crate) async fn count_tokens(
    Extension(_principal): Extension<tenancy::Principal>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    match sb_protocols::anthropic::request_from_anthropic(&body) {
        Ok(req) => {
            let input_tokens = sb_protocols::anthropic::estimate_input_tokens(&req);
            (
                StatusCode::OK,
                Json(serde_json::json!({ "input_tokens": input_tokens })),
            )
                .into_response()
        }
        Err(message) => (
            StatusCode::BAD_REQUEST,
            Json(openai_error(&message, "invalid_request_error")),
        )
            .into_response(),
    }
}
