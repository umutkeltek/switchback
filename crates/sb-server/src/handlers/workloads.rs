use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde_json::json;

use crate::{tenancy, AppState};

pub(crate) async fn images_generations(
    State(state): State<AppState>,
    Extension(principal): Extension<tenancy::Principal>,
    Json(body): Json<sb_core::ImageGenerationRequest>,
) -> Response {
    let model = body.model.clone();
    let job = match state.workloads.create_mock_image_job(
        body,
        principal.tenant.clone(),
        principal.project.clone(),
    ) {
        Ok(job) => job,
        Err(message) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": {"message": message, "type": "invalid_request_error"}})),
            )
                .into_response();
        }
    };

    let data: Vec<_> = job
        .artifacts
        .iter()
        .map(|artifact| {
            json!({
                "url": format!("/v1/artifacts/{}", artifact.artifact_id),
                "artifact_id": artifact.artifact_id,
                "media_type": artifact.media_type,
                "width": artifact.width,
                "height": artifact.height,
            })
        })
        .collect();

    Json(json!({
        "object": "image.generation",
        "created": job.created_at_ms / 1000,
        "model": model,
        "job": {
            "id": job.id,
            "kind": job.kind,
            "status": job.status,
        },
        "data": data,
    }))
    .into_response()
}

pub(crate) async fn jobs(State(state): State<AppState>) -> Response {
    let workflows = state.workloads.workflows();
    Json(json!({
        "object": "list",
        "data": [],
        "available_workflows": workflows.len(),
    }))
    .into_response()
}

pub(crate) async fn job_by_id(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match state.workloads.job(&id) {
        Some(job) => Json(job).into_response(),
        None => not_found("job not found"),
    }
}

pub(crate) async fn job_events(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let Some(job) = state.workloads.job(&id) else {
        return not_found("job not found");
    };
    let mut body = String::new();
    for event in job.events {
        let data = serde_json::to_string(&event).unwrap_or_else(|_| "{}".to_string());
        body.push_str(&format!("event: {}\n", event.event));
        body.push_str(&format!("data: {data}\n\n"));
    }
    ([(header::CONTENT_TYPE, "text/event-stream")], body).into_response()
}

pub(crate) async fn artifact_by_id(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    match state.workloads.artifact(&id) {
        Some(artifact) => (
            [(header::CONTENT_TYPE, artifact.record.media_type)],
            artifact.bytes,
        )
            .into_response(),
        None => not_found("artifact not found"),
    }
}

pub(crate) async fn artifact_thumb(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    artifact_by_id(State(state), Path(id)).await
}

pub(crate) async fn workflows(State(state): State<AppState>) -> Response {
    Json(json!({
        "object": "list",
        "data": state.workloads.workflows(),
    }))
    .into_response()
}

fn not_found(message: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({"error": {"message": message, "type": "not_found"}})),
    )
        .into_response()
}
