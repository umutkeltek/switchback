use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde_json::json;

use futures::StreamExt;
use tokio::io::AsyncReadExt;

use crate::artifact_store::Owner;
use crate::artifact_store::{ArtifactBody, ArtifactError, Missing};
use crate::workloads::{JobContext, WorkloadError};
use crate::{tenancy, AppState};

pub(crate) async fn images_generations(
    State(state): State<AppState>,
    Extension(principal): Extension<tenancy::Principal>,
    Json(body): Json<sb_core::ImageGenerationRequest>,
) -> Response {
    let model = body.model.clone();
    let job = match state
        .workloads
        .create_image_job(job_context(&state), body, owner_of(&principal))
        .await
    {
        Ok(job) => job,
        Err(error) => return workload_error(error),
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

fn job_context(state: &AppState) -> JobContext {
    JobContext {
        snapshot: state.snapshot(),
        ledger: state.ledger.clone(),
        capacity: state.capacity.clone(),
    }
}

fn owner_of(principal: &tenancy::Principal) -> Owner {
    Owner::new(principal.tenant.clone(), principal.project.clone())
}

fn workload_error(error: WorkloadError) -> Response {
    let (status, kind) = match error {
        WorkloadError::InvalidRequest(_) => (StatusCode::BAD_REQUEST, "workload_error"),
        WorkloadError::TooManyJobs(_) => (StatusCode::TOO_MANY_REQUESTS, "rate_limit_error"),
        WorkloadError::Upstream(_) => (StatusCode::BAD_GATEWAY, "workload_error"),
        WorkloadError::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "workload_error"),
    };
    (
        status,
        Json(json!({"error": {"message": error.message(), "type": kind}})),
    )
        .into_response()
}

/// `POST /v1/jobs`: start a job in the background. Answers 202 at once with
/// the job (`accepted`); follow it with `GET /v1/jobs/{id}` or its events.
pub(crate) async fn submit_job(
    State(state): State<AppState>,
    Extension(principal): Extension<tenancy::Principal>,
    Json(body): Json<sb_core::JobSubmitRequest>,
) -> Response {
    let reader = principal.clone();
    match state.workloads.submit_job(
        job_context(&state),
        body,
        owner_of(&principal),
        move |owner| may_read(&reader, &owner.tenant, &owner.project),
    ) {
        Ok(job) => (StatusCode::ACCEPTED, Json(job)).into_response(),
        Err(error) => workload_error(error),
    }
}

/// `POST /v1/uploads`: store an input image for a later job. The body is
/// the image itself (`Content-Type: image/png`, `image/jpeg` or
/// `image/webp`); the answer (201) is its artifact record, whose
/// `artifact_id` a job names in `inputs.image`. It belongs to the caller's
/// tenant and project and follows the artifact retention policy.
/// A raw body (not base64 inside the job, not a multipart form) keeps the
/// job request small and typed, lets one upload feed several jobs, and
/// needs no parser.
/// The body arrives as a stream and goes into the store as it comes, so it
/// is never held in memory whole on a disk store; one larger than
/// `server.artifacts.max_upload_bytes` (or `server.max_request_bytes`, when
/// lower) is refused with 413, and one whose first bytes are not the
/// declared image format with 415.
pub(crate) async fn upload(
    State(state): State<AppState>,
    Extension(principal): Extension<tenancy::Principal>,
    headers: axum::http::HeaderMap,
    body: Body,
) -> Response {
    let media_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            value
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase()
        })
        .unwrap_or_default();
    if !matches!(
        media_type.as_str(),
        "image/png" | "image/jpeg" | "image/webp"
    ) {
        return (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Json(json!({"error": {
                "message": "upload an image: Content-Type image/png, image/jpeg or image/webp",
                "type": "invalid_request_error"
            }})),
        )
            .into_response();
    }
    let server = state.snapshot().config.server.clone();
    let max_upload = server
        .max_request_bytes
        .map_or(server.artifacts.max_upload_bytes, |max| {
            max.min(server.artifacts.max_upload_bytes)
        });
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    if declared.is_some_and(|length| length > max_upload) {
        return upload_too_large(max_upload);
    }
    let record = sb_core::ArtifactRecord {
        artifact_id: sb_core::new_id("art"),
        job_id: String::new(),
        kind: sb_core::ArtifactKind::Image,
        media_type: media_type.clone(),
        bytes: 0,
        sha256: String::new(),
        storage_ref: String::new(),
        width: None,
        height: None,
        duration_ms: None,
        fps: None,
        created_at_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis() as u64)
            .unwrap_or(0),
        retention: String::new(),
        provenance: std::collections::BTreeMap::from([(
            "source".to_string(),
            "upload".to_string(),
        )]),
    };
    let mut writer = match state
        .workloads
        .artifacts()
        .writer(record, owner_of(&principal))
    {
        Ok(writer) => writer,
        Err(error) => return workload_error(WorkloadError::Internal(error.to_string())),
    };
    // A dropped writer (any early return below) deletes what it wrote.
    let mut received: u64 = 0;
    let mut head: Vec<u8> = Vec::with_capacity(12);
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(_) => {
                return workload_error(WorkloadError::InvalidRequest(
                    "the upload's body could not be read".to_string(),
                ))
            }
        };
        received = received.saturating_add(chunk.len() as u64);
        if received > max_upload {
            return upload_too_large(max_upload);
        }
        if head.len() < 12 {
            let take = (12 - head.len()).min(chunk.len());
            head.extend_from_slice(&chunk[..take]);
        }
        match writer.write(&chunk) {
            Ok(()) => {}
            Err(error @ ArtifactError::TooLarge { .. }) => {
                return (
                    StatusCode::PAYLOAD_TOO_LARGE,
                    Json(json!({"error": {"message": error.to_string(), "type": "invalid_request_error"}})),
                )
                    .into_response()
            }
            Err(error) => return workload_error(WorkloadError::Internal(error.to_string())),
        }
    }
    if received == 0 {
        return workload_error(WorkloadError::InvalidRequest(
            "the upload is empty".to_string(),
        ));
    }
    if !image_bytes_match(&media_type, &head) {
        return (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Json(json!({"error": {
                "message": format!("the upload's bytes are not a {media_type} image"),
                "type": "invalid_request_error"
            }})),
        )
            .into_response();
    }
    match writer.finish() {
        Ok(record) => (StatusCode::CREATED, Json(record)).into_response(),
        Err(error @ ArtifactError::TooLarge { .. }) => (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({"error": {"message": error.to_string(), "type": "invalid_request_error"}})),
        )
            .into_response(),
        Err(error) => workload_error(WorkloadError::Internal(error.to_string())),
    }
}

fn upload_too_large(max_upload: u64) -> Response {
    (
        StatusCode::PAYLOAD_TOO_LARGE,
        Json(json!({"error": {
            "message": format!(
                "the upload is larger than {max_upload} bytes (server.artifacts.max_upload_bytes)"
            ),
            "type": "invalid_request_error"
        }})),
    )
        .into_response()
}

/// Whether an upload's first bytes are the image format its `Content-Type`
/// names (PNG signature, JPEG start-of-image, RIFF/WEBP).
fn image_bytes_match(media_type: &str, head: &[u8]) -> bool {
    match media_type {
        "image/png" => head.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]),
        "image/jpeg" => head.starts_with(&[0xFF, 0xD8, 0xFF]),
        "image/webp" => head.len() >= 12 && &head[..4] == b"RIFF" && &head[8..12] == b"WEBP",
        _ => false,
    }
}

/// Whether `principal` may see what a job made by `tenant`/`project` holds:
/// operator and admin keys see every job; a client key sees the jobs its own
/// tenant and project made. Everyone else gets "not found", never
/// "forbidden", so a job id does not confirm that the job exists.
pub(crate) fn may_read(
    principal: &tenancy::Principal,
    tenant: &Option<String>,
    project: &Option<String>,
) -> bool {
    principal.is_operator_or_admin()
        || (principal.tenant == *tenant && principal.project == *project)
}

fn readable_job(
    state: &AppState,
    principal: &tenancy::Principal,
    id: &str,
) -> Option<sb_core::JobRecord> {
    state
        .workloads
        .job(id)
        .filter(|job| may_read(principal, &job.tenant, &job.project))
}

pub(crate) async fn jobs(
    State(state): State<AppState>,
    Extension(principal): Extension<tenancy::Principal>,
) -> Response {
    let cfg = state.snapshot().config.clone();
    let workflows = state.workloads.workflows(&cfg);
    let jobs: Vec<_> = state
        .workloads
        .jobs()
        .into_iter()
        .filter(|job| may_read(&principal, &job.tenant, &job.project))
        .collect();
    Json(json!({
        "object": "list",
        "data": jobs,
        "available_workflows": workflows.len(),
    }))
    .into_response()
}

pub(crate) async fn job_by_id(
    State(state): State<AppState>,
    Extension(principal): Extension<tenancy::Principal>,
    Path(id): Path<String>,
) -> Response {
    match readable_job(&state, &principal, &id) {
        Some(job) => Json(job).into_response(),
        None => not_found("job not found"),
    }
}

/// Cancel a job. A client key may cancel the jobs its tenant and project
/// made; another's answers 404 like a read.
pub(crate) async fn cancel_job(
    State(state): State<AppState>,
    Extension(principal): Extension<tenancy::Principal>,
    Path(id): Path<String>,
) -> Response {
    if readable_job(&state, &principal, &id).is_none() {
        return not_found("job not found");
    }
    let snapshot = state.snapshot();
    match state
        .workloads
        .cancel_job(&snapshot.config, &snapshot.resolver, &id)
        .await
    {
        Ok(job) => Json(job).into_response(),
        Err(error) => workload_error(error),
    }
}

pub(crate) async fn job_events(
    State(state): State<AppState>,
    Extension(principal): Extension<tenancy::Principal>,
    Path(id): Path<String>,
) -> Response {
    let Some(job) = readable_job(&state, &principal, &id) else {
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

/// An artifact's body, streamed from the store (a large video is never read
/// into memory whole). An artifact removed by the retention policy answers
/// 404 `artifact_evicted`; one never stored answers 404 `not_found`.
pub(crate) async fn artifact_by_id(
    State(state): State<AppState>,
    Extension(principal): Extension<tenancy::Principal>,
    Path(id): Path<String>,
) -> Response {
    let opened = match state.workloads.artifacts().open(&id) {
        Ok(opened) if may_read(&principal, &opened.owner.tenant, &opened.owner.project) => opened,
        Err(Missing::Evicted(owner)) if may_read(&principal, &owner.tenant, &owner.project) => {
            return evicted()
        }
        Ok(_) | Err(_) => return not_found("artifact not found"),
    };
    let length = opened.record.bytes;
    let body = match opened.body {
        ArtifactBody::Memory(bytes) => Body::from(bytes.as_ref().clone()),
        ArtifactBody::File(file) => Body::from_stream(file_chunks(tokio::fs::File::from_std(file))),
    };
    let (content_type, disposition) = served_media_type(&opened.record.media_type);
    let mut response = (
        [
            (header::CONTENT_TYPE, content_type.to_string()),
            (header::CONTENT_LENGTH, length.to_string()),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
            // Range requests are not served: the whole body, always.
            (header::ACCEPT_RANGES, "none".to_string()),
        ],
        body,
    )
        .into_response();
    if let Some(disposition) = disposition {
        response
            .headers_mut()
            .insert(header::CONTENT_DISPOSITION, disposition);
    }
    response
}

/// The `Content-Type` an artifact is served with, and a
/// `Content-Disposition: attachment` for anything that is not an image or a
/// video on the allow-list: a stored media type is never trusted to be safe
/// to render from the gateway's own origin.
fn served_media_type(media_type: &str) -> (&str, Option<header::HeaderValue>) {
    if crate::workloads::is_safe_media_type(media_type) {
        (media_type, None)
    } else {
        (
            "application/octet-stream",
            Some(header::HeaderValue::from_static("attachment")),
        )
    }
}

/// A file as a stream of 64 KiB chunks.
fn file_chunks(
    file: tokio::fs::File,
) -> impl futures::Stream<Item = std::io::Result<Vec<u8>>> + Send + 'static {
    futures::stream::try_unfold(file, |mut file| async move {
        let mut chunk = vec![0u8; 64 * 1024];
        let read = file.read(&mut chunk).await?;
        if read == 0 {
            return Ok(None);
        }
        chunk.truncate(read);
        Ok(Some((chunk, file)))
    })
}

fn evicted() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({"error": {
            "message": "artifact was removed by the artifact retention policy (server.artifacts)",
            "type": "artifact_evicted"
        }})),
    )
        .into_response()
}

pub(crate) async fn artifact_thumb(
    State(state): State<AppState>,
    principal: Extension<tenancy::Principal>,
    Path(id): Path<String>,
) -> Response {
    artifact_by_id(State(state), principal, Path(id)).await
}

pub(crate) async fn workflows(State(state): State<AppState>) -> Response {
    let cfg = state.snapshot().config.clone();
    Json(json!({
        "object": "list",
        "data": state.workloads.workflows(&cfg),
    }))
    .into_response()
}

/// Live per-lane local-executor capacity: current state, queue depth, in-flight
/// count, retries used, wake attempts, last wake result, last error, the
/// power-off countdown and the machine's holds. This is the running-server
/// view (the CLI `sb doctor local` is the stateless config + health
/// snapshot). Metadata only — no prompts or media bodies.
///
/// A client key gets a read-only view: per lane its name, machine, state,
/// level, queue depth, in-flight count, power-off countdown and its own
/// holds. No details, errors, commands, artifact store or alert state, and
/// nothing of another tenant.
pub(crate) async fn capacity(
    State(state): State<AppState>,
    Extension(principal): Extension<tenancy::Principal>,
) -> Response {
    let reports = state.capacity.reports();
    if !principal.is_operator_or_admin() {
        let data: Vec<_> = reports
            .iter()
            .map(|report| {
                let holds: Vec<_> = report
                    .holds
                    .iter()
                    .filter(|hold| may_read(&principal, &hold.owner, &hold.project))
                    .collect();
                json!({
                    "name": report.name,
                    "host": report.host.clone().unwrap_or_else(|| report.name.clone()),
                    "state": report.state,
                    "level": report.level,
                    "queue_depth": report.queue_depth,
                    "in_flight": report.in_flight,
                    "poweroff_in_secs": report.poweroff_in_secs,
                    "holds": holds,
                })
            })
            .collect();
        return Json(json!({"object": "list", "view": "client", "data": data})).into_response();
    }
    let holds = state.capacity.holds();
    Json(json!({
        "object": "list",
        "data": reports,
        "artifacts": state.workloads.artifacts().status(),
        "holds": {
            "durable": holds.durable(),
            "last_save_error": holds.last_save_error(),
            "load_error": holds.load_error(),
        },
        "alerts": state.capacity.alerts().status(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_allow_listed_media_types_are_served_inline() {
        assert_eq!(served_media_type("image/png"), ("image/png", None));
        assert_eq!(served_media_type("video/mp4"), ("video/mp4", None));
        for stored in ["text/html", "image/svg+xml", "application/javascript", ""] {
            let (content_type, disposition) = served_media_type(stored);
            assert_eq!(content_type, "application/octet-stream", "{stored}");
            assert_eq!(disposition.unwrap(), "attachment", "{stored}");
        }
    }
}
