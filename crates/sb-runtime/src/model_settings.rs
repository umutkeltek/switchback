//! Per-model settings at run time: the registry that holds each target's
//! settings document (durable in the state store, revisioned, with
//! history), and the per-request plan that applies them to the request an
//! attempt sends.
//!
//! The registry lives on the [`crate::Engine`], beside the ledger and the
//! scorecard: it is state, not config, so it survives a config publish.
//! Reads never touch a machine; only the engine control (in `sb-server`)
//! does, and only on load/unload/apply.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, RwLock};

use serde_json::{Map, Value as Json};

use sb_core::{
    AiRequest, ExecutionTarget, ModelSettingsDoc, OpenAiEngine, ProviderKind, RequestSettings,
    SettingsEngine,
};
use sb_store::{ModelSettingsChange, ModelSettingsWrite, StateStore};

use crate::{ExecError, Snapshot};

/// Makes (or finds) the engine variant a request's load settings ask for:
/// `sb-server`'s engine control. `Ok` is the upstream model name the
/// attempt sends (`<model>~<hash8>`); `Err` is the answer to give.
pub trait ModelVariants: Send + Sync {
    fn ensure_variant<'a>(
        &'a self,
        target: &'a ExecutionTarget,
        load: &'a Map<String, Json>,
        policy: &'a sb_core::RequestLoadPolicy,
    ) -> crate::BoxFuture<'a, Result<String, ExecError>>;
}

/// One target's current settings.
#[derive(Debug, Clone)]
pub struct SettingsEntry {
    pub revision: u64,
    pub doc: Arc<ModelSettingsDoc>,
    pub updated_at_ms: i64,
}

/// Who is writing: tenant/project and role of the key (never the key), and
/// where the write came from.
#[derive(Debug, Clone, Default)]
pub struct SettingsActor {
    pub actor: String,
    pub role: String,
    pub source: String,
}

/// Why a settings write was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsWriteError {
    /// The writer edited `expected`; the target is at `current` now.
    Conflict { expected: u64, current: u64 },
    /// The document is not valid.
    Invalid(String),
    /// No such revision in the history (revert).
    UnknownRevision(u64),
    /// The state store refused the write.
    Store(String),
}

impl std::fmt::Display for SettingsWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SettingsWriteError::Conflict { expected, current } => write!(
                f,
                "settings changed: you edited revision {expected}, the current one is {current}; \
                 read it again and retry"
            ),
            SettingsWriteError::Invalid(message) => f.write_str(message),
            SettingsWriteError::UnknownRevision(revision) => {
                write!(f, "no settings revision {revision} in the history")
            }
            SettingsWriteError::Store(message) => write!(f, "state store: {message}"),
        }
    }
}

/// The settings documents of every target, revisioned.
#[derive(Default)]
pub struct ModelSettingsRegistry {
    entries: RwLock<BTreeMap<String, SettingsEntry>>,
    store: RwLock<Option<Arc<dyn StateStore>>>,
    /// History kept here when there is no state store (process lifetime).
    memory_history: Mutex<BTreeMap<String, Vec<ModelSettingsChange>>>,
    /// Serializes writes so read-revision → check → write is atomic in
    /// this process (the store's transaction covers other processes).
    write_lock: Mutex<()>,
    /// Requests in flight per engine model (`provider/model`, a variant's
    /// own name included) and when each model was last used.
    in_flight: Mutex<HashMap<String, ModelUse>>,
}

#[derive(Debug, Clone, Copy, Default)]
struct ModelUse {
    count: usize,
    last_used_ms: i64,
}

/// One request in flight on an engine model; dropping it ends it.
pub struct ModelInFlight {
    registry: Arc<ModelSettingsRegistry>,
    key: String,
}

impl Drop for ModelInFlight {
    fn drop(&mut self) {
        let mut map = self
            .registry
            .in_flight
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = map.get_mut(&self.key) {
            entry.count = entry.count.saturating_sub(1);
            entry.last_used_ms = sb_store::now_millis();
        }
    }
}

impl ModelSettingsRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Count one request in flight on an engine model (`provider/model`).
    pub fn enter(self: &Arc<Self>, key: &str) -> ModelInFlight {
        let mut map = self.in_flight.lock().unwrap_or_else(|e| e.into_inner());
        let entry = map.entry(key.to_string()).or_default();
        entry.count += 1;
        entry.last_used_ms = sb_store::now_millis();
        ModelInFlight {
            registry: self.clone(),
            key: key.to_string(),
        }
    }

    /// Requests in flight on an engine model now.
    pub fn in_flight(&self, key: &str) -> usize {
        self.in_flight
            .lock()
            .map(|map| map.get(key).map(|entry| entry.count).unwrap_or(0))
            .unwrap_or(0)
    }

    /// When a request last started or ended on an engine model (ms).
    pub fn last_used_ms(&self, key: &str) -> Option<i64> {
        self.in_flight
            .lock()
            .ok()
            .and_then(|map| map.get(key).map(|entry| entry.last_used_ms))
    }

    /// Attach the state store and load every stored document. A stored
    /// document that no longer parses is skipped with a warning (it stays
    /// in the store and its history).
    pub fn attach_store(&self, store: Arc<dyn StateStore>) {
        match store.model_settings_list() {
            Ok(records) => {
                let mut entries = self.entries.write().unwrap_or_else(|e| e.into_inner());
                for record in records {
                    match serde_json::from_str::<ModelSettingsDoc>(&record.doc) {
                        Ok(doc) => {
                            entries.insert(
                                record.target,
                                SettingsEntry {
                                    revision: record.revision,
                                    doc: Arc::new(doc),
                                    updated_at_ms: record.updated_at_ms,
                                },
                            );
                        }
                        Err(e) => tracing::warn!(
                            target_model = %record.target,
                            error = %e,
                            "stored model settings do not parse; skipped"
                        ),
                    }
                }
            }
            Err(e) => tracing::warn!(error = %e, "model settings: state store read failed"),
        }
        *self.store.write().unwrap_or_else(|e| e.into_inner()) = Some(store);
    }

    /// Whether writes are durable.
    pub fn durable(&self) -> bool {
        self.store
            .read()
            .map(|store| store.is_some())
            .unwrap_or(false)
    }

    pub fn get(&self, target: &str) -> Option<SettingsEntry> {
        self.entries
            .read()
            .ok()
            .and_then(|entries| entries.get(target).cloned())
    }

    /// The target's revision (0 = never written).
    pub fn revision(&self, target: &str) -> u64 {
        self.get(target).map(|entry| entry.revision).unwrap_or(0)
    }

    pub fn all(&self) -> Vec<(String, SettingsEntry)> {
        self.entries
            .read()
            .map(|entries| {
                entries
                    .iter()
                    .map(|(target, entry)| (target.clone(), entry.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Whether any target has settings (the request path skips all work
    /// when none does and the request asked nothing).
    pub fn is_empty(&self) -> bool {
        self.entries
            .read()
            .map(|entries| entries.values().all(|entry| entry.doc.is_empty()))
            .unwrap_or(true)
    }

    /// Replace a target's document if the writer edited the current
    /// revision. `validate` runs the engine-specific checks the caller
    /// knows (load keys, engine arguments).
    pub fn write(
        &self,
        target: &str,
        doc: ModelSettingsDoc,
        expected: u64,
        actor: &SettingsActor,
        action: &str,
    ) -> Result<SettingsEntry, SettingsWriteError> {
        doc.validate()
            .map_err(|e| SettingsWriteError::Invalid(e.0))?;
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let previous = self.get(target);
        let current = previous.as_ref().map(|entry| entry.revision).unwrap_or(0);
        if expected != current {
            return Err(SettingsWriteError::Conflict { expected, current });
        }
        let before = previous
            .as_ref()
            .map(|entry| (*entry.doc).clone())
            .unwrap_or_default();
        let doc_json =
            serde_json::to_string(&doc).map_err(|e| SettingsWriteError::Invalid(e.to_string()))?;
        let now = sb_store::now_millis();
        let change = ModelSettingsChange {
            target: target.to_string(),
            revision: current + 1,
            doc: doc_json,
            actor: actor.actor.clone(),
            role: actor.role.clone(),
            source: actor.source.clone(),
            action: action.to_string(),
            changed: changed_paths(&before, &doc),
            at_ms: now,
        };
        let store = self.store.read().ok().and_then(|store| store.clone());
        match store {
            Some(store) => match store.model_settings_write(&change) {
                Ok(ModelSettingsWrite::Written) => {}
                Ok(ModelSettingsWrite::Conflict { current: stored }) => {
                    // Another gateway sharing the store wrote first: take
                    // its document so the next read is current.
                    self.refresh_from_store(target, store.as_ref());
                    return Err(SettingsWriteError::Conflict {
                        expected,
                        current: stored,
                    });
                }
                Err(e) => return Err(SettingsWriteError::Store(e.0)),
            },
            None => {
                self.memory_history
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .entry(target.to_string())
                    .or_default()
                    .push(change);
            }
        }
        let entry = SettingsEntry {
            revision: current + 1,
            doc: Arc::new(doc),
            updated_at_ms: now,
        };
        self.entries
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(target.to_string(), entry.clone());
        Ok(entry)
    }

    fn refresh_from_store(&self, target: &str, store: &dyn StateStore) {
        let Ok(records) = store.model_settings_list() else {
            return;
        };
        let Some(record) = records.into_iter().find(|record| record.target == target) else {
            return;
        };
        if let Ok(doc) = serde_json::from_str::<ModelSettingsDoc>(&record.doc) {
            self.entries
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .insert(
                    target.to_string(),
                    SettingsEntry {
                        revision: record.revision,
                        doc: Arc::new(doc),
                        updated_at_ms: record.updated_at_ms,
                    },
                );
        }
    }

    /// The target's changes, newest first.
    pub fn history(&self, target: &str, limit: usize) -> Result<Vec<ModelSettingsChange>, String> {
        let store = self.store.read().ok().and_then(|store| store.clone());
        match store {
            Some(store) => store.model_settings_history(target, limit).map_err(|e| e.0),
            None => Ok(self
                .memory_history
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(target)
                .map(|changes| changes.iter().rev().take(limit).cloned().collect())
                .unwrap_or_default()),
        }
    }

    /// Write revision `to`'s document again as a new revision.
    pub fn revert(
        &self,
        target: &str,
        to: u64,
        expected: u64,
        actor: &SettingsActor,
    ) -> Result<SettingsEntry, SettingsWriteError> {
        let history = self
            .history(target, 10_000)
            .map_err(SettingsWriteError::Store)?;
        let change = history
            .into_iter()
            .find(|change| change.revision == to)
            .ok_or(SettingsWriteError::UnknownRevision(to))?;
        let doc: ModelSettingsDoc = serde_json::from_str(&change.doc)
            .map_err(|e| SettingsWriteError::Invalid(format!("revision {to}: {e}")))?;
        self.write(target, doc, expected, actor, &format!("revert:{to}"))
    }
}

/// The key paths that differ between two documents (`defaults.temperature`,
/// `profiles.nothink.reasoning`, `locked`, `load.ctx_size`, ...).
pub fn changed_paths(before: &ModelSettingsDoc, after: &ModelSettingsDoc) -> Vec<String> {
    let (a, b) = (flat_doc(before), flat_doc(after));
    let mut keys: Vec<String> = a
        .keys()
        .chain(b.keys())
        .filter(|key| a.get(*key) != b.get(*key))
        .cloned()
        .collect();
    keys.sort();
    keys.dedup();
    keys
}

fn flat_doc(doc: &ModelSettingsDoc) -> BTreeMap<String, Json> {
    let mut out = BTreeMap::new();
    let Ok(Json::Object(top)) = serde_json::to_value(doc) else {
        return out;
    };
    for (name, value) in top {
        match (name.as_str(), value) {
            ("profiles", Json::Object(profiles)) => {
                for (profile, layer) in profiles {
                    match layer {
                        Json::Object(layer) if !layer.is_empty() => {
                            for (key, value) in layer {
                                out.insert(format!("profiles.{profile}.{key}"), value);
                            }
                        }
                        other => {
                            out.insert(format!("profiles.{profile}"), other);
                        }
                    }
                }
            }
            (_, Json::Object(object)) => {
                for (key, value) in object {
                    out.insert(format!("{name}.{key}"), value);
                }
            }
            (_, other) => {
                out.insert(name, other);
            }
        }
    }
    out
}

/// The settings engine family of a provider.
pub fn settings_engine(snap: &Snapshot, provider_id: &str) -> SettingsEngine {
    let engine = snap
        .config
        .providers
        .iter()
        .find(|provider| provider.id == provider_id)
        .and_then(|provider| match &provider.kind {
            ProviderKind::OpenaiCompatible { engine, .. } => Some(*engine),
            _ => None,
        });
    match engine {
        Some(OpenAiEngine::LlamaCpp) => SettingsEngine::LlamaCpp,
        Some(OpenAiEngine::Vllm) => SettingsEngine::Vllm,
        Some(OpenAiEngine::Ollama) => SettingsEngine::Ollama,
        _ => SettingsEngine::Generic,
    }
}

/// The request each attempt sends, per target, with that target's
/// settings applied. Targets with no settings (and a request that asked
/// nothing) are absent: their attempts send the request unchanged.
#[derive(Default)]
pub(crate) struct SettingsPlan {
    per_target: HashMap<String, PlannedTarget>,
    registry: Option<Arc<ModelSettingsRegistry>>,
}

struct PlannedTarget {
    request: AiRequest,
    /// The upstream model the attempt names instead of the target's own
    /// (a load-settings variant).
    model: Option<String>,
    summary: String,
}

impl SettingsPlan {
    /// The request (and target) an attempt on `target` sends.
    pub(crate) fn prepare(
        &self,
        req: &AiRequest,
        target: &ExecutionTarget,
    ) -> (AiRequest, ExecutionTarget) {
        match self.per_target.get(&target.id) {
            Some(planned) => {
                let mut target = target.clone();
                if let Some(model) = &planned.model {
                    target.model = model.clone();
                }
                (planned.request.clone(), target)
            }
            None => (req.clone(), target.clone()),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.per_target.is_empty()
    }

    /// Count an attempt on `target` (its variant name when it has one) as
    /// in flight on that engine model until the guard drops.
    pub(crate) fn enter(&self, target: &ExecutionTarget) -> Option<ModelInFlight> {
        let registry = self.registry.as_ref()?;
        let model = self
            .per_target
            .get(&target.id)
            .and_then(|planned| planned.model.as_deref())
            .unwrap_or(&target.model);
        Some(registry.enter(&format!("{}/{model}", target.provider_id)))
    }

    /// Send attempts on `target` to the engine model `model` (a variant).
    pub(crate) fn set_model(&mut self, target: &ExecutionTarget, req: &AiRequest, model: String) {
        let planned = self
            .per_target
            .entry(target.id.clone())
            .or_insert_with(|| PlannedTarget {
                request: req.clone(),
                model: None,
                summary: String::new(),
            });
        planned.summary = if planned.summary.is_empty() {
            format!("variant={model}")
        } else {
            format!("{}; variant={model}", planned.summary)
        };
        planned.model = Some(model);
    }

    /// The settings summary for a target, if any.
    pub(crate) fn summary_for(&self, target_id: &str) -> Option<&str> {
        self.per_target
            .get(target_id)
            .map(|planned| planned.summary.as_str())
            .filter(|summary| !summary.is_empty())
    }
}

/// Build the settings plan for a request's candidates. `Err` is a 400 for
/// a profile no candidate defines.
pub(crate) fn plan_settings(
    registry: &Arc<ModelSettingsRegistry>,
    snap: &Snapshot,
    route_name: &str,
    req: &AiRequest,
    request: Option<&RequestSettings>,
    candidates: &[ExecutionTarget],
) -> Result<SettingsPlan, ExecError> {
    let mut plan = SettingsPlan {
        registry: Some(registry.clone()),
        ..SettingsPlan::default()
    };
    if request.is_none() && registry.is_empty() {
        return Ok(plan);
    }
    let route_profile = snap
        .config
        .routes
        .iter()
        .find(|route| route.name == route_name)
        .and_then(|route| route.settings_profile.as_deref());
    let entries: Vec<(&ExecutionTarget, Option<SettingsEntry>)> = candidates
        .iter()
        .map(|target| (target, registry.get(&target.id)))
        .collect();
    if let Some(profile) = request.and_then(|request| request.profile.as_deref()) {
        let (known, names) = sb_core::profile_known(
            entries
                .iter()
                .map(|(_, entry)| entry.as_ref().map(|entry| entry.doc.as_ref())),
            profile,
        );
        if !known {
            let list = if names.is_empty() {
                "none are defined".to_string()
            } else {
                format!(
                    "defined: {}",
                    names.into_iter().collect::<Vec<_>>().join(", ")
                )
            };
            return Err(ExecError::new(
                400,
                "unknown_profile",
                format!(
                    "settings profile `{profile}` is not defined for `{}`; {list}",
                    req.model
                ),
                None,
            ));
        }
    }
    for (target, entry) in entries {
        let doc = entry.as_ref().map(|entry| entry.doc.as_ref());
        if doc.is_none_or(|doc| !doc.has_request_settings() && doc.locked.is_empty())
            && request
                .is_none_or(|request| request.settings.is_empty() && request.profile.is_none())
        {
            continue;
        }
        let layered = sb_core::layer_settings(doc, route_profile, request);
        let mut prepared = req.clone();
        let applied = sb_core::apply_layered(
            &mut prepared,
            doc,
            &layered,
            settings_engine(snap, &target.provider_id),
        );
        if applied.is_empty() {
            continue;
        }
        plan.per_target.insert(
            target.id.clone(),
            PlannedTarget {
                request: prepared,
                model: None,
                summary: applied.header_value(),
            },
        );
    }
    Ok(plan)
}

/// Check a request's load settings against the first candidate's
/// `request_load` policy. `Ok` is that target and its policy; `Err` a 400.
/// Client keys may set only `allow`ed keys; every key stays within
/// `bounds`; file paths never.
pub(crate) fn check_request_load(
    registry: &ModelSettingsRegistry,
    req: &AiRequest,
    role: &str,
    load: &Map<String, Json>,
    candidates: &[ExecutionTarget],
) -> Result<(ExecutionTarget, sb_core::RequestLoadPolicy), ExecError> {
    let bad = |kind: &str, message: String| ExecError::new(400, kind, message, None);
    let Some(first) = candidates.first() else {
        return Err(bad(
            "request_load_disabled",
            format!("no target for `{}`", req.model),
        ));
    };
    let policy = registry
        .get(&first.id)
        .and_then(|entry| entry.doc.request_load.clone())
        .ok_or_else(|| {
            bad(
                "request_load_disabled",
                format!(
                    "`switchback.load` needs a `request_load` policy on `{}`; it has none, so \
                     per-request load settings are refused",
                    first.id
                ),
            )
        })?;
    for (key, value) in load {
        sb_core::validate_load_value(key, value)
            .map_err(|e| bad("invalid_settings", format!("switchback.load: {e}")))?;
        if sb_core::load_key(key).is_some_and(|k| k.kind == sb_core::LoadKind::Path) {
            return Err(bad(
                "request_load_refused",
                format!("switchback.load: `{key}` names a file; a request may not set it"),
            ));
        }
        if role == "client" && !policy.allow.iter().any(|allowed| allowed == key) {
            return Err(bad(
                "request_load_refused",
                format!(
                    "switchback.load: `{key}` is not allowed for this key on `{}`; allowed: {}",
                    first.id,
                    if policy.allow.is_empty() {
                        "none".to_string()
                    } else {
                        policy.allow.join(", ")
                    }
                ),
            ));
        }
        if let (Some([min, max]), Some(number)) = (policy.bounds.get(key), value.as_f64()) {
            if number < *min || number > *max {
                return Err(bad(
                    "out_of_bounds",
                    format!(
                        "switchback.load: `{key}` = {number} is outside [{min}, {max}] on `{}`",
                        first.id
                    ),
                ));
            }
        }
    }
    Ok((first.clone(), policy))
}

/// A request's own settings layer, as a map for display.
pub fn request_settings_map(request: &RequestSettings) -> Map<String, Json> {
    request.settings.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn actor() -> SettingsActor {
        SettingsActor {
            actor: "tenant=ops".into(),
            role: "admin".into(),
            source: "test".into(),
        }
    }

    fn doc(value: Json) -> ModelSettingsDoc {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn write_conflict_history_revert_in_memory() {
        let registry = ModelSettingsRegistry::new();
        let first = registry
            .write(
                "text/m",
                doc(json!({"defaults": {"temperature": 0.6}})),
                0,
                &actor(),
                "put",
            )
            .unwrap();
        assert_eq!(first.revision, 1);
        let stale = registry.write("text/m", doc(json!({})), 0, &actor(), "put");
        assert_eq!(
            stale.unwrap_err(),
            SettingsWriteError::Conflict {
                expected: 0,
                current: 1
            }
        );
        registry
            .write(
                "text/m",
                doc(json!({"defaults": {"temperature": 0.2}})),
                1,
                &actor(),
                "patch",
            )
            .unwrap();
        let history = registry.history("text/m", 10).unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].changed, vec!["defaults.temperature".to_string()]);
        let reverted = registry.revert("text/m", 1, 2, &actor()).unwrap();
        assert_eq!(reverted.revision, 3);
        assert_eq!(reverted.doc.defaults["temperature"], json!(0.6));
    }

    #[test]
    fn invalid_document_is_refused() {
        let registry = ModelSettingsRegistry::new();
        let e = registry
            .write(
                "text/m",
                doc(json!({"defaults": {"nope": 1}})),
                0,
                &actor(),
                "put",
            )
            .unwrap_err();
        assert!(matches!(e, SettingsWriteError::Invalid(_)));
        assert_eq!(registry.revision("text/m"), 0);
    }

    #[test]
    fn changed_paths_name_nested_keys() {
        let before =
            doc(json!({"defaults": {"temperature": 0.6}, "profiles": {"a": {"top_k": 1}}}));
        let after = doc(
            json!({"defaults": {"temperature": 0.6, "min_p": 0.1}, "profiles": {"a": {"top_k": 2}}, "locked": ["top_k"]}),
        );
        assert_eq!(
            changed_paths(&before, &after),
            vec!["defaults.min_p", "locked", "profiles.a.top_k"]
        );
    }
}
