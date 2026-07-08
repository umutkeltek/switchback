use axum::extract::State;
use axum::http::Method;
use axum::response::Response;

use crate::{tenancy, AppState};

/// Client-facing endpoints that tenant/client keys may use.
fn client_surface(method: &Method, path: &str) -> bool {
    (method == Method::GET && path == "/v1/models")
        || (method == Method::POST
            && matches!(
                path,
                "/v1/chat/completions"
                    | "/v1/responses"
                    | "/v1/messages"
                    | "/v1/messages/count_tokens"
                    | "/v1/embeddings"
                    | "/v1/images/generations"
            ))
}

/// Read-only operator surfaces plus safe previews/validation.
fn operator_surface(method: &Method, path: &str) -> bool {
    if client_surface(method, path) {
        return true;
    }
    if method == Method::GET && (path.starts_with("/v1/") || path.starts_with("/cp/v1")) {
        return true;
    }
    method == Method::POST
        && (matches!(path, "/cp/v1/route-preview" | "/cp/v1/admission-preview")
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
