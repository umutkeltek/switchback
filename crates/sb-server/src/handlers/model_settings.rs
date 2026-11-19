//! Per-model settings over HTTP: `/v1/models/{model}/settings` (GET, PUT,
//! PATCH), `/settings/history` (GET), `/settings/revert` (POST), plus the
//! engine side (`/load`, `/unload`) in [`crate::engine_control`].
//!
//! `{model}` is a public name (a route's model or name, a combo) or a
//! `provider/model` target; it may contain `/`, so the router hands the
//! whole tail to these handlers. Settings attach to targets: a read of a
//! public name shows every target behind it; a write needs exactly one.
//!
//! Operator and admin keys write. Client keys read only the targets their
//! tenant reaches, and never see load settings, engine arguments or paths.
//! Nothing here wakes a machine.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::Deserialize;
use serde_json::{json, Map, Value};

use sb_core::{
    layer_settings, merge_patch, Config, ModelSettingsDoc, RequestSettings, SettingSource,
    REQUEST_KEYS,
};
use sb_runtime::model_settings::{SettingsActor, SettingsEntry, SettingsWriteError};

use crate::tenancy::Principal;
use crate::AppState;

pub(crate) fn error(status: StatusCode, kind: &str, message: impl Into<String>) -> Response {
    (
        status,
        Json(json!({"error": {"message": message.into(), "type": kind}})),
    )
        .into_response()
}

/// What the tail of `/v1/models/{tail}` asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ModelPath {
    Settings(String),
    History(String),
    Revert(String),
    Load(String),
    Unload(String),
}

pub(crate) fn parse_model_path(tail: &str) -> Option<ModelPath> {
    let tail = tail.trim_start_matches('/');
    let take = |suffix: &str| {
        tail.strip_suffix(suffix)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
    };
    if let Some(name) = take("/settings/history") {
        Some(ModelPath::History(name))
    } else if let Some(name) = take("/settings/revert") {
        Some(ModelPath::Revert(name))
    } else if let Some(name) = take("/settings") {
        Some(ModelPath::Settings(name))
    } else if let Some(name) = take("/load") {
        Some(ModelPath::Load(name))
    } else {
        take("/unload").map(ModelPath::Unload)
    }
}

fn not_found() -> Response {
    error(
        StatusCode::NOT_FOUND,
        "not_found",
        "no such model endpoint (settings, settings/history, settings/revert, load, unload)",
    )
}

/// One target behind a public name, and the route profile it gets there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NamedTarget {
    pub target: String,
    pub route_profile: Option<String>,
}

/// The targets a name reaches in a (tenant-scoped) config: a route matched
/// by model or name, a combo, or a `provider/model` whose provider the
/// config has. `None` = not a model this caller can reach.
pub(crate) fn targets_for_name(config: &Config, name: &str) -> Option<Vec<NamedTarget>> {
    if let Some(route) = config
        .routes
        .iter()
        .find(|route| route.match_.model.as_deref() == Some(name))
        .or_else(|| config.routes.iter().find(|route| route.name == name))
    {
        return Some(
            route
                .targets
                .iter()
                .map(|target| NamedTarget {
                    target: target.clone(),
                    route_profile: route.settings_profile.clone(),
                })
                .collect(),
        );
    }
    if let Some(combo) = config.combos.get(name) {
        return Some(
            combo
                .models
                .iter()
                .map(|target| NamedTarget {
                    target: target.clone(),
                    route_profile: None,
                })
                .collect(),
        );
    }
    let (provider, model) = name.split_once('/')?;
    if model.is_empty() || !config.providers.iter().any(|p| p.id == provider) {
        return None;
    }
    Some(vec![NamedTarget {
        target: name.to_string(),
        route_profile: None,
    }])
}

/// Resolve a name for this caller (tenant-scoped). `Err` is the response.
#[allow(clippy::result_large_err)]
pub(crate) fn resolve_for(
    state: &AppState,
    principal: &Principal,
    name: &str,
) -> Result<Vec<NamedTarget>, Response> {
    let snap = state.snapshot();
    let scoped = crate::controlplane::scoped_config_for_principal(&snap.config, principal);
    targets_for_name(&scoped, name).ok_or_else(|| {
        error(
            StatusCode::NOT_FOUND,
            "model_not_found",
            format!("unknown model `{name}` (a route, a combo, or provider/model)"),
        )
    })
}

/// Resolve a name that must name one target (writes, load, unload).
#[allow(clippy::result_large_err)]
pub(crate) fn resolve_one(
    state: &AppState,
    principal: &Principal,
    name: &str,
) -> Result<NamedTarget, Response> {
    let mut targets = resolve_for(state, principal, name)?;
    if targets.len() != 1 {
        return Err(error(
            StatusCode::BAD_REQUEST,
            "ambiguous_model",
            format!(
                "`{name}` reaches {} targets; settings attach to one target: {}",
                targets.len(),
                targets
                    .iter()
                    .map(|t| t.target.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ));
    }
    Ok(targets.remove(0))
}

/// The writer as the history records it: tenant/project and role of the
/// key, never the key.
pub(crate) fn actor_for(principal: &Principal, headers: &HeaderMap) -> SettingsActor {
    let actor = match (&principal.tenant, &principal.project) {
        (Some(tenant), Some(project)) => format!("tenant={tenant} project={project}"),
        (Some(tenant), None) => format!("tenant={tenant}"),
        _ => "gateway key".to_string(),
    };
    let agent: String = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_ascii_graphic() || *c == ' ')
        .take(64)
        .collect();
    SettingsActor {
        actor,
        role: principal.role_name().to_string(),
        source: if agent.is_empty() {
            "api".to_string()
        } else {
            format!("api ({agent})")
        },
    }
}

/// The revision a write edits: `If-Match` (quotes and `W/` allowed) or a
/// body `revision`. `Err` = 428/400.
#[allow(clippy::result_large_err)]
fn expected_revision(
    headers: &HeaderMap,
    body_revision: Option<u64>,
    current: u64,
    required: bool,
) -> Result<u64, Response> {
    let header = headers
        .get(axum::http::header::IF_MATCH)
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            value
                .trim()
                .trim_start_matches("W/")
                .trim_matches('"')
                .to_string()
        });
    match (header, body_revision) {
        (Some(value), _) => value.parse::<u64>().map_err(|_| {
            error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                format!("If-Match must be a settings revision number, got `{value}`"),
            )
        }),
        (None, Some(revision)) => Ok(revision),
        (None, None) if !required => Ok(current),
        (None, None) => Err(error(
            StatusCode::PRECONDITION_REQUIRED,
            "revision_required",
            format!(
                "send the revision you edited (If-Match: {current}, or \"revision\": {current} in the body)"
            ),
        )),
    }
}

fn write_error(e: SettingsWriteError) -> Response {
    let status = match &e {
        SettingsWriteError::Conflict { .. } => StatusCode::CONFLICT,
        SettingsWriteError::Invalid(_) => StatusCode::BAD_REQUEST,
        SettingsWriteError::UnknownRevision(_) => StatusCode::NOT_FOUND,
        SettingsWriteError::Store(_) => StatusCode::SERVICE_UNAVAILABLE,
    };
    let kind = match &e {
        SettingsWriteError::Conflict { .. } => "revision_conflict",
        SettingsWriteError::Invalid(_) => "invalid_settings",
        SettingsWriteError::UnknownRevision(_) => "not_found",
        SettingsWriteError::Store(_) => "state_store_unavailable",
    };
    let mut body = json!({"error": {"message": e.to_string(), "type": kind}});
    if let SettingsWriteError::Conflict { current, .. } = e {
        body["error"]["current_revision"] = json!(current);
        body["error"]["reason"] = json!("revision_conflict");
    }
    (status, Json(body)).into_response()
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct SettingsQuery {
    /// Show the effective view as a request naming this profile gets it.
    #[serde(default)]
    pub profile: Option<String>,
}

/// The effective view of one target.
pub(crate) fn target_view(
    state: &AppState,
    named: &NamedTarget,
    entry: Option<&SettingsEntry>,
    profile: Option<&str>,
    operator: bool,
) -> Value {
    let doc = entry.map(|entry| entry.doc.as_ref());
    let request = profile.map(|profile| RequestSettings {
        profile: Some(profile.to_string()),
        ..RequestSettings::default()
    });
    let layered = layer_settings(doc, named.route_profile.as_deref(), request.as_ref());
    let mut effective = Map::new();
    for key in REQUEST_KEYS {
        let prefix = format!("{}.", key.name);
        let mut any = false;
        for (path, (value, source)) in layered
            .values
            .iter()
            .filter(|(path, _)| *path == key.name || path.starts_with(&prefix))
        {
            any = true;
            effective.insert(
                path.clone(),
                json!({
                    "value": value,
                    "source": source_label(source),
                    "locked": doc.is_some_and(|doc| doc.is_locked(path)),
                }),
            );
        }
        if !any {
            effective.insert(
                key.name.to_string(),
                json!({
                    "value": null,
                    "source": "engine",
                    "locked": doc.is_some_and(|doc| doc.is_locked(key.name)),
                }),
            );
        }
    }
    let mut view = json!({
        "target": named.target,
        "revision": entry.map(|entry| entry.revision).unwrap_or(0),
        "updated_at_ms": entry.map(|entry| entry.updated_at_ms),
        "durable": state.engine.model_settings().durable(),
        "route_profile": named.route_profile,
        "effective": effective,
        "profiles": doc.map(|doc| json!(doc.profiles)).unwrap_or_else(|| json!({})),
        "locked": doc.map(|doc| json!(doc.locked)).unwrap_or_else(|| json!([])),
        "request_load": doc.and_then(|doc| doc.request_load.clone()),
    });
    if operator {
        view["document"] = doc
            .map(|doc| serde_json::to_value(doc).unwrap_or(Value::Null))
            .unwrap_or_else(|| json!({}));
        crate::engine_control::add_load_view(
            &crate::engine_control::EngineCtx::from(state),
            named,
            doc,
            &mut view,
        );
    }
    view
}

fn source_label(source: &SettingSource) -> String {
    match source {
        SettingSource::Request => "request".to_string(),
        SettingSource::RequestProfile(name) => format!("profile:{name}"),
        SettingSource::RouteProfile(name) => format!("route_profile:{name}"),
        SettingSource::Defaults => "defaults".to_string(),
    }
}

/// `GET /v1/models/{tail}`.
pub(crate) async fn get(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(tail): Path<String>,
    Query(query): Query<SettingsQuery>,
) -> Response {
    match parse_model_path(&tail) {
        Some(ModelPath::Settings(name)) => {
            get_settings(&state, &principal, &name, query.profile.as_deref())
        }
        Some(ModelPath::History(name)) => {
            if !principal.is_operator_or_admin() {
                return crate::tenancy::forbidden();
            }
            history(&state, &principal, &name)
        }
        _ => not_found(),
    }
}

fn get_settings(
    state: &AppState,
    principal: &Principal,
    name: &str,
    profile: Option<&str>,
) -> Response {
    let targets = match resolve_for(state, principal, name) {
        Ok(targets) => targets,
        Err(response) => return response,
    };
    let registry = state.engine.model_settings();
    if let Some(profile) = profile {
        let entries: Vec<_> = targets.iter().map(|t| registry.get(&t.target)).collect();
        let (known, names) = sb_core::profile_known(
            entries
                .iter()
                .map(|entry| entry.as_ref().map(|e| e.doc.as_ref())),
            profile,
        );
        if !known {
            return error(
                StatusCode::BAD_REQUEST,
                "unknown_profile",
                format!(
                    "settings profile `{profile}` is not defined for `{name}`; defined: {}",
                    names.into_iter().collect::<Vec<_>>().join(", ")
                ),
            );
        }
    }
    let operator = principal.is_operator_or_admin();
    let views: Vec<Value> = targets
        .iter()
        .map(|named| {
            target_view(
                state,
                named,
                registry.get(&named.target).as_ref(),
                profile,
                operator,
            )
        })
        .collect();
    let mut body = json!({"model": name, "targets": views});
    if targets.len() == 1 {
        body["revision"] = json!(registry.revision(&targets[0].target));
    }
    let mut response = Json(body).into_response();
    if targets.len() == 1 {
        if let Ok(value) =
            axum::http::HeaderValue::from_str(&registry.revision(&targets[0].target).to_string())
        {
            response
                .headers_mut()
                .insert(axum::http::header::ETAG, value);
        }
    }
    response
}

fn history(state: &AppState, principal: &Principal, name: &str) -> Response {
    let named = match resolve_one(state, principal, name) {
        Ok(named) => named,
        Err(response) => return response,
    };
    match state.engine.model_settings().history(&named.target, 200) {
        Ok(changes) => Json(json!({
            "target": named.target,
            "revision": state.engine.model_settings().revision(&named.target),
            "history": changes.iter().map(|change| json!({
                "revision": change.revision,
                "at_ms": change.at_ms,
                "actor": change.actor,
                "role": change.role,
                "source": change.source,
                "action": change.action,
                "changed": change.changed,
                "document": serde_json::from_str::<Value>(&change.doc).unwrap_or(Value::Null),
            })).collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(e) => error(
            StatusCode::SERVICE_UNAVAILABLE,
            "state_store_unavailable",
            e,
        ),
    }
}

/// Split a write body into the document part and its `revision` (and, for
/// load settings, `apply`/`wake`).
pub(crate) struct WriteBody {
    pub doc: Value,
    pub revision: Option<u64>,
    pub apply: Option<sb_core::ApplyMode>,
    pub wake: bool,
}

#[allow(clippy::result_large_err)]
fn split_write_body(body: Value) -> Result<WriteBody, Response> {
    let Value::Object(mut object) = body else {
        return Err(error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "the body must be a settings document object",
        ));
    };
    let revision = match object.remove("revision") {
        None | Some(Value::Null) => None,
        Some(value) => Some(value.as_u64().ok_or_else(|| {
            error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "`revision` must be a whole number",
            )
        })?),
    };
    let apply = match object.remove("apply") {
        None | Some(Value::Null) => None,
        Some(value) => Some(serde_json::from_value(value).map_err(|_| {
            error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "`apply` must be next_load or now",
            )
        })?),
    };
    let wake = object
        .remove("wake")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    Ok(WriteBody {
        doc: Value::Object(object),
        revision,
        apply,
        wake,
    })
}

/// `PUT /v1/models/{model}/settings`: replace the whole document.
pub(crate) async fn put(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(tail): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let Some(ModelPath::Settings(name)) = parse_model_path(&tail) else {
        return not_found();
    };
    write(&state, &principal, &headers, &name, body, false).await
}

/// `PATCH /v1/models/{model}/settings`: JSON merge patch (`null` removes).
pub(crate) async fn patch(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(tail): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let Some(ModelPath::Settings(name)) = parse_model_path(&tail) else {
        return not_found();
    };
    write(&state, &principal, &headers, &name, body, true).await
}

async fn write(
    state: &AppState,
    principal: &Principal,
    headers: &HeaderMap,
    name: &str,
    body: Value,
    merge: bool,
) -> Response {
    if !principal.is_operator_or_admin() {
        return crate::tenancy::forbidden();
    }
    let named = match resolve_one(state, principal, name) {
        Ok(named) => named,
        Err(response) => return response,
    };
    let body = match split_write_body(body) {
        Ok(body) => body,
        Err(response) => return response,
    };
    let registry = state.engine.model_settings();
    let current = registry.get(&named.target);
    let current_revision = current.as_ref().map(|e| e.revision).unwrap_or(0);
    let expected = match expected_revision(headers, body.revision, current_revision, true) {
        Ok(expected) => expected,
        Err(response) => return response,
    };
    let doc_value = if merge {
        let mut base = current
            .as_ref()
            .map(|entry| serde_json::to_value(entry.doc.as_ref()).unwrap_or(json!({})))
            .unwrap_or_else(|| json!({}));
        merge_patch(&mut base, &body.doc);
        base
    } else {
        body.doc
    };
    let doc: ModelSettingsDoc = match serde_json::from_value(doc_value) {
        Ok(doc) => doc,
        Err(e) => {
            return error(
                StatusCode::BAD_REQUEST,
                "invalid_settings",
                format!(
                    "{e}; a settings document has defaults, profiles, locked, load, engine_args, request_load"
                ),
            )
        }
    };
    if let Err(message) = crate::engine_control::validate_for_target(
        &crate::engine_control::EngineCtx::from(state),
        &named.target,
        &doc,
    ) {
        return error(StatusCode::BAD_REQUEST, "invalid_settings", message);
    }
    let actor = actor_for(principal, headers);
    let action = if merge { "patch" } else { "put" };
    let before = current.as_ref().map(|entry| entry.doc.clone());
    let entry = match registry.write(&named.target, doc, expected, &actor, action) {
        Ok(entry) => entry,
        Err(e) => return write_error(e),
    };
    let apply = crate::engine_control::after_settings_write(
        &crate::engine_control::EngineCtx::from(state),
        &named.target,
        before.as_deref(),
        &entry,
        body.apply.unwrap_or_default(),
        body.wake,
    )
    .await;
    let mut view = target_view(state, &named, Some(&entry), None, true);
    let mut status = StatusCode::OK;
    if let Some(apply) = apply {
        if apply.conflict {
            status = StatusCode::CONFLICT;
            view["error"] = json!({
                "type": "model_busy",
                "reason": "model_busy",
                "message": apply.value["message"],
            });
        }
        view["apply"] = apply.value;
    }
    let mut response = (status, Json(view)).into_response();
    if let Ok(value) = axum::http::HeaderValue::from_str(&entry.revision.to_string()) {
        response
            .headers_mut()
            .insert(axum::http::header::ETAG, value);
    }
    response
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RevertBody {
    revision: u64,
}

/// `POST /v1/models/{tail}`: `settings/revert`, `load`, `unload`.
pub(crate) async fn post(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(tail): Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if !principal.is_operator_or_admin() {
        return crate::tenancy::forbidden();
    }
    let body: Value = if body.is_empty() {
        json!({})
    } else {
        match serde_json::from_slice(&body) {
            Ok(body) => body,
            Err(e) => {
                return error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    format!("body is not JSON: {e}"),
                )
            }
        }
    };
    match parse_model_path(&tail) {
        Some(ModelPath::Revert(name)) => revert(&state, &principal, &headers, &name, body),
        Some(ModelPath::Load(name)) => {
            crate::engine_control::load_endpoint(&state, &principal, &name, body).await
        }
        Some(ModelPath::Unload(name)) => {
            crate::engine_control::unload_endpoint(&state, &principal, &name).await
        }
        _ => not_found(),
    }
}

fn revert(
    state: &AppState,
    principal: &Principal,
    headers: &HeaderMap,
    name: &str,
    body: Value,
) -> Response {
    let named = match resolve_one(state, principal, name) {
        Ok(named) => named,
        Err(response) => return response,
    };
    let body: RevertBody = match serde_json::from_value(body) {
        Ok(body) => body,
        Err(e) => {
            return error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                format!("{e}; send {{\"revision\": N}}"),
            )
        }
    };
    let registry = state.engine.model_settings();
    let current = registry.revision(&named.target);
    let expected = match expected_revision(headers, None, current, false) {
        Ok(expected) => expected,
        Err(response) => return response,
    };
    let actor = actor_for(principal, headers);
    match registry.revert(&named.target, body.revision, expected, &actor) {
        Ok(entry) => Json(target_view(state, &named, Some(&entry), None, true)).into_response(),
        Err(e) => write_error(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_paths_parse_with_slashes() {
        assert_eq!(
            parse_model_path("text/qwen/settings"),
            Some(ModelPath::Settings("text/qwen".into()))
        );
        assert_eq!(
            parse_model_path("/fast/settings/history"),
            Some(ModelPath::History("fast".into()))
        );
        assert_eq!(
            parse_model_path("text/m/settings/revert"),
            Some(ModelPath::Revert("text/m".into()))
        );
        assert_eq!(
            parse_model_path("text/m/load"),
            Some(ModelPath::Load("text/m".into()))
        );
        assert_eq!(parse_model_path("/settings"), None);
        assert_eq!(parse_model_path("text/m"), None);
    }
}
