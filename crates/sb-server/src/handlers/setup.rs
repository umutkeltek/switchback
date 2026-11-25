use std::collections::{BTreeMap, BTreeSet};

use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use sb_core::{
    config_route_for_role, role_name_problem, ApplyMode, Config, ModelSettingsDoc, RouteRoleDoc,
};
use sb_runtime::model_settings::{SettingsActor, SettingsWriteError};
use sb_runtime::revisioned::RevisionedDoc;
use sb_runtime::route_roles::RouteRoles;

use super::model_settings::{
    actor_for, error, lock_problem, mutate, write_error, LockPolicy, SettingsMutation,
};
use super::route_roles::{check_target, role_names, validate_targets_with};
use crate::engine_control::{validate_for_target, EngineCtx};
use crate::tenancy::Principal;
use crate::AppState;

const SCHEMA: &str = "switchback/setup@1";

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Revisions {
    names: BTreeMap<String, u64>,
    models: BTreeMap<String, u64>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct SetupRequest {
    schema: String,
    names: BTreeMap<String, Value>,
    models: BTreeMap<String, Value>,
    unlock: BTreeSet<String>,
    check: bool,
    apply_now: bool,
    wake: bool,
    if_match: Revisions,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum SetupFile {
    Text(String),
    Document(SetupRequest),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileRequest {
    setup: SetupFile,
    #[serde(default)]
    unlock: BTreeSet<String>,
    #[serde(default)]
    check: bool,
    #[serde(default)]
    wake: bool,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Request {
    File(FileRequest),
    Direct(SetupRequest),
}

#[derive(Default, Deserialize)]
pub(crate) struct ExportQuery {
    #[serde(default)]
    effective: bool,
}

fn config_name(config: &Config, name: &str) -> Option<RouteRoleDoc> {
    let route = config_route_for_role(config, name)?;
    Some(RouteRoleDoc {
        targets: route.targets.clone(),
        pin_order: Some(route.pin_order),
        prefer_awake: route.prefer_awake,
        ..RouteRoleDoc::default()
    })
}

pub(crate) async fn export(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Query(query): Query<ExportQuery>,
) -> Response {
    if !principal.is_operator_or_admin() {
        return crate::tenancy::forbidden();
    }
    let config = &state.snapshot().config;
    let roles = state.engine.route_roles();
    let entries = match roles.snapshot_entries() {
        Ok(entries) => entries,
        Err(e) => return write_error(e),
    };
    let mut names: BTreeMap<_, _> = entries
        .into_iter()
        .map(|(name, entry)| (name, json!(entry.doc.as_ref())))
        .collect();
    if query.effective {
        for name in role_names(config, &roles) {
            if let Some(doc) = config_name(config, &name) {
                names.entry(name).or_insert_with(|| {
                    let mut value = json!(doc);
                    value["source"] = json!("config");
                    value
                });
            }
        }
    }
    let models: BTreeMap<_, _> = match state.engine.model_settings().snapshot_entries() {
        Ok(entries) => entries
            .into_iter()
            .map(|(target, entry)| (target, json!(entry.doc.as_ref())))
            .collect(),
        Err(e) => return write_error(e),
    };
    Json(json!({"schema": SCHEMA, "names": names, "models": models})).into_response()
}

fn parse_request(body: &[u8]) -> Result<SetupRequest, String> {
    let input: serde_yaml::Value = serde_yaml::from_slice(body).map_err(|e| e.to_string())?;
    let request: Request = serde_yaml::from_value(input).map_err(|e| e.to_string())?;
    let request = match request {
        Request::Direct(request) => request,
        Request::File(file) => {
            let mut document = match file.setup {
                SetupFile::Text(text) => {
                    let input = serde_yaml::from_str(&text).map_err(|e| e.to_string())?;
                    serde_yaml::from_value(input).map_err(|e| e.to_string())?
                }
                SetupFile::Document(document) => document,
            };
            document.unlock = file.unlock;
            document.check = file.check;
            document.wake = file.wake;
            document
        }
    };
    if request.schema != SCHEMA {
        return Err(format!("setup documents need schema: {SCHEMA}"));
    }
    Ok(request)
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    Create,
    Update,
    Unchanged,
    Refused,
    Invalid,
}

#[derive(Serialize)]
struct Item {
    kind: &'static str,
    name: String,
    status: Status,
    changed: Vec<String>,
    revision_before: Option<u64>,
    revision_after: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    apply: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    check: Option<Value>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pending: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    undo: Option<Value>,
}

impl Item {
    fn new(kind: &'static str, name: String) -> Self {
        Self {
            kind,
            name,
            status: Status::Invalid,
            changed: Vec::new(),
            revision_before: None,
            revision_after: None,
            reason: None,
            source: None,
            apply: None,
            check: None,
            pending: false,
            undo: None,
        }
    }

    fn previous(&mut self, revision: u64, source: Option<&'static str>) {
        self.revision_before = Some(revision);
        self.revision_after = Some(revision);
        self.source = source;
        self.status = if revision == 0 {
            Status::Create
        } else {
            Status::Update
        };
    }

    fn fail(&mut self, status: Status, why: impl Into<String>) {
        self.status = status;
        self.reason = Some(why.into());
    }

    fn write_error(&mut self, e: SettingsWriteError) {
        if let SettingsWriteError::Conflict { current, .. } = e {
            self.revision_after = Some(current);
        }
        let status = if matches!(e, SettingsWriteError::Invalid(_)) {
            Status::Invalid
        } else {
            Status::Refused
        };
        self.fail(status, e.to_string());
    }

    fn written<D: RevisionedDoc>(&mut self, revision: u64, unchanged: bool, before: &D) {
        self.revision_after = Some(revision);
        if unchanged {
            self.status = Status::Unchanged;
            self.changed.clear();
            return;
        }
        let family = if self.kind == "model" {
            "models"
        } else {
            "names"
        };
        self.undo = Some(json!({
            "schema": SCHEMA,
            (family): { (&self.name): before },
            "if_match": { (family): { (&self.name): revision } },
            "unlock": [&self.name],
        }));
    }
}

struct Run<'a> {
    state: &'a AppState,
    config: &'a Config,
    request: &'a SetupRequest,
    actor: &'a SettingsActor,
    apply: bool,
}

impl Run<'_> {
    async fn lane_asleep(&self, target: &str) -> Option<String> {
        if self.request.wake {
            return None;
        }
        let (lane, _) = target.split_once('/')?;
        let executor = self.state.capacity.executor(lane)?;
        executor.up_without_wake().await.err()
    }

    async fn model(&self, target: &str, value: Value) -> Item {
        let mut item = Item::new("model", target.to_string());
        let registry = self.state.engine.model_settings();
        let current = match registry.current(target) {
            Ok(entry) => entry,
            Err(e) => {
                item.write_error(e);
                return item;
            }
        };
        let revision = current.as_ref().map_or(0, |entry| entry.revision);
        item.previous(revision, current.as_ref().map(|_| "store"));
        let before = current.as_ref().map(|entry| entry.doc.as_ref());
        let doc: ModelSettingsDoc = match serde_json::from_value(value) {
            Ok(doc) => doc,
            Err(e) => {
                item.fail(Status::Invalid, e.to_string());
                return item;
            }
        };
        if before.is_some_and(|old| old.equivalent(&doc)) {
            item.status = Status::Unchanged;
            return item;
        }
        let empty = ModelSettingsDoc::default();
        let before = before.unwrap_or(&empty);
        item.changed = ModelSettingsDoc::changed(before, &doc);
        let expected = self
            .request
            .if_match
            .models
            .get(target)
            .copied()
            .unwrap_or(revision);
        if expected != revision {
            item.write_error(SettingsWriteError::Conflict {
                expected,
                current: revision,
            });
            return item;
        }
        let unlock = self.request.unlock.contains(target);
        let locks = LockPolicy::Setup { unlock };
        if let Some(why) = lock_problem(self.config, target, before, &doc, locks) {
            item.fail(Status::Refused, why);
            return item;
        }
        if let Some(why) = sb_core::route_roles::target_shape_problem(target) {
            item.fail(Status::Invalid, why);
            return item;
        }
        let provider = target.split_once('/').map(|(lane, _)| lane);
        if !self
            .config
            .providers
            .iter()
            .any(|entry| Some(entry.id.as_str()) == provider)
        {
            item.fail(Status::Invalid, "target needs a provider in gateway config");
            return item;
        }
        if let Err(why) = doc.check() {
            item.fail(Status::Invalid, why);
            return item;
        }
        if !self.apply {
            if let Err(why) = validate_for_target(&EngineCtx::from(self.state), target, &doc).await
            {
                item.fail(Status::Invalid, why);
            }
            return item;
        }
        let mut options = SettingsMutation {
            expected,
            actor: self.actor,
            action: if unlock { "setup:unlock" } else { "setup" },
            apply: if self.request.check || self.request.wake || self.request.apply_now {
                ApplyMode::Now
            } else {
                ApplyMode::NextLoad
            },
            wake: self.request.wake,
            locks,
        };
        let sleeping = if self.request.check
            || before.load != doc.load
            || before.engine_args != doc.engine_args
        {
            self.lane_asleep(target).await
        } else {
            None
        };
        let written = match mutate(self.state, target, doc, options).await {
            Ok((written, applied)) => {
                item.pending = applied.as_ref().is_some_and(|result| {
                    result["pending"] == true || (sleeping.is_some() && result["reloaded"] != true)
                });
                item.apply = applied;
                item.written(written.revision, written.unchanged, before);
                if written.unchanged || !self.request.check {
                    return item;
                }
                written.revision
            }
            Err(e) => {
                item.write_error(e);
                return item;
            }
        };
        if let Some(why) = sleeping {
            item.pending = true;
            item.check = Some(json!({"pending": true, "reason": why}));
            return item;
        }
        let checked = match item.apply.as_ref() {
            Some(value)
                if value["ok"] == false
                    || value["reason"] == "old_settings"
                    || value["reason"] == "model_busy" =>
            {
                Err(value["error"]
                    .as_str()
                    .or(value["message"].as_str())
                    .unwrap_or("engine did not apply settings")
                    .to_string())
            }
            _ => check_target(self.state, target, self.request.wake).await,
        };
        let why = match checked {
            Ok(check) => {
                item.check = Some(check);
                return item;
            }
            Err(why) => why,
        };
        item.fail(Status::Refused, format!("check failed: {why}"));
        item.check = Some(json!({"ok": false, "reason": why}));
        options.expected = written;
        options.action = "setup:check_failed";
        options.locks = LockPolicy::Restore;
        match mutate(self.state, target, before.clone(), options).await {
            Ok((restored, restored_apply)) => {
                item.check = Some(json!({
                    "ok": false, "reason": why,
                    "restored": {"revision": restored.revision, "apply": restored_apply},
                }));
                item.revision_after = Some(restored.revision);
                item.undo = None;
            }
            Err(e) => {
                item.reason = Some(format!(
                    "check failed: {why}. Could not restore revision {revision}: {e}"
                ));
                if let SettingsWriteError::Conflict { current, .. } = e {
                    item.revision_after = Some(current);
                }
            }
        }
        item
    }

    async fn name(&self, name: &str, mut value: Value, planned_models: &BTreeSet<String>) -> Item {
        let mut item = Item::new("name", name.to_string());
        let roles = self.state.engine.route_roles();
        let current = match roles.current(name) {
            Ok(entry) => entry,
            Err(e) => {
                item.write_error(e);
                return item;
            }
        };
        let revision = current.as_ref().map_or(0, |entry| entry.revision);
        let base = config_route_for_role(self.config, name);
        let source = if current.is_some() {
            Some("store")
        } else {
            base.map(|_| "config")
        };
        item.previous(revision, source);
        if let Some(object) = value.as_object_mut() {
            if object.get("source") == Some(&json!("config")) {
                object.remove("source");
            }
        }
        let doc: RouteRoleDoc = match serde_json::from_value(value) {
            Ok(doc) => doc,
            Err(e) => {
                item.fail(Status::Invalid, e.to_string());
                return item;
            }
        };
        let same_config = current.is_none()
            && !doc.locked
            && doc.lock_reason.is_none()
            && base.is_some_and(|base| {
                doc.overrides()
                    && doc.route_over(name, Some(base)).is_some_and(|route| {
                        route.targets == base.targets
                            && route.pin_order == base.pin_order
                            && route
                                .prefer_awake
                                .unwrap_or_default()
                                .over(&self.config.server.prefer_awake)
                                == base
                                    .prefer_awake
                                    .unwrap_or_default()
                                    .over(&self.config.server.prefer_awake)
                    })
            });
        let before = current.as_ref().map(|entry| entry.doc.as_ref());
        if same_config || before.is_some_and(|old| old.equivalent(&doc)) {
            item.status = Status::Unchanged;
            return item;
        }
        let empty = RouteRoleDoc::default();
        let before = before.unwrap_or(&empty);
        item.changed = match base.filter(|_| revision == 0) {
            Some(base) => {
                let prior = config_name(self.config, name).unwrap_or_default();
                let route = doc.route_over(name, Some(base));
                let route = route.as_ref().unwrap_or(base);
                let effective = RouteRoleDoc {
                    targets: route.targets.clone(),
                    pin_order: Some(route.pin_order),
                    prefer_awake: route.prefer_awake,
                    locked: doc.locked,
                    lock_reason: doc.lock_reason.clone(),
                };
                RouteRoleDoc::changed(&prior, &effective)
            }
            None => RouteRoleDoc::changed(before, &doc),
        };
        if source == Some("config") {
            item.status = Status::Update;
        }
        let expected = self
            .request
            .if_match
            .names
            .get(name)
            .copied()
            .unwrap_or(revision);
        if expected != revision {
            item.write_error(SettingsWriteError::Conflict {
                expected,
                current: revision,
            });
            return item;
        }
        let unlock = self.request.unlock.contains(name);
        if let Some(why) = RouteRoles::lock_refusal(name, before).filter(|_| !unlock) {
            item.fail(Status::Refused, why);
            return item;
        }
        if let Some(why) = role_name_problem(self.config, name) {
            item.fail(Status::Invalid, why);
            return item;
        }
        if let Err(why) = doc.check() {
            item.fail(Status::Invalid, why);
            return item;
        }
        if let Err(why) = validate_targets_with(self.state, self.config, &doc, planned_models).await
        {
            item.fail(Status::Invalid, why);
            return item;
        }
        if !self.apply {
            return item;
        }
        if let Some(target) = doc.targets.first().filter(|_| self.request.check) {
            if let Some(why) = self.lane_asleep(target).await {
                item.pending = true;
                item.check = Some(json!({"pending": true, "reason": why}));
            } else {
                match check_target(self.state, target, self.request.wake).await {
                    Ok(check) => item.check = Some(check),
                    Err(why) => {
                        item.fail(Status::Refused, format!("check failed: {why}"));
                        return item;
                    }
                }
            }
        }
        let action = if unlock { "setup:unlock" } else { "setup" };
        match roles.replace(name, doc, expected, self.actor, action, unlock) {
            Ok(written) => item.written(written.revision, written.unchanged, before),
            Err(e) => item.write_error(e),
        }
        item
    }
}

async fn run(
    state: AppState,
    principal: Principal,
    headers: HeaderMap,
    body: Bytes,
    apply: bool,
) -> Response {
    if !principal.is_operator_or_admin() {
        return crate::tenancy::forbidden();
    }
    let mut request = match parse_request(&body) {
        Ok(request) => request,
        Err(why) => return error(StatusCode::BAD_REQUEST, "invalid_setup", why),
    };
    let snapshot = state.snapshot();
    let config = &snapshot.config;
    let actor = actor_for(&principal, &headers);
    let models = std::mem::take(&mut request.models);
    let names = std::mem::take(&mut request.names);
    let runner = Run {
        state: &state,
        config,
        request: &request,
        actor: &actor,
        apply,
    };
    let model_entries = match state.engine.model_settings().snapshot_entries() {
        Ok(entries) => entries,
        Err(e) => return write_error(e),
    };
    let role_entries = match state.engine.route_roles().snapshot_entries() {
        Ok(entries) => entries,
        Err(e) => return write_error(e),
    };
    let mut not_in_file = Vec::new();
    let file_names: BTreeSet<_> = names
        .keys()
        .map(|name| RouteRoles::key_for(config, name))
        .collect();
    let config_names: BTreeSet<_> = role_names(config, &state.engine.route_roles())
        .into_iter()
        .collect();
    let stored_names: BTreeSet<_> = role_entries.iter().map(|(name, _)| name.as_str()).collect();
    for name in config_names
        .iter()
        .map(String::as_str)
        .chain(stored_names.iter().copied())
        .collect::<BTreeSet<_>>()
    {
        if !file_names.contains(name) {
            let source = if stored_names.contains(name) {
                "store"
            } else {
                "config"
            };
            not_in_file.push(json!({"kind": "name", "name": name, "source": source}));
        }
    }
    for (target, _) in model_entries {
        if !models.contains_key(&target) {
            not_in_file.push(json!({"kind": "model", "name": target, "source": "store"}));
        }
    }

    let mut items = Vec::new();
    let mut planned_models = BTreeSet::new();
    for (target, value) in models {
        let item = runner.model(&target, value).await;
        if !apply
            && matches!(
                item.status,
                Status::Create | Status::Update | Status::Unchanged
            )
        {
            planned_models.insert(target);
        }
        items.push(item);
    }
    let mut occurrences = BTreeMap::new();
    for name in names.keys() {
        *occurrences
            .entry(RouteRoles::key_for(config, name).to_string())
            .or_insert(0) += 1;
    }
    for (name, value) in names {
        let key = RouteRoles::key_for(config, &name);
        if occurrences.get(key).copied().unwrap_or(0) > 1 {
            let mut item = Item::new("name", name.clone());
            item.fail(
                Status::Invalid,
                format!("more than one item names route {key}"),
            );
            items.push(item);
        } else if role_name_problem(config, &name).is_some()
            && config_route_for_role(config, &name).is_none()
        {
            let mut item = Item::new("name", name.clone());
            item.fail(
                Status::Invalid,
                role_name_problem(config, &name).unwrap_or_default(),
            );
            items.push(item);
        } else {
            items.push(runner.name(key, value, &planned_models).await);
        }
    }
    items.sort_by(|left, right| (left.kind, &left.name).cmp(&(right.kind, &right.name)));
    Json(json!({"schema": SCHEMA, "applied": apply, "items": items, "not_in_file": not_in_file}))
        .into_response()
}

pub(crate) async fn plan(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    run(state, principal, headers, body, false).await
}

pub(crate) async fn apply(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    match tokio::spawn(run(state, principal, headers, body, true)).await {
        Ok(response) => response,
        Err(e) => {
            tracing::error!(error = %e, "setup apply task stopped");
            error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "setup_error",
                "setup apply stopped",
            )
        }
    }
}
