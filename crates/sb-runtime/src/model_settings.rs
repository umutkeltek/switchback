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
use sb_store::{ModelSettingsChange, StateStore};

use crate::{ExecError, Snapshot};

/// A variant found or made for a request.
#[derive(Debug)]
pub struct VariantLease {
    /// The engine model name the attempt sends (`<model>~<hash8>`).
    pub name: String,
    /// The request's admission on the variant.
    pub admission: ModelInFlight,
    /// Extra words for `x-switchback-settings` (`fit=unchecked`).
    pub note: Option<String>,
}

/// Makes (or finds) the engine variant a request's load settings ask for:
/// `sb-server`'s engine control. `Ok` is the upstream model name the
/// attempt sends (`<model>~<hash8>`); `Err` is the answer to give.
pub trait ModelVariants: Send + Sync {
    /// `Ok` carries the variant's admission: the request is counted on the
    /// variant from the moment it is found or made, so nothing removes the
    /// variant while the request still waits for its lane.
    fn ensure_variant<'a>(
        &'a self,
        target: &'a ExecutionTarget,
        load: &'a Map<String, Json>,
        policy: &'a sb_core::RequestLoadPolicy,
        role: &'a str,
    ) -> crate::BoxFuture<'a, Result<VariantLease, ExecError>>;

    /// Before an attempt takes its lane's place: when an earlier apply did
    /// not reach the engine and `model`'s own section is still pending, run
    /// it now (when due and no apply is running), so the model loads with
    /// its current settings. Never waits for another apply, never runs for a
    /// model whose section is current, and never fails the request (a failed
    /// apply stays pending).
    fn before_dispatch<'a>(
        &'a self,
        _provider_id: &'a str,
        _model: &'a str,
    ) -> crate::BoxFuture<'a, ()> {
        Box::pin(async {})
    }
}

/// One target's current settings.
pub type SettingsEntry = crate::revisioned::DocEntry<ModelSettingsDoc>;

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
    /// The document is locked: the message says why and how to unlock.
    Locked(String),
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
            SettingsWriteError::Locked(message) => f.write_str(message),
        }
    }
}

/// The settings documents of every target, revisioned.
pub struct ModelSettingsRegistry {
    /// Every target's document, revisioned, with its history.
    docs: crate::revisioned::DocRegistry<ModelSettingsDoc>,
    /// Per engine model (`provider/model`, a variant's own name included):
    /// requests admitted and dispatched, when it was last used, and its
    /// admission gate.
    in_flight: Mutex<HashMap<String, ModelUse>>,
    /// Bumped on every change of `in_flight` (a request ends, a gate opens
    /// or closes): waiters on a gate or a drain watch it.
    changed: tokio::sync::watch::Sender<u64>,
    /// Called (outside every lock) with an engine model's key when its last
    /// dispatched request ends: the engine control retries an apply it held
    /// back for that model.
    idle_hook: RwLock<Option<IdleHook>>,
}

/// See [`ModelSettingsRegistry::set_idle_hook`].
pub type IdleHook = Arc<dyn Fn(&str) + Send + Sync>;

/// The default for `server.model_settings_history_keep`.
pub const DEFAULT_HISTORY_KEEP: usize = 500;

impl Default for ModelSettingsRegistry {
    fn default() -> Self {
        ModelSettingsRegistry {
            docs: crate::revisioned::DocRegistry::default(),
            in_flight: Mutex::default(),
            changed: tokio::sync::watch::channel(0).0,
            idle_hook: RwLock::default(),
        }
    }
}

/// One engine model's use and gate.
#[derive(Debug, Clone, Copy, Default)]
struct ModelUse {
    /// Requests whose routing committed to this model: queued, waking and
    /// dispatched ones.
    count: usize,
    /// Of those, the ones sent (or about to be sent) to the engine.
    dispatched: usize,
    last_used_ms: i64,
    /// Open [`GateKind::Drain`] closers (unload, `apply: now`).
    drains: u32,
    /// Open [`GateKind::Hold`] closers (a preset re-read).
    holds: u32,
}

/// How a closer closes an engine model's admission gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateKind {
    /// No new request is admitted; admitted ones (queued or waking
    /// included) go on and the closer waits for them to end. Unload and
    /// `apply: now`.
    Drain,
    /// No new request is admitted and no admitted one is dispatched; the
    /// closer waits only for dispatched ones. A preset re-read: a queued
    /// request reaches the engine after it, with the new settings.
    Hold,
}

/// A request could not pass a closed gate within its wait.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateTimeout {
    pub key: String,
    pub waited: std::time::Duration,
}

impl std::fmt::Display for GateTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "`{}` is being unloaded or reloaded with new settings and did not reopen within {} s",
            self.key,
            self.waited.as_secs()
        )
    }
}

/// One request admitted to an engine model (and, once
/// [`ModelInFlight::dispatch`] passed, dispatched to it); dropping it ends
/// it.
pub struct ModelInFlight {
    registry: Arc<ModelSettingsRegistry>,
    key: String,
    dispatched: bool,
}

impl std::fmt::Debug for ModelInFlight {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelInFlight")
            .field("key", &self.key)
            .field("dispatched", &self.dispatched)
            .finish()
    }
}

impl ModelInFlight {
    /// The engine model this request is counted on.
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Mark the request as reaching the engine: waits (bounded) while a
    /// [`GateKind::Hold`] closer re-reads the model's preset. Idempotent.
    pub async fn dispatch(&mut self, wait: std::time::Duration) -> Result<(), GateTimeout> {
        if self.dispatched {
            return Ok(());
        }
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let mut changed = self.registry.changed.subscribe();
            {
                let mut map = self.registry.lock_use();
                let entry = map.entry(self.key.clone()).or_default();
                if entry.holds == 0 {
                    entry.dispatched += 1;
                    entry.last_used_ms = sb_store::now_millis();
                    self.dispatched = true;
                    return Ok(());
                }
            }
            if tokio::time::timeout_at(deadline, changed.changed())
                .await
                .is_err()
            {
                return Err(GateTimeout {
                    key: self.key.clone(),
                    waited: wait,
                });
            }
        }
    }
}

impl Drop for ModelInFlight {
    fn drop(&mut self) {
        let idle = {
            let mut map = self.registry.lock_use();
            match map.get_mut(&self.key) {
                Some(entry) => {
                    entry.count = entry.count.saturating_sub(1);
                    if self.dispatched {
                        entry.dispatched = entry.dispatched.saturating_sub(1);
                    }
                    entry.last_used_ms = sb_store::now_millis();
                    self.dispatched && entry.dispatched == 0
                }
                None => false,
            }
        };
        self.registry.bump();
        if idle {
            self.registry.run_idle_hook(&self.key);
        }
    }
}

/// A closed admission gate of one engine model; dropping it reopens it.
pub struct ModelGate {
    registry: Arc<ModelSettingsRegistry>,
    key: String,
    kind: GateKind,
}

impl ModelGate {
    pub fn key(&self) -> &str {
        &self.key
    }

    /// What this closer waits for: admitted requests (drain) or dispatched
    /// ones (hold).
    pub fn busy(&self) -> usize {
        let map = self.registry.lock_use();
        map.get(&self.key)
            .map(|entry| match self.kind {
                GateKind::Drain => entry.count,
                GateKind::Hold => entry.dispatched,
            })
            .unwrap_or(0)
    }

    /// Wait (bounded) until [`Self::busy`] is 0. `Err` is the count left.
    pub async fn wait_idle(&self, wait: std::time::Duration) -> Result<(), usize> {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let mut changed = self.registry.changed.subscribe();
            let busy = self.busy();
            if busy == 0 {
                return Ok(());
            }
            if tokio::time::timeout_at(deadline, changed.changed())
                .await
                .is_err()
            {
                let busy = self.busy();
                return if busy == 0 { Ok(()) } else { Err(busy) };
            }
        }
    }
}

impl Drop for ModelGate {
    fn drop(&mut self) {
        {
            let mut map = self.registry.lock_use();
            if let Some(entry) = map.get_mut(&self.key) {
                match self.kind {
                    GateKind::Drain => entry.drains = entry.drains.saturating_sub(1),
                    GateKind::Hold => entry.holds = entry.holds.saturating_sub(1),
                }
            }
        }
        self.registry.bump();
    }
}

impl ModelSettingsRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock_use(&self) -> std::sync::MutexGuard<'_, HashMap<String, ModelUse>> {
        self.in_flight.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn bump(&self) {
        self.changed.send_modify(|n| *n = n.wrapping_add(1));
    }

    fn run_idle_hook(&self, key: &str) {
        let hook = self
            .idle_hook
            .read()
            .ok()
            .and_then(|hook| hook.as_ref().cloned());
        if let Some(hook) = hook {
            hook(key);
        }
    }

    /// Called with an engine model's key whenever its last dispatched
    /// request ends (outside every lock of the registry).
    pub fn set_idle_hook(&self, hook: IdleHook) {
        *self.idle_hook.write().unwrap_or_else(|e| e.into_inner()) = Some(hook);
    }

    /// How many revisions per target the history keeps (at least 1).
    pub fn set_history_keep(&self, keep: usize) {
        self.docs.set_history_keep(keep);
    }

    pub fn history_keep(&self) -> usize {
        self.docs.history_keep()
    }

    /// Admit one request to an engine model now, if its gate is open.
    pub fn try_admit(self: &Arc<Self>, key: &str) -> Option<ModelInFlight> {
        {
            let mut map = self.lock_use();
            let entry = map.entry(key.to_string()).or_default();
            if entry.drains > 0 || entry.holds > 0 {
                return None;
            }
            entry.count += 1;
            entry.last_used_ms = sb_store::now_millis();
        }
        self.bump();
        Some(ModelInFlight {
            registry: self.clone(),
            key: key.to_string(),
            dispatched: false,
        })
    }

    /// Admit one request to an engine model, waiting (bounded) while its
    /// gate is closed.
    pub async fn admit(
        self: &Arc<Self>,
        key: &str,
        wait: std::time::Duration,
    ) -> Result<ModelInFlight, GateTimeout> {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let mut changed = self.changed.subscribe();
            if let Some(admitted) = self.try_admit(key) {
                return Ok(admitted);
            }
            if tokio::time::timeout_at(deadline, changed.changed())
                .await
                .is_err()
            {
                return Err(GateTimeout {
                    key: key.to_string(),
                    waited: wait,
                });
            }
        }
    }

    /// Close an engine model's admission gate. Under the same lock as every
    /// admission, so a request is either counted before (and the closer
    /// sees it in [`ModelGate::busy`]) or waits for the gate to reopen.
    pub fn close(self: &Arc<Self>, key: &str, kind: GateKind) -> ModelGate {
        {
            let mut map = self.lock_use();
            let entry = map.entry(key.to_string()).or_default();
            match kind {
                GateKind::Drain => entry.drains += 1,
                GateKind::Hold => entry.holds += 1,
            }
        }
        self.bump();
        ModelGate {
            registry: self.clone(),
            key: key.to_string(),
            kind,
        }
    }

    /// Requests admitted to an engine model now (queued, waking and
    /// dispatched).
    pub fn in_flight(&self, key: &str) -> usize {
        self.lock_use()
            .get(key)
            .map(|entry| entry.count)
            .unwrap_or(0)
    }

    /// Requests dispatched to an engine model now.
    pub fn dispatched(&self, key: &str) -> usize {
        self.lock_use()
            .get(key)
            .map(|entry| entry.dispatched)
            .unwrap_or(0)
    }

    /// Requests in flight on every engine model of a provider (lane).
    pub fn in_flight_on_provider(&self, provider_id: &str) -> usize {
        let prefix = format!("{provider_id}/");
        self.in_flight
            .lock()
            .map(|map| {
                map.iter()
                    .filter(|(key, _)| key.starts_with(&prefix))
                    .map(|(_, entry)| entry.count)
                    .sum()
            })
            .unwrap_or(0)
    }

    /// When a request last started or ended on an engine model (ms).
    pub fn last_used_ms(&self, key: &str) -> Option<i64> {
        self.lock_use()
            .get(key)
            .map(|entry| entry.last_used_ms)
            .filter(|ms| *ms > 0)
    }

    /// Attach the state store and load every stored document. A stored
    /// document that no longer parses is skipped with a warning (it stays
    /// in the store and its history).
    pub fn attach_store(&self, store: Arc<dyn StateStore>) {
        self.docs.attach_store(store);
    }

    /// Whether writes are durable.
    pub fn durable(&self) -> bool {
        self.docs.durable()
    }

    pub fn get(&self, target: &str) -> Option<SettingsEntry> {
        self.docs.get(target)
    }

    /// The target's revision (0 = never written).
    pub fn revision(&self, target: &str) -> u64 {
        self.docs.revision(target)
    }

    pub fn all(&self) -> Vec<(String, SettingsEntry)> {
        self.docs.all()
    }

    /// Whether any target has settings (the request path skips all work
    /// when none does and the request asked nothing).
    pub fn is_empty(&self) -> bool {
        self.docs
            .all()
            .iter()
            .all(|(_, entry)| entry.doc.is_empty())
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
        self.docs.write(target, doc, expected, actor, action)
    }

    /// The target's changes, newest first.
    pub fn history(&self, target: &str, limit: usize) -> Result<Vec<ModelSettingsChange>, String> {
        self.docs.history(target, limit)
    }

    /// Revision `to`'s document, searched in every revision the history
    /// keeps. Revision 0 is the state before the first write: no saved
    /// settings and no locks, so a revert to 0 is a new revision with an
    /// empty document (the history stays).
    pub fn revision_doc(
        &self,
        target: &str,
        to: u64,
    ) -> Result<ModelSettingsDoc, SettingsWriteError> {
        self.docs.revision_doc(target, to)
    }

    /// Write revision `to`'s document again as a new revision. The HTTP
    /// revert runs the engine checks and the apply around this, as a PUT
    /// does.
    pub fn revert(
        &self,
        target: &str,
        to: u64,
        expected: u64,
        actor: &SettingsActor,
    ) -> Result<SettingsEntry, SettingsWriteError> {
        let doc = self.revision_doc(target, to)?;
        self.write(target, doc, expected, actor, &format!("revert:{to}"))
    }
}

impl crate::revisioned::RevisionedDoc for ModelSettingsDoc {
    const FAMILY: crate::revisioned::DocFamily = crate::revisioned::DocFamily::ModelSettings;

    fn check(&self) -> Result<(), String> {
        self.validate().map_err(|e| e.0)
    }

    fn changed(before: &Self, after: &Self) -> Vec<String> {
        changed_paths(before, after)
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

/// How long a request waits at a closed admission gate of a model on
/// `provider_id`'s lane: its engine's `apply_wait_secs`, and never less than
/// an apply can hold a model (`command_timeout_secs` for the settings
/// command plus 30 s for the engine's re-read), so a request is never failed
/// while an apply that will finish is still running. `None` = the lane has
/// no engine control, so nothing closes the gate and nothing is counted.
pub fn gate_wait(snap: &Snapshot, provider_id: &str) -> Option<std::time::Duration> {
    snap.config
        .local_executors
        .iter()
        .find(|lane| lane.name == provider_id)
        .and_then(|lane| {
            let engine = lane.engine.as_ref()?;
            let apply_hold = lane.command_timeout_secs.max(1).saturating_add(30);
            Some(std::time::Duration::from_secs(
                engine.apply_wait_secs.max(apply_hold),
            ))
        })
}

/// A closed gate answered: 503 with a retry delay and a typed reason.
pub(crate) fn gate_error(e: &GateTimeout) -> ExecError {
    ExecError::new(503, "model_paused", e.to_string(), None)
        .with_retry_after(std::time::Duration::from_secs(5))
}

/// A closed gate met by a request that another target can serve: skipped
/// at once, never waited for.
pub(crate) fn gate_closed_error(key: &str) -> ExecError {
    ExecError::new(
        503,
        "model_paused",
        format!("`{key}` is being unloaded or reloaded with new settings"),
        None,
    )
    .with_retry_after(std::time::Duration::from_secs(5))
}

/// Admissions only (embeddings and audio: no request settings), for the
/// same per-model counting and gates as chat.
pub(crate) fn admissions_only(
    registry: &Arc<ModelSettingsRegistry>,
    variants: Option<Arc<dyn ModelVariants>>,
) -> SettingsPlan {
    SettingsPlan {
        registry: Some(registry.clone()),
        variants,
        ..SettingsPlan::default()
    }
}

/// The lane's busy guard with the engine model's admission added: both held
/// until the answer (or the stream) ends.
pub(crate) fn hold_with_lane(
    lane_busy: Option<crate::LaneBusy>,
    admission: ModelInFlight,
) -> crate::LaneBusy {
    match lane_busy {
        Some(busy) => Box::new((busy, admission)),
        None => Box::new(admission),
    }
}

/// The request each attempt sends, per target, with that target's
/// settings applied. Targets with no settings (and a request that asked
/// nothing) are absent: their attempts send the request unchanged.
#[derive(Default)]
pub(crate) struct SettingsPlan {
    per_target: HashMap<String, PlannedTarget>,
    registry: Option<Arc<ModelSettingsRegistry>>,
    /// Admissions taken before the attempt (the first choice's, when
    /// routing committed to it; a variant's, when it was made), per target.
    admissions: HashMap<String, ModelInFlight>,
    /// The engine control's seam (pending applies before a dispatch).
    variants: Option<Arc<dyn ModelVariants>>,
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

    /// The engine control's seam, for its pending applies before a dispatch.
    pub(crate) fn set_variants(&mut self, variants: Option<Arc<dyn ModelVariants>>) {
        self.variants = variants;
    }

    /// Keep an admission taken before the attempt (replacing an earlier
    /// one for the same target).
    pub(crate) fn add_admission(&mut self, target_id: &str, admission: ModelInFlight) {
        self.admissions.insert(target_id.to_string(), admission);
    }

    /// The engine model an attempt on `target` reaches (`provider/model`,
    /// a variant's own name when it has one).
    fn model_key(&self, target: &ExecutionTarget) -> String {
        let model = self
            .per_target
            .get(&target.id)
            .and_then(|planned| planned.model.as_deref())
            .unwrap_or(&target.model);
        format!("{}/{model}", target.provider_id)
    }

    /// The admission of an attempt on `target`'s engine model: the one
    /// taken earlier when there is one, else a new one. At a closed gate the
    /// request waits (bounded) only when `may_wait` (no later target can
    /// serve it); otherwise the target is skipped at once. Then the lane's
    /// pending apply of this model, when one is due. All of it before the
    /// attempt's lane place, so a request never holds a place while it waits
    /// for a gate or an apply. `Ok(None)`: the lane has no engine control.
    pub(crate) async fn admission_for(
        &mut self,
        snap: &Snapshot,
        target: &ExecutionTarget,
        may_wait: bool,
    ) -> Result<Option<ModelInFlight>, ExecError> {
        let key = self.model_key(target);
        let stored = self
            .admissions
            .remove(&target.id)
            .filter(|admission| admission.key() == key);
        let (Some(registry), Some(wait)) =
            (self.registry.as_ref(), gate_wait(snap, &target.provider_id))
        else {
            return Ok(None);
        };
        let admission = match stored {
            Some(admission) => admission,
            None if may_wait => registry
                .admit(&key, wait)
                .await
                .map_err(|e| gate_error(&e))?,
            None => registry
                .try_admit(&key)
                .ok_or_else(|| gate_closed_error(&key))?,
        };
        self.pending_apply(target).await;
        Ok(Some(admission))
    }

    /// Whether this plan holds an admission for `target_id` on engine
    /// model `key` (it counts in [`ModelSettingsRegistry::in_flight`]).
    pub(crate) fn holds_admission(&self, target_id: &str, key: &str) -> bool {
        self.admissions
            .get(target_id)
            .is_some_and(|admission| admission.key() == key)
    }

    /// Let go of an admission taken earlier for `target` (the attempt
    /// skipped it before dispatch: circuit open, over budget, lane down).
    pub(crate) fn release(&mut self, target_id: &str) {
        self.admissions.remove(target_id);
    }

    async fn pending_apply(&self, target: &ExecutionTarget) {
        if let Some(variants) = &self.variants {
            let key = self.model_key(target);
            let model = key
                .strip_prefix(&format!("{}/", target.provider_id))
                .unwrap_or(&target.model);
            variants.before_dispatch(&target.provider_id, model).await;
        }
    }

    /// Right before the attempt reaches the engine (the lane's place held):
    /// the dispatch, waiting (bounded) while a preset re-read holds the
    /// model.
    pub(crate) async fn dispatch(
        &self,
        snap: &Snapshot,
        target: &ExecutionTarget,
        admission: &mut ModelInFlight,
    ) -> Result<(), ExecError> {
        let wait =
            gate_wait(snap, &target.provider_id).unwrap_or(std::time::Duration::from_secs(1));
        admission.dispatch(wait).await.map_err(|e| gate_error(&e))
    }

    /// A hedge racer's admission and dispatch: always a new admission (the
    /// plan's own stays with the sequential path), and never a wait: a racer
    /// whose model's gate is closed or held is not started. A racer already
    /// holds its lane's place, so it never runs a pending apply either.
    pub(crate) async fn admit_racer(
        &self,
        snap: &Snapshot,
        target: &ExecutionTarget,
    ) -> Result<Option<ModelInFlight>, ExecError> {
        let (Some(registry), Some(_)) =
            (self.registry.as_ref(), gate_wait(snap, &target.provider_id))
        else {
            return Ok(None);
        };
        let key = self.model_key(target);
        let mut admission = registry
            .try_admit(&key)
            .ok_or_else(|| gate_closed_error(&key))?;
        admission
            .dispatch(std::time::Duration::ZERO)
            .await
            .map_err(|e| gate_error(&e))?;
        Ok(Some(admission))
    }

    /// Send attempts on `target` to the engine model `model` (a variant).
    pub(crate) fn set_model(
        &mut self,
        target: &ExecutionTarget,
        req: &AiRequest,
        model: String,
        note: Option<String>,
    ) {
        let planned = self
            .per_target
            .entry(target.id.clone())
            .or_insert_with(|| PlannedTarget {
                request: req.clone(),
                model: None,
                summary: String::new(),
            });
        let variant = match note {
            Some(note) => format!("variant={model}; {note}"),
            None => format!("variant={model}"),
        };
        planned.summary = if planned.summary.is_empty() {
            variant
        } else {
            format!("{}; {variant}", planned.summary)
        };
        planned.model = Some(model);
    }

    /// Set one top-level field on the request attempts on `target` send
    /// (the sticky conversation's `id_slot`).
    pub(crate) fn set_passthrough(
        &mut self,
        target: &ExecutionTarget,
        req: &AiRequest,
        key: &str,
        value: Json,
    ) {
        let planned = self
            .per_target
            .entry(target.id.clone())
            .or_insert_with(|| PlannedTarget {
                request: req.clone(),
                model: None,
                summary: String::new(),
            });
        planned.request.passthrough.insert(key.to_string(), value);
    }

    /// The engine model attempts on `target` name (its variant, else its
    /// own model).
    pub(crate) fn engine_model(&self, target: &ExecutionTarget) -> String {
        self.per_target
            .get(&target.id)
            .and_then(|planned| planned.model.clone())
            .unwrap_or_else(|| target.model.clone())
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
        let guard = doc.and_then(|doc| doc.active_reasoning_guard());
        if doc.is_none_or(|doc| !doc.has_request_settings() && doc.locked.is_empty())
            && request
                .is_none_or(|request| request.settings.is_empty() && request.profile.is_none())
            && guard.is_none()
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
        let mut summary = (!applied.is_empty()).then(|| applied.header_value());
        // The reasoning guard (`reasoning: true` + `reasoning_guard`): a
        // small `max_tokens` turns thinking off for this request.
        if let (Some(doc), Some(guard)) = (doc, guard) {
            if let Some(note) =
                crate::optimize::apply_reasoning_guard(&mut prepared, req, request, doc, guard)
            {
                summary = Some(match summary {
                    Some(summary) => format!("{summary}; {note}"),
                    None => note,
                });
            }
        }
        let Some(summary) = summary else {
            continue;
        };
        plan.per_target.insert(
            target.id.clone(),
            PlannedTarget {
                request: prepared,
                model: None,
                summary,
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
    fn engine_snapshot(extra: &str) -> Arc<crate::Snapshot> {
        let cfg = sb_core::Config::from_yaml(&format!(
            r#"
providers:
  - id: text
    type: openai_compatible
    base_url: "http://127.0.0.1:1/v1"
    accounts:
      - id: a
        auth: {{ kind: api_key, inline: "k" }}
local_executors:
  - name: text
    base_url: "http://127.0.0.1:1"
    health_endpoint: "http://127.0.0.1:1/health"
{extra}
    engine:
      kind: llama_cpp_router
      settings_command: "true"
      apply_wait_secs: 5
"#
        ))
        .unwrap();
        let cfg = Arc::new(cfg);
        let registry = Arc::new(sb_adapters::AdapterRegistry::from_config(&cfg).unwrap());
        let resolver = Arc::new(sb_credentials::CredentialResolver::from_config(&cfg).unwrap());
        crate::Engine::new(
            cfg,
            registry,
            resolver,
            Arc::new(sb_ledger::UsageLedger::in_memory()),
        )
        .snapshot()
    }

    /// A hedge racer never waits at a closed or held gate: it is not
    /// started, at once.
    #[tokio::test]
    async fn a_hedge_racer_never_waits_at_a_gate() {
        let snap = engine_snapshot("");
        let registry = Arc::new(ModelSettingsRegistry::new());
        let plan = admissions_only(&registry, None);
        let target = ExecutionTarget::new("text", "m", sb_core::ExecutionTargetKind::ModelApi);
        for kind in [GateKind::Drain, GateKind::Hold] {
            let gate = registry.close("text/m", kind);
            let started = std::time::Instant::now();
            let racer = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                plan.admit_racer(&snap, &target),
            )
            .await
            .expect("the racer waited at the gate");
            assert!(racer.is_err(), "{kind:?}: started at a closed gate");
            assert!(started.elapsed() < std::time::Duration::from_millis(500));
            assert_eq!(registry.in_flight("text/m"), 0, "{kind:?}: still counted");
            drop(gate);
        }
        let racer = plan.admit_racer(&snap, &target).await.unwrap().unwrap();
        assert_eq!(registry.dispatched("text/m"), 1);
        drop(racer);
        assert_eq!(registry.in_flight("text/m"), 0);
    }

    #[test]
    fn a_request_waits_at_a_gate_at_least_as_long_as_an_apply_holds_it() {
        // Default command timeout 60 s: the 5 s apply wait becomes 90 s.
        assert_eq!(
            gate_wait(&engine_snapshot(""), "text"),
            Some(std::time::Duration::from_secs(90))
        );
        assert_eq!(
            gate_wait(&engine_snapshot("    command_timeout_secs: 2"), "text"),
            Some(std::time::Duration::from_secs(32))
        );
        assert_eq!(gate_wait(&engine_snapshot(""), "other"), None);
    }

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
    fn history_keeps_the_newest_revisions_and_revert_reaches_them() {
        let registry = ModelSettingsRegistry::new();
        registry.set_history_keep(3);
        for revision in 0..5u64 {
            registry
                .write(
                    "text/m",
                    doc(json!({"defaults": {"top_k": revision + 1}})),
                    revision,
                    &actor(),
                    "put",
                )
                .unwrap();
        }
        let history = registry.history("text/m", 100).unwrap();
        assert_eq!(
            history.iter().map(|c| c.revision).collect::<Vec<_>>(),
            vec![5, 4, 3]
        );
        assert_eq!(
            registry.revision_doc("text/m", 3).unwrap().defaults["top_k"],
            json!(3)
        );
        assert_eq!(
            registry.revision_doc("text/m", 2).unwrap_err(),
            SettingsWriteError::UnknownRevision(2)
        );
    }

    #[test]
    fn revert_to_zero_writes_an_empty_document_as_a_new_revision() {
        let registry = ModelSettingsRegistry::new();
        registry
            .write(
                "text/m",
                doc(json!({"defaults": {"top_k": 20}, "locked": ["top_k"],
                           "load": {"ctx_size": 8192}})),
                0,
                &actor(),
                "put",
            )
            .unwrap();
        let reverted = registry.revert("text/m", 0, 1, &actor()).unwrap();
        assert_eq!(reverted.revision, 2);
        assert_eq!(*reverted.doc, ModelSettingsDoc::default());
        let history = registry.history("text/m", 10).unwrap();
        assert_eq!(
            history.iter().map(|c| c.revision).collect::<Vec<_>>(),
            vec![2, 1]
        );
        assert_eq!(history[0].action, "revert:0");
        // A target that never had settings reverts to 0 the same way.
        let fresh = registry.revert("text/n", 0, 0, &actor()).unwrap();
        assert_eq!(fresh.revision, 1);
        assert_eq!(*fresh.doc, ModelSettingsDoc::default());
    }

    #[tokio::test]
    async fn a_closed_gate_holds_new_requests_and_a_drain_waits_for_queued_ones() {
        let registry = Arc::new(ModelSettingsRegistry::new());
        let wait = std::time::Duration::from_millis(500);
        // Admitted (queued: not dispatched): a drain waits for it.
        let queued = registry.admit("text/m", wait).await.unwrap();
        let gate = registry.close("text/m", GateKind::Drain);
        assert_eq!(gate.busy(), 1);
        // A new request waits at the gate, and gives up at its bound.
        assert!(registry.try_admit("text/m").is_none());
        let refused = registry
            .admit("text/m", std::time::Duration::from_millis(50))
            .await
            .unwrap_err();
        assert_eq!(refused.key, "text/m");
        // The queued one still goes on (a drain does not stop dispatch).
        let mut queued = queued;
        queued
            .dispatch(std::time::Duration::from_millis(50))
            .await
            .unwrap();
        let waiter = {
            let registry = registry.clone();
            tokio::spawn(async move { registry.admit("text/m", wait).await.is_ok() })
        };
        let drained = {
            let gate_wait = gate.wait_idle(wait);
            tokio::pin!(gate_wait);
            tokio::select! {
                _ = &mut gate_wait => panic!("drained with a request still in flight"),
                _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
            }
            drop(queued);
            gate_wait.await
        };
        assert_eq!(drained, Ok(()));
        assert_eq!(registry.in_flight("text/m"), 0);
        // Reopened: the waiting request is admitted.
        drop(gate);
        assert!(waiter.await.unwrap());
    }

    #[tokio::test]
    async fn a_hold_waits_only_for_dispatched_requests_and_holds_dispatch() {
        let registry = Arc::new(ModelSettingsRegistry::new());
        let wait = std::time::Duration::from_millis(300);
        let mut queued = registry.admit("text/m", wait).await.unwrap();
        let gate = registry.close("text/m", GateKind::Hold);
        // Admitted but not at the engine: the hold does not wait for it...
        assert_eq!(gate.busy(), 0);
        // ...and it may not reach the engine while the hold lasts.
        assert!(queued
            .dispatch(std::time::Duration::from_millis(50))
            .await
            .is_err());
        drop(gate);
        queued.dispatch(wait).await.unwrap();
        assert_eq!(registry.dispatched("text/m"), 1);
        // The idle hook fires when the last dispatched request ends.
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        registry.set_idle_hook(Arc::new(move |key: &str| {
            sink.lock().unwrap().push(key.to_string());
        }));
        drop(queued);
        assert_eq!(*seen.lock().unwrap(), vec!["text/m".to_string()]);
        assert_eq!(registry.dispatched("text/m"), 0);
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
