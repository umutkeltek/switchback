use axum::extract::State;
use axum::http::Method;
use axum::response::Response;

use crate::{tenancy, AppState};

/// `/v1/hosts/{host}/holds` (`{id}` with `with_id`).
fn holds_path(path: &str, with_id: bool) -> bool {
    let Some(rest) = path.strip_prefix("/v1/hosts/") else {
        return false;
    };
    let parts: Vec<&str> = rest.split('/').collect();
    match parts.as_slice() {
        [host, "holds"] => !with_id && !host.is_empty(),
        [host, "holds", id] => with_id && !host.is_empty() && !id.is_empty(),
        _ => false,
    }
}

/// `/v1/models/{model}/settings` (a model name may hold `/`).
fn model_settings_path(path: &str) -> bool {
    path.strip_prefix("/v1/models/")
        .and_then(|rest| rest.strip_suffix("/settings"))
        .is_some_and(|name| !name.is_empty())
}

/// Model settings writes and engine control: `PUT`/`PATCH`
/// `/v1/models/{model}/settings`, `POST .../settings/revert`, `.../load`,
/// `.../unload`. Operator and admin keys only.
fn model_control_write(method: &Method, path: &str) -> bool {
    let Some(rest) = path.strip_prefix("/v1/models/") else {
        return false;
    };
    let named = |suffix: &str| {
        rest.strip_suffix(suffix)
            .is_some_and(|name| !name.is_empty())
    };
    (matches!(*method, Method::PUT | Method::PATCH) && named("/settings"))
        || (method == Method::POST
            && (named("/settings/revert") || named("/load") || named("/unload")))
}

/// `/v1/hosts/{host}/power` — a machine's power is the operator's
/// business: operator and admin keys only, never a client key.
fn power_path(path: &str) -> bool {
    let Some(rest) = path.strip_prefix("/v1/hosts/") else {
        return false;
    };
    let parts: Vec<&str> = rest.split('/').collect();
    matches!(parts.as_slice(), [host, "power"] if !host.is_empty())
}

/// Client-facing endpoints that tenant/client keys may use. Job, artifact
/// and hold reads are on it: the handlers then show a client only what its
/// own tenant and project made (another's answers 404). The capacity report
/// is on it too: a client key gets its read-only view. So is the per-target
/// speed summary (`/v1/targets/stats`), which is read-only aggregate
/// metadata scoped to the key's own tenant.
fn client_surface(method: &Method, path: &str) -> bool {
    if (matches!(*method, Method::GET | Method::POST) && holds_path(path, false))
        || (matches!(*method, Method::PATCH | Method::DELETE) && holds_path(path, true))
    {
        return true;
    }
    (method == Method::GET
        && (path == "/v1/models"
            || model_settings_path(path)
            || path == "/v1/workloads/capacity"
            || path == "/v1/targets/stats"
            || path == "/v1/optimize/report"
            || path == "/v1/jobs"
            || path.starts_with("/v1/jobs/")
            || path.starts_with("/v1/artifacts/")))
        || (method == Method::POST
            && (matches!(
                path,
                "/v1/chat/completions"
                    | "/v1/responses"
                    | "/v1/messages"
                    | "/v1/messages/count_tokens"
                    | "/v1/embeddings"
                    | "/v1/audio/transcriptions"
                    | "/v1/audio/translations"
                    | "/v1/audio/speech"
                    | "/v1/images/generations"
                    | "/v1/jobs"
                    | "/v1/uploads"
            ) || (path.starts_with("/v1/jobs/") && path.ends_with("/cancel"))))
}

/// Read-only operator surfaces plus safe previews/validation.
fn operator_surface(method: &Method, path: &str) -> bool {
    if client_surface(method, path) {
        return true;
    }
    if method == Method::GET && (path.starts_with("/v1/") || path.starts_with("/cp/v1")) {
        return true;
    }
    if model_control_write(method, path) {
        return true;
    }
    // `sb ai tune` / `sb ai eval`: tuning locks, the experiment ledger and
    // the quality ledger.
    if path.starts_with("/v1/optimize/locks")
        || path == "/v1/optimize/experiments"
        || path == "/v1/optimize/quality"
    {
        return matches!(*method, Method::POST | Method::PATCH | Method::DELETE);
    }
    method == Method::POST
        && (matches!(path, "/cp/v1/route-preview" | "/cp/v1/admission-preview")
            || (path.starts_with("/v1/jobs/") && path.ends_with("/cancel"))
            || power_path(path)
            || (path.starts_with("/cp/v1/drafts/") && path.ends_with("/validate")))
}

fn unauthorized_role(
    principal: &tenancy::Principal,
    method: &Method,
    path: &str,
) -> Option<Response> {
    if principal.is_admin()
        || (principal.is_operator_or_admin() && operator_surface(method, path))
        || client_surface(method, path)
    {
        None
    } else {
        Some(tenancy::forbidden())
    }
}

/// Auth gate for every endpoint except the public shell (`/`, `/health`). When
/// no `api_key`/`api_keys` is configured the gateway is open (local default);
/// when one is, all `/v1/*` and `/cp/v1/*` endpoints require it. Multi-tenant
/// keys are also role-checked here before handlers run.
pub async fn require_auth(
    State(state): State<AppState>,
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    if path == "/" || path == "/health" || path.starts_with("/requests/") {
        return next.run(req).await;
    }
    match tenancy::authenticate(&state, req.headers()) {
        Ok(principal) => {
            if let Some(resp) = unauthorized_role(&principal, &method, &path) {
                return resp;
            }
            req.extensions_mut().insert(principal);
            next.run(req).await
        }
        Err(resp) => resp,
    }
}
