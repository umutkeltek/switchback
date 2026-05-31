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

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use arc_swap::ArcSwap;
use futures::StreamExt;
use sb_adapter::{AdapterError, EventStream, PreparedRequest};
use sb_core::{
    AiRequest, AiResponse, AiStreamEvent, ComboStrategy, Config, ContentPart, ErrorClass,
    ExecutionProfile, FinishReason, Message, Role, RouteRequire, Usage,
};
use sb_credentials::ResolveOutcome;
use tracing::Instrument as _;

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
    /// only (persistence disabled). Writes are best-effort: a store failure logs
    /// a warning but never blocks a publish or request serving.
    store: Option<Arc<dyn sb_store::StateStore>>,
    /// Per-combo target cursor for `strategy: round_robin`. Runtime state, not
    /// config, so it survives hot reload like latency and breaker state.
    combo_rr: Mutex<HashMap<String, usize>>,
}

/// A stable fingerprint of a config (so drift between revisions is detectable)
/// without persisting the body — keeps secrets out of the state store.
fn config_hash(config: &Config) -> String {
    use std::hash::{Hash, Hasher};
    let json = serde_json::to_string(config).unwrap_or_default();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    json.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

impl Engine {
    /// Compile config into snapshot revision 1. The trace log defaults to an
    /// in-memory ring; override it with [`Engine::with_traces`] before sharing.
    pub fn new(
        config: Arc<Config>,
        registry: Arc<sb_adapters::AdapterRegistry>,
        resolver: Arc<sb_credentials::CredentialResolver>,
        ledger: Arc<sb_ledger::UsageLedger>,
    ) -> Self {
        let runtime = Runtime::from_config(&config);
        let plugins = sb_plugin::PluginHost::from_config(&config.plugins);
        let snapshot = Snapshot {
            revision: 1,
            config,
            registry,
            resolver,
            runtime,
            plugins,
        };
        Engine {
            snapshot: ArcSwap::from_pointee(snapshot),
            ledger,
            traces: Arc::new(sb_trace::TraceLog::default()),
            config_path: OnceLock::new(),
            store: None,
            combo_rr: Mutex::new(HashMap::new()),
        }
    }

    /// Replace the default trace log (e.g. with a sampling-configured one).
    /// Consuming builder — call before the engine is shared behind an `Arc`.
    pub fn with_traces(mut self, traces: Arc<sb_trace::TraceLog>) -> Self {
        self.traces = traces;
        self
    }

    /// Attach a durable state store and record the current (bootstrap) revision
    /// as the first entry. Consuming builder — call before sharing.
    pub fn with_store(mut self, store: Arc<dyn sb_store::StateStore>) -> Self {
        self.store = Some(store);
        let cur = self.snapshot.load();
        let hash = config_hash(&cur.config);
        let revision = cur.revision;
        drop(cur);
        self.persist(revision, hash, "bootstrap", "engine start");
        self
    }

    /// The durable state store handle, if persistence is enabled.
    pub fn store(&self) -> Option<Arc<dyn sb_store::StateStore>> {
        self.store.clone()
    }

    /// Best-effort durable record of a published revision + an audit row. A
    /// store error is logged, never propagated — persistence is a control-plane
    /// concern and must not break a publish or request serving.
    fn persist(&self, revision: u64, config_hash: String, source: &str, detail: &str) {
        let Some(store) = &self.store else {
            return;
        };
        let now = sb_store::now_millis();
        if let Err(e) = store.record_revision(&sb_store::RevisionRecord {
            revision,
            config_hash,
            source: source.to_string(),
            created_at_ms: now,
        }) {
            tracing::warn!(error = %e, revision, "state store: record_revision failed");
        }
        if let Err(e) = store.record_audit(&sb_store::AuditEntry {
            revision,
            action: source.to_string(),
            detail: detail.to_string(),
            created_at_ms: now,
        }) {
            tracing::warn!(error = %e, revision, "state store: record_audit failed");
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
        let registry = sb_adapters::AdapterRegistry::from_config(&config)?;
        let resolver = sb_credentials::CredentialResolver::from_config(&config)?;
        let revision = self.snapshot.load().revision + 1;
        let hash = config_hash(&config);
        let plugins = sb_plugin::PluginHost::from_config(&config.plugins);
        self.snapshot.store(Arc::new(Snapshot {
            revision,
            runtime: Runtime::from_config(&config),
            config: Arc::new(config),
            registry: Arc::new(registry),
            resolver: Arc::new(resolver),
            plugins,
        }));
        self.persist(revision, hash, "reload", "config file reload");
        Ok(revision)
    }

    /// Re-read the config file and reload (for `POST /v1/reload`).
    pub fn reload_from_file(&self) -> Result<u64, String> {
        let path = self
            .config_path
            .get()
            .ok_or("no config file path to reload from")?;
        let config = Config::from_path(path).map_err(|e| e.to_string())?;
        self.reload(config)
    }

    /// Apply a runtime-knob change: reuse the current registry/resolver (so
    /// health/credential state is preserved), swap in the new knobs, bump the
    /// revision. Returns the new revision.
    pub fn update_runtime(&self, edit: impl FnOnce(&mut Runtime)) -> u64 {
        let cur = self.snapshot.load();
        let mut runtime = cur.runtime.clone();
        edit(&mut runtime);
        let revision = cur.revision + 1;
        // Same config (knobs only), so the hash is unchanged — the revision row
        // records that knobs changed; the audit detail is the new knob state.
        let hash = config_hash(&cur.config);
        let detail = serde_json::to_string(&runtime).unwrap_or_default();
        self.snapshot.store(Arc::new(Snapshot {
            revision,
            runtime,
            config: cur.config.clone(),
            registry: cur.registry.clone(),
            resolver: cur.resolver.clone(),
            plugins: cur.plugins.clone(),
        }));
        self.persist(revision, hash, "runtime_patch", &detail);
        revision
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

    /// Compute the `RouteDecision` for a request WITHOUT executing it — the same
    /// routing the hot path uses (candidate resolution + pool-health stamp +
    /// `plan_route`), surfaced for `/cp/v1/route-preview`. Returns the plan (the
    /// decision + surviving candidates) and the pinned revision.
    pub fn preview_route(&self, req: &AiRequest) -> Result<(u64, sb_router::RoutePlan), ExecError> {
        let snap = self.snapshot();
        let resolved = resolve_candidates(&snap, &req.model)?;
        let (_route_name, plan) = self.plan_resolved_route(&snap, req, resolved, false);
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
                return EmbeddingsOutcome::Error {
                    request_id: sb_core::new_id("req"),
                    error: ExecError::new(
                        400,
                        "invalid_request_error",
                        "missing or invalid \"model\"",
                        None,
                    ),
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
                return EmbeddingsOutcome::Error {
                    request_id: req.id,
                    error: ExecError::new(
                        402,
                        "budget_exceeded",
                        format!("budget exceeded: spent ${spent:.4} of ${max:.4} cap"),
                        None,
                    ),
                };
            }
        }
        if let Some(tenant) = req.tenant.as_deref() {
            if let Some(budget) = snap.config.tenant(tenant).and_then(|t| t.budget_usd) {
                let spent = self.ledger.tenant_spend_usd(tenant);
                if spent >= budget {
                    return EmbeddingsOutcome::Error {
                        request_id: req.id,
                        error: ExecError::new(
                            402,
                            "tenant_budget_exceeded",
                            format!(
                                "tenant `{tenant}` budget exceeded: spent ${spent:.4} of ${budget:.4} cap"
                            ),
                            None,
                        ),
                    };
                }
            }
        }

        if let sb_plugin::PluginOutcome::Reject { status, message } =
            snap.plugins.pre_route(&mut req)
        {
            return EmbeddingsOutcome::Error {
                request_id: req.id,
                error: ExecError::new(status, "plugin_rejected", message, None),
            };
        }

        let resolved = match resolve_candidates(snap, &req.model) {
            Ok(resolved) => resolved,
            Err(e) => {
                return EmbeddingsOutcome::Error {
                    request_id: req.id,
                    error: e,
                }
            }
        };
        let unknown = resolved.unknown.clone();
        let (route_name, plan) = self.plan_resolved_route(snap, &req, resolved, true);
        snap.plugins.post_route(&req, &plan.decision);
        let summary = format!("{} embeddings", plan.decision.summary());
        let mut trace = sb_trace::RequestTrace::start(
            req.id.clone(),
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
                return ExecOutcome::Error(ExecError::new(
                    402,
                    "budget_exceeded",
                    format!("budget exceeded: spent ${spent:.4} of ${max:.4} cap"),
                    None,
                ));
            }
        }

        // Per-tenant hard spend cap (Oracle #4): reject before dispatch once the
        // tenant's attributed spend reaches its configured budget. Reconciliation
        // happens after — `record_usage` accrues the actual cost to the tenant.
        if let Some(tenant) = req.tenant.as_deref() {
            if let Some(budget) = snap.config.tenant(tenant).and_then(|t| t.budget_usd) {
                let spent = self.ledger.tenant_spend_usd(tenant);
                if spent >= budget {
                    return ExecOutcome::Error(ExecError::new(
                        402,
                        "tenant_budget_exceeded",
                        format!(
                            "tenant `{tenant}` budget exceeded: spent ${spent:.4} of ${budget:.4} cap"
                        ),
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
            return ExecOutcome::Error(ExecError::new(status, "plugin_rejected", message, None));
        }

        // Resolve the request's model to candidate targets (route → provider/model
        // → default provider → 404), pool-health-stamped. Shared with route-preview.
        let resolved = match resolve_candidates(snap, &req.model) {
            Ok(resolved) => resolved,
            Err(e) => return ExecOutcome::Error(e),
        };
        let unknown = resolved.unknown.clone();

        let (route_name, plan) = self.plan_resolved_route(snap, &req, resolved, true);
        // Plugin post-route hook (Oracle #6): observe the explainable decision.
        snap.plugins.post_route(&req, &plan.decision);
        let summary = plan.decision.summary();
        let mut last_err: Option<AdapterError> = None;

        // One trace per request: the route decision + every attempt + outcome + cost
        // + the egress path each attempt took. Metadata only (sb-trace upholds the
        // no-secrets invariant).
        let mut trace = sb_trace::RequestTrace::start(
            req.id.clone(),
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

/// A committed execution failure, rendered to the client's wire format by the
/// HTTP edge. Carries an HTTP-ish status hint, an error type string, the
/// message, and (when a routing decision was made) the route summary so the
/// edge can still stamp `x-switchback-route`.
#[derive(Debug, Clone)]
pub struct ExecError {
    pub status: u16,
    pub error_type: String,
    pub message: String,
    pub summary: Option<String>,
}

impl ExecError {
    pub fn new(
        status: u16,
        error_type: impl Into<String>,
        message: impl Into<String>,
        summary: Option<String>,
    ) -> Self {
        ExecError {
            status,
            error_type: error_type.into(),
            message: message.into(),
            summary,
        }
    }

    /// An upstream attempt failure (after a routing decision was made).
    fn upstream(error: &AdapterError, summary: &str) -> Self {
        ExecError {
            status: error.class.http_status(),
            error_type: "upstream_error".to_string(),
            message: error.message.clone(),
            summary: Some(summary.to_string()),
        }
    }
}

/// Committed result of the shared execution core: a live stream (client wants
/// streaming), a collected response (non-streaming), or a structured error.
pub enum ExecOutcome {
    Stream {
        stream: EventStream,
        summary: String,
    },
    Collected {
        response: AiResponse,
        summary: String,
    },
    Error(ExecError),
}

/// Committed result of the embeddings runtime path. The response stays in the
/// OpenAI-compatible embeddings wire shape because embeddings are not canonical
/// chat/message IR.
pub enum EmbeddingsOutcome {
    Json {
        value: serde_json::Value,
        summary: String,
        request_id: String,
    },
    Error {
        error: ExecError,
        request_id: String,
    },
}

fn embeddings_usage(value: &serde_json::Value) -> Usage {
    let prompt = value
        .pointer("/usage/prompt_tokens")
        .and_then(serde_json::Value::as_u64);
    let total = value
        .pointer("/usage/total_tokens")
        .and_then(serde_json::Value::as_u64);
    Usage {
        input_tokens: prompt.or(total).unwrap_or_default(),
        ..Usage::default()
    }
}

/// Collect a canonical event stream into a single `AiResponse` (the
/// non-streaming path is just collection of the one streaming path).
async fn collect_response(
    mut stream: EventStream,
    req_id: String,
    model: String,
    max_bytes: Option<u64>,
) -> Result<AiResponse, AdapterError> {
    let mut content = String::new();
    let mut tool_uses: BTreeMap<u32, (String, String, String)> = BTreeMap::new();
    let mut finish_reason = None;
    let mut usage = Usage::default();
    // Running tally of assembled bytes — the collect-path ceiling (Oracle #8)
    // aborts rather than buffering an unbounded non-streaming response.
    let mut assembled: u64 = 0;
    let over_cap = |assembled: u64| -> Option<AdapterError> {
        max_bytes.filter(|max| assembled > *max).map(|max| {
            AdapterError::new(
                ErrorClass::ServerError,
                format!("response exceeded max_response_bytes ({max})"),
            )
        })
    };

    while let Some(item) = stream.next().await {
        match item? {
            AiStreamEvent::TextDelta { text } => {
                assembled += text.len() as u64;
                if let Some(err) = over_cap(assembled) {
                    return Err(err);
                }
                content.push_str(&text);
            }
            AiStreamEvent::ToolCallStart(start) => {
                tool_uses.insert(start.index, (start.id, start.name, String::new()));
            }
            AiStreamEvent::ToolCallArgsDelta { index, json } => {
                assembled += json.len() as u64;
                if let Some(err) = over_cap(assembled) {
                    return Err(err);
                }
                if let Some((_, _, args)) = tool_uses.get_mut(&index) {
                    args.push_str(&json);
                }
            }
            AiStreamEvent::ToolCallEnd { .. } => {}
            AiStreamEvent::UsageDelta { usage: delta } => {
                usage = delta;
            }
            AiStreamEvent::MessageEnd {
                finish_reason: finish,
            } => {
                finish_reason = Some(finish);
            }
            AiStreamEvent::Error { message, class } => {
                return Err(AdapterError::new(class, message));
            }
            AiStreamEvent::MessageStart { .. } | AiStreamEvent::ReasoningDelta { .. } => {}
        }
    }

    let mut parts = Vec::new();
    if !content.is_empty() {
        parts.push(ContentPart::text(content));
    }

    for (_, (id, name, args)) in tool_uses {
        parts.push(ContentPart::ToolUse {
            id,
            name,
            args: serde_json::from_str(&args).unwrap_or(serde_json::Value::String(args)),
        });
    }

    Ok(AiResponse {
        id: req_id,
        model,
        message: Message {
            role: Role::Assistant,
            content: parts,
        },
        finish_reason: finish_reason.unwrap_or(FinishReason::Stop),
        usage,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamFinish {
    Clean,
    UpstreamError(ErrorClass),
    Aborted,
}

/// Holds the stream finalizer. A clean finish or upstream error fires it and
/// DISARMS the guard; if the guard reaches `Drop` still armed, the stream was
/// dropped mid-flight (the client hung up) and it fires as `Aborted`.
struct FinishGuard<F: FnOnce(Usage, StreamFinish)> {
    usage: Usage,
    on_finish: Option<F>,
}

impl<F: FnOnce(Usage, StreamFinish)> FinishGuard<F> {
    /// Clean finish: fire and disarm.
    fn complete(&mut self) {
        if let Some(finish) = self.on_finish.take() {
            finish(std::mem::take(&mut self.usage), StreamFinish::Clean);
        }
    }

    /// Upstream stream error: fire and disarm before yielding the error.
    fn error(&mut self, class: ErrorClass) {
        if let Some(finish) = self.on_finish.take() {
            finish(
                std::mem::take(&mut self.usage),
                StreamFinish::UpstreamError(class),
            );
        }
    }
}

impl<F: FnOnce(Usage, StreamFinish)> Drop for FinishGuard<F> {
    fn drop(&mut self) {
        // Still armed at drop ⇒ the stream never reached a terminal state.
        if let Some(finish) = self.on_finish.take() {
            finish(std::mem::take(&mut self.usage), StreamFinish::Aborted);
        }
    }
}

/// Wrap a streamed response so: (1) `on_first` fires with the elapsed ms when the
/// FIRST event arrives (time-to-first-token), and (2) `on_finish(usage, outcome)`
/// runs exactly once when the stream ends cleanly, yields an upstream error, or
/// is dropped before completion. `on_first` simply never fires if the client
/// drops before the first event.
fn meter_stream<G, F>(
    stream: EventStream,
    started: Instant,
    on_first: G,
    on_finish: F,
) -> EventStream
where
    G: FnOnce(f64) + Send + 'static,
    F: FnOnce(Usage, StreamFinish) + Send + 'static,
{
    let guard = FinishGuard {
        usage: Usage::default(),
        on_finish: Some(on_finish),
    };
    futures::stream::unfold(
        (stream, guard, Some(on_first), started),
        |(mut stream, mut guard, mut on_first, started)| async move {
            match stream.next().await {
                Some(item) => {
                    if let Some(first) = on_first.take() {
                        first(started.elapsed().as_millis() as f64);
                    }
                    if let Ok(AiStreamEvent::UsageDelta { usage: latest }) = &item {
                        guard.usage = latest.clone();
                    }
                    if let Err(error) = &item {
                        guard.error(error.class);
                    }
                    Some((item, (stream, guard, on_first, started)))
                }
                None => {
                    guard.complete();
                    None
                }
            }
        },
    )
    .boxed()
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

fn routing_policy(snap: &Snapshot, profile: Option<ExecutionProfile>) -> sb_core::RoutingPolicy {
    let mut policy = sb_core::RoutingPolicy {
        profile,
        cost_aware: snap.runtime.cost_aware,
        max_price_per_mtok: snap.config.server.cost_max_per_mtok,
        latency_aware: snap.runtime.latency_aware,
        allow_free: snap.config.server.cost_allow_free,
        allow_promo: snap.config.server.cost_allow_promo,
        allow_aggregator: snap.config.server.cost_allow_aggregator,
        enforce_lane_policy: false,
    };

    match profile {
        Some(ExecutionProfile::Cheap) => {
            policy.cost_aware = true;
            policy.latency_aware = false;
        }
        Some(ExecutionProfile::Fast) => {
            policy.cost_aware = false;
            policy.latency_aware = true;
        }
        Some(ExecutionProfile::Private) => {
            policy.allow_free = false;
            policy.allow_promo = false;
            policy.allow_aggregator = false;
            policy.enforce_lane_policy = true;
        }
        Some(
            ExecutionProfile::Auto | ExecutionProfile::Coding | ExecutionProfile::LargeContext,
        )
        | None => {}
    }

    policy
}

#[derive(Debug, Clone)]
struct CandidateResolution {
    route_name: String,
    require: RouteRequire,
    candidates: Vec<sb_core::ExecutionTarget>,
    unknown: Vec<String>,
    profile: Option<ExecutionProfile>,
    combo: Option<ResolvedCombo>,
}

#[derive(Debug, Clone)]
struct ResolvedCombo {
    name: String,
    strategy: ComboStrategy,
}

impl Engine {
    fn plan_resolved_route(
        &self,
        snap: &Snapshot,
        req: &AiRequest,
        mut resolved: CandidateResolution,
        advance_combo_cursor: bool,
    ) -> (String, sb_router::RoutePlan) {
        self.apply_combo_order(&mut resolved, advance_combo_cursor);
        let route_name = resolved.route_name.clone();
        let policy = routing_policy(snap, resolved.profile);
        let mut plan = sb_router::plan_route(
            req,
            &resolved.route_name,
            &resolved.require,
            &resolved.candidates,
            &policy,
        );
        if let Some(combo) = &resolved.combo {
            plan.decision.strategy = match combo.strategy {
                ComboStrategy::Fallback => "combo_fallback",
                ComboStrategy::RoundRobin => "combo_round_robin",
            }
            .to_string();
            plan.decision.add_reason(format!("combo={}", combo.name));
            plan.decision
                .add_reason(format!("combo_strategy={}", combo.strategy.as_str()));
        }
        (route_name, plan)
    }

    fn apply_combo_order(&self, resolved: &mut CandidateResolution, advance_cursor: bool) {
        let Some(combo) = &resolved.combo else {
            return;
        };
        match combo.strategy {
            ComboStrategy::Fallback => {}
            ComboStrategy::RoundRobin => {
                let len = resolved.candidates.len();
                if len <= 1 {
                    return;
                }
                let mut cursors = self.combo_rr.lock().expect("combo rr mutex");
                let cursor = cursors.entry(combo.name.clone()).or_default();
                let offset = *cursor % len;
                if advance_cursor {
                    *cursor = cursor.wrapping_add(1);
                }
                resolved.candidates.rotate_left(offset);
            }
        }
    }
}

/// Resolve a model to ordered candidate targets — the routing front-half shared
/// by `execute` and `preview_route`. Precedence: execution profile route →
/// exact route → combo profile → wildcard route → explicit `provider/model` →
/// default pass-through provider → 404. Each candidate is stamped with its
/// non-secret account-pool health so the router can demote locked pools.
fn resolve_candidates(snap: &Snapshot, model: &str) -> Result<CandidateResolution, ExecError> {
    let profile = ExecutionProfile::from_model(model);
    let (route_name, require, mut candidates, unknown, combo): (
        String,
        RouteRequire,
        Vec<sb_core::ExecutionTarget>,
        Vec<String>,
        Option<ResolvedCombo>,
    ) = if let Some(profile) = profile {
        let route = snap
            .config
            .exact_route_for(model)
            .or_else(|| snap.config.wildcard_route())
            .ok_or_else(|| {
                ExecError::new(
                    404,
                    "invalid_request_error",
                    format!(
                        "execution profile `{}` needs a matching route or catch-all `*` route",
                        profile.id()
                    ),
                    None,
                )
            })?;
        let mut candidates = Vec::new();
        let mut unknown = Vec::new();
        for target_id in &route.targets {
            match snap.registry.target_for(target_id) {
                Some(target) => candidates.push(target),
                None => unknown.push(target_id.clone()),
            }
        }
        let route_name = if route.match_.model.as_deref() == Some(model) {
            route.name.clone()
        } else {
            format!("{} via {}", profile.id(), route.name)
        };
        (route_name, route.require.clone(), candidates, unknown, None)
    } else if let Some(route) = snap.config.exact_route_for(model) {
        let mut candidates = Vec::new();
        let mut unknown = Vec::new();
        for target_id in &route.targets {
            match snap.registry.target_for(target_id) {
                Some(target) => candidates.push(target),
                None => unknown.push(target_id.clone()),
            }
        }
        (
            route.name.clone(),
            route.require.clone(),
            candidates,
            unknown,
            None,
        )
    } else if let Some(combo_cfg) = snap.config.combo_for(model) {
        let mut candidates = Vec::new();
        let mut unknown = Vec::new();
        for target_id in &combo_cfg.models {
            match snap.registry.target_for(target_id) {
                Some(target) => candidates.push(target),
                None => unknown.push(target_id.clone()),
            }
        }
        (
            format!("combo/{model}"),
            combo_cfg.require.clone(),
            candidates,
            unknown,
            Some(ResolvedCombo {
                name: model.to_string(),
                strategy: combo_cfg.strategy,
            }),
        )
    } else if let Some(route) = snap.config.wildcard_route() {
        let mut candidates = Vec::new();
        let mut unknown = Vec::new();
        for target_id in &route.targets {
            match snap.registry.target_for(target_id) {
                Some(target) => candidates.push(target),
                None => unknown.push(target_id.clone()),
            }
        }
        (
            route.name.clone(),
            route.require.clone(),
            candidates,
            unknown,
            None,
        )
    } else if let Some(target) = snap.registry.target_for(model) {
        (
            "direct".to_string(),
            RouteRequire::default(),
            vec![target],
            Vec::new(),
            None,
        )
    } else if let Some(provider) = snap.config.server.default_provider.as_deref() {
        match snap.registry.target_for_provider_model(provider, model) {
            Some(mut target) => {
                // Unknown-model pass-through: forwarded verbatim, so its
                // capabilities + price are NOT catalog-verified (Oracle #5).
                target.unverified = true;
                (
                    format!("default:{provider}"),
                    RouteRequire::default(),
                    vec![target],
                    Vec::new(),
                    None,
                )
            }
            None => {
                return Err(ExecError::new(
                    404,
                    "invalid_request_error",
                    format!("default_provider `{provider}` is not a configured provider"),
                    None,
                ));
            }
        }
    } else {
        return Err(ExecError::new(
            404,
            "invalid_request_error",
            format!(
                "no route or target for model `{model}` — add a route, use `provider/model`, or set server.default_provider"
            ),
            None,
        ));
    };

    for candidate in candidates.iter_mut() {
        let ph = snap
            .resolver
            .pool_health(&candidate.provider_id, &candidate.model);
        candidate.healthy_accounts = Some(if ph.circuit_open { 0 } else { ph.healthy });
    }
    Ok(CandidateResolution {
        route_name,
        require,
        candidates,
        unknown,
        profile,
        combo,
    })
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
    use std::sync::{Arc, Mutex};

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

    fn channel_stream() -> (
        futures::channel::mpsc::UnboundedSender<Result<AiStreamEvent, AdapterError>>,
        EventStream,
    ) {
        let (tx, rx) = futures::channel::mpsc::unbounded();
        (tx, rx.boxed())
    }

    #[tokio::test]
    async fn meter_stream_records_a_clean_finish() {
        let outcome = Arc::new(Mutex::new(None));
        let sink = outcome.clone();
        let (tx, stream) = channel_stream();
        let mut metered = meter_stream(
            stream,
            Instant::now(),
            |_| {},
            move |_usage, finish| {
                *sink.lock().unwrap() = Some(finish);
            },
        );
        tx.unbounded_send(Ok(AiStreamEvent::TextDelta { text: "hi".into() }))
            .unwrap();
        drop(tx); // close the channel → the stream ends cleanly
        while metered.next().await.is_some() {}
        assert_eq!(
            *outcome.lock().unwrap(),
            Some(StreamFinish::Clean),
            "clean finish"
        );
    }

    #[tokio::test]
    async fn meter_stream_records_an_early_drop_as_aborted() {
        let outcome = Arc::new(Mutex::new(None));
        let sink = outcome.clone();
        let (tx, stream) = channel_stream();
        let mut metered = meter_stream(
            stream,
            Instant::now(),
            |_| {},
            move |_usage, finish| {
                *sink.lock().unwrap() = Some(finish);
            },
        );
        tx.unbounded_send(Ok(AiStreamEvent::TextDelta { text: "hi".into() }))
            .unwrap();
        assert!(metered.next().await.is_some());
        // The client hangs up before the stream completes (tx kept alive). The
        // FinishGuard fires synchronously on drop with completed=false.
        drop(metered);
        assert_eq!(
            *outcome.lock().unwrap(),
            Some(StreamFinish::Aborted),
            "early drop = aborted"
        );
        drop(tx);
    }

    #[tokio::test]
    async fn meter_stream_records_upstream_error_before_drop() {
        let outcome = Arc::new(Mutex::new(None));
        let sink = outcome.clone();
        let (tx, stream) = channel_stream();
        let mut metered = meter_stream(
            stream,
            Instant::now(),
            |_| {},
            move |_usage, finish| {
                *sink.lock().unwrap() = Some(finish);
            },
        );
        tx.unbounded_send(Err(AdapterError::new(
            ErrorClass::StreamInterrupted,
            "broken stream",
        )))
        .unwrap();

        let item = metered.next().await.expect("error item");
        assert!(item.is_err());
        assert_eq!(
            *outcome.lock().unwrap(),
            Some(StreamFinish::UpstreamError(ErrorClass::StreamInterrupted)),
            "upstream stream errors are not client aborts"
        );
        drop(metered);
        assert_eq!(
            *outcome.lock().unwrap(),
            Some(StreamFinish::UpstreamError(ErrorClass::StreamInterrupted)),
            "drop after the error must not fire a second outcome"
        );
        drop(tx);
    }

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
}
