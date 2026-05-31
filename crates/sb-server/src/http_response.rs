use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use sb_runtime::ExecError;

pub(crate) fn openai_error(message: &str, type_: &str) -> serde_json::Value {
    serde_json::json!({"error": {"message": message, "type": type_}})
}

pub(crate) fn with_route_header(mut response: Response, summary: &str) -> Response {
    if let Ok(value) = HeaderValue::from_str(summary) {
        response.headers_mut().insert("x-switchback-route", value);
    }
    response
}

/// Stamp the request id on a response so clients can correlate it with the
/// `GET /v1/traces/{id}` record (the trace key == this id).
pub(crate) fn with_request_id(mut response: Response, request_id: &str) -> Response {
    if let Ok(value) = HeaderValue::from_str(request_id) {
        response
            .headers_mut()
            .insert("x-switchback-request-id", value);
    }
    response
}

/// Stamp the compiled-snapshot revision this request was pinned to, so a client
/// can tell which config generation served it.
pub(crate) fn with_revision_header(mut response: Response, revision: u64) -> Response {
    if let Ok(value) = HeaderValue::from_str(&revision.to_string()) {
        response
            .headers_mut()
            .insert("x-switchback-revision", value);
    }
    response
}

/// Stamp how long the request queued for a global admission slot.
pub(crate) fn with_queue_header(mut response: Response, queue_ms: u64) -> Response {
    if queue_ms > 0 {
        if let Ok(value) = HeaderValue::from_str(&queue_ms.to_string()) {
            response
                .headers_mut()
                .insert("x-switchback-queue-ms", value);
        }
    }
    response
}

/// Render a runtime [`ExecError`] as an HTTP response in the OpenAI error shape.
pub(crate) fn render_exec_error(error: &ExecError) -> Response {
    let status = StatusCode::from_u16(error.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let response = (
        status,
        Json(openai_error(&error.message, &error.error_type)),
    )
        .into_response();
    match &error.summary {
        Some(summary) => with_route_header(response, summary),
        None => response,
    }
}

pub(crate) fn sse_response(body: axum::body::Body, summary: &str) -> Response {
    match Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .body(body)
    {
        Ok(response) => with_route_header(response, summary),
        Err(_) => with_route_header(
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(openai_error(
                    "failed to build stream response",
                    "upstream_error",
                )),
            )
                .into_response(),
            summary,
        ),
    }
}
