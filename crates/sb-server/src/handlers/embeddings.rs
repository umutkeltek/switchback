use std::time::Instant;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;

use crate::handlers::common::session_id_from_headers;
use crate::http_response::{
    render_exec_error, with_queue_header, with_request_id, with_revision_header, with_route_header,
};
use crate::{tenancy, AppState};

pub(crate) async fn embeddings(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let started = Instant::now();
    let principal = match tenancy::authenticate(&state, &headers) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let (_admit, queue_ms) = match state.admission.acquire().await {
        Ok(slot) => slot,
        Err(resp) => return resp,
    };
    let _conc = match tenancy::admit_concurrency(&state, &principal) {
        Ok(guard) => guard,
        Err(resp) => return resp,
    };

    let (revision, outcome) = state
        .engine
        .execute_embeddings(
            body,
            principal.tenant,
            principal.project,
            session_id_from_headers(&headers),
            started,
        )
        .await;
    let (response, request_id) = match outcome {
        sb_runtime::EmbeddingsOutcome::Json {
            value,
            summary,
            request_id,
        } => (
            with_route_header((StatusCode::OK, Json(value)).into_response(), &summary),
            request_id,
        ),
        sb_runtime::EmbeddingsOutcome::Error { error, request_id } => {
            (render_exec_error(&error), request_id)
        }
    };
    with_queue_header(
        with_revision_header(with_request_id(response, &request_id), revision),
        queue_ms,
    )
}
