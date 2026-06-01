use std::time::Instant;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use sb_runtime::ExecOutcome;

use crate::handlers::common::attach_session_metadata;
use crate::http_response::{
    openai_error, render_exec_error, sse_response, with_queue_header, with_request_id,
    with_revision_header, with_route_header,
};
use crate::{idempotency, sse, tenancy, AppState};

pub(crate) async fn chat_completions(
    State(state): State<AppState>,
    Extension(principal): Extension<tenancy::Principal>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let started = Instant::now();
    let idem = idempotency::key_from(&headers);
    let idem_scope = idem
        .as_deref()
        .map(|key| idempotency::scoped_key(key, &principal, "/v1/chat/completions"));
    let idem_fp = idem_scope.as_ref().map(|_| idempotency::fingerprint(&body));
    if let (Some(key), Some(fp)) = (idem_scope.as_deref(), idem_fp.as_deref()) {
        if let Some(resp) = idempotency::precheck(&state, key, fp) {
            return resp;
        }
    }
    let _guard = match idem_scope.as_deref() {
        Some(key) => match state.inflight.try_claim(key) {
            Some(guard) => Some(guard),
            None => return idempotency::in_progress_response(),
        },
        None => None,
    };
    // Global admission (bounded backpressure): wait for an in-flight slot, or 503.
    let (_admit, queue_ms) = match state.admission.acquire().await {
        Ok(slot) => slot,
        Err(resp) => return resp,
    };
    let _conc = match tenancy::admit_concurrency(&state, &principal) {
        Ok(guard) => guard,
        Err(resp) => return resp,
    };
    let mut req = match sb_protocols::openai::request_from_openai_chat(&body) {
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
    let (req_id, req_model) = (req.id.clone(), req.model.clone());
    let trace_id = req.id.clone();
    let (revision, outcome) = state.engine.execute(req, started).await;
    let response = match outcome {
        ExecOutcome::Stream { stream, summary } => {
            let mut encoder = sb_protocols::openai::OpenAiStreamEncoder::new(req_id, req_model);
            let body = sse::body(
                stream,
                // Hold the single-flight + concurrency guards for the stream's life.
                move |event| {
                    let _hold = (&_guard, &_conc, &_admit);
                    encoder.encode(event)
                },
                sse::openai_error_frame,
                Some("data: [DONE]\n\n".to_string()),
            );
            sse_response(body, &summary)
        }
        ExecOutcome::Collected { response, summary } => {
            let value = sb_protocols::openai::response_to_openai_chat(&response);
            if let (Some(key), Some(fp)) = (idem_scope.as_deref(), idem_fp.as_deref()) {
                idempotency::store_json(&state, key, fp, &value);
            }
            with_route_header((StatusCode::OK, Json(value)).into_response(), &summary)
        }
        ExecOutcome::Error(e) => render_exec_error(&e),
    };
    with_queue_header(
        with_revision_header(with_request_id(response, &trace_id), revision),
        queue_ms,
    )
}

pub(crate) async fn responses(
    State(state): State<AppState>,
    Extension(principal): Extension<tenancy::Principal>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let started = Instant::now();
    let idem = idempotency::key_from(&headers);
    let idem_scope = idem
        .as_deref()
        .map(|key| idempotency::scoped_key(key, &principal, "/v1/responses"));
    let idem_fp = idem_scope.as_ref().map(|_| idempotency::fingerprint(&body));
    if let (Some(key), Some(fp)) = (idem_scope.as_deref(), idem_fp.as_deref()) {
        if let Some(resp) = idempotency::precheck(&state, key, fp) {
            return resp;
        }
    }
    let _guard = match idem_scope.as_deref() {
        Some(key) => match state.inflight.try_claim(key) {
            Some(guard) => Some(guard),
            None => return idempotency::in_progress_response(),
        },
        None => None,
    };
    // Global admission (bounded backpressure): wait for an in-flight slot, or 503.
    let (_admit, queue_ms) = match state.admission.acquire().await {
        Ok(slot) => slot,
        Err(resp) => return resp,
    };
    let _conc = match tenancy::admit_concurrency(&state, &principal) {
        Ok(guard) => guard,
        Err(resp) => return resp,
    };
    let mut req = match sb_protocols::responses::request_from_openai_responses(&body) {
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
    let (req_id, req_model) = (req.id.clone(), req.model.clone());
    let trace_id = req.id.clone();
    let (revision, outcome) = state.engine.execute(req, started).await;
    let response = match outcome {
        ExecOutcome::Stream { stream, summary } => {
            let mut encoder =
                sb_protocols::responses::OpenAiResponsesStreamEncoder::new(req_id, req_model);
            let body = sse::body(
                stream,
                move |event| {
                    let _hold = (&_guard, &_conc, &_admit);
                    encoder.encode(event)
                },
                sse::responses_error_frame,
                None,
            );
            sse_response(body, &summary)
        }
        ExecOutcome::Collected { response, summary } => {
            let value = sb_protocols::responses::response_to_openai_responses(&response);
            if let (Some(key), Some(fp)) = (idem_scope.as_deref(), idem_fp.as_deref()) {
                idempotency::store_json(&state, key, fp, &value);
            }
            with_route_header((StatusCode::OK, Json(value)).into_response(), &summary)
        }
        ExecOutcome::Error(e) => render_exec_error(&e),
    };
    with_queue_header(
        with_revision_header(with_request_id(response, &trace_id), revision),
        queue_ms,
    )
}
