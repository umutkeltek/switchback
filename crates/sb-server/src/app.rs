use axum::middleware;
use axum::routing::{get, post};
use axum::Router;

use crate::{auth, controlplane, cp, handlers, AppState};

pub fn build_app(state: AppState) -> Router {
    Router::new()
        .route("/", get(handlers::meta::dashboard))
        .route("/health", get(handlers::meta::health))
        .route("/v1/models", get(handlers::meta::models))
        .route("/v1/embeddings", post(handlers::embeddings::embeddings))
        .route(
            "/v1/chat/completions",
            post(handlers::openai::chat_completions),
        )
        .route("/v1/responses", post(handlers::openai::responses))
        .route("/v1/messages", post(handlers::anthropic::messages))
        .route(
            "/v1/messages/count_tokens",
            post(handlers::anthropic::count_tokens),
        )
        .route("/v1/usage", get(handlers::meta::usage))
        .route("/v1/usage/reconcile", get(handlers::meta::usage_reconcile))
        .route("/v1/traces", get(handlers::meta::traces))
        .route("/v1/traces/{id}", get(handlers::meta::trace_by_id))
        .route(
            "/v1/traces/{id}/route-preview",
            get(handlers::meta::trace_route_preview),
        )
        .route("/v1/sessions", get(handlers::meta::sessions))
        .route("/v1/sessions/{id}", get(handlers::meta::session_by_id))
        .route(
            "/v1/sessions/{id}/traces",
            get(handlers::meta::session_traces),
        )
        .route("/v1/config", get(controlplane::config_endpoint))
        .route("/v1/providers", get(controlplane::providers_endpoint))
        .route(
            "/v1/runtime",
            get(controlplane::runtime_get).patch(controlplane::runtime_patch),
        )
        .route("/v1/reload", post(controlplane::reload_endpoint))
        .route("/v1/revisions", get(controlplane::revisions_endpoint))
        .route("/v1/audit", get(controlplane::audit_endpoint))
        .route("/v1/usage/events", get(controlplane::usage_events_endpoint))
        .route("/v1/health", get(controlplane::health_endpoint))
        .route("/v1/tenants", get(controlplane::tenants_endpoint))
        .route("/v1/plugins", get(controlplane::plugins_endpoint))
        .route("/v1/client-profiles", get(handlers::meta::client_profiles))
        .nest(
            "/admin",
            Router::new()
                .route("/lanes", get(handlers::admin::lanes))
                .layer(middleware::from_fn(handlers::admin::require_loopback)),
        )
        .route("/cp/v1", get(cp::root))
        .route("/cp/v1/resources/{kind}", get(cp::list_resources))
        .route("/cp/v1/resources/{kind}/{name}", get(cp::get_resource))
        .route("/cp/v1/runtime-state", get(cp::runtime_state))
        .route(
            "/cp/v1/runtime-state/reset-lockout",
            post(cp::reset_lockout),
        )
        .route("/cp/v1/route-preview", post(cp::route_preview))
        .route("/cp/v1/admission-preview", post(cp::admission_preview))
        .route("/cp/v1/watch", get(cp::watch))
        .route("/cp/v1/drafts", get(cp::list_drafts).post(cp::create_draft))
        .route("/cp/v1/drafts/{id}", get(cp::get_draft))
        .route("/cp/v1/drafts/{id}/validate", post(cp::validate_draft))
        .route("/cp/v1/drafts/{id}/publish", post(cp::publish_draft))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_auth,
        ))
        .with_state(state)
}
