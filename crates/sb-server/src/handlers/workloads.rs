use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde_json::json;

use crate::workloads::WorkloadError;
use crate::{tenancy, AppState};

pub(crate) async fn images_generations(
    State(state): State<AppState>,
    Extension(principal): Extension<tenancy::Principal>,
    Json(body): Json<sb_core::ImageGenerationRequest>,
) -> Response {
    let model = body.model.clone();
    let snapshot = state.snapshot();
    let job = match state
        .workloads
        .create_image_job(
            &snapshot.config,
            &snapshot.resolver,
            &snapshot.registry,
            &state.ledger,
            body,
            principal.tenant.clone(),
            principal.project.clone(),
        )
        .await
    {
        Ok(job) => job,
        Err(error) => {
            let status = match error {
                WorkloadError::InvalidRequest(_) => StatusCode::BAD_REQUEST,
                WorkloadError::Upstream(_) => StatusCode::BAD_GATEWAY,
                WorkloadError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            };
            return (
                status,
                Json(json!({"error": {"message": error.message(), "type": "workload_error"}})),
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
    let cfg = state.snapshot().config.clone();
    let workflows = state.workloads.workflows(&cfg);
    Json(json!({
        "object": "list",
        "data": state.workloads.jobs(),
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

pub(crate) async fn cancel_job(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let snapshot = state.snapshot();
    match state
        .workloads
        .cancel_job(&snapshot.config, &snapshot.resolver, &id)
        .await
    {
        Ok(job) => Json(job).into_response(),
        Err(error) => {
            let status = match error {
                WorkloadError::InvalidRequest(_) => StatusCode::BAD_REQUEST,
                WorkloadError::Upstream(_) => StatusCode::BAD_GATEWAY,
                WorkloadError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            };
            (
                status,
                Json(json!({"error": {"message": error.message(), "type": "workload_error"}})),
            )
                .into_response()
        }
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
    let cfg = state.snapshot().config.clone();
    Json(json!({
        "object": "list",
        "data": state.workloads.workflows(&cfg),
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
