//! Engine control: the load-time side of model settings (load, unload,
//! apply load settings) for local engines. Phase 1 holds the seams the
//! settings API calls; the engine implementations come with load settings.

use axum::http::StatusCode;
use axum::response::Response;
use serde_json::Value;

use sb_core::{ApplyMode, ModelSettingsDoc};
use sb_runtime::model_settings::SettingsEntry;

use crate::handlers::model_settings::{error, NamedTarget};
use crate::tenancy::Principal;
use crate::AppState;

/// Engine-specific checks of a settings document before it is written.
pub(crate) fn validate_for_target(
    _state: &AppState,
    _target: &str,
    doc: &ModelSettingsDoc,
) -> Result<(), String> {
    if !doc.load.is_empty() || !doc.engine_args.is_empty() || doc.request_load.is_some() {
        return Err(
            "load settings need an engine control on the target's lane (engine: kind: llama_cpp_router)"
                .to_string(),
        );
    }
    Ok(())
}

/// Add the load-time part of a target's view (operator keys only).
pub(crate) fn add_load_view(
    _state: &AppState,
    _named: &NamedTarget,
    _doc: Option<&ModelSettingsDoc>,
    _view: &mut Value,
) {
}

/// After a settings write: apply changed load settings. `None` = nothing
/// to apply.
pub(crate) async fn after_settings_write(
    _state: &AppState,
    _target: &str,
    _before: Option<&ModelSettingsDoc>,
    _entry: &SettingsEntry,
    _apply: ApplyMode,
    _wake: bool,
) -> Option<Value> {
    None
}

pub(crate) async fn load_endpoint(
    _state: &AppState,
    _principal: &Principal,
    name: &str,
    _body: Value,
) -> Response {
    error(
        StatusCode::NOT_IMPLEMENTED,
        "no_engine_control",
        format!("`{name}` has no engine control"),
    )
}

pub(crate) async fn unload_endpoint(
    _state: &AppState,
    _principal: &Principal,
    name: &str,
) -> Response {
    error(
        StatusCode::NOT_IMPLEMENTED,
        "no_engine_control",
        format!("`{name}` has no engine control"),
    )
}
