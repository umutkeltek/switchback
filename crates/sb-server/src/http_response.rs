use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use sb_runtime::ExecError;
use std::time::Duration;

pub(crate) fn openai_error(message: &str, type_: &str) -> serde_json::Value {
    serde_json::json!({"error": {"message": message, "type": type_}})
}

pub(crate) fn with_route_header(mut response: Response, summary: &str) -> Response {
    if let Ok(value) = HeaderValue::from_str(summary) {
        response.headers_mut().insert("x-switchback-route", value);
    }
    response
}

/// Stamp the native client compatibility profile that served this request.
pub(crate) fn with_client_profile_header(
    mut response: Response,
    profile: &str,
    protocol: &str,
) -> Response {
    if let Ok(value) = HeaderValue::from_str(profile) {
        response
            .headers_mut()
            .insert("x-switchback-client-profile", value);
    }
    if let Ok(value) = HeaderValue::from_str(protocol) {
        response
            .headers_mut()
            .insert("x-switchback-client-protocol", value);
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

/// Stamp both the standard whole-second retry hint and the millisecond hint
/// understood by Claude/OpenAI SDKs. `Retry-After` rounds up so standards-only
/// clients never retry before the typed cooldown has elapsed.
fn with_retry_after_headers(mut response: Response, retry_after: Duration) -> Response {
    let retry_after_seconds = retry_after
        .as_secs()
        .saturating_add(u64::from(retry_after.subsec_nanos() > 0));
    if let Ok(value) = HeaderValue::from_str(&retry_after_seconds.to_string()) {
        response.headers_mut().insert("retry-after", value);
    }
    if let Ok(value) = HeaderValue::from_str(&retry_after.as_millis().to_string()) {
        response.headers_mut().insert("retry-after-ms", value);
    }
    response
}

/// Render a runtime [`ExecError`] as an HTTP response in the OpenAI error shape.
pub(crate) fn render_exec_error(error: &ExecError) -> Response {
    let status = StatusCode::from_u16(error.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut response = (
        status,
        Json(openai_error(&error.message, &error.error_type)),
    )
        .into_response();
    if let Some(retry_after) = error.retry_after {
        response = with_retry_after_headers(response, retry_after);
    }
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::render_exec_error;
    use sb_runtime::ExecError;

    #[test]
    fn exec_error_retry_timing_is_rendered_for_standard_and_claude_clients() {
        let response = render_exec_error(
            &ExecError::new(503, "provider_unavailable", "cooling down", None)
                .with_retry_after(Duration::from_millis(1_501)),
        );

        assert_eq!(response.headers()["retry-after"], "2");
        assert_eq!(response.headers()["retry-after-ms"], "1501");
    }
}
