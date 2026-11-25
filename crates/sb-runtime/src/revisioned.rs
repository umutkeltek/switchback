//! Documents kept by name with revisions and a change history: durable in
//! the state store when one is attached, in memory otherwise. One registry
//! serves every family (per-model settings, route roles): a write names the
//! revision it edited (compare-and-set, also across gateways sharing the
//! store), every write is a new revision with who, what and when, the
//! history keeps the newest revisions, and a revert writes an old revision's
//! document again as a new one (revision 0 = the empty document).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use serde::de::DeserializeOwned;
use serde::Serialize;

use sb_store::{ModelSettingsChange, ModelSettingsRecord, ModelSettingsWrite, StateStore};

use crate::model_settings::{SettingsActor, SettingsWriteError, DEFAULT_HISTORY_KEEP};

/// Which store tables a family of documents lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocFamily {
    /// Per-model settings (`model_settings`, key = `provider/model`).
    ModelSettings,
    /// Route roles (`route_roles`, key = the route name).
    RouteRoles,
}

impl DocFamily {
    fn label(self) -> &'static str {
        match self {
            DocFamily::ModelSettings => "model settings",
            DocFamily::RouteRoles => "route roles",
        }
    }

    fn write(
        self,
        store: &dyn StateStore,
        change: &ModelSettingsChange,
    ) -> sb_store::Result<ModelSettingsWrite> {
        match self {
            DocFamily::ModelSettings => store.model_settings_write(change),
            DocFamily::RouteRoles => store.route_role_write(change),
        }
    }

    fn get(
        self,
        store: &dyn StateStore,
        key: &str,
    ) -> sb_store::Result<Option<ModelSettingsRecord>> {
        match self {
            DocFamily::ModelSettings => store.model_settings_get(key),
            DocFamily::RouteRoles => store.route_role_get(key),
        }
    }

    fn list(self, store: &dyn StateStore) -> sb_store::Result<Vec<ModelSettingsRecord>> {
        match self {
            DocFamily::ModelSettings => store.model_settings_list(),
            DocFamily::RouteRoles => store.route_role_list(),
        }
    }

    fn history(
        self,
        store: &dyn StateStore,
        key: &str,
        limit: usize,
    ) -> sb_store::Result<Vec<ModelSettingsChange>> {
        match self {
            DocFamily::ModelSettings => store.model_settings_history(key, limit),
            DocFamily::RouteRoles => store.route_role_history(key, limit),
        }
    }

    fn prune(self, store: &dyn StateStore, key: &str, keep: usize) -> sb_store::Result<usize> {
        match self {
            DocFamily::ModelSettings => store.model_settings_prune(key, keep),
            DocFamily::RouteRoles => store.route_role_prune(key, keep),
        }
    }
}

/// A document type the registry keeps.
pub trait RevisionedDoc:
    Clone + Default + PartialEq + Serialize + DeserializeOwned + Send + Sync + 'static
{
    const FAMILY: DocFamily;

    /// The document's own checks; `Err` is the refusal.
    fn check(&self) -> Result<(), String>;

    /// The key paths that differ between two documents, for the history.
    fn changed(before: &Self, after: &Self) -> Vec<String>;

    fn equivalent(&self, other: &Self) -> bool {
        self == other
    }
}

/// One key's current document.
#[derive(Debug, Clone)]
pub struct DocEntry<D> {
    pub revision: u64,
    pub doc: Arc<D>,
    pub updated_at_ms: i64,
}

#[derive(Debug)]
pub struct DocWrite<D> {
    pub entry: DocEntry<D>,
    pub unchanged: bool,
}

impl<D> std::ops::Deref for DocWrite<D> {
    type Target = DocEntry<D>;

    fn deref(&self) -> &Self::Target {
        &self.entry
    }
}

/// The documents of one family, by key, revisioned.
pub struct DocRegistry<D> {
    entries: RwLock<BTreeMap<String, DocEntry<D>>>,
    store: RwLock<Option<Arc<dyn StateStore>>>,
    /// History kept here when there is no state store (process lifetime).
    memory_history: Mutex<BTreeMap<String, Vec<ModelSettingsChange>>>,
    /// Serializes writes so read-revision → check → write is atomic in
    /// this process (the store's transaction covers other processes).
    write_lock: Mutex<()>,
    /// How many revisions of each key the history keeps (newest first).
    history_keep: AtomicUsize,
}

impl<D> Default for DocRegistry<D> {
    fn default() -> Self {
        DocRegistry {
            entries: RwLock::default(),
            store: RwLock::default(),
            memory_history: Mutex::default(),
            write_lock: Mutex::default(),
            history_keep: AtomicUsize::new(DEFAULT_HISTORY_KEEP),
        }
    }
}

impl<D: RevisionedDoc> DocRegistry<D> {
    /// How many revisions per key the history keeps (at least 1).
    pub fn set_history_keep(&self, keep: usize) {
        self.history_keep.store(keep.max(1), Ordering::Relaxed);
    }

    pub fn history_keep(&self) -> usize {
        self.history_keep.load(Ordering::Relaxed)
    }

    /// Attach the state store and load every stored document. A stored
    /// document that no longer parses is skipped with a warning (it stays
    /// in the store and its history).
    pub fn attach_store(&self, store: Arc<dyn StateStore>) {
        let family = D::FAMILY;
        match family.list(store.as_ref()) {
            Ok(records) => {
                let mut entries = self.entries.write().unwrap_or_else(|e| e.into_inner());
                for record in records {
                    match serde_json::from_str::<D>(&record.doc) {
                        Ok(doc) => {
                            entries.insert(
                                record.target,
                                DocEntry {
                                    revision: record.revision,
                                    doc: Arc::new(doc),
                                    updated_at_ms: record.updated_at_ms,
                                },
                            );
                        }
                        Err(e) => tracing::warn!(
                            key = %record.target,
                            family = family.label(),
                            error = %e,
                            "stored document does not parse; skipped"
                        ),
                    }
                }
            }
            Err(e) => {
                tracing::warn!(family = family.label(), error = %e, "state store read failed")
            }
        }
        *self.store.write().unwrap_or_else(|e| e.into_inner()) = Some(store);
    }

    fn store(&self) -> Option<Arc<dyn StateStore>> {
        self.store.read().ok().and_then(|store| store.clone())
    }

    /// Whether writes are durable.
    pub fn durable(&self) -> bool {
        self.store().is_some()
    }

    pub fn get(&self, key: &str) -> Option<DocEntry<D>> {
        self.entries
            .read()
            .ok()
            .and_then(|entries| entries.get(key).cloned())
    }

    /// The key's revision (0 = never written).
    pub fn revision(&self, key: &str) -> u64 {
        self.get(key).map(|entry| entry.revision).unwrap_or(0)
    }

    pub fn all(&self) -> Vec<(String, DocEntry<D>)> {
        self.entries
            .read()
            .map(|entries| {
                entries
                    .iter()
                    .map(|(key, entry)| (key.clone(), entry.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn snapshot_entries(&self) -> Result<Vec<(String, DocEntry<D>)>, SettingsWriteError> {
        let Some(store) = self.store() else {
            return Ok(self.all());
        };
        D::FAMILY
            .list(store.as_ref())
            .map_err(|e| SettingsWriteError::Store(e.0))?
            .into_iter()
            .map(|record| {
                let doc = serde_json::from_str(&record.doc).map_err(|e| {
                    SettingsWriteError::Store(format!("reading {}: {e}", record.target))
                })?;
                Ok((
                    record.target,
                    DocEntry {
                        revision: record.revision,
                        doc: Arc::new(doc),
                        updated_at_ms: record.updated_at_ms,
                    },
                ))
            })
            .collect()
    }

    pub fn current(&self, key: &str) -> Result<Option<DocEntry<D>>, SettingsWriteError> {
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        self.read_current(key, self.store().as_deref())
    }

    fn read_current(
        &self,
        key: &str,
        store: Option<&dyn StateStore>,
    ) -> Result<Option<DocEntry<D>>, SettingsWriteError> {
        let Some(store) = store else {
            return Ok(self.get(key));
        };
        let Some(record) = D::FAMILY
            .get(store, key)
            .map_err(|e| SettingsWriteError::Store(e.0))?
        else {
            return Ok(None);
        };
        if let Some(entry) = self
            .get(key)
            .filter(|entry| entry.revision == record.revision)
        {
            return Ok(Some(entry));
        }
        let doc = serde_json::from_str(&record.doc)
            .map_err(|e| SettingsWriteError::Store(format!("reading {key}: {e}")))?;
        let entry = DocEntry {
            revision: record.revision,
            doc: Arc::new(doc),
            updated_at_ms: record.updated_at_ms,
        };
        self.entries
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.to_string(), entry.clone());
        Ok(Some(entry))
    }

    /// Replace a key's document if the writer edited the current revision.
    pub fn write(
        &self,
        key: &str,
        doc: D,
        expected: u64,
        actor: &SettingsActor,
        action: &str,
    ) -> Result<DocWrite<D>, SettingsWriteError> {
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let store = self.store();
        let previous = self.read_current(key, store.as_deref())?;
        if let Some(entry) = previous.as_ref().filter(|entry| entry.doc.equivalent(&doc)) {
            return Ok(DocWrite {
                entry: entry.clone(),
                unchanged: true,
            });
        }
        let current = previous.as_ref().map(|entry| entry.revision).unwrap_or(0);
        if expected != current {
            return Err(SettingsWriteError::Conflict { expected, current });
        }
        doc.check().map_err(SettingsWriteError::Invalid)?;
        let before = previous
            .as_ref()
            .map(|entry| (*entry.doc).clone())
            .unwrap_or_default();
        let doc_json =
            serde_json::to_string(&doc).map_err(|e| SettingsWriteError::Invalid(e.to_string()))?;
        let now = sb_store::now_millis();
        let change = ModelSettingsChange {
            target: key.to_string(),
            revision: current + 1,
            doc: doc_json,
            actor: actor.actor.clone(),
            role: actor.role.clone(),
            source: actor.source.clone(),
            action: action.to_string(),
            changed: D::changed(&before, &doc),
            at_ms: now,
        };
        match store {
            Some(store) => match D::FAMILY.write(store.as_ref(), &change) {
                Ok(ModelSettingsWrite::Written) => {
                    // The write stands; a failed prune only leaves older rows.
                    if let Err(e) = D::FAMILY.prune(store.as_ref(), key, self.history_keep()) {
                        tracing::warn!(key = %key, family = D::FAMILY.label(), error = %e.0, "history prune failed");
                    }
                }
                Ok(ModelSettingsWrite::Conflict { current: stored }) => {
                    let fresh = self.read_current(key, Some(store.as_ref()))?;
                    if let Some(entry) = fresh.as_ref().filter(|entry| entry.doc.equivalent(&doc)) {
                        return Ok(DocWrite {
                            entry: entry.clone(),
                            unchanged: true,
                        });
                    }
                    return Err(SettingsWriteError::Conflict {
                        expected,
                        current: fresh.map_or(stored, |entry| entry.revision),
                    });
                }
                Err(e) => return Err(SettingsWriteError::Store(e.0)),
            },
            None => {
                let mut history = self
                    .memory_history
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                let changes = history.entry(key.to_string()).or_default();
                changes.push(change);
                let keep = self.history_keep();
                if changes.len() > keep {
                    let extra = changes.len() - keep;
                    changes.drain(..extra);
                }
            }
        }
        let entry = DocEntry {
            revision: current + 1,
            doc: Arc::new(doc),
            updated_at_ms: now,
        };
        self.entries
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.to_string(), entry.clone());
        Ok(DocWrite {
            entry,
            unchanged: false,
        })
    }

    /// The key's changes, newest first.
    pub fn history(&self, key: &str, limit: usize) -> Result<Vec<ModelSettingsChange>, String> {
        match self.store() {
            Some(store) => D::FAMILY
                .history(store.as_ref(), key, limit)
                .map_err(|e| e.0),
            None => Ok(self
                .memory_history
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(key)
                .map(|changes| changes.iter().rev().take(limit).cloned().collect())
                .unwrap_or_default()),
        }
    }

    /// Revision `to`'s document, searched in every revision the history
    /// keeps. Revision 0 is the state before the first write: the empty
    /// document, so a revert to 0 is a new revision with it (the history
    /// stays).
    pub fn revision_doc(&self, key: &str, to: u64) -> Result<D, SettingsWriteError> {
        if to == 0 {
            return Ok(D::default());
        }
        let history = self
            .history(key, self.history_keep())
            .map_err(SettingsWriteError::Store)?;
        let change = history
            .into_iter()
            .find(|change| change.revision == to)
            .ok_or(SettingsWriteError::UnknownRevision(to))?;
        serde_json::from_str(&change.doc)
            .map_err(|e| SettingsWriteError::Invalid(format!("revision {to}: {e}")))
    }
}
