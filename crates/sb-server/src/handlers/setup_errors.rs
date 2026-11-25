use axum::body::{to_bytes, Body};
use axum::extract::Request;
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use serde_json::{json, Value};

use super::model_settings::{parse_model_path, ModelPath};

enum Surface {
    Settings,
    Roles,
    Setup,
}

impl Surface {
    fn from_path(path: &str) -> Option<Self> {
        if path == "/v1/setup" || path.starts_with("/v1/setup/") {
            Some(Self::Setup)
        } else if path == "/v1/roles" || path.starts_with("/v1/roles/") {
            Some(Self::Roles)
        } else if let Some(tail) = path.strip_prefix("/v1/models/") {
            matches!(
                parse_model_path(tail),
                Some(ModelPath::Settings(_) | ModelPath::History(_) | ModelPath::Revert(_))
            )
            .then_some(Self::Settings)
        } else {
            None
        }
    }

    fn next(&self, status: StatusCode, kind: &str) -> &'static str {
        match (status, kind, self) {
            (StatusCode::UNAUTHORIZED, _, _) => {
                "Send a valid gateway key in the Authorization bearer header."
            }
            (StatusCode::FORBIDDEN, _, _) => "Use a gateway key with the operator or admin role.",
            (StatusCode::METHOD_NOT_ALLOWED, _, _) => {
                "Use an HTTP method listed in the Allow response header."
            }
            (StatusCode::PAYLOAD_TOO_LARGE, _, _) => {
                "Send a smaller request, splitting setup into smaller groups if needed."
            }
            (StatusCode::UNSUPPORTED_MEDIA_TYPE, _, _) => {
                "Set Content-Type to application/json and send a JSON object."
            }
            (StatusCode::CONFLICT | StatusCode::PRECONDITION_REQUIRED, _, _) => {
                "Read the current document and revision, then resend your edit with that revision or resend a document equal to the current one."
            }
            (StatusCode::LOCKED, _, Self::Roles) => {
                "Unlock the name with POST /v1/roles/{name}/unlock before changing its targets."
            }
            (StatusCode::LOCKED, _, _) => {
                "Remove the reported key from locked and lock_reasons in a separate revision before changing its value."
            }
            (_, "model_not_found" | "unknown_target", _) => {
                "List targets with GET /v1/models and GET /v1/engines and retry with a valid provider/model."
            }
            (_, "ambiguous_model", _) => {
                "Pick a concrete provider/model from the targets listed in the error."
            }
            (_, "unknown_profile", _) => "Choose a listed profile or omit the profile query parameter.",
            (_, "role_not_found", _) => "List available names with GET /v1/roles.",
            (_, "check_failed", _) => {
                "Inspect GET /v1/engines and retry after the first target can answer requests."
            }
            (_, _, Self::Setup) => {
                "Use GET /v1/setup/schema to fix the reported field, then retry with POST /v1/setup/plan."
            }
            (StatusCode::NOT_FOUND, _, Self::Roles) => {
                "Read GET /v1/roles/{name} for valid revisions and use revert, lock, or unlock for actions."
            }
            (StatusCode::NOT_FOUND, _, Self::Settings) => {
                "Read GET /v1/models/{target}/settings/history for valid revisions and use settings/revert to restore one."
            }
            (_, _, Self::Roles) => {
                "Fix the field named in the error and read GET /v1/roles/{name} for the current targets and revision."
            }
            (_, _, Self::Settings) => {
                "Fix the field named in the error and read GET /v1/models/{target}/settings for the current document and revision."
            }
        }
    }
}

pub(crate) async fn add_next(request: Request, next: Next) -> Response {
    let surface = Surface::from_path(request.uri().path());
    let response = next.run(request).await;
    let Some(surface) = surface.filter(|_| response.status().is_client_error()) else {
        return response;
    };
    let (mut parts, body) = response.into_parts();
    let bytes = to_bytes(body, usize::MAX).await.unwrap_or_default();
    let mut value = serde_json::from_slice::<Value>(&bytes)
        .ok()
        .filter(|value| value["error"].is_object())
        .unwrap_or_else(|| {
            let message = String::from_utf8_lossy(&bytes);
            let message = if message.is_empty() {
                parts
                    .status
                    .canonical_reason()
                    .unwrap_or("Request rejected")
            } else {
                &message
            };
            json!({"error": {"message": message, "type": "invalid_request_error"}})
        });
    let kind = value["error"]["type"].as_str().unwrap_or_default();
    value["error"]["next"] = json!(surface.next(parts.status, kind));
    parts.headers.remove(header::CONTENT_LENGTH);
    parts.headers.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/json"),
    );
    Response::from_parts(parts, Body::from(value.to_string()))
}
