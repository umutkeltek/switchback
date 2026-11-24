use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use sb_core::Config;
use sb_runtime::{Engine, Runtime, Snapshot};

use crate::activator::CapacityActivator;
use crate::{admission, cp, idempotency, tenancy, workloads};

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
    /// Optional eval evidence snapshot for preview/report surfaces. This is not
    /// part of the runtime hot path and never affects route selection.
    pub eval_evidence: Arc<RwLock<Option<Arc<sb_eval::EvalEvidenceSnapshot>>>>,
    pub workloads: workloads::WorkloadStore,
    /// On-demand local executor capacity (wake/drain/idle-poweroff/self-heal).
    /// Built once from config; live lane state survives across requests.
    pub capacity: Arc<CapacityActivator>,
    /// Engine controls of local text engines (load, unload, load-time
    /// settings). Shared state per lane: last apply, variants, listings.
    pub(crate) engines: Arc<crate::engine_control::EngineControls>,
}

impl AppState {
    /// Wrap a fully-built engine (call `Engine::with_traces`/`set_config_path`
    /// before this, while it's still unshared).
    pub fn from_engine(engine: Engine) -> Self {
        let capacity = CapacityActivator::from_config(&engine.snapshot().config);
        Self::from_engine_with_capacity(engine, capacity)
    }

    /// [`Self::from_engine`] with an activator built by the caller (tests
    /// inject one with fake seams).
    pub fn from_engine_with_capacity(engine: Engine, capacity: Arc<CapacityActivator>) -> Self {
        let snapshot = engine.snapshot();
        let server = &snapshot.config.server;
        let admission =
            admission::Admission::new(server.max_concurrency, server.admission_timeout_ms);
        // Always attached, even with no lanes yet, so a reload that adds one
        // takes effect. With no opted-in lane it changes nothing: routing
        // reads a lane only when the request's pinned config lists it with
        // `route_health` (skip when down) or `wake_on_request` (busy guard,
        // wake before dispatch).
        let engine = Arc::new(engine.with_lane_status(capacity.clone()));
        // Lane probes lease their provider's credential from the live
        // engine (a ComfyUI behind a token proxy).
        capacity.attach_credentials(Arc::new(crate::activator::EngineCredentials(
            Arc::downgrade(&engine),
        )));
        // Per-request load settings make engine variants through the
        // engine control, which the runtime reaches through this seam.
        let engines = Arc::new(crate::engine_control::EngineControls::default());
        engine.set_model_variants(Arc::new(crate::engine_control::EngineVariants {
            engine: Arc::downgrade(&engine),
            capacity: capacity.clone(),
            engines: engines.clone(),
        }));
        // Pending applies are retried when a lane comes up and when the
        // last request on a held-back model ends.
        crate::engine_control::install_hooks(&engine, &capacity, &engines);
        // Loaded-model preference and swap counting read the engine
        // control's last listing (never a call to a machine).
        engine
            .optimize()
            .set_engine_view(Arc::new(crate::engine_control::ListingView(
                engines.clone(),
            )));
        // Token-pool admission reads each engine model's real pool from the
        // same listing (else its settings document) and counts large
        // prompts with the engine's own tokenizer.
        engine
            .token_pools()
            .set_view(Arc::new(crate::pool_view::EnginePoolView::new(
                Arc::downgrade(&engine),
                capacity.clone(),
                engines.clone(),
            )));
        AppState {
            ledger: engine.ledger(),
            traces: engine.traces(),
            inflight: idempotency::InFlight::default(),
            concurrency: tenancy::Concurrency::default(),
            admission,
            drafts: cp::DraftStore::new(engine.store(), engine.store_required()),
            eval_evidence: Arc::new(RwLock::new(None)),
            workloads: workloads::WorkloadStore::with_artifacts(
                crate::artifact_store::ArtifactStore::from_config(&server.artifacts, None),
            ),
            capacity,
            engine,
            engines,
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

    /// Start the engine listing poller: every `every`, the loaded models of
    /// each engine lane that is up (never one that is off, waking, powering
    /// off or draining; never a wake). `serve` starts it every 30 s.
    /// Returns whether it started: with no lane that has an engine control
    /// at start-up there is nothing to poll and no task is spawned (a
    /// reload that adds the first such lane is polled after a restart; the
    /// served requests keep its loaded-model view meanwhile).
    pub fn spawn_engine_listing_poller(&self, every: std::time::Duration) -> bool {
        crate::engine_control::spawn_listing_poller(self, every)
    }

    /// Prune the experiment ledger now and start its daily pruner when an
    /// optimizer feature is configured or rows are left. Returns whether
    /// the daily task started; `serve` calls it at start-up.
    pub fn spawn_experiment_pruner(&self) -> bool {
        crate::optimize::spawn_experiment_pruner(self)
    }

    /// One pass of the engine listing poller (tests and tools).
    pub async fn poll_engine_listings(&self) {
        let ctx = crate::engine_control::EngineCtx::from(self);
        self.engines.poll_listings(&ctx).await;
    }

    /// Keep artifacts under `state_root` when `server.artifacts` names no
    /// directory (the gateway's start-up; a library caller without one keeps
    /// them in memory). Call before the state is shared.
    ///
    /// Host holds are kept in `<state_root>/holds.json` too (they survive a
    /// restart), unless `server.artifacts.storage` is `memory` (a deployment
    /// with no writable state directory).
    pub fn with_artifact_state_root(mut self, state_root: &std::path::Path) -> Self {
        let snapshot = self.snapshot();
        let artifacts = &snapshot.config.server.artifacts;
        if artifacts.storage != sb_core::ArtifactStorage::Memory {
            self.capacity.keep_holds_in(state_root);
        }
        if artifacts.dir.is_some() || artifacts.storage == sb_core::ArtifactStorage::Memory {
            return self; // already opened from config alone
        }
        self.workloads = workloads::WorkloadStore::with_artifacts(
            crate::artifact_store::ArtifactStore::from_config(
                &snapshot.config.server.artifacts,
                Some(state_root),
            ),
        );
        self
    }

    /// Remember the config file so `POST /v1/reload` can re-read it.
    pub fn with_config_path(self, path: PathBuf) -> Self {
        self.engine.set_config_path(path);
        self
    }

    pub fn with_eval_evidence(mut self, snapshot: Arc<sb_eval::EvalEvidenceSnapshot>) -> Self {
        self.eval_evidence = Arc::new(RwLock::new(Some(snapshot)));
        self
    }

    pub fn set_eval_evidence(
        &self,
        snapshot: Option<Arc<sb_eval::EvalEvidenceSnapshot>>,
    ) -> Result<(), String> {
        let mut guard = self
            .eval_evidence
            .write()
            .map_err(|_| "eval evidence lock poisoned".to_string())?;
        *guard = snapshot;
        Ok(())
    }

    pub fn eval_evidence_snapshot(&self) -> Option<Arc<sb_eval::EvalEvidenceSnapshot>> {
        self.eval_evidence
            .read()
            .ok()
            .and_then(|guard| guard.as_ref().cloned())
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
