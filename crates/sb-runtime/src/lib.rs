//! The Switchback execution runtime.
//!
//! This crate owns the request execution state machine that used to live inside
//! the Axum handlers: route selection, account resolution, retries, two-level
//! (target × account) fallback, hedging, budget enforcement, trace emission, and
//! attempt lifecycle. `sb-server` is reduced to HTTP ingress/egress + protocol
//! translation; it hands a canonical `AiRequest` to [`Engine::execute`] and
//! renders the [`ExecOutcome`] back in the client's wire format.
//!
//! Configuration is compiled into an immutable, revisioned [`Snapshot`] held
//! behind an `ArcSwap`. Each request pins ONE snapshot for its whole lifetime,
//! so a config publish (hot-swap) never tears a request across revisions:
//! in-flight requests finish on the old revision, new ones start on the new.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use arc_swap::ArcSwap;
use futures::StreamExt;
use sb_adapter::{AdapterError, PreparedRequest};
use sb_core::{AiRequest, AiResponse, Config, ErrorClass, RouteDecision, Usage};
use sb_credentials::ResolveOutcome;
use tracing::Instrument as _;

mod collect;
mod outcome;
mod profiles;
mod stream;

pub use outcome::{EmbeddingsOutcome, ExecError, ExecOutcome};

use collect::{collect_response, precommit_stream};
use outcome::embeddings_usage;
use profiles::{plan_resolved_route, resolve_candidates};
use stream::{meter_stream, StreamFinish};

/// Live, runtime-toggleable operational knobs — the subset of `server` config a
/// control-plane client (dashboard / CLI) can flip without a restart. Read per
/// request; updated through `PATCH /v1/runtime`. Structural config (providers,
/// routes, accounts) is NOT live and stays file-driven.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Runtime {
    pub cost_aware: bool,
    pub latency_aware: bool,
    pub hedge_enabled: bool,
    pub retry_max: u32,
    pub budget_max_usd: Option<f64>,
}

impl Runtime {
    pub fn from_config(cfg: &Config) -> Self {
        Runtime {
            cost_aware: cfg.server.cost_aware,
            latency_aware: cfg.server.latency_aware,
            hedge_enabled: cfg.server.hedge.enabled,
            retry_max: cfg.server.retry.max_retries,
            budget_max_usd: cfg.server.budget.max_usd,
        }
    }
}

/// An immutable, revisioned compilation of config into everything a request
/// needs: the parsed config, the adapter registry, the credential resolver, and
/// the live knobs. Each request pins ONE snapshot for its whole lifetime, so a
/// config publish (hot-swap) never tears a request across revisions.
pub struct Snapshot {
    pub revision: u64,
    pub config: Arc<Config>,
    pub registry: Arc<sb_adapters::AdapterRegistry>,
    pub resolver: Arc<sb_credentials::CredentialResolver>,
    pub runtime: Runtime,
    /// Built-in plugins, compiled from `config.plugins` at publish time and run
    /// on the hot path (pre_route / post_route / select_egress / post_attempt).
    pub plugins: sb_plugin::PluginHost,
}

/// The execution runtime: holds the current compiled snapshot (swapped
/// atomically on publish/reload) plus the persistent sinks (usage ledger +
/// trace log) that accumulate across reloads — they are NOT config, so they
/// survive a hot-swap. `sb-server` wraps this in its Axum `AppState`.
pub struct Engine {
    /// The current compiled snapshot, swapped atomically on publish/reload.
    snapshot: ArcSwap<Snapshot>,
    /// Persistent across reloads (usage + traces accumulate; not config).
    ledger: Arc<sb_ledger::UsageLedger>,
    traces: Arc<sb_trace::TraceLog>,
    /// Config file path, for `reload_from_file` (unset when built from memory).
    config_path: OnceLock<PathBuf>,
    /// Durable control-plane state (config revisions + audit). `None` = in-memory
    /// only (persistence disabled). Optional stores are best-effort; required
    /// stores fail control-plane mutations before a runtime swap.
    store: Option<Arc<dyn sb_store::StateStore>>,
    store_required: bool,
    /// Per-combo target cursor for `strategy: round_robin`. Runtime state, not
    /// config, so it survives hot reload like latency and breaker state.
    combo_rr: Mutex<HashMap<String, usize>>,
}

#[derive(Clone, Debug)]
pub struct AuditContext {
    pub source: String,
    pub detail: String,
    pub object_id: Option<String>,
    pub actor_role: Option<String>,
    pub actor_tenant: Option<String>,
    pub actor_project: Option<String>,
}

impl AuditContext {
    pub fn new(source: impl Into<String>, detail: impl Into<String>) -> Self {
        let source = source.into();
        Self {
            detail: detail.into(),
            object_id: None,
            actor_role: None,
            actor_tenant: None,
            actor_project: None,
            source,
        }
    }

    pub fn with_object_id(mut self, object_id: impl Into<String>) -> Self {
        self.object_id = Some(object_id.into());
        self
    }

    pub fn with_actor(
        mut self,
        role: impl Into<String>,
        tenant: Option<String>,
        project: Option<String>,
    ) -> Self {
        self.actor_role = Some(role.into());
        self.actor_tenant = tenant;
        self.actor_project = project;
        self
    }
}

struct DenialTrace<'a> {
    request_id: &'a str,
    revision: u64,
    inbound_model: &'a str,
    status: u16,
    error_type: &'a str,
    message: &'a str,
    started: Instant,
    streamed: bool,
}

/// A stable fingerprint of a config (so drift between revisions is detectable)
/// without persisting the body — keeps secrets out of the state store.
fn config_hash(config: &Config) -> String {
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

    /// Append a usage/cost record for a completed (non-streamed) request,
    /// attributed to `tenant` (for per-tenant rollups + budget enforcement). Cost
    /// is priced from the registry's price index — the SAME one the router routes
    /// on — so a request's route decision and its ledger cost never diverge (#5).
    #[allow(clippy::too_many_arguments)]
    fn record_usage(
        &self,
        registry: &sb_adapters::AdapterRegistry,
        request_id: &str,
        provider_id: &str,
        model: &str,
        account_id: &str,
        tenant: Option<&str>,
        usage: Usage,
        started: Instant,
        streamed: bool,
    ) {
        let cost = registry.cost_micros(provider_id, model, &usage);
        self.ledger.record(
            sb_ledger::UsageRecord::priced(
                request_id,
                provider_id,
                model,
                Some(account_id.to_string()),
                usage,
                started.elapsed().as_millis() as u64,
                streamed,
                cost,
            )
            .with_tenant(tenant.map(str::to_string)),
        );
    }

    /// Attributed spend (USD) for one provider, from the usage ledger summary.
    fn provider_spend_usd(&self, provider_id: &str) -> f64 {
        self.ledger
            .summary()
            .by_provider
            .get(provider_id)
            .map(|(_count, micros)| *micros as f64 / 1_000_000.0)
            .unwrap_or(0.0)
    }

    fn record_denial_trace(&self, denial: DenialTrace<'_>) {
        let mut decision = RouteDecision::new(denial.request_id, "denied");
        decision.add_reason(format!("{}: {}", denial.error_type, denial.message));
        decision.reject(denial.inbound_model, denial.error_type);
        let trace = sb_trace::RequestTrace::start(
            denial.request_id,
            denial.revision,
            denial.inbound_model,
            "denied",
            decision,
        )
        .finish(
            denial.status,
            denial.started.elapsed().as_millis() as u64,
            denial.streamed,
        );
        self.traces.record(trace);
    }

    /// Compute the `RouteDecision` for a request WITHOUT executing it — the same
    /// routing the hot path uses (candidate resolution + pool-health stamp +
    /// `plan_route`), surfaced for `/cp/v1/route-preview`. Returns the plan (the
    /// decision + surviving candidates) and the pinned revision.
    pub fn preview_route(&self, req: &AiRequest) -> Result<(u64, sb_router::RoutePlan), ExecError> {
        let snap = self.snapshot();
        let resolved = resolve_candidates(&snap, &req.model)?;
        let (_route_name, plan) = plan_resolved_route(&self.combo_rr, &snap, req, resolved, false);
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

    /// Pin a snapshot and run the request to a committed outcome. Returns the
    /// pinned revision alongside the outcome so the HTTP edge can stamp
    /// `x-switchback-revision`. This is the runtime's public entry point.
    pub async fn execute(&self, req: AiRequest, started: Instant) -> (u64, ExecOutcome) {
        let snap = self.snapshot();
        let revision = snap.revision;
        let outcome = self.execute_inner(&snap, req, started).await;
        (revision, outcome)
    }

    /// Execute an OpenAI-compatible embeddings request through the same runtime
    /// controls as chat/messages: route decision, account fallback, budgets,
    /// egress selection, trace, ledger, and snapshot pinning. The HTTP edge
    /// remains responsible only for auth/admission and wire rendering.
    pub async fn execute_embeddings(
        &self,
        body: serde_json::Value,
        tenant: Option<String>,
        project: Option<String>,
        session_id: Option<String>,
        started: Instant,
    ) -> (u64, EmbeddingsOutcome) {
        let snap = self.snapshot();
        let revision = snap.revision;
        let outcome = self
            .execute_embeddings_inner(&snap, body, tenant, project, session_id, started)
            .await;
        (revision, outcome)
    }

    async fn execute_embeddings_inner(
        &self,
        snap: &Snapshot,
        body: serde_json::Value,
        tenant: Option<String>,
        project: Option<String>,
        session_id: Option<String>,
        started: Instant,
    ) -> EmbeddingsOutcome {
        let model = match body.get("model").and_then(|m| m.as_str()) {
            Some(model) if !model.is_empty() => model.to_string(),
            _ => {
                let request_id = sb_core::new_id("req");
                let message = "missing or invalid \"model\"";
                self.record_denial_trace(DenialTrace {
                    request_id: &request_id,
                    revision: snap.revision,
                    inbound_model: "<missing>",
                    status: 400,
                    error_type: "invalid_request_error",
                    message,
                    started,
                    streamed: false,
                });
                return EmbeddingsOutcome::Error {
                    request_id,
                    error: ExecError::new(400, "invalid_request_error", message, None),
                };
            }
        };

        let mut req = AiRequest::new(model, Vec::new());
        req.tenant = tenant;
        req.project = project;
        if let Some(session_id) = session_id {
            req.metadata.insert("session_id".to_string(), session_id);
        }

        if let Some(max) = snap.runtime.budget_max_usd {
            let spent = self.ledger.summary().total_cost_micros as f64 / 1_000_000.0;
            if spent >= max {
                let message = format!("budget exceeded: spent ${spent:.4} of ${max:.4} cap");
                self.record_denial_trace(DenialTrace {
                    request_id: &req.id,
                    revision: snap.revision,
                    inbound_model: &req.model,
                    status: 402,
                    error_type: "budget_exceeded",
                    message: &message,
                    started,
                    streamed: false,
                });
                return EmbeddingsOutcome::Error {
                    request_id: req.id,
                    error: ExecError::new(402, "budget_exceeded", message, None),
                };
            }
        }
        if let Some(tenant) = req.tenant.as_deref() {
            if let Some(budget) = snap.config.tenant(tenant).and_then(|t| t.budget_usd) {
                let spent = self.ledger.tenant_spend_usd(tenant);
                if spent >= budget {
                    let message = format!(
                        "tenant `{tenant}` budget exceeded: spent ${spent:.4} of ${budget:.4} cap"
                    );
                    self.record_denial_trace(DenialTrace {
                        request_id: &req.id,
                        revision: snap.revision,
                        inbound_model: &req.model,
                        status: 402,
                        error_type: "tenant_budget_exceeded",
                        message: &message,
                        started,
                        streamed: false,
                    });
                    return EmbeddingsOutcome::Error {
                        request_id: req.id,
                        error: ExecError::new(402, "tenant_budget_exceeded", message, None),
                    };
                }
            }
        }

        if let sb_plugin::PluginOutcome::Reject { status, message } =
            snap.plugins.pre_route(&mut req)
        {
            self.record_denial_trace(DenialTrace {
                request_id: &req.id,
                revision: snap.revision,
                inbound_model: &req.model,
                status,
                error_type: "plugin_rejected",
                message: &message,
                started,
                streamed: false,
            });
            return EmbeddingsOutcome::Error {
                request_id: req.id,
                error: ExecError::new(status, "plugin_rejected", message, None),
            };
        }

        let resolved = match resolve_candidates(snap, &req.model) {
            Ok(resolved) => resolved,
            Err(e) => {
                self.record_denial_trace(DenialTrace {
                    request_id: &req.id,
                    revision: snap.revision,
                    inbound_model: &req.model,
                    status: e.status,
                    error_type: &e.error_type,
                    message: &e.message,
                    started,
                    streamed: false,
                });
                return EmbeddingsOutcome::Error {
                    request_id: req.id,
                    error: e,
                };
            }
        };
        let unknown = resolved.unknown.clone();
        let (route_name, plan) = plan_resolved_route(&self.combo_rr, snap, &req, resolved, true);
        snap.plugins.post_route(&req, &plan.decision);
        let summary = format!("{} embeddings", plan.decision.summary());
        let mut trace = sb_trace::RequestTrace::start(
            req.id.clone(),
            snap.revision,
            req.model.clone(),
            route_name,
            plan.decision.clone(),
        );
        let mut last_err: Option<AdapterError> = None;

        'targets: for target in plan.candidates.iter() {
            let Some(adapter) = snap.registry.adapter(&target.provider_id) else {
                continue 'targets;
            };
            if !snap.resolver.circuit_allows(&target.provider_id) {
                continue 'targets;
            }
            if let Some(cap) = snap
                .config
                .server
                .budget
                .per_provider_usd
                .get(&target.provider_id)
            {
                if self.provider_spend_usd(&target.provider_id) >= *cap {
                    continue 'targets;
                }
            }

            let mut tried_accounts = HashSet::new();
            loop {
                match snap.resolver.resolve_with_session(
                    &target.provider_id,
                    &target.model,
                    &tried_accounts,
                    session_affinity_key(&req),
                ) {
                    ResolveOutcome::Selected { account_id, lease } => {
                        let attempt_started = Instant::now();
                        let egress_id =
                            snap.plugins.select_egress(&req, &target.id).or_else(|| {
                                resolve_egress(&snap.config, &target.provider_id, &account_id)
                            });
                        let egress_eff = snap.registry.effective_egress(egress_id.as_deref());
                        let lease = match snap
                            .resolver
                            .fresh_lease(&target.provider_id, &account_id, lease)
                            .await
                        {
                            Ok(lease) => lease,
                            Err(e) => {
                                let error = AdapterError::new(
                                    ErrorClass::Authentication,
                                    format!("oauth refresh failed: {e}"),
                                );
                                snap.resolver.report_failure(
                                    &target.provider_id,
                                    &account_id,
                                    &target.model,
                                    error.class,
                                );
                                trace.attempt(sb_trace::Attempt::failed(
                                    &target.id,
                                    &target.provider_id,
                                    &target.model,
                                    &account_id,
                                    egress_eff.as_str(),
                                    attempt_started.elapsed().as_millis() as u64,
                                    error.class.as_str(),
                                    true,
                                ));
                                tried_accounts.insert(account_id);
                                last_err = Some(error);
                                continue;
                            }
                        };

                        let mut call_body = body.clone();
                        if let Some(obj) = call_body.as_object_mut() {
                            obj.insert(
                                "model".to_string(),
                                serde_json::Value::String(target.model.clone()),
                            );
                        }

                        match adapter
                            .embeddings(call_body, target.clone(), Some(lease), egress_id.clone())
                            .await
                        {
                            Ok(value) => {
                                snap.resolver
                                    .report_success(&target.provider_id, &account_id);
                                snap.resolver.circuit_record(&target.provider_id, true);
                                let usage = embeddings_usage(&value);
                                self.record_usage(
                                    &snap.registry,
                                    &req.id,
                                    &target.provider_id,
                                    &target.model,
                                    &account_id,
                                    req.tenant.as_deref(),
                                    usage.clone(),
                                    started,
                                    false,
                                );
                                let attempt_ms = attempt_started.elapsed().as_millis() as u64;
                                trace.attempt(sb_trace::Attempt::success(
                                    &target.id,
                                    &target.provider_id,
                                    &target.model,
                                    &account_id,
                                    egress_eff.as_str(),
                                    attempt_ms,
                                ));
                                snap.plugins.post_attempt(&sb_plugin::AttemptInfo {
                                    request_id: &req.id,
                                    target_id: &target.id,
                                    provider_id: &target.provider_id,
                                    account_id: &account_id,
                                    egress: egress_eff.as_str(),
                                    ok: true,
                                    error_class: None,
                                    latency_ms: attempt_ms,
                                });
                                snap.registry.record_latency(
                                    &target.provider_id,
                                    &target.model,
                                    attempt_ms as f64,
                                );
                                let cost = snap.registry.cost_micros(
                                    &target.provider_id,
                                    &target.model,
                                    &usage,
                                );
                                trace.set_usage(usage, cost);
                                self.traces.record(trace.finish(
                                    200,
                                    started.elapsed().as_millis() as u64,
                                    false,
                                ));
                                return EmbeddingsOutcome::Json {
                                    value,
                                    summary,
                                    request_id: req.id,
                                };
                            }
                            Err(error) => {
                                snap.resolver.report_failure(
                                    &target.provider_id,
                                    &account_id,
                                    &target.model,
                                    error.class,
                                );
                                snap.resolver.circuit_record(&target.provider_id, false);
                                let fell_over = error.should_fallback();
                                let attempt_ms = attempt_started.elapsed().as_millis() as u64;
                                trace.attempt(sb_trace::Attempt::failed(
                                    &target.id,
                                    &target.provider_id,
                                    &target.model,
                                    &account_id,
                                    egress_eff.as_str(),
                                    attempt_ms,
                                    error.class.as_str(),
                                    fell_over,
                                ));
                                snap.plugins.post_attempt(&sb_plugin::AttemptInfo {
                                    request_id: &req.id,
                                    target_id: &target.id,
                                    provider_id: &target.provider_id,
                                    account_id: &account_id,
                                    egress: egress_eff.as_str(),
                                    ok: false,
                                    error_class: Some(error.class.as_str()),
                                    latency_ms: attempt_ms,
                                });
                                if fell_over {
                                    tried_accounts.insert(account_id);
                                    last_err = Some(error);
                                    continue;
                                }
                                self.traces.record(trace.finish(
                                    error.class.http_status(),
                                    started.elapsed().as_millis() as u64,
                                    false,
                                ));
                                return EmbeddingsOutcome::Error {
                                    request_id: req.id,
                                    error: ExecError::upstream(&error, &summary),
                                };
                            }
                        }
                    }
                    ResolveOutcome::AllUnavailable { .. } => continue 'targets,
                    ResolveOutcome::NoAccounts => continue 'targets,
                }
            }
        }

        if let Some(error) = last_err {
            self.traces.record(trace.finish(
                error.class.http_status(),
                started.elapsed().as_millis() as u64,
                false,
            ));
            return EmbeddingsOutcome::Error {
                request_id: req.id,
                error: ExecError::upstream(&error, &summary),
            };
        }

        let rejected = plan
            .decision
            .rejected
            .iter()
            .map(|rejected| format!("{}:{}", rejected.target_id, rejected.reason))
            .collect::<Vec<_>>()
            .join(",");
        self.traces
            .record(trace.finish(400, started.elapsed().as_millis() as u64, false));
        EmbeddingsOutcome::Error {
            request_id: req.id,
            error: ExecError::new(
                400,
                "invalid_request_error",
                format!(
                    "no eligible target: rejected={} unknown=[{}]",
                    rejected,
                    unknown.join(",")
                ),
                Some(summary),
            ),
        }
    }

    /// The shared execution core — route resolution + two-level (target ×
    /// account) fallback. Format-agnostic: every ingress format funnels through
    /// here, then renders the committed result in its own wire format. (One
    /// loop, not two — the 9router duplication trap avoided.)
    async fn execute_inner(
        &self,
        snap: &Snapshot,
        mut req: AiRequest,
        started: Instant,
    ) -> ExecOutcome {
        // The caller pinned ONE compiled snapshot for this request's whole
        // lifetime — a config publish mid-request never tears it across revisions.
        let rt = &snap.runtime;

        // Global spend cap: once attributed spend reaches `budget.max_usd`, reject
        // new requests (402) rather than keep billing. Per-provider caps are checked
        // per target in the loop below.
        if let Some(max) = rt.budget_max_usd {
            let spent = self.ledger.summary().total_cost_micros as f64 / 1_000_000.0;
            if spent >= max {
                let message = format!("budget exceeded: spent ${spent:.4} of ${max:.4} cap");
                self.record_denial_trace(DenialTrace {
                    request_id: &req.id,
                    revision: snap.revision,
                    inbound_model: &req.model,
                    status: 402,
                    error_type: "budget_exceeded",
                    message: &message,
                    started,
                    streamed: req.stream,
                });
                return ExecOutcome::Error(ExecError::new(402, "budget_exceeded", message, None));
            }
        }

        // Per-tenant hard spend cap (Oracle #4): reject before dispatch once the
        // tenant's attributed spend reaches its configured budget. Reconciliation
        // happens after — `record_usage` accrues the actual cost to the tenant.
        if let Some(tenant) = req.tenant.as_deref() {
            if let Some(budget) = snap.config.tenant(tenant).and_then(|t| t.budget_usd) {
                let spent = self.ledger.tenant_spend_usd(tenant);
                if spent >= budget {
                    let message = format!(
                        "tenant `{tenant}` budget exceeded: spent ${spent:.4} of ${budget:.4} cap"
                    );
                    self.record_denial_trace(DenialTrace {
                        request_id: &req.id,
                        revision: snap.revision,
                        inbound_model: &req.model,
                        status: 402,
                        error_type: "tenant_budget_exceeded",
                        message: &message,
                        started,
                        streamed: req.stream,
                    });
                    return ExecOutcome::Error(ExecError::new(
                        402,
                        "tenant_budget_exceeded",
                        message,
                        None,
                    ));
                }
            }
        }

        // RTK-style tool-result compression (opt-in): shrink bulky tool outputs in
        // the prompt before dispatch. Fail-safe (never-grow/never-empty), so the
        // worst case is a no-op. Metadata-only log, never the content.
        if snap.config.server.compress_tool_results {
            let stats = sb_compress::compress_request(&mut req);
            if stats.saved() > 0 {
                tracing::info!(
                    request_id = %req.id,
                    rtk_bytes_before = stats.bytes_before,
                    rtk_bytes_after = stats.bytes_after,
                    rtk_saved = stats.saved(),
                    rtk_filters = ?stats.filters_applied,
                    "rtk compression"
                );
            }
        }

        // Plugin pre-route hook (Oracle #6): inspect / modify / reject the
        // request before routing. A plugin rejection short-circuits here.
        if let sb_plugin::PluginOutcome::Reject { status, message } =
            snap.plugins.pre_route(&mut req)
        {
            self.record_denial_trace(DenialTrace {
                request_id: &req.id,
                revision: snap.revision,
                inbound_model: &req.model,
                status,
                error_type: "plugin_rejected",
                message: &message,
                started,
                streamed: req.stream,
            });
            return ExecOutcome::Error(ExecError::new(status, "plugin_rejected", message, None));
        }

        // Resolve the request's model to candidate targets (route → provider/model
        // → default provider → 404), pool-health-stamped. Shared with route-preview.
        let resolved = match resolve_candidates(snap, &req.model) {
            Ok(resolved) => resolved,
            Err(e) => {
                self.record_denial_trace(DenialTrace {
                    request_id: &req.id,
                    revision: snap.revision,
                    inbound_model: &req.model,
                    status: e.status,
                    error_type: &e.error_type,
                    message: &e.message,
                    started,
                    streamed: req.stream,
                });
                return ExecOutcome::Error(e);
            }
        };
        let unknown = resolved.unknown.clone();

        let (route_name, plan) = plan_resolved_route(&self.combo_rr, snap, &req, resolved, true);
        // Plugin post-route hook (Oracle #6): observe the explainable decision.
        snap.plugins.post_route(&req, &plan.decision);
        let summary = plan.decision.summary();
        let mut last_err: Option<AdapterError> = None;

        // One trace per request: the route decision + every attempt + outcome + cost
        // + the egress path each attempt took. Metadata only (sb-trace upholds the
        // no-secrets invariant).
        let mut trace = sb_trace::RequestTrace::start(
            req.id.clone(),
            snap.revision,
            req.model.clone(),
            route_name.clone(),
            plan.decision.clone(),
        );

        // Parent span for this request; each attempt opens a child span around the
        // upstream call. A `tracing-opentelemetry` layer exports this tree as one
        // distributed trace with no changes here — the OTel-ready seam.
        let request_span = tracing::info_span!(
            "switchback.request",
            request_id = %req.id,
            inbound_model = %req.model,
            route = %route_name,
            streamed = req.stream,
        );

        // Hedging fast-path (non-streaming only): race the top candidates, take the
        // first success, cancel the losers. On total hedge failure, fall through to
        // the normal sequential fallback loop below.
        if rt.hedge_enabled && !req.stream && plan.candidates.len() >= 2 {
            if let Some(win) = run_hedge(snap, &req, &plan.candidates).await {
                tracing::info!(
                    request_id = %req.id, model = %req.model, target = %win.target_id,
                    account = %win.account_id, status = 200u16,
                    latency_ms = started.elapsed().as_millis() as u64, route = %summary
                );
                self.record_usage(
                    &snap.registry,
                    &req.id,
                    &win.provider_id,
                    &win.model,
                    &win.account_id,
                    req.tenant.as_deref(),
                    win.response.usage.clone(),
                    started,
                    false,
                );
                trace.attempt(sb_trace::Attempt::success(
                    &win.target_id,
                    &win.provider_id,
                    &win.model,
                    &win.account_id,
                    &win.egress,
                    win.latency_ms,
                ));
                for canceled in &win.canceled {
                    trace.attempt(sb_trace::Attempt::failed(
                        &canceled.target_id,
                        &canceled.provider_id,
                        &canceled.model,
                        "unknown",
                        "unknown",
                        started.elapsed().as_millis() as u64,
                        "hedge_cancelled",
                        false,
                    ));
                }
                let cost =
                    snap.registry
                        .cost_micros(&win.provider_id, &win.model, &win.response.usage);
                trace.set_usage(win.response.usage.clone(), cost);
                self.traces
                    .record(trace.finish(200, started.elapsed().as_millis() as u64, false));
                return ExecOutcome::Collected {
                    response: win.response,
                    summary,
                };
            }
        }

        'targets: for target in plan.candidates.iter() {
            let Some(adapter) = snap.registry.adapter(&target.provider_id) else {
                continue 'targets;
            };
            // Circuit breaker: if this provider is OPEN (it's been failing), don't
            // even attempt it — fall straight over to the next target.
            if !snap.resolver.circuit_allows(&target.provider_id) {
                tracing::info!(
                    request_id = %req.id, target = %target.id, provider = %target.provider_id,
                    "circuit open — skipping provider"
                );
                continue 'targets;
            }
            // Per-provider spend cap: route around a provider that has hit its cap.
            if let Some(cap) = snap
                .config
                .server
                .budget
                .per_provider_usd
                .get(&target.provider_id)
            {
                let spent = self.provider_spend_usd(&target.provider_id);
                if spent >= *cap {
                    tracing::info!(
                        request_id = %req.id, provider = %target.provider_id,
                        spent_usd = spent, cap_usd = *cap, "provider over budget — skipping"
                    );
                    continue 'targets;
                }
            }

            let mut tried_accounts: HashSet<String> = HashSet::new();

            loop {
                match snap.resolver.resolve_with_session(
                    &target.provider_id,
                    &target.model,
                    &tried_accounts,
                    session_affinity_key(&req),
                ) {
                    ResolveOutcome::Selected { account_id, lease } => {
                        let attempt_started = Instant::now();
                        // Outbound path for this account: account override → provider
                        // default → server default. `egress_eff` is what the pool will
                        // actually use (falls back to "direct" if it's disabled), so
                        // the trace records the truth. A `select_egress` plugin
                        // (Oracle #6) may pin a named path, overriding the config.
                        let egress_id =
                            snap.plugins.select_egress(&req, &target.id).or_else(|| {
                                resolve_egress(&snap.config, &target.provider_id, &account_id)
                            });
                        let egress_eff = snap.registry.effective_egress(egress_id.as_deref());
                        // Upgrade an OAuth account's lease to a freshly-refreshed
                        // token (no-op for api-key accounts). A refresh failure is
                        // an auth failure on this account → fall over like any other.
                        let lease = match snap
                            .resolver
                            .fresh_lease(&target.provider_id, &account_id, lease)
                            .await
                        {
                            Ok(lease) => lease,
                            Err(e) => {
                                let error = AdapterError::new(
                                    ErrorClass::Authentication,
                                    format!("oauth refresh failed: {e}"),
                                );
                                snap.resolver.report_failure(
                                    &target.provider_id,
                                    &account_id,
                                    &target.model,
                                    error.class,
                                );
                                trace.attempt(sb_trace::Attempt::failed(
                                    &target.id,
                                    &target.provider_id,
                                    &target.model,
                                    &account_id,
                                    egress_eff.as_str(),
                                    attempt_started.elapsed().as_millis() as u64,
                                    error.class.as_str(),
                                    true,
                                ));
                                tried_accounts.insert(account_id);
                                last_err = Some(error);
                                continue;
                            }
                        };
                        for warning in adapter.request_warnings(&req, target) {
                            tracing::warn!(
                                request_id = %req.id,
                                target = %target.id,
                                warning = %warning,
                                "request translation warning"
                            );
                            trace.warning(format!("{}: {warning}", target.id));
                        }
                        let attempt_span = tracing::info_span!(
                            parent: &request_span,
                            "switchback.attempt",
                            target = %target.id,
                            provider = %target.provider_id,
                            account = %account_id,
                            egress = %egress_eff,
                        );
                        // Same-target retry on transient errors (timeout/network/5xx)
                        // before we fall over to another account. The lease + egress
                        // are reused; each retry waits an exponential backoff.
                        let prepared =
                            PreparedRequest::new(req.clone(), target.clone(), Some(lease.clone()))
                                .with_egress(egress_id.clone());
                        let mut exec = adapter
                            .execute(prepared)
                            .instrument(attempt_span.clone())
                            .await;
                        let mut retry_n = 0u32;
                        while let Err(err) = &exec {
                            let retry = &snap.config.server.retry;
                            if retry_n >= rt.retry_max || !retryable(err.class) {
                                break;
                            }
                            retry_n += 1;
                            let delay = retry_backoff(retry, retry_n);
                            tracing::info!(
                                request_id = %req.id, target = %target.id, account = %account_id,
                                retry = retry_n, class = err.class.as_str(),
                                delay_ms = delay.as_millis() as u64, "retrying transient failure"
                            );
                            tokio::time::sleep(delay).await;
                            let prepared = PreparedRequest::new(
                                req.clone(),
                                target.clone(),
                                Some(lease.clone()),
                            )
                            .with_egress(egress_id.clone());
                            exec = adapter
                                .execute(prepared)
                                .instrument(attempt_span.clone())
                                .await;
                        }
                        match exec {
                            Ok(stream) => {
                                if req.stream {
                                    let stream = match precommit_stream(stream).await {
                                        Ok(stream) => stream,
                                        Err(error) => {
                                            snap.resolver.report_failure(
                                                &target.provider_id,
                                                &account_id,
                                                &target.model,
                                                error.class,
                                            );
                                            snap.resolver
                                                .circuit_record(&target.provider_id, false);
                                            let fell_over = error.should_fallback();
                                            let attempt_ms =
                                                attempt_started.elapsed().as_millis() as u64;
                                            trace.attempt(sb_trace::Attempt::failed(
                                                &target.id,
                                                &target.provider_id,
                                                &target.model,
                                                &account_id,
                                                egress_eff.as_str(),
                                                attempt_ms,
                                                error.class.as_str(),
                                                fell_over,
                                            ));
                                            snap.plugins.post_attempt(&sb_plugin::AttemptInfo {
                                                request_id: &req.id,
                                                target_id: &target.id,
                                                provider_id: &target.provider_id,
                                                account_id: &account_id,
                                                egress: egress_eff.as_str(),
                                                ok: false,
                                                error_class: Some(error.class.as_str()),
                                                latency_ms: attempt_ms,
                                            });
                                            if fell_over {
                                                tried_accounts.insert(account_id);
                                                last_err = Some(error);
                                                continue;
                                            }
                                            self.traces.record(trace.finish(
                                                error.class.http_status(),
                                                started.elapsed().as_millis() as u64,
                                                false,
                                            ));
                                            return ExecOutcome::Error(ExecError::upstream(
                                                &error, &summary,
                                            ));
                                        }
                                    };
                                    tracing::info!(
                                        request_id = %req.id, model = %req.model, target = %target.id,
                                        account = %account_id, status = 200u16,
                                        latency_ms = started.elapsed().as_millis() as u64, route = %summary
                                    );
                                    // Meter the stream: record usage/cost AND finalize
                                    // the trace when it completes (the terminal
                                    // UsageDelta is known only after the client drains
                                    // the stream). One callback does both.
                                    let ledger = self.ledger.clone();
                                    let traces = self.traces.clone();
                                    let registry = snap.registry.clone();
                                    let resolver = snap.resolver.clone();
                                    let plugins = snap.plugins.clone();
                                    let (rid, tid, pid, mdl, acct, egress, tnt) = (
                                        req.id.clone(),
                                        target.id.clone(),
                                        target.provider_id.clone(),
                                        target.model.clone(),
                                        account_id.clone(),
                                        egress_eff.clone(),
                                        req.tenant.clone(),
                                    );
                                    // TTFT: record time-to-first-event against this
                                    // attempt's start, so interactive routing learns
                                    // each host's first-byte responsiveness.
                                    let registry_ttft = registry.clone();
                                    let (pid_ttft, mdl_ttft) =
                                        (target.provider_id.clone(), target.model.clone());
                                    let metered = meter_stream(
                                        stream,
                                        attempt_started,
                                        move |ttft_ms| {
                                            registry_ttft.record_ttft(&pid_ttft, &mdl_ttft, ttft_ms)
                                        },
                                        move |usage, finish| {
                                            let latency = started.elapsed().as_millis() as u64;
                                            let attempt_ms =
                                                attempt_started.elapsed().as_millis() as u64;
                                            match finish {
                                                StreamFinish::Clean => {
                                                    resolver.report_success(&pid, &acct);
                                                    resolver.circuit_record(&pid, true);
                                                    trace.attempt(sb_trace::Attempt::success(
                                                        &tid,
                                                        &pid,
                                                        &mdl,
                                                        &acct,
                                                        egress.as_str(),
                                                        attempt_ms,
                                                    ));
                                                    plugins.post_attempt(&sb_plugin::AttemptInfo {
                                                        request_id: &rid,
                                                        target_id: &tid,
                                                        provider_id: &pid,
                                                        account_id: &acct,
                                                        egress: egress.as_str(),
                                                        ok: true,
                                                        error_class: None,
                                                        latency_ms: attempt_ms,
                                                    });
                                                    registry.record_latency(
                                                        &pid,
                                                        &mdl,
                                                        attempt_ms as f64,
                                                    );
                                                    let cost =
                                                        registry.cost_micros(&pid, &mdl, &usage);
                                                    ledger.record(
                                                        sb_ledger::UsageRecord::priced(
                                                            rid,
                                                            pid,
                                                            mdl,
                                                            Some(acct),
                                                            usage.clone(),
                                                            latency,
                                                            true,
                                                            cost,
                                                        )
                                                        .with_tenant(tnt),
                                                    );
                                                    trace.set_usage(usage, cost);
                                                    traces.record(trace.finish(200, latency, true));
                                                }
                                                StreamFinish::UpstreamError(class) => {
                                                    resolver
                                                        .report_failure(&pid, &acct, &mdl, class);
                                                    resolver.circuit_record(&pid, false);
                                                    trace.attempt(sb_trace::Attempt::failed(
                                                        &tid,
                                                        &pid,
                                                        &mdl,
                                                        &acct,
                                                        egress.as_str(),
                                                        attempt_ms,
                                                        class.as_str(),
                                                        false,
                                                    ));
                                                    plugins.post_attempt(&sb_plugin::AttemptInfo {
                                                        request_id: &rid,
                                                        target_id: &tid,
                                                        provider_id: &pid,
                                                        account_id: &acct,
                                                        egress: egress.as_str(),
                                                        ok: false,
                                                        error_class: Some(class.as_str()),
                                                        latency_ms: attempt_ms,
                                                    });
                                                    traces.record(trace.finish(
                                                        class.http_status(),
                                                        latency,
                                                        true,
                                                    ));
                                                }
                                                StreamFinish::Aborted => {
                                                    tracing::info!(
                                                        request_id = %rid, latency_ms = latency,
                                                        "client aborted stream"
                                                    );
                                                    trace.attempt(sb_trace::Attempt::failed(
                                                        &tid,
                                                        &pid,
                                                        &mdl,
                                                        &acct,
                                                        egress.as_str(),
                                                        attempt_ms,
                                                        "client_aborted",
                                                        false,
                                                    ));
                                                    plugins.post_attempt(&sb_plugin::AttemptInfo {
                                                        request_id: &rid,
                                                        target_id: &tid,
                                                        provider_id: &pid,
                                                        account_id: &acct,
                                                        egress: egress.as_str(),
                                                        ok: false,
                                                        error_class: Some("client_aborted"),
                                                        latency_ms: attempt_ms,
                                                    });
                                                    traces.record(trace.finish(499, latency, true));
                                                }
                                            }
                                        },
                                    );
                                    return ExecOutcome::Stream {
                                        stream: metered,
                                        summary,
                                    };
                                }

                                match collect_response(
                                    stream,
                                    req.id.clone(),
                                    req.model.clone(),
                                    snap.config.server.max_response_bytes,
                                )
                                .await
                                {
                                    Ok(response) => {
                                        snap.resolver
                                            .report_success(&target.provider_id, &account_id);
                                        snap.resolver.circuit_record(&target.provider_id, true);
                                        tracing::info!(
                                            request_id = %req.id, model = %req.model, target = %target.id,
                                            account = %account_id, status = 200u16,
                                            latency_ms = started.elapsed().as_millis() as u64, route = %summary
                                        );
                                        self.record_usage(
                                            &snap.registry,
                                            &req.id,
                                            &target.provider_id,
                                            &target.model,
                                            &account_id,
                                            req.tenant.as_deref(),
                                            response.usage.clone(),
                                            started,
                                            false,
                                        );
                                        let attempt_ms =
                                            attempt_started.elapsed().as_millis() as u64;
                                        trace.attempt(sb_trace::Attempt::success(
                                            &target.id,
                                            &target.provider_id,
                                            &target.model,
                                            &account_id,
                                            egress_eff.as_str(),
                                            attempt_ms,
                                        ));
                                        snap.plugins.post_attempt(&sb_plugin::AttemptInfo {
                                            request_id: &req.id,
                                            target_id: &target.id,
                                            provider_id: &target.provider_id,
                                            account_id: &account_id,
                                            egress: egress_eff.as_str(),
                                            ok: true,
                                            error_class: None,
                                            latency_ms: attempt_ms,
                                        });
                                        snap.registry.record_latency(
                                            &target.provider_id,
                                            &target.model,
                                            attempt_ms as f64,
                                        );
                                        let cost = snap.registry.cost_micros(
                                            &target.provider_id,
                                            &target.model,
                                            &response.usage,
                                        );
                                        trace.set_usage(response.usage.clone(), cost);
                                        self.traces.record(trace.finish(
                                            200,
                                            started.elapsed().as_millis() as u64,
                                            false,
                                        ));
                                        return ExecOutcome::Collected { response, summary };
                                    }
                                    Err(error) => {
                                        snap.resolver.report_failure(
                                            &target.provider_id,
                                            &account_id,
                                            &target.model,
                                            error.class,
                                        );
                                        snap.resolver.circuit_record(&target.provider_id, false);
                                        let fell_over = error.should_fallback();
                                        trace.attempt(sb_trace::Attempt::failed(
                                            &target.id,
                                            &target.provider_id,
                                            &target.model,
                                            &account_id,
                                            egress_eff.as_str(),
                                            attempt_started.elapsed().as_millis() as u64,
                                            error.class.as_str(),
                                            fell_over,
                                        ));
                                        if fell_over {
                                            tried_accounts.insert(account_id);
                                            last_err = Some(error);
                                            continue;
                                        }
                                        self.traces.record(trace.finish(
                                            error.class.http_status(),
                                            started.elapsed().as_millis() as u64,
                                            false,
                                        ));
                                        return ExecOutcome::Error(ExecError::upstream(
                                            &error, &summary,
                                        ));
                                    }
                                }
                            }
                            Err(error) => {
                                snap.resolver.report_failure(
                                    &target.provider_id,
                                    &account_id,
                                    &target.model,
                                    error.class,
                                );
                                snap.resolver.circuit_record(&target.provider_id, false);
                                let fell_over = error.should_fallback();
                                let attempt_ms = attempt_started.elapsed().as_millis() as u64;
                                trace.attempt(sb_trace::Attempt::failed(
                                    &target.id,
                                    &target.provider_id,
                                    &target.model,
                                    &account_id,
                                    egress_eff.as_str(),
                                    attempt_ms,
                                    error.class.as_str(),
                                    fell_over,
                                ));
                                snap.plugins.post_attempt(&sb_plugin::AttemptInfo {
                                    request_id: &req.id,
                                    target_id: &target.id,
                                    provider_id: &target.provider_id,
                                    account_id: &account_id,
                                    egress: egress_eff.as_str(),
                                    ok: false,
                                    error_class: Some(error.class.as_str()),
                                    latency_ms: attempt_ms,
                                });
                                if fell_over {
                                    tried_accounts.insert(account_id);
                                    last_err = Some(error);
                                    continue;
                                }
                                self.traces.record(trace.finish(
                                    error.class.http_status(),
                                    started.elapsed().as_millis() as u64,
                                    false,
                                ));
                                return ExecOutcome::Error(ExecError::upstream(&error, &summary));
                            }
                        }
                    }
                    ResolveOutcome::AllUnavailable { .. } => continue 'targets,
                    ResolveOutcome::NoAccounts => continue 'targets,
                }
            }
        }

        if let Some(error) = last_err {
            self.traces.record(trace.finish(
                error.class.http_status(),
                started.elapsed().as_millis() as u64,
                false,
            ));
            return ExecOutcome::Error(ExecError::upstream(&error, &summary));
        }

        let rejected = plan
            .decision
            .rejected
            .iter()
            .map(|rejected| format!("{}:{}", rejected.target_id, rejected.reason))
            .collect::<Vec<_>>()
            .join(",");
        self.traces
            .record(trace.finish(400, started.elapsed().as_millis() as u64, false));
        ExecOutcome::Error(ExecError::new(
            400,
            "invalid_request_error",
            format!(
                "no eligible target: rejected={} unknown=[{}]",
                rejected,
                unknown.join(",")
            ),
            Some(summary),
        ))
    }
}

/// A hedged attempt's winning result + the metadata to record it.
struct HedgeWin {
    response: AiResponse,
    target_id: String,
    provider_id: String,
    model: String,
    account_id: String,
    egress: String,
    latency_ms: u64,
    canceled: Vec<HedgeCancel>,
}

#[derive(Clone)]
struct HedgeCancel {
    target_id: String,
    provider_id: String,
    model: String,
}

/// One self-contained non-streaming attempt for the hedge race: resolve an
/// account, refresh the lease, execute, and collect. `None` on any failure.
async fn hedge_attempt(
    snap: &Snapshot,
    req: &AiRequest,
    target: &sb_core::ExecutionTarget,
) -> Option<HedgeWin> {
    let started = Instant::now();
    let adapter = snap.registry.adapter(&target.provider_id)?;
    let ResolveOutcome::Selected { account_id, lease } = snap.resolver.resolve_with_session(
        &target.provider_id,
        &target.model,
        &HashSet::new(),
        session_affinity_key(req),
    ) else {
        return None;
    };
    let egress_id = snap
        .plugins
        .select_egress(req, &target.id)
        .or_else(|| resolve_egress(&snap.config, &target.provider_id, &account_id));
    let egress_eff = snap.registry.effective_egress(egress_id.as_deref());
    let lease = snap
        .resolver
        .fresh_lease(&target.provider_id, &account_id, lease)
        .await
        .ok()?;
    let prepared =
        PreparedRequest::new(req.clone(), target.clone(), Some(lease)).with_egress(egress_id);
    let stream = adapter.execute(prepared).await.ok()?;
    let response = collect_response(
        stream,
        req.id.clone(),
        req.model.clone(),
        snap.config.server.max_response_bytes,
    )
    .await
    .ok()?;
    snap.resolver
        .report_success(&target.provider_id, &account_id);
    snap.resolver.circuit_record(&target.provider_id, true);
    Some(HedgeWin {
        response,
        target_id: target.id.clone(),
        provider_id: target.provider_id.clone(),
        model: target.model.clone(),
        account_id,
        egress: egress_eff,
        latency_ms: started.elapsed().as_millis() as u64,
        canceled: Vec::new(),
    })
}

/// Race the top `max_parallel` candidates (the n-th delayed by `n*delay_ms`),
/// returning the first success. Losers are cancelled when this returns.
async fn run_hedge(
    snap: &Snapshot,
    req: &AiRequest,
    candidates: &[sb_core::ExecutionTarget],
) -> Option<HedgeWin> {
    let hedge = &snap.config.server.hedge;
    let n = (hedge.max_parallel.max(1) as usize).min(candidates.len());
    let mut futs = futures::stream::FuturesUnordered::new();
    let launched = candidates
        .iter()
        .take(n)
        .map(|target| HedgeCancel {
            target_id: target.id.clone(),
            provider_id: target.provider_id.clone(),
            model: target.model.clone(),
        })
        .collect::<Vec<_>>();
    for (i, target) in candidates.iter().take(n).enumerate() {
        let delay = std::time::Duration::from_millis(hedge.delay_ms.saturating_mul(i as u64));
        futs.push(async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            hedge_attempt(snap, req, target).await
        });
    }
    while let Some(result) = futs.next().await {
        if let Some(mut win) = result {
            win.canceled = launched
                .iter()
                .filter(|launched| launched.target_id != win.target_id)
                .cloned()
                .collect();
            return Some(win); // first success wins; remaining futures are dropped
        }
    }
    None
}

/// The outbound egress for an attempt: account override → provider default →
/// `server.default_egress`. `None` means the default (direct) path. The pool
/// turns an unknown/disabled id back into direct.
fn resolve_egress(config: &Config, provider_id: &str, account_id: &str) -> Option<String> {
    if let Some(provider) = config.providers.iter().find(|p| p.id == provider_id) {
        if let Some(account) = provider.accounts.iter().find(|a| a.id == account_id) {
            if account.egress.is_some() {
                return account.egress.clone();
            }
        }
        if provider.egress.is_some() {
            return provider.egress.clone();
        }
    }
    config.server.default_egress.clone()
}

/// Transient errors an immediate same-account retry might fix. Rate-limit /
/// overload / auth deliberately fall over to a different account instead.
fn retryable(class: ErrorClass) -> bool {
    matches!(
        class,
        ErrorClass::Timeout | ErrorClass::Network | ErrorClass::ServerError
    )
}

/// Capped exponential backoff for retry attempt `n` (1-based). Deterministic.
fn retry_backoff(retry: &sb_core::RetryConfig, attempt: u32) -> std::time::Duration {
    let factor = 2u64.saturating_pow(attempt.saturating_sub(1));
    let ms = retry
        .base_delay_ms
        .saturating_mul(factor)
        .min(retry.max_delay_ms);
    std::time::Duration::from_millis(ms)
}

fn session_affinity_key(req: &AiRequest) -> Option<&str> {
    for key in ["session_id", "switchback_session_id", "codex_session_id"] {
        if let Some(value) = req.metadata.get(key).filter(|v| !v.is_empty()) {
            return Some(value.as_str());
        }
    }
    let metadata = req
        .passthrough
        .get("metadata")
        .and_then(|v| v.as_object())?;
    for key in ["session_id", "switchback_session_id", "codex_session_id"] {
        if let Some(value) = metadata.get(key).and_then(|v| v.as_str()) {
            if !value.is_empty() {
                return Some(value);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use sb_core::{AiStreamEvent, Message};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct FailingAfterBootstrapStore {
        revision_writes: AtomicUsize,
    }

    impl sb_store::StateStore for FailingAfterBootstrapStore {
        fn record_revision(&self, _rec: &sb_store::RevisionRecord) -> sb_store::Result<()> {
            Ok(())
        }

        fn record_revision_and_audit(
            &self,
            _revision: &sb_store::RevisionRecord,
            _audit: &sb_store::AuditEntry,
        ) -> sb_store::Result<()> {
            if self.revision_writes.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(())
            } else {
                Err(sb_store::StoreError("forced revision write failure".into()))
            }
        }

        fn list_revisions(&self, _limit: usize) -> sb_store::Result<Vec<sb_store::RevisionRecord>> {
            Ok(Vec::new())
        }

        fn get_revision(
            &self,
            _revision: u64,
        ) -> sb_store::Result<Option<sb_store::RevisionRecord>> {
            Ok(None)
        }

        fn record_audit(&self, _entry: &sb_store::AuditEntry) -> sb_store::Result<()> {
            Ok(())
        }

        fn list_audit(&self, _limit: usize) -> sb_store::Result<Vec<sb_store::AuditEntry>> {
            Ok(Vec::new())
        }

        fn record_usage(&self, _event: &sb_store::UsageEvent) -> sb_store::Result<()> {
            Ok(())
        }

        fn usage_rollup(&self) -> sb_store::Result<sb_store::UsageRollup> {
            Ok(sb_store::UsageRollup::default())
        }

        fn recent_usage(&self, _limit: usize) -> sb_store::Result<Vec<sb_store::UsageEvent>> {
            Ok(Vec::new())
        }

        fn idempotency_get(
            &self,
            _key: &str,
        ) -> sb_store::Result<Option<sb_store::IdempotencyRecord>> {
            Ok(None)
        }

        fn idempotency_put(&self, _rec: &sb_store::IdempotencyRecord) -> sb_store::Result<bool> {
            Ok(true)
        }

        fn put_draft(&self, _rec: &sb_store::DraftRecord) -> sb_store::Result<()> {
            Ok(())
        }

        fn get_draft(&self, _id: &str) -> sb_store::Result<Option<sb_store::DraftRecord>> {
            Ok(None)
        }

        fn list_drafts(&self) -> sb_store::Result<Vec<sb_store::DraftRecord>> {
            Ok(Vec::new())
        }

        fn delete_draft(&self, _id: &str) -> sb_store::Result<()> {
            Ok(())
        }
    }

    const BASIC_CONFIG: &str = r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: mock
    type: mock
    accounts:
      - id: a
        auth: { kind: api_key, inline: "k" }
routes:
  - name: default
    match: { model: "*" }
    targets:
      - "mock/echo"
"#;

    #[test]
    fn validate_config_rejects_api_keys_for_unknown_tenants() {
        let cfg = Config::from_yaml(&format!(
            "{BASIC_CONFIG}\napi_keys:\n  - key: sk-live\n    tenant: missing\n"
        ))
        .unwrap();

        let err = Engine::validate_config(&cfg).expect_err("unknown tenant must be rejected");

        assert!(
            err.contains("api_keys[0].tenant"),
            "error should name the broken reference: {err}"
        );
    }

    #[test]
    fn validate_config_rejects_closed_wasm_plugin_that_cannot_activate() {
        let cfg = Config::from_yaml(&format!(
            "{BASIC_CONFIG}\nplugins:\n  - type: wasm\n    path: \"/tmp/switchback-missing-policy.wasm\"\n    failure_mode: closed\n"
        ))
        .unwrap();

        let err = Engine::validate_config(&cfg)
            .expect_err("fail-closed wasm activation must reject config validation");

        assert!(
            err.contains("plugins:"),
            "error should mention plugins: {err}"
        );
        assert!(
            err.contains("plugins[0]"),
            "error should name the plugin: {err}"
        );
    }

    #[test]
    fn engine_try_new_rejects_fail_closed_broken_plugin() {
        let cfg = Arc::new(
            Config::from_yaml(&format!(
                "{BASIC_CONFIG}\nplugins:\n  - type: wasm\n    path: \"/tmp/switchback-missing-policy.wasm\"\n    failure_mode: closed\n"
            ))
            .unwrap(),
        );
        let registry = Arc::new(sb_adapters::AdapterRegistry::from_config(&cfg).unwrap());
        let resolver = Arc::new(sb_credentials::CredentialResolver::from_config(&cfg).unwrap());

        let err = match Engine::try_new(
            cfg,
            registry,
            resolver,
            Arc::new(sb_ledger::UsageLedger::in_memory()),
        ) {
            Ok(_) => panic!("try_new must not silently disable fail-closed plugins"),
            Err(err) => err,
        };

        assert!(
            err.contains("plugins[0]"),
            "error should name plugin: {err}"
        );
    }

    #[test]
    fn config_hash_is_stable_sha256() {
        let cfg = Config::from_yaml(BASIC_CONFIG).unwrap();

        let first = config_hash(&cfg);
        let second = config_hash(&cfg);

        assert_eq!(first, second);
        assert_eq!(first.len(), 64);
    }

    #[test]
    fn config_hash_changes_when_route_changes() {
        let first = Config::from_yaml(BASIC_CONFIG).unwrap();
        let second = Config::from_yaml(&BASIC_CONFIG.replace("mock/echo", "mock/other")).unwrap();

        assert_ne!(config_hash(&first), config_hash(&second));
    }

    fn engine_from_config(config: Config) -> Engine {
        let cfg = Arc::new(config);
        let registry = Arc::new(sb_adapters::AdapterRegistry::from_config(&cfg).unwrap());
        let resolver = Arc::new(sb_credentials::CredentialResolver::from_config(&cfg).unwrap());
        Engine::new(
            cfg,
            registry,
            resolver,
            Arc::new(sb_ledger::UsageLedger::in_memory()),
        )
    }

    #[test]
    fn required_store_reload_failure_does_not_swap_runtime() {
        let engine = engine_from_config(Config::from_yaml(BASIC_CONFIG).unwrap())
            .with_store_policy(Arc::new(FailingAfterBootstrapStore::default()), true)
            .unwrap();
        let replacement =
            Config::from_yaml(&BASIC_CONFIG.replace("mock/echo", "mock/replacement")).unwrap();

        let err = engine
            .reload(replacement)
            .expect_err("required store failure must reject reload");

        assert!(err.contains("state store persistence failed"));
        assert_eq!(engine.revision(), 1);
        let req = AiRequest::new("mock/echo", vec![Message::user("hi")]);
        let (_revision, plan) = engine.preview_route(&req).unwrap();
        assert_eq!(plan.candidates[0].id, "mock/echo");
    }

    #[test]
    fn required_store_runtime_patch_failure_does_not_swap_runtime() {
        let engine = engine_from_config(Config::from_yaml(BASIC_CONFIG).unwrap())
            .with_store_policy(Arc::new(FailingAfterBootstrapStore::default()), true)
            .unwrap();

        let err = engine
            .update_runtime(|runtime| runtime.cost_aware = true)
            .expect_err("required store failure must reject runtime patch");

        assert!(err.contains("state store persistence failed"));
        assert_eq!(engine.revision(), 1);
        assert!(!engine.snapshot().runtime.cost_aware);
    }

    #[tokio::test]
    async fn streaming_precommit_error_falls_over_before_client_commit() {
        let cfg = Config::from_yaml(
            r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: mock
    type: mock
    accounts:
      - id: stream-fail-account
        auth: { kind: api_key, inline: "bad" }
        priority: 0
      - id: good-account
        auth: { kind: api_key, inline: "good" }
        priority: 1
routes:
  - name: default
    match: { model: "*" }
    targets:
      - "mock/echo"
"#,
        )
        .unwrap();
        let engine = engine_from_config(cfg);
        let mut req = AiRequest::new("mock/echo", vec![Message::user("hi")]);
        req.stream = true;
        let request_id = req.id.clone();

        let (_revision, outcome) = engine.execute(req, Instant::now()).await;
        let ExecOutcome::Stream { mut stream, .. } = outcome else {
            panic!("expected fallback to commit a healthy stream");
        };

        let mut text = String::new();
        while let Some(item) = stream.next().await {
            if let AiStreamEvent::TextDelta { text: delta } = item.unwrap() {
                text.push_str(&delta);
            }
        }

        assert!(text.contains("echo: hi"));
        let trace = engine.traces().get(&request_id).expect("stream trace");
        assert_eq!(trace.revision, 1);
        assert_eq!(trace.final_status, 200);
        assert_eq!(trace.attempts.len(), 2);
        assert_eq!(trace.attempts[0].account_id, "stream-fail-account");
        assert_eq!(trace.attempts[1].account_id, "good-account");
    }

    #[test]
    fn validate_config_rejects_route_targets_with_unknown_providers() {
        let cfg = Config::from_yaml(
            r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: mock
    type: mock
routes:
  - name: default
    match: { model: "*" }
    targets:
      - "ghost/echo"
"#,
        )
        .unwrap();

        let err = Engine::validate_config(&cfg).expect_err("dangling target must be rejected");

        assert!(
            err.contains("routes[0].targets[0]"),
            "error should name the broken target: {err}"
        );
    }

    #[test]
    fn explicit_provider_model_previews_before_wildcard_route() {
        let cfg = Config::from_yaml(
            r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: mock
    type: mock
    accounts:
      - id: a
        auth: { kind: api_key, inline: "k" }
  - id: openai
    type: openai_compatible
    base_url: "http://127.0.0.1:1/v1"
    accounts:
      - id: a
        auth: { kind: api_key, inline: "k" }
routes:
  - name: default
    match: { model: "*" }
    targets:
      - "mock/echo"
"#,
        )
        .unwrap();
        let cfg = Arc::new(cfg);
        let registry = Arc::new(sb_adapters::AdapterRegistry::from_config(&cfg).unwrap());
        let resolver = Arc::new(sb_credentials::CredentialResolver::from_config(&cfg).unwrap());
        let engine = Engine::new(
            cfg,
            registry,
            resolver,
            Arc::new(sb_ledger::UsageLedger::in_memory()),
        );
        let req = AiRequest::new("openai/gpt-test", vec![Message::user("hi")]);

        let (_revision, plan) = engine.preview_route(&req).unwrap();

        assert_eq!(plan.decision.selected.unwrap().target_id, "openai/gpt-test");
        assert_eq!(plan.candidates[0].id, "openai/gpt-test");
    }
}
