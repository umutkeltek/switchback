use std::path::PathBuf;
use std::sync::Arc;

use sb_core::Config;
use sb_runtime::{Engine, Runtime, Snapshot};

use crate::{admission, cp, idempotency, tenancy};

/// Axum application state: a thin handle over the execution [`Engine`] (which
/// owns the compiled snapshot + the attempt state machine) plus the two
/// persistent sinks the handlers read directly (usage ledger + trace log). The
/// `ledger`/`traces` fields are the SAME `Arc`s the engine holds — exposed here
/// so the `/v1/usage` and `/v1/traces` handlers can read them without going
/// through the runtime. Cloned per request by Axum; all clones share one engine.
#[derive(Clone)]
pub struct AppState {
    pub engine: Arc<Engine>,
    pub ledger: Arc<sb_ledger::UsageLedger>,
    pub traces: Arc<sb_trace::TraceLog>,
    /// Per-process in-flight idempotency keys (concurrent single-flight).
    pub inflight: idempotency::InFlight,
    /// Per-tenant in-flight request counters (concurrency admission).
    pub concurrency: tenancy::Concurrency,
    /// Global admission control (in-flight cap + bounded-wait backpressure). The
    /// configured limit is fixed at startup; with a state store, request slots
    /// are coordinated durably across gateway processes.
    pub admission: admission::Admission,
    /// Staged `/cp/v1` config drafts (in-memory, process-lifetime).
    pub drafts: cp::DraftStore,
    /// Optional eval evidence reader for preview/report surfaces. This is not
    /// part of the runtime hot path and never affects route selection.
    pub eval_store: Option<Arc<sb_store::SqliteStore>>,
}

impl AppState {
    /// Wrap a fully-built engine (call `Engine::with_traces`/`set_config_path`
    /// before this, while it's still unshared).
    pub fn from_engine(engine: Engine) -> Self {
        let server = &engine.snapshot().config.server;
        let admission =
            admission::Admission::new(server.max_concurrency, server.admission_timeout_ms);
        AppState {
            ledger: engine.ledger(),
            traces: engine.traces(),
            inflight: idempotency::InFlight::default(),
            concurrency: tenancy::Concurrency::default(),
            admission,
            drafts: cp::DraftStore::new(engine.store(), engine.store_required()),
            eval_store: None,
            engine: Arc::new(engine),
        }
    }

    /// Build state from the core dependencies. Stable signature so adding fields
    /// doesn't churn call sites (tests use this).
    pub fn new(
        config: Arc<Config>,
        registry: Arc<sb_adapters::AdapterRegistry>,
        resolver: Arc<sb_credentials::CredentialResolver>,
        ledger: Arc<sb_ledger::UsageLedger>,
    ) -> Self {
        Self::from_engine(Engine::new(config, registry, resolver, ledger))
    }

    /// Remember the config file so `POST /v1/reload` can re-read it.
    pub fn with_config_path(self, path: PathBuf) -> Self {
        self.engine.set_config_path(path);
        self
    }

    pub fn with_eval_store(mut self, store: Arc<sb_store::SqliteStore>) -> Self {
        self.eval_store = Some(store);
        self
    }

    /// Pin the current snapshot for a request's lifetime (cheap Arc clone).
    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.engine.snapshot()
    }

    pub fn revision(&self) -> u64 {
        self.engine.revision()
    }

    /// Re-read the config file and hot-swap a new snapshot (for `POST /v1/reload`).
    pub fn reload_from_file(&self) -> Result<u64, String> {
        self.engine.reload_from_file()
    }

    pub fn reload_from_file_with_audit(
        &self,
        audit: sb_runtime::AuditContext,
    ) -> Result<u64, String> {
        self.engine.reload_from_file_with_audit(audit)
    }

    /// Apply a runtime-knob change (reuses registry/resolver; bumps revision).
    pub fn update_runtime(&self, edit: impl FnOnce(&mut Runtime)) -> Result<u64, String> {
        self.engine.update_runtime(edit)
    }

    pub fn update_runtime_with_audit(
        &self,
        edit: impl FnOnce(&mut Runtime),
        audit: sb_runtime::AuditContext,
    ) -> Result<u64, String> {
        self.engine.update_runtime_with_audit(edit, Some(audit))
    }
}
