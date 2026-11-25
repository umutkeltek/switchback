//! Route roles over HTTP: which targets answer each public name.
//!
//! - `GET /v1/roles`: every name, its targets now, where they come from
//!   (`store` or `config`) and the store revision. Client keys see the names
//!   and targets their tenant reaches, as `/v1/models` does.
//! - `GET /v1/roles/{name}`: one name with its history (operator keys).
//! - `PUT /v1/roles/{name}`: set the targets (operator keys). Every target
//!   must exist before anything is written; `check: true` first sends one
//!   short request through the first target (never waking a machine unless
//!   `wake: true`).
//! - `POST /v1/roles/{name}/revert` (`revision`, 0 = the config's route),
//!   `/lock` (`reason`), `/unlock` (operator keys).
//!
//! A write takes effect on the next request (routing reads the role per
//! request); a config reload never replaces it.

use std::collections::BTreeSet;
use std::time::Instant;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::Deserialize;
use serde_json::{json, Value};

use sb_core::{
    config_route_for_role, role_name_problem, Config, PreferAwakeOverride, RouteRoleDoc,
};
use sb_runtime::model_settings::SettingsWriteError;
use sb_runtime::revisioned::RevisionedDoc;
use sb_runtime::route_roles::{RoleEntry, RouteRoles};
use sb_runtime::ExecOutcome;

use super::model_settings::{actor_for, error, with_etag, write_error};
use crate::tenancy::Principal;
use crate::AppState;

/// How many history rows a detail read shows by default.
const HISTORY_LIMIT: usize = 50;

/// Every role name: the config's routes (not `*`) and the stored roles.
pub(super) fn role_names(config: &Config, roles: &RouteRoles) -> Vec<String> {
    let mut names: BTreeSet<String> = config
        .routes
        .iter()
        .filter(|route| route.match_.model.as_deref() != Some("*"))
        .map(|route| route.name.clone())
        .collect();
    for (name, _) in roles.all() {
        names.insert(name);
    }
    names.into_iter().collect()
}

/// One name as `sb ai roles` shows it.
fn role_row(
    config: &Config,
    registry: &sb_adapters::AdapterRegistry,
    name: &str,
    entry: Option<&RoleEntry>,
    durable: bool,
) -> Value {
    let base = config_route_for_role(config, name);
    let doc = entry.map(|entry| entry.doc.as_ref());
    let over = doc.and_then(|doc| doc.route_over(name, base));
    let route = over.as_ref().or(base);
    let targets: Vec<String> = route.map(|route| route.targets.clone()).unwrap_or_default();
    let unknown: Vec<&String> = targets
        .iter()
        .filter(|target| registry.target_for(target).is_none())
        .collect();
    let source = match (&over, base) {
        (Some(_), _) => "store",
        (None, Some(_)) => "config",
        (None, None) => "none",
    };
    json!({
        "name": name,
        "targets": targets,
        "source": source,
        "revision": entry.map(|entry| entry.revision).unwrap_or(0),
        "updated_at_ms": entry.map(|entry| entry.updated_at_ms),
        "config_targets": base.map(|route| route.targets.clone()),
        "pin_order": route.map(|route| route.pin_order),
        "prefer_awake": route.and_then(|route| route.prefer_awake),
        "locked": doc.is_some_and(|doc| doc.locked),
        "lock_reason": doc.and_then(|doc| doc.lock_reason.clone()),
        "unknown_targets": unknown,
        "durable": durable,
    })
}

/// `GET /v1/roles`.
pub(crate) async fn list(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
) -> Response {
    let snap = state.snapshot();
    let roles = state.engine.route_roles();
    let durable = roles.durable();
    if !principal.is_operator_or_admin() {
        // A client key: the names and targets its tenant reaches.
        let effective = roles.effective_config(&snap.config);
        let scoped = crate::controlplane::scoped_config_for_principal(&effective, &principal);
        let data: Vec<Value> = scoped
            .routes
            .iter()
            .filter(|route| route.match_.model.as_deref() != Some("*"))
            .map(|route| {
                let source = if roles
                    .get(&route.name)
                    .is_some_and(|entry| entry.doc.overrides())
                {
                    "store"
                } else {
                    "config"
                };
                json!({"name": route.name, "targets": route.targets, "source": source})
            })
            .collect();
        return Json(json!({"object": "list", "data": data})).into_response();
    }
    let data: Vec<Value> = role_names(&snap.config, &roles)
        .iter()
        .map(|name| {
            role_row(
                &snap.config,
                &snap.registry,
                name,
                roles.get(name).as_ref(),
                durable,
            )
        })
        .collect();
    Json(json!({"object": "list", "durable": durable, "data": data})).into_response()
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct DetailQuery {
    #[serde(default)]
    pub limit: Option<usize>,
}

/// The store key of `name`, after the name checks. `Err` is the answer.
#[allow(clippy::result_large_err)]
pub(super) fn role_key(config: &Config, name: &str) -> Result<String, Response> {
    if let Some(problem) = role_name_problem(config, name) {
        return Err(error(StatusCode::BAD_REQUEST, "invalid_role", problem));
    }
    Ok(RouteRoles::key_for(config, name).to_string())
}

/// `GET /v1/roles/{name}`.
pub(crate) async fn get(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(name): Path<String>,
    Query(query): Query<DetailQuery>,
) -> Response {
    if !principal.is_operator_or_admin() {
        return crate::tenancy::forbidden();
    }
    let snap = state.snapshot();
    let roles = state.engine.route_roles();
    let key = match role_key(&snap.config, &name) {
        Ok(key) => key,
        Err(response) => return response,
    };
    let entry = roles.get(&key);
    if entry.is_none() && config_route_for_role(&snap.config, &key).is_none() {
        return error(
            StatusCode::NOT_FOUND,
            "role_not_found",
            format!("no role `{name}`: the config has no such route and the store no such role"),
        );
    }
    let mut row = role_row(
        &snap.config,
        &snap.registry,
        &key,
        entry.as_ref(),
        roles.durable(),
    );
    let limit = query
        .limit
        .unwrap_or(HISTORY_LIMIT)
        .clamp(1, roles.history_keep());
    match roles.history(&key, limit) {
        Ok(history) => {
            row["history"] = json!(history
                .iter()
                .map(|change| {
                    let doc: Value = serde_json::from_str(&change.doc).unwrap_or(Value::Null);
                    json!({
                        "revision": change.revision,
                        "at_ms": change.at_ms,
                        "action": change.action,
                        "changed": change.changed,
                        "actor": change.actor,
                        "role": change.role,
                        "source": change.source,
                        "targets": doc.get("targets").cloned().unwrap_or_else(|| json!([])),
                        "locked": doc.get("locked").cloned().unwrap_or(json!(false)),
                    })
                })
                .collect::<Vec<_>>());
        }
        Err(e) => {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "state_store_unavailable",
                format!("reading the role history: {e}"),
            )
        }
    }
    with_etag(row, entry.as_ref().map_or(0, |entry| entry.revision))
}

/// The revision a write edits: `If-Match` (quotes and `W/` allowed, `*`
/// = whatever is current) or a body `revision`. `required`: one of them
/// must be there (428 otherwise).
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
        (Some(value), _) if value == "*" => Ok(current),
        (Some(value), _) => value.parse::<u64>().map_err(|_| {
            error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                format!("If-Match must be a role revision number, got `{value}`"),
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

/// Why `target` is not a known lane/model the gateway can route to, if it
/// is not: its provider is configured, and the model is named by a route, a
/// combo or the catalog, has a settings document, or is in its engine's
/// model list (read now when the lane is up; never wakes a machine).
async fn target_problem(state: &AppState, config: &Config, target: &str) -> Option<String> {
    let snap = state.snapshot();
    let Some((provider, model)) = target.split_once('/') else {
        return Some(format!("`{target}` is not provider/model"));
    };
    if snap.registry.target_for(target).is_none() {
        return Some(format!(
            "`{target}`: no provider `{provider}` in the gateway's config"
        ));
    }
    let in_config = config
        .routes
        .iter()
        .flat_map(|route| route.targets.iter())
        .chain(config.combos.values().flat_map(|combo| combo.models.iter()))
        .any(|known| known == target)
        || config.catalog.as_ref().is_some_and(|catalog| {
            catalog
                .models
                .iter()
                .any(|m| m.provider_id == provider && m.id == model)
        });
    if in_config || state.engine.model_settings().get(target).is_some() {
        return None;
    }
    match crate::engine_control::engine_model_ids(state, provider).await {
        Some(ids) if ids.iter().any(|id| id == model) => None,
        Some(ids) => Some(format!(
            "`{target}`: lane `{provider}` lists no model `{model}` (it lists {})",
            if ids.is_empty() {
                "none".to_string()
            } else {
                ids.join(", ")
            }
        )),
        None => Some(format!(
            "`{target}`: model `{model}` is in no route, combo, catalog or settings document, \
             and provider `{provider}` has no engine model list to find it in"
        )),
    }
}

/// Send one fresh short request through `target`. Without `wake` a managed
/// lane must be up already (known up, or answering a probe now): the check
/// never wakes a machine on its own.
pub(super) async fn check_target(
    state: &AppState,
    target: &str,
    wake: bool,
) -> Result<Value, String> {
    let provider = target.split_once('/').map(|(p, _)| p).unwrap_or(target);
    if !wake {
        if let Some(lane) = state.capacity.executor(provider) {
            lane.up_without_wake()
                .await
                .map_err(|why| format!("{why}; the check wakes no machine without --wake"))?;
        }
    }
    let nonce = sb_core::new_id("check");
    let mut req = sb_core::AiRequest::new(
        target.to_string(),
        vec![sb_core::Message::user(format!(
            "Reply with the single word OK. ({nonce})"
        ))],
    );
    req.max_output_tokens = Some(64);
    req.temperature = Some(0.0);
    let started = Instant::now();
    let execution = state.engine.execute_exact_target(req, started);
    let outcome = if wake {
        execution.await
    } else {
        crate::activator::NO_REQUEST_WAKE.scope((), execution).await
    };
    let ms = started.elapsed().as_millis() as u64;
    match outcome {
        ExecOutcome::Collected { response, .. } => {
            let reply: String = response.message.text().chars().take(80).collect();
            Ok(json!({"target": target, "ok": true, "ms": ms, "reply": reply}))
        }
        ExecOutcome::Stream { .. } => Ok(json!({"target": target, "ok": true, "ms": ms})),
        ExecOutcome::Error(e) => Err(format!(
            "the check request through `{target}` failed ({}): {}",
            e.status, e.message
        )),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SetBody {
    pub targets: Vec<String>,
    #[serde(default)]
    pub pin_order: Option<bool>,
    #[serde(default)]
    pub prefer_awake: Option<PreferAwakeOverride>,
    #[serde(default)]
    pub revision: Option<u64>,
    #[serde(default)]
    pub check: bool,
    #[serde(default)]
    pub wake: bool,
}

pub(super) async fn validate_targets_with(
    state: &AppState,
    config: &Config,
    doc: &RouteRoleDoc,
    planned_models: &BTreeSet<String>,
) -> Result<(), String> {
    let mut problems = Vec::new();
    for target in &doc.targets {
        if planned_models.contains(target) {
            continue;
        }
        if let Some(problem) = target_problem(state, config, target).await {
            problems.push(problem);
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "nothing was written: {} (sb ai engines lists each lane's models)",
            problems.join("; ")
        ))
    }
}

/// `PUT /v1/roles/{name}`: set the targets.
pub(crate) async fn put(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
    Path(name): Path<String>,
    body: Result<Json<SetBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if !principal.is_operator_or_admin() {
        return crate::tenancy::forbidden();
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(e) => {
            return error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                format!(
                    "{}; a role write has targets, and may have pin_order, prefer_awake, \
                     revision, check, wake",
                    e.body_text()
                ),
            )
        }
    };
    let snap = state.snapshot();
    let roles = state.engine.route_roles();
    let key = match role_key(&snap.config, &name) {
        Ok(key) => key,
        Err(response) => return response,
    };
    if body.targets.is_empty() {
        return error(
            StatusCode::BAD_REQUEST,
            "invalid_role",
            format!(
                "give at least one target; `sb ai role revert {name} 0` hands `{name}` back to \
                 the config's route"
            ),
        );
    }
    let entry = match roles.current(&key) {
        Ok(entry) => entry,
        Err(e) => return write_error(e),
    };
    let current = entry.as_ref().map_or(0, |entry| entry.revision);
    let expected = match expected_revision(&headers, body.revision, current, true) {
        Ok(expected) => expected,
        Err(response) => return response,
    };
    let prior = entry.as_ref().map(|entry| entry.doc.as_ref());
    let doc = RouteRoleDoc {
        targets: body.targets,
        pin_order: body.pin_order,
        prefer_awake: body.prefer_awake,
        locked: prior.is_some_and(|doc| doc.locked),
        lock_reason: prior.and_then(|doc| doc.lock_reason.clone()),
    };
    if prior.is_some_and(|old| old.equivalent(&doc)) {
        let mut row = role_row(
            &snap.config,
            &snap.registry,
            &key,
            entry.as_ref(),
            roles.durable(),
        );
        row["unchanged"] = json!(true);
        row["check"] = Value::Null;
        return with_etag(row, current);
    }
    if expected != current {
        return write_error(SettingsWriteError::Conflict { expected, current });
    }
    if let Some(why) = prior.and_then(|doc| RouteRoles::lock_refusal(&key, doc)) {
        return write_error(SettingsWriteError::Locked(why));
    }
    if let Err(e) = doc.validate() {
        return error(StatusCode::BAD_REQUEST, "invalid_role", e.0);
    }
    if let Err(message) = validate_targets_with(&state, &snap.config, &doc, &BTreeSet::new()).await
    {
        return error(StatusCode::BAD_REQUEST, "unknown_target", message);
    }
    let check = if body.check {
        match check_target(&state, &doc.targets[0], body.wake).await {
            Ok(check) => Some(check),
            Err(why) => {
                return error(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "check_failed",
                    format!("nothing was written: {why}"),
                )
            }
        }
    } else {
        None
    };
    let actor = actor_for(&principal, &headers);
    match roles.replace(&key, doc, expected, &actor, "set", false) {
        Ok(entry) => {
            let mut row = role_row(
                &snap.config,
                &snap.registry,
                &key,
                Some(&entry),
                roles.durable(),
            );
            row["check"] = check.unwrap_or(Value::Null);
            row["unchanged"] = json!(entry.unchanged);
            with_etag(row, entry.revision)
        }
        Err(e) => write_error(e),
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ActionBody {
    /// `revert`: the revision to write again (0 = the config's route).
    #[serde(default)]
    pub to: Option<u64>,
    /// `lock`: why.
    #[serde(default)]
    pub reason: Option<String>,
    /// The revision the writer edited (or `If-Match`).
    #[serde(default)]
    pub revision: Option<u64>,
}

/// `POST /v1/roles/{name}/{revert|lock|unlock}`.
pub(crate) async fn post(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
    Path((name, action)): Path<(String, String)>,
    body: axum::body::Bytes,
) -> Response {
    if !principal.is_operator_or_admin() {
        return crate::tenancy::forbidden();
    }
    let body: ActionBody = if body.is_empty() {
        ActionBody::default()
    } else {
        match serde_json::from_slice(&body) {
            Ok(body) => body,
            Err(e) => {
                return error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    format!("{e}; a role action body has to, reason, revision"),
                )
            }
        }
    };
    let snap = state.snapshot();
    let roles = state.engine.route_roles();
    let key = match role_key(&snap.config, &name) {
        Ok(key) => key,
        Err(response) => return response,
    };
    let entry = match roles.current(&key) {
        Ok(entry) => entry,
        Err(e) => return write_error(e),
    };
    let current = entry.as_ref().map(|entry| entry.revision).unwrap_or(0);
    let known = entry.is_some() || config_route_for_role(&snap.config, &key).is_some();
    if !known {
        return error(
            StatusCode::NOT_FOUND,
            "role_not_found",
            format!("no role `{name}`: the config has no such route and the store no such role"),
        );
    }
    let actor = actor_for(&principal, &headers);
    let written = match action.as_str() {
        "revert" => {
            let Some(to) = body.to else {
                return error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    "a revert names the revision to write again: {\"to\": N} (0 = the config's route)",
                );
            };
            let expected = match expected_revision(&headers, body.revision, current, true) {
                Ok(expected) => expected,
                Err(response) => return response,
            };
            roles.revert(&key, to, expected, &actor)
        }
        "lock" => {
            if let Some(reason) = &body.reason {
                let ok = !reason.trim().is_empty()
                    && reason.chars().count() <= 200
                    && !reason.chars().any(char::is_control);
                if !ok {
                    return error(
                        StatusCode::BAD_REQUEST,
                        "invalid_request_error",
                        "the reason is one line of 1 to 200 characters",
                    );
                }
            }
            let expected = match expected_revision(&headers, body.revision, current, false) {
                Ok(expected) => expected,
                Err(response) => return response,
            };
            roles.lock(&key, body.reason.clone(), expected, &actor)
        }
        "unlock" => {
            if entry.is_none() {
                let mut row = role_row(&snap.config, &snap.registry, &key, None, roles.durable());
                row["unchanged"] = json!(true);
                return with_etag(row, current);
            }
            let expected = match expected_revision(&headers, body.revision, current, false) {
                Ok(expected) => expected,
                Err(response) => return response,
            };
            roles.unlock(&key, expected, &actor)
        }
        _ => {
            return error(
                StatusCode::NOT_FOUND,
                "not_found",
                "no such role endpoint (revert, lock, unlock)",
            )
        }
    };
    match written {
        Ok(entry) => {
            let mut row = role_row(
                &snap.config,
                &snap.registry,
                &key,
                Some(&entry),
                roles.durable(),
            );
            row["unchanged"] = json!(entry.unchanged);
            with_etag(row, entry.revision)
        }
        Err(e) => write_error(e),
    }
}
