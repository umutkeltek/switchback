use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use arc_swap::ArcSwap;
use sb_core::{AiRequest, Config};

use super::execution_meta::{attach_execution_receipt, preview_cache_receipt};
use super::profiles::{apply_request_client_profile, plan_resolved_route, resolve_candidates};
use super::{AuditContext, Engine, ExecError, Runtime, Snapshot};

/// A stable fingerprint of a config (so drift between revisions is detectable)
/// without persisting the body — keeps secrets out of the state store.
pub(crate) fn config_hash(config: &Config) -> String {
    use sha2::{Digest, Sha256};
    let json = serde_json::to_vec(config).unwrap_or_default();
    format!("{:x}", Sha256::digest(&json))
}

impl Engine {
    /// Compile config into snapshot revision 1. The trace log defaults to an
    /// in-memory ring; override it with [`Engine::with_traces`] before sharing.
    /// This constructor panics if plugin activation fails; production paths
    /// should use [`Engine::try_new`] so fail-closed plugin semantics are
    /// surfaced as a startup/config error instead of a no-op plugin host.
    pub fn new(
        config: Arc<Config>,
        registry: Arc<sb_adapters::AdapterRegistry>,
        resolver: Arc<sb_credentials::CredentialResolver>,
        ledger: Arc<sb_ledger::UsageLedger>,
    ) -> Self {
        Self::try_new(config, registry, resolver, ledger)
            .expect("engine config should have been validated")
    }

    /// Checked constructor for embedders and production startup. It uses the
    /// plugin host's checked build path so fail-closed plugin activation errors
    /// cannot be silently converted into a no-op host.
    pub fn try_new(
        config: Arc<Config>,
        registry: Arc<sb_adapters::AdapterRegistry>,
        resolver: Arc<sb_credentials::CredentialResolver>,
        ledger: Arc<sb_ledger::UsageLedger>,
    ) -> Result<Self, String> {
        let runtime = Runtime::from_config(&config);
        let plugins = sb_plugin::PluginHost::try_from_config(&config.plugins)?;
        let snapshot = Snapshot {
            revision: 1,
            config,
            registry,
            resolver,
            runtime,
            plugins,
        };
        Ok(Engine {
            snapshot: ArcSwap::from_pointee(snapshot),
            ledger,
            traces: Arc::new(sb_trace::TraceLog::default()),
            config_path: OnceLock::new(),
            store: None,
            store_required: false,
            combo_rr: Mutex::new(HashMap::new()),
            exact_cache: Mutex::new(sb_core::ExactRequestCache::new()),
            reload_lock: Mutex::new(()),
        })
    }

    /// Replace the default trace log (e.g. with a sampling-configured one).
    /// Consuming builder — call before the engine is shared behind an `Arc`.
    pub fn with_traces(mut self, traces: Arc<sb_trace::TraceLog>) -> Self {
        self.traces = traces;
        self
    }

    /// Attach an optional durable state store and record the current
    /// (bootstrap) revision as the first entry. Consuming builder — call before
    /// sharing.
    pub fn with_store(mut self, store: Arc<dyn sb_store::StateStore>) -> Self {
        self = self
            .with_store_policy(store, false)
            .expect("optional state store attach is best-effort");
        self
    }

    /// Attach a durable state store with explicit failure policy. When
    /// `required` is true, bootstrap persistence and later control-plane
    /// mutations fail closed if revision/audit writes fail.
    pub fn with_store_policy(
        mut self,
        store: Arc<dyn sb_store::StateStore>,
        required: bool,
    ) -> Result<Self, String> {
        self.store = Some(store);
        self.store_required = required;
        let cur = self.snapshot.load();
        let hash = config_hash(&cur.config);
        let revision = cur.revision;
        drop(cur);
        let audit = AuditContext::new("bootstrap", "engine start");
        if required {
            self.persist_checked(revision, hash, &audit)?;
        } else {
            self.persist_best_effort(revision, hash, &audit);
        }
        Ok(self)
    }

    /// The durable state store handle, if persistence is enabled.
    pub fn store(&self) -> Option<Arc<dyn sb_store::StateStore>> {
        self.store.clone()
    }

    /// Whether state-store writes are required to complete control-plane
    /// mutations.
    pub fn store_required(&self) -> bool {
        self.store_required
    }

    fn persist_checked(
        &self,
        revision: u64,
        config_hash: String,
        audit: &AuditContext,
    ) -> Result<(), String> {
        let Some(store) = &self.store else {
            return Ok(());
        };
        let now = sb_store::now_millis();
        store
            .record_revision_and_audit(
                &sb_store::RevisionRecord {
                    revision,
                    config_hash,
                    source: audit.source.clone(),
                    created_at_ms: now,
                },
                &sb_store::AuditEntry {
                    revision,
                    action: audit.source.clone(),
                    detail: audit.detail.clone(),
                    actor_role: audit.actor_role.clone(),
                    actor_tenant: audit.actor_tenant.clone(),
                    actor_project: audit.actor_project.clone(),
                    source: audit.source.clone(),
                    object_id: audit.object_id.clone(),
                    created_at_ms: now,
                },
            )
            .map_err(|e| format!("state store persistence failed for revision {revision}: {e}"))
    }

    /// Best-effort durable record of a published revision + an audit row. A
    /// store error is logged, never propagated.
    fn persist_best_effort(&self, revision: u64, config_hash: String, audit: &AuditContext) {
        if let Err(e) = self.persist_checked(revision, config_hash, audit) {
            tracing::warn!(error = %e, revision, "state store: revision/audit write failed");
        }
    }

    fn persist_for_publish(
        &self,
        revision: u64,
        config_hash: String,
        audit: &AuditContext,
    ) -> Result<(), String> {
        if self.store_required {
            self.persist_checked(revision, config_hash, audit)
        } else {
            self.persist_best_effort(revision, config_hash, audit);
            Ok(())
        }
    }

    /// Remember the config file so [`Engine::reload_from_file`] can re-read it.
    /// Takes `&self` (the field is a `OnceLock`) so it works post-sharing too.
    pub fn set_config_path(&self, path: PathBuf) {
        let _ = self.config_path.set(path);
    }

    /// The usage ledger handle (shared; cheap clone).
    pub fn ledger(&self) -> Arc<sb_ledger::UsageLedger> {
        self.ledger.clone()
    }

    /// The trace log handle (shared; cheap clone).
    pub fn traces(&self) -> Arc<sb_trace::TraceLog> {
        self.traces.clone()
    }

    /// Pin the current snapshot for a request's lifetime (cheap Arc clone).
    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.snapshot.load_full()
    }

    pub fn revision(&self) -> u64 {
        self.snapshot.load().revision
    }

    /// Recompile a new config into a fresh snapshot (registry + resolver +
    /// runtime), bump the revision, and swap atomically. Returns the new
    /// revision. Health/breaker/refresh state resets (a deliberate operator
    /// action); ledger + traces persist.
    pub fn reload(&self, config: Config) -> Result<u64, String> {
        self.reload_with_audit(config, AuditContext::new("reload", "config file reload"))
    }

    pub fn reload_with_audit(&self, config: Config, audit: AuditContext) -> Result<u64, String> {
        let _guard = self
            .reload_lock
            .lock()
            .map_err(|_| "reload lock poisoned".to_string())?;
        self.swap_config_locked(config, audit)
    }

    /// Publish a config under optional optimistic concurrency. The reload lock is
    /// held across BOTH the `If-Match` revision check and the swap, so the
    /// precondition is enforced atomically — two concurrent publishers with the
    /// same expected revision can't both win (one swaps, the other sees the new
    /// revision and gets `Conflict`).
    pub fn publish_with_audit(
        &self,
        config: Config,
        audit: AuditContext,
        expected_revision: Option<u64>,
    ) -> Result<u64, crate::PublishError> {
        let _guard = self
            .reload_lock
            .lock()
            .map_err(|_| crate::PublishError::Failed("reload lock poisoned".to_string()))?;
        if let Some(expected) = expected_revision {
            let current = self.snapshot.load().revision;
            if current != expected {
                return Err(crate::PublishError::Conflict { expected, current });
            }
        }
        self.swap_config_locked(config, audit)
            .map_err(crate::PublishError::Failed)
    }

    /// The actual config compile + atomic swap. **The caller must hold
    /// `reload_lock`** so the revision read→build→store sequence can't interleave
    /// with another publish.
    fn swap_config_locked(&self, config: Config, audit: AuditContext) -> Result<u64, String> {
        Self::validate_config(&config)?;
        let registry = sb_adapters::AdapterRegistry::from_config(&config)?;
        let resolver = sb_credentials::CredentialResolver::from_config(&config)?;
        let revision = self.snapshot.load().revision + 1;
        let hash = config_hash(&config);
        let plugins = sb_plugin::PluginHost::try_from_config(&config.plugins)?;
        let next = Arc::new(Snapshot {
            revision,
            runtime: Runtime::from_config(&config),
            config: Arc::new(config),
            registry: Arc::new(registry),
            resolver: Arc::new(resolver),
            plugins,
        });
        if self.store_required {
            self.persist_for_publish(revision, hash, &audit)?;
            self.snapshot.store(next);
        } else {
            self.snapshot.store(next);
            self.persist_for_publish(revision, hash, &audit)?;
        }
        Ok(revision)
    }

    /// Re-read the config file and reload (for `POST /v1/reload`).
    pub fn reload_from_file(&self) -> Result<u64, String> {
        let path = self
            .config_path
            .get()
            .ok_or("no config file path to reload from")?;
        self.reload_from_file_with_audit(
            AuditContext::new("file_reload", "config file reload")
                .with_object_id(path.display().to_string()),
        )
    }

    pub fn reload_from_file_with_audit(&self, audit: AuditContext) -> Result<u64, String> {
        let path = self
            .config_path
            .get()
            .ok_or("no config file path to reload from")?;
        let config = Config::from_path(path).map_err(|e| e.to_string())?;
        self.reload_with_audit(config, audit.with_object_id(path.display().to_string()))
    }

    /// Apply a runtime-knob change: reuse the current registry/resolver (so
    /// health/credential state is preserved), swap in the new knobs, bump the
    /// revision. Returns the new revision.
    pub fn update_runtime(&self, edit: impl FnOnce(&mut Runtime)) -> Result<u64, String> {
        self.update_runtime_with_audit(edit, None)
    }

    pub fn update_runtime_with_audit(
        &self,
        edit: impl FnOnce(&mut Runtime),
        audit: Option<AuditContext>,
    ) -> Result<u64, String> {
        // Serialize with config publishes/reloads so the revision bump is atomic.
        let _guard = self
            .reload_lock
            .lock()
            .map_err(|_| "reload lock poisoned".to_string())?;
        let cur = self.snapshot.load();
        let mut runtime = cur.runtime.clone();
        edit(&mut runtime);
        let revision = cur.revision + 1;
        // Same config (knobs only), so the hash is unchanged — the revision row
        // records that knobs changed; the audit detail is the new knob state.
        let hash = config_hash(&cur.config);
        let detail = serde_json::to_string(&runtime).unwrap_or_default();
        let mut audit = audit.unwrap_or_else(|| AuditContext::new("runtime_patch", detail.clone()));
        if audit.detail.is_empty() {
            audit.detail = detail;
        }
        let next = Arc::new(Snapshot {
            revision,
            runtime,
            config: cur.config.clone(),
            registry: cur.registry.clone(),
            resolver: cur.resolver.clone(),
            plugins: cur.plugins.clone(),
        });
        drop(cur);
        if self.store_required {
            self.persist_for_publish(revision, hash, &audit)?;
            self.snapshot.store(next);
        } else {
            self.snapshot.store(next);
            self.persist_for_publish(revision, hash, &audit)?;
        }
        Ok(revision)
    }

    /// Compute the `RouteDecision` for a request WITHOUT executing it — the same
    /// routing the hot path uses (candidate resolution + pool-health stamp +
    /// `plan_route`), surfaced for `/cp/v1/route-preview`. Returns the plan (the
    /// decision + surviving candidates) and the pinned revision.
    pub fn preview_route(&self, req: &AiRequest) -> Result<(u64, sb_router::RoutePlan), ExecError> {
        let snap = self.snapshot();
        let mut req = req.clone();
        if let sb_plugin::PluginOutcome::Reject { status, message } =
            snap.plugins.pre_route(&mut req)
        {
            return Err(ExecError::new(status, "plugin_rejected", message, None));
        }
        let client_profile = apply_request_client_profile(&snap, &mut req)?;
        let resolved = resolve_candidates(&snap, &req.model)?;
        let (_route_name, mut plan) = plan_resolved_route(
            &self.combo_rr,
            &snap,
            &req,
            client_profile.as_ref(),
            resolved,
            false,
        )?;
        attach_execution_receipt(&mut plan, &req, preview_cache_receipt(&req));
        Ok((snap.revision, plan))
    }

    /// Validate a candidate config WITHOUT publishing it: check cross-references,
    /// catalog integrity, then build the adapter registry + credential resolver
    /// (the same compile the snapshot does) and discard them.
    pub fn validate_config(config: &Config) -> Result<(), String> {
        let mut problems = config.semantic_problems();
        if let Some(catalog) = &config.catalog {
            problems.extend(
                catalog
                    .validate()
                    .into_iter()
                    .map(|p| format!("catalog: {p}")),
            );
        }
        if let Err(e) = sb_adapters::AdapterRegistry::from_config(config) {
            problems.push(format!("adapters: {e}"));
        }
        if let Err(e) = sb_credentials::CredentialResolver::from_config(config) {
            problems.push(format!("credentials: {e}"));
        }
        if let Err(e) = sb_plugin::PluginHost::try_from_config(&config.plugins) {
            problems.push(format!("plugins: {e}"));
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems.join("; "))
        }
    }
}
