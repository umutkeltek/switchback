use std::collections::{HashSet, VecDeque};
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use clap::{Parser, Subcommand};
use futures::StreamExt;
use sb_adapter::{AdapterError, EventStream};
use sb_core::{AiStreamEvent, Config, ErrorClass, ProviderKind};
use sb_credentials::ResolveOutcome;
use sb_runtime::{Engine, ExecError, ExecOutcome, Runtime, Snapshot};

mod admission;
mod controlplane;
mod cp;
mod idempotency;
mod tenancy;

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
    /// Global admission control (in-flight cap + bounded-wait backpressure).
    /// Process-lifetime (a semaphore can't be rebuilt on reload without losing
    /// the in-flight count), so `max_concurrency` is fixed at startup.
    pub admission: admission::Admission,
    /// Staged `/cp/v1` config drafts (in-memory, process-lifetime).
    pub drafts: cp::DraftStore,
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
            drafts: cp::DraftStore::new(engine.store()),
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

    /// Apply a runtime-knob change (reuses registry/resolver; bumps revision).
    pub fn update_runtime(&self, edit: impl FnOnce(&mut Runtime)) -> u64 {
        self.engine.update_runtime(edit)
    }
}

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    Serve {
        #[arg(long, default_value = "config/switchback.example.yaml")]
        config: PathBuf,
        #[arg(long)]
        bind: Option<String>,
    },
    Doctor {
        #[arg(long, default_value = "config/switchback.example.yaml")]
        config: PathBuf,
    },
    /// Manage the encrypted credential vault (age file + OS-keychain key).
    Vault {
        #[command(subcommand)]
        action: VaultCmd,
        // global so it's accepted after the subcommand (`vault set X --config Y`).
        #[arg(long, global = true, default_value = "config/switchback.example.yaml")]
        config: PathBuf,
    },
    /// Inspect the configuration (machine-friendly JSON; for tools and AIs).
    Config {
        #[command(subcommand)]
        action: ConfigCmd,
        #[arg(long, global = true, default_value = "config/switchback.example.yaml")]
        config: PathBuf,
    },
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Print the full effective config as redacted JSON.
    Show,
    /// Print one value by dotted path (e.g. `server.cost_aware`, `providers.0.id`).
    Get { pointer: String },
    /// Load + validate the config; exit non-zero on problems.
    Validate,
    /// List providers (id, type, egress, account ids).
    Providers,
    /// List routes (name + targets).
    Routes,
}

#[derive(Subcommand)]
enum VaultCmd {
    /// Generate a key (stored in the OS keychain) and create an empty vault file.
    Init,
    /// Print a fresh age key for SWITCHBACK_VAULT_KEY (headless / CI / no keychain).
    Keygen,
    /// Add or replace a secret. Value from --value, else read from stdin.
    Set {
        name: String,
        #[arg(long)]
        value: Option<String>,
    },
    /// List secret names (never values).
    List,
    /// Remove a secret.
    Rm { name: String },
}

pub fn run() -> anyhow::Result<()> {
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async_run())
}

/// Install the tracing subscriber: an env-filtered fmt layer that prints span
/// closes (so the request/attempt span tree is visible), plus — when built with
/// the `otel` feature and an OTLP endpoint is configured — an OpenTelemetry
/// export layer. The spans are the same either way; OTel just ships them out.
fn init_tracing(otel_endpoint: Option<&str>) {
    use tracing_subscriber::prelude::*;
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE);

    #[cfg(feature = "otel")]
    {
        let otel_layer = match otel_endpoint {
            Some(endpoint) => match otel_export::build_tracer(endpoint) {
                Ok(tracer) => {
                    tracing::info!(%endpoint, "otel: exporting spans via OTLP");
                    Some(tracing_opentelemetry::layer().with_tracer(tracer))
                }
                Err(e) => {
                    eprintln!("otel: {e}; export disabled (spans still render locally)");
                    None
                }
            },
            None => None,
        };
        let _ = tracing_subscriber::registry()
            .with(filter)
            .with(fmt_layer)
            .with(otel_layer)
            .try_init();
    }

    #[cfg(not(feature = "otel"))]
    {
        if otel_endpoint.is_some() {
            eprintln!(
                "otel_endpoint is set but this binary was built without the `otel` feature"
            );
        }
        let _ = tracing_subscriber::registry()
            .with(filter)
            .with(fmt_layer)
            .try_init();
    }
}

/// OTLP exporter wiring. Builds a batch span exporter over OTLP/HTTP and a
/// tracer the `tracing-opentelemetry` layer drives. Only compiled with `otel`.
#[cfg(feature = "otel")]
mod otel_export {
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_otlp::WithExportConfig;

    pub fn build_tracer(endpoint: &str) -> Result<opentelemetry_sdk::trace::Tracer, String> {
        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_endpoint(endpoint)
            .build()
            .map_err(|e| format!("build OTLP exporter: {e}"))?;
        let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
            .with_batch_exporter(exporter)
            .with_resource(
                opentelemetry_sdk::Resource::builder()
                    .with_service_name("switchback")
                    .build(),
            )
            .build();
        let tracer = provider.tracer("switchback");
        // Keep the provider installed globally so the batch exporter keeps
        // flushing for the process lifetime.
        opentelemetry::global::set_tracer_provider(provider);
        Ok(tracer)
    }
}

async fn async_run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    // Pre-load the serve config so tracing init can wire the OTLP exporter from
    // `server.otel_endpoint` before any spans are emitted.
    let serve_cfg = match &cli.cmd {
        Cmd::Serve { config, .. } => Some(Config::from_path(config)?),
        _ => None,
    };
    init_tracing(serve_cfg.as_ref().and_then(|c| c.server.otel_endpoint.as_deref()));

    match cli.cmd {
        Cmd::Serve { bind, config } => {
            let cfg = serve_cfg.expect("serve config pre-loaded above");
            let registry =
                sb_adapters::AdapterRegistry::from_config(&cfg).map_err(|e| anyhow::anyhow!(e))?;
            let resolver = sb_credentials::CredentialResolver::from_config(&cfg)
                .map_err(|e| anyhow::anyhow!(e))?;
            // Durable control-plane + usage state (opt-in via `server.state_store`).
            // Opened once and shared by the ledger (usage events) and the engine
            // (config revisions + audit). A failed open disables persistence rather
            // than refusing to start — the gateway still serves from memory.
            let store: Option<Arc<dyn sb_store::StateStore>> = match cfg.server.state_store.as_deref()
            {
                Some(path) => match sb_store::SqliteStore::open(path) {
                    Ok(s) => {
                        tracing::info!(%path, "state store enabled (revisions + audit + usage)");
                        Some(Arc::new(s))
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, %path, "state store disabled: open failed");
                        None
                    }
                },
                None => None,
            };
            let mut ledger = match &cfg.server.usage_log {
                Some(path) => sb_ledger::UsageLedger::with_sink(path),
                None => sb_ledger::UsageLedger::in_memory(),
            };
            if let Some(s) = &store {
                ledger = ledger.with_store(s.clone());
            }
            let traces = sb_trace::TraceLog::new(
                cfg.server.trace_ring_size,
                cfg.server.trace_log.clone().map(Into::into),
                cfg.server.trace_sample,
            );
            let bind = bind.unwrap_or_else(|| cfg.server.bind.clone());
            let mut engine = Engine::new(
                Arc::new(cfg),
                Arc::new(registry),
                Arc::new(resolver),
                Arc::new(ledger),
            )
            .with_traces(Arc::new(traces));
            if let Some(s) = store {
                engine = engine.with_store(s);
            }
            engine.set_config_path(config);
            let app = build_app(AppState::from_engine(engine));
            let listener = tokio::net::TcpListener::bind(&bind).await?;
            tracing::info!(%bind, "switchback listening");
            axum::serve(listener, app).await?;
        }
        Cmd::Vault { action, config } => {
            // Keygen needs no config/vault section — it just mints a key.
            if let VaultCmd::Keygen = action {
                println!("{}", sb_credentials::vault::generate_identity_string());
                return Ok(());
            }
            let cfg = Config::from_path(&config)?;
            let vc = cfg.vault.ok_or_else(|| {
                anyhow::anyhow!(
                    "no `vault:` section in {} — add one (path + keychain_service)",
                    config.display()
                )
            })?;
            let path = std::path::Path::new(&vc.path);
            let service = &vc.keychain_service;
            match action {
                VaultCmd::Keygen => unreachable!("handled above"),
                VaultCmd::Init => {
                    sb_credentials::vault::init(path, service).map_err(|e| anyhow::anyhow!(e))?;
                    println!("vault initialized at {}", vc.path);
                }
                VaultCmd::Set { name, value } => {
                    let value = match value {
                        Some(value) => value,
                        None => {
                            use std::io::Read;
                            let mut buf = String::new();
                            std::io::stdin().read_to_string(&mut buf)?;
                            buf.trim_end_matches(['\n', '\r']).to_string()
                        }
                    };
                    sb_credentials::vault::set_secret(path, service, &name, &value)
                        .map_err(|e| anyhow::anyhow!(e))?;
                    println!("set secret `{name}`");
                }
                VaultCmd::List => {
                    let names = sb_credentials::vault::list_secrets(path, service)
                        .map_err(|e| anyhow::anyhow!(e))?;
                    if names.is_empty() {
                        println!("(vault is empty)");
                    }
                    for name in names {
                        println!("{name}");
                    }
                }
                VaultCmd::Rm { name } => {
                    let removed = sb_credentials::vault::remove_secret(path, service, &name)
                        .map_err(|e| anyhow::anyhow!(e))?;
                    println!(
                        "{}",
                        if removed {
                            format!("removed `{name}`")
                        } else {
                            format!("`{name}` not found")
                        }
                    );
                }
            }
        }
        Cmd::Doctor { config } => {
            let cfg = Config::from_path(&config)?;
            for provider in &cfg.providers {
                match &provider.kind {
                    ProviderKind::Mock => {
                        println!("provider {} mock", provider.id);
                    }
                    ProviderKind::OpenaiCompatible {
                        base_url,
                        api_key_env,
                        ..
                    } => {
                        println!(
                            "provider {} openai_compatible base_url={}",
                            provider.id, base_url
                        );
                        if let Some(name) = api_key_env {
                            println!(
                                "provider {} api_key_env={} present={}",
                                provider.id,
                                name,
                                std::env::var(name).is_ok()
                            );
                        }
                    }
                    ProviderKind::Anthropic {
                        base_url,
                        api_key_env,
                        ..
                    } => {
                        println!("provider {} anthropic base_url={}", provider.id, base_url);
                        if let Some(name) = api_key_env {
                            println!(
                                "provider {} api_key_env={} present={}",
                                provider.id,
                                name,
                                std::env::var(name).is_ok()
                            );
                        }
                    }
                    ProviderKind::Gemini {
                        base_url,
                        api_key_env,
                        ..
                    } => {
                        println!("provider {} gemini base_url={}", provider.id, base_url);
                        if let Some(name) = api_key_env {
                            println!(
                                "provider {} api_key_env={} present={}",
                                provider.id,
                                name,
                                std::env::var(name).is_ok()
                            );
                        }
                    }
                    ProviderKind::Vertex {
                        project,
                        region,
                        api_key_env,
                        ..
                    } => {
                        println!(
                            "provider {} vertex project={} region={}",
                            provider.id, project, region
                        );
                        if let Some(name) = api_key_env {
                            println!(
                                "provider {} api_key_env={} present={}",
                                provider.id,
                                name,
                                std::env::var(name).is_ok()
                            );
                        }
                    }
                    ProviderKind::Bedrock {
                        region,
                        access_key_env,
                        secret_key_env,
                        ..
                    } => {
                        println!("provider {} bedrock region={}", provider.id, region);
                        println!(
                            "provider {} aws creds present={}",
                            provider.id,
                            std::env::var(access_key_env).is_ok()
                                && std::env::var(secret_key_env).is_ok()
                        );
                    }
                }
            }

            for route in &cfg.routes {
                println!("route {} targets={}", route.name, route.targets.join(","));
            }

            // Egress reachability: TCP-connect to each enabled proxy so a dead
            // path is caught before traffic falls over to it at request time.
            if !cfg.egress.is_empty() {
                println!("egress: master_switch={}", cfg.server.egress_enabled);
            }
            for egress in &cfg.egress {
                match &egress.kind {
                    sb_core::EgressKind::Direct => {
                        println!("egress {} direct enabled={}", egress.id, egress.enabled);
                    }
                    sb_core::EgressKind::Proxy { url, url_env } => {
                        let resolved = url_env
                            .as_deref()
                            .and_then(|name| std::env::var(name).ok())
                            .or_else(|| url.clone());
                        match resolved.as_deref().and_then(proxy_host_port) {
                            None => println!(
                                "egress {} proxy PROBLEM: no reachable url/url_env",
                                egress.id
                            ),
                            Some(host_port) => {
                                let reachable = if egress.enabled {
                                    probe_tcp(&host_port).await
                                } else {
                                    false
                                };
                                println!(
                                    "egress {} proxy enabled={} target={} reachable={}",
                                    egress.id, egress.enabled, host_port, reachable
                                );
                            }
                        }
                    }
                }
            }

            if let Some(catalog) = &cfg.catalog {
                println!(
                    "catalog: {} providers, {} models, {} accounts, {} credentials, {} prices",
                    catalog.providers.len(),
                    catalog.models.len(),
                    catalog.accounts.len(),
                    catalog.credentials.len(),
                    catalog.prices.len()
                );
                let problems = catalog.validate();
                if problems.is_empty() {
                    println!("catalog: referential integrity OK");
                } else {
                    for problem in &problems {
                        println!("catalog PROBLEM: {problem}");
                    }
                }
            }
        }
        Cmd::Config { action, config } => {
            let cfg = Config::from_path(&config)?;
            match action {
                ConfigCmd::Show => {
                    println!("{}", to_pretty(&controlplane::redact_config(&cfg)));
                }
                ConfigCmd::Get { pointer } => {
                    let v = controlplane::redact_config(&cfg);
                    match controlplane::pointer_get(&v, &pointer) {
                        Some(found) => println!("{}", to_pretty(found)),
                        None => {
                            eprintln!("no value at `{pointer}`");
                            std::process::exit(1);
                        }
                    }
                }
                ConfigCmd::Validate => {
                    // Build the same subsystems `serve` would, surfacing any error.
                    let mut problems: Vec<String> = Vec::new();
                    if let Err(e) = sb_adapters::AdapterRegistry::from_config(&cfg) {
                        problems.push(format!("adapters: {e}"));
                    }
                    if let Err(e) = sb_credentials::CredentialResolver::from_config(&cfg) {
                        problems.push(format!("credentials: {e}"));
                    }
                    if let Some(catalog) = &cfg.catalog {
                        problems.extend(catalog.validate().into_iter().map(|p| format!("catalog: {p}")));
                    }
                    if problems.is_empty() {
                        println!("{}", to_pretty(&serde_json::json!({"ok": true})));
                    } else {
                        println!(
                            "{}",
                            to_pretty(&serde_json::json!({"ok": false, "problems": problems}))
                        );
                        std::process::exit(1);
                    }
                }
                ConfigCmd::Providers => {
                    let providers: Vec<serde_json::Value> = cfg
                        .providers
                        .iter()
                        .map(|p| {
                            serde_json::json!({
                                "id": p.id,
                                "type": controlplane::provider_type_name(&p.kind),
                                "egress": p.egress,
                                "accounts": p.accounts.iter().map(|a| a.id.clone()).collect::<Vec<_>>(),
                            })
                        })
                        .collect();
                    println!("{}", to_pretty(&serde_json::json!({ "providers": providers })));
                }
                ConfigCmd::Routes => {
                    let routes: Vec<serde_json::Value> = cfg
                        .routes
                        .iter()
                        .map(|r| serde_json::json!({ "name": r.name, "targets": r.targets }))
                        .collect();
                    println!("{}", to_pretty(&serde_json::json!({ "routes": routes })));
                }
            }
        }
    }

    Ok(())
}

/// Pretty JSON for CLI output (falls back to compact on the impossible error).
fn to_pretty(value: &serde_json::Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

/// The embedded single-page dashboard (no build step, no external assets).
async fn dashboard() -> impl IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
        include_str!("dashboard.html"),
    )
}

/// Auth gate for every endpoint except the public shell (`/`, `/health`). When
/// no `api_key`/`api_keys` is configured the gateway is open (local default);
/// when one is, ALL `/v1/*` and `/cp/v1/*` endpoints — config, providers, traces,
/// usage, control plane — require it, not just the inference path.
async fn require_auth(
    State(state): State<AppState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let path = req.uri().path();
    if path == "/" || path == "/health" {
        return next.run(req).await;
    }
    match tenancy::authenticate(&state, req.headers()) {
        Ok(_) => next.run(req).await,
        Err(resp) => resp,
    }
}

pub fn build_app(state: AppState) -> Router {
    Router::new()
        .route("/", get(dashboard))
        .route("/health", get(health))
        .route("/v1/models", get(models))
        .route("/v1/embeddings", post(embeddings))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/responses", post(responses))
        .route("/v1/messages", post(messages))
        .route("/v1/messages/count_tokens", post(count_tokens))
        .route("/v1/usage", get(usage))
        .route("/v1/traces", get(traces))
        .route("/v1/traces/{id}", get(trace_by_id))
        .route("/v1/config", get(controlplane::config_endpoint))
        .route("/v1/providers", get(controlplane::providers_endpoint))
        .route(
            "/v1/runtime",
            get(controlplane::runtime_get).patch(controlplane::runtime_patch),
        )
        .route("/v1/reload", post(controlplane::reload_endpoint))
        .route("/v1/revisions", get(controlplane::revisions_endpoint))
        .route("/v1/audit", get(controlplane::audit_endpoint))
        .route("/v1/usage/events", get(controlplane::usage_events_endpoint))
        .route("/v1/health", get(controlplane::health_endpoint))
        .route("/v1/tenants", get(controlplane::tenants_endpoint))
        .route("/v1/plugins", get(controlplane::plugins_endpoint))
        // --- /cp/v1 declarative control plane ---
        .route("/cp/v1", get(cp::root))
        .route("/cp/v1/resources/{kind}", get(cp::list_resources))
        .route("/cp/v1/resources/{kind}/{name}", get(cp::get_resource))
        .route("/cp/v1/route-preview", post(cp::route_preview))
        .route("/cp/v1/admission-preview", post(cp::admission_preview))
        .route("/cp/v1/watch", get(cp::watch))
        .route("/cp/v1/drafts", get(cp::list_drafts).post(cp::create_draft))
        .route("/cp/v1/drafts/{id}", get(cp::get_draft))
        .route("/cp/v1/drafts/{id}/validate", post(cp::validate_draft))
        .route("/cp/v1/drafts/{id}/publish", post(cp::publish_draft))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_auth,
        ))
        .with_state(state)
}

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": true }))
}

/// Usage/cost summary from the append-only ledger — requests + attributed cost
/// (micro-USD and USD) by model and provider. The "see every cost" surface that
/// complements the explainable "see every decision" route headers.
async fn usage(State(state): State<AppState>) -> Json<serde_json::Value> {
    let summary = state.ledger.summary();
    Json(serde_json::json!({
        "requests": summary.requests,
        "total_cost_micros": summary.total_cost_micros,
        "total_cost_usd": summary.total_cost_micros as f64 / 1_000_000.0,
        "by_model": summary.by_model,
        "by_provider": summary.by_provider,
        "by_tenant": summary.by_tenant,
    }))
}

#[derive(serde::Deserialize)]
struct TracesQuery {
    limit: Option<usize>,
}

/// Recent request traces, newest first — the "see every request, end to end"
/// surface (route decision + every account/egress attempt + cost). Metadata
/// only; never secrets or message content.
async fn traces(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<TracesQuery>,
) -> Json<serde_json::Value> {
    let recent = state.traces.recent(q.limit.unwrap_or(50).min(1000));
    Json(serde_json::json!({ "count": recent.len(), "traces": recent }))
}

/// One trace by request id.
async fn trace_by_id(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    match state.traces.get(&id) {
        Some(rec) => (StatusCode::OK, Json(rec)).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(openai_error(&format!("no trace `{id}`"), "not_found")),
        )
            .into_response(),
    }
}

async fn models(State(state): State<AppState>) -> Json<serde_json::Value> {
    let snap = state.snapshot();
    let mut seen = HashSet::new();
    let mut ids = Vec::new();

    for route in &snap.config.routes {
        for target in &route.targets {
            if seen.insert(target.clone()) {
                ids.push(target.clone());
            }
        }
    }

    for provider_id in snap.registry.provider_ids() {
        if seen.insert(provider_id.clone()) {
            ids.push(provider_id);
        }
    }

    let data: Vec<serde_json::Value> = ids
        .into_iter()
        .map(|id| serde_json::json!({"id": id, "object": "model", "owned_by": "switchback"}))
        .collect();

    Json(serde_json::json!({"object": "list", "data": data}))
}

fn openai_error(message: &str, type_: &str) -> serde_json::Value {
    serde_json::json!({"error": {"message": message, "type": type_}})
}

/// An SSE error frame, emitted mid-stream so a truncated-by-error response is
/// VISIBLE to the client rather than masquerading as a clean completion.
fn stream_error_frame(message: &str) -> String {
    format!(
        "data: {}\n\n",
        serde_json::json!({"error": {"message": message, "type": "upstream_error"}})
    )
}

fn with_route_header(mut response: Response, summary: &str) -> Response {
    if let Ok(value) = HeaderValue::from_str(summary) {
        response.headers_mut().insert("x-switchback-route", value);
    }
    response
}

/// Stamp the request id on a response so clients can correlate it with the
/// `GET /v1/traces/{id}` record (the trace key == this id).
fn with_request_id(mut response: Response, request_id: &str) -> Response {
    if let Ok(value) = HeaderValue::from_str(request_id) {
        response
            .headers_mut()
            .insert("x-switchback-request-id", value);
    }
    response
}

/// Stamp the compiled-snapshot revision this request was pinned to, so a client
/// can tell which config generation served it (and detect a hot-swap between
/// calls). Pairs with `GET /v1/runtime`'s `revision`.
fn with_revision_header(mut response: Response, revision: u64) -> Response {
    if let Ok(value) = HeaderValue::from_str(&revision.to_string()) {
        response.headers_mut().insert("x-switchback-revision", value);
    }
    response
}

/// Stamp how long the request queued for a global admission slot (only when it
/// actually waited), so backpressure is visible to clients and operators.
fn with_queue_header(mut response: Response, queue_ms: u64) -> Response {
    if queue_ms > 0 {
        if let Ok(value) = HeaderValue::from_str(&queue_ms.to_string()) {
            response.headers_mut().insert("x-switchback-queue-ms", value);
        }
    }
    response
}

/// Render a runtime [`ExecError`] as an HTTP response in the OpenAI error shape
/// (the wire format all three ingress handlers already used for execution
/// errors), re-stamping the route summary when the failure happened after a
/// routing decision was made.
fn render_exec_error(error: &ExecError) -> Response {
    let status =
        StatusCode::from_u16(error.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let response = (
        status,
        Json(openai_error(&error.message, &error.error_type)),
    )
        .into_response();
    match &error.summary {
        Some(summary) => with_route_header(response, summary),
        None => response,
    }
}

/// Extract `host:port` from a proxy URL (`scheme://[user:pass@]host:port[/...]`).
fn proxy_host_port(url: &str) -> Option<String> {
    let after_scheme = url.split("://").nth(1).unwrap_or(url);
    let after_auth = after_scheme.rsplit('@').next()?;
    let host_port = after_auth.split(['/', '?']).next()?;
    (!host_port.is_empty()).then(|| host_port.to_string())
}

/// Best-effort TCP reachability probe with a short timeout (for `doctor`).
async fn probe_tcp(host_port: &str) -> bool {
    matches!(
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            tokio::net::TcpStream::connect(host_port),
        )
        .await,
        Ok(Ok(_))
    )
}

/// Render a canonical event stream as an SSE body in a wire format. `encode`
/// maps each event to frames; `error_frame` surfaces a mid-stream failure
/// (never swallowed — the 9router silent-failure anti-pattern); `done` is the
/// optional terminator (OpenAI sends `data: [DONE]`, Responses sends none).
fn sse_body<F, G>(
    stream: EventStream,
    encode: F,
    error_frame: G,
    done: Option<String>,
) -> axum::body::Body
where
    F: FnMut(&AiStreamEvent) -> Vec<String> + Send + 'static,
    G: Fn(&str) -> String + Send + 'static,
{
    let sse = futures::stream::unfold(
        (
            stream,
            encode,
            error_frame,
            VecDeque::<String>::new(),
            done,
            false,
            false,
        ),
        |(mut stream, mut encode, error_frame, mut pending, done, mut done_sent, mut finished)| async move {
            loop {
                if let Some(frame) = pending.pop_front() {
                    return Some((
                        Ok::<String, Infallible>(frame),
                        (
                            stream,
                            encode,
                            error_frame,
                            pending,
                            done,
                            done_sent,
                            finished,
                        ),
                    ));
                }
                if finished {
                    if !done_sent {
                        done_sent = true;
                        if let Some(frame) = done.clone() {
                            return Some((
                                Ok(frame),
                                (
                                    stream,
                                    encode,
                                    error_frame,
                                    pending,
                                    done,
                                    done_sent,
                                    finished,
                                ),
                            ));
                        }
                    }
                    return None;
                }
                match stream.next().await {
                    Some(Ok(AiStreamEvent::Error { message, .. })) => {
                        pending.push_back(error_frame(&message));
                        finished = true;
                    }
                    Some(Ok(event)) => pending.extend(encode(&event)),
                    Some(Err(error)) => {
                        pending.push_back(error_frame(&error.message));
                        finished = true;
                    }
                    None => finished = true,
                }
            }
        },
    );
    axum::body::Body::from_stream(sse)
}

fn sse_response(body: axum::body::Body, summary: &str) -> Response {
    match Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .body(body)
    {
        Ok(response) => with_route_header(response, summary),
        Err(_) => with_route_header(
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(openai_error(
                    "failed to build stream response",
                    "upstream_error",
                )),
            )
                .into_response(),
            summary,
        ),
    }
}

fn responses_error_frame(message: &str) -> String {
    format!(
        "event: response.failed\ndata: {}\n\n",
        serde_json::json!({"type":"response.failed","response":{"status":"failed","error":{"message":message}}})
    )
}

/// An Anthropic SSE error frame — surfaced mid-stream so a failure is VISIBLE to
/// the client, never masquerading as a clean completion.
fn anthropic_error_frame(message: &str) -> String {
    format!(
        "event: error\ndata: {}\n\n",
        serde_json::json!({"type":"error","error":{"type":"api_error","message":message}})
    )
}

async fn chat_completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let started = Instant::now();
    let principal = match tenancy::authenticate(&state, &headers) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let idem = idempotency::key_from(&headers);
    let idem_fp = idem.as_ref().map(|_| idempotency::fingerprint(&body));
    if let (Some(key), Some(fp)) = (idem.as_deref(), idem_fp.as_deref()) {
        if let Some(resp) = idempotency::precheck(&state, key, fp) {
            return resp;
        }
    }
    let _guard = match idem.as_deref() {
        Some(key) => match state.inflight.try_claim(key) {
            Some(guard) => Some(guard),
            None => return idempotency::in_progress_response(),
        },
        None => None,
    };
    // Global admission (bounded backpressure): wait for an in-flight slot, or 503.
    let (_admit, queue_ms) = match state.admission.acquire().await {
        Ok(slot) => slot,
        Err(resp) => return resp,
    };
    let _conc = match tenancy::admit_concurrency(&state, &principal) {
        Ok(guard) => guard,
        Err(resp) => return resp,
    };
    let mut req = match sb_protocols::openai::request_from_openai_chat(&body) {
        Ok(request) => request,
        Err(message) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(openai_error(&message, "invalid_request_error")),
            )
                .into_response();
        }
    };
    req.tenant = principal.tenant.clone();
    req.project = principal.project.clone();
    let (req_id, req_model) = (req.id.clone(), req.model.clone());
    let trace_id = req.id.clone();
    let (revision, outcome) = state.engine.execute(req, started).await;
    let response = match outcome {
        ExecOutcome::Stream { stream, summary } => {
            let mut encoder = sb_protocols::openai::OpenAiStreamEncoder::new(req_id, req_model);
            let body = sse_body(
                stream,
                // Hold the single-flight + concurrency guards for the stream's life.
                move |event| {
                    let _hold = (&_guard, &_conc, &_admit);
                    encoder.encode(event)
                },
                stream_error_frame,
                Some("data: [DONE]\n\n".to_string()),
            );
            sse_response(body, &summary)
        }
        ExecOutcome::Collected { response, summary } => {
            let value = sb_protocols::openai::response_to_openai_chat(&response);
            if let (Some(key), Some(fp)) = (idem.as_deref(), idem_fp.as_deref()) {
                idempotency::store_json(&state, key, fp, &value);
            }
            with_route_header((StatusCode::OK, Json(value)).into_response(), &summary)
        }
        ExecOutcome::Error(e) => render_exec_error(&e),
    };
    with_queue_header(
        with_revision_header(with_request_id(response, &trace_id), revision),
        queue_ms,
    )
}

async fn responses(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let started = Instant::now();
    let principal = match tenancy::authenticate(&state, &headers) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let idem = idempotency::key_from(&headers);
    let idem_fp = idem.as_ref().map(|_| idempotency::fingerprint(&body));
    if let (Some(key), Some(fp)) = (idem.as_deref(), idem_fp.as_deref()) {
        if let Some(resp) = idempotency::precheck(&state, key, fp) {
            return resp;
        }
    }
    let _guard = match idem.as_deref() {
        Some(key) => match state.inflight.try_claim(key) {
            Some(guard) => Some(guard),
            None => return idempotency::in_progress_response(),
        },
        None => None,
    };
    // Global admission (bounded backpressure): wait for an in-flight slot, or 503.
    let (_admit, queue_ms) = match state.admission.acquire().await {
        Ok(slot) => slot,
        Err(resp) => return resp,
    };
    let _conc = match tenancy::admit_concurrency(&state, &principal) {
        Ok(guard) => guard,
        Err(resp) => return resp,
    };
    let mut req = match sb_protocols::responses::request_from_openai_responses(&body) {
        Ok(request) => request,
        Err(message) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(openai_error(&message, "invalid_request_error")),
            )
                .into_response();
        }
    };
    req.tenant = principal.tenant.clone();
    req.project = principal.project.clone();
    let (req_id, req_model) = (req.id.clone(), req.model.clone());
    let trace_id = req.id.clone();
    let (revision, outcome) = state.engine.execute(req, started).await;
    let response = match outcome {
        ExecOutcome::Stream { stream, summary } => {
            let mut encoder =
                sb_protocols::responses::OpenAiResponsesStreamEncoder::new(req_id, req_model);
            let body = sse_body(
                stream,
                move |event| {
                    let _hold = (&_guard, &_conc, &_admit);
                    encoder.encode(event)
                },
                responses_error_frame,
                None,
            );
            sse_response(body, &summary)
        }
        ExecOutcome::Collected { response, summary } => {
            let value = sb_protocols::responses::response_to_openai_responses(&response);
            if let (Some(key), Some(fp)) = (idem.as_deref(), idem_fp.as_deref()) {
                idempotency::store_json(&state, key, fp, &value);
            }
            with_route_header((StatusCode::OK, Json(value)).into_response(), &summary)
        }
        ExecOutcome::Error(e) => render_exec_error(&e),
    };
    with_queue_header(
        with_revision_header(with_request_id(response, &trace_id), revision),
        queue_ms,
    )
}

/// Anthropic `/v1/messages` ingress: an Anthropic-shaped client (Claude Code,
/// the Anthropic SDK) parsed into the canonical IR, routed across ANY provider
/// by the same `execute_request` core, then rendered back as Anthropic SSE or
/// JSON. This is the "never rewrite client code" promise for Anthropic clients.
async fn messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let started = Instant::now();
    let principal = match tenancy::authenticate(&state, &headers) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let idem = idempotency::key_from(&headers);
    let idem_fp = idem.as_ref().map(|_| idempotency::fingerprint(&body));
    if let (Some(key), Some(fp)) = (idem.as_deref(), idem_fp.as_deref()) {
        if let Some(resp) = idempotency::precheck(&state, key, fp) {
            return resp;
        }
    }
    let _guard = match idem.as_deref() {
        Some(key) => match state.inflight.try_claim(key) {
            Some(guard) => Some(guard),
            None => return idempotency::in_progress_response(),
        },
        None => None,
    };
    // Global admission (bounded backpressure): wait for an in-flight slot, or 503.
    let (_admit, queue_ms) = match state.admission.acquire().await {
        Ok(slot) => slot,
        Err(resp) => return resp,
    };
    let _conc = match tenancy::admit_concurrency(&state, &principal) {
        Ok(guard) => guard,
        Err(resp) => return resp,
    };
    let mut req = match sb_protocols::anthropic::request_from_anthropic(&body) {
        Ok(request) => request,
        Err(message) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(openai_error(&message, "invalid_request_error")),
            )
                .into_response();
        }
    };
    req.tenant = principal.tenant.clone();
    req.project = principal.project.clone();
    let (req_id, req_model) = (req.id.clone(), req.model.clone());
    let trace_id = req.id.clone();
    let (revision, outcome) = state.engine.execute(req, started).await;
    let response = match outcome {
        ExecOutcome::Stream { stream, summary } => {
            let mut encoder =
                sb_protocols::anthropic::AnthropicStreamEncoder::new(req_id, req_model);
            let body = sse_body(
                stream,
                move |event| {
                    let _hold = (&_guard, &_conc, &_admit);
                    encoder.encode(event)
                },
                anthropic_error_frame,
                None,
            );
            sse_response(body, &summary)
        }
        ExecOutcome::Collected { response, summary } => {
            let value = sb_protocols::anthropic::response_to_anthropic(&response);
            if let (Some(key), Some(fp)) = (idem.as_deref(), idem_fp.as_deref()) {
                idempotency::store_json(&state, key, fp, &value);
            }
            with_route_header((StatusCode::OK, Json(value)).into_response(), &summary)
        }
        ExecOutcome::Error(e) => render_exec_error(&e),
    };
    with_queue_header(
        with_revision_header(with_request_id(response, &trace_id), revision),
        queue_ms,
    )
}

/// Anthropic `/v1/messages/count_tokens`. Returns an approximate `input_tokens`
/// (chars/4 heuristic) — the shape Claude Code expects for context budgeting.
async fn count_tokens(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    if let Err(resp) = tenancy::authenticate(&state, &headers) {
        return resp;
    }
    match sb_protocols::anthropic::request_from_anthropic(&body) {
        Ok(req) => {
            let input_tokens = sb_protocols::anthropic::estimate_input_tokens(&req);
            (
                StatusCode::OK,
                Json(serde_json::json!({ "input_tokens": input_tokens })),
            )
                .into_response()
        }
        Err(message) => (
            StatusCode::BAD_REQUEST,
            Json(openai_error(&message, "invalid_request_error")),
        )
            .into_response(),
    }
}

async fn embeddings(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let started = Instant::now();
    if let Err(resp) = tenancy::authenticate(&state, &headers) {
        return resp;
    }
    let snap = state.snapshot();

    let model = match body.get("model").and_then(|m| m.as_str()) {
        Some(model) if !model.is_empty() => model.to_string(),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(openai_error(
                    "missing or invalid \"model\"",
                    "invalid_request_error",
                )),
            )
                .into_response();
        }
    };

    let (route_name, target_strings) = match snap.config.route_for(&model) {
        Some(route) => (route.name.clone(), route.targets.clone()),
        None => {
            if snap.registry.target_for(&model).is_some() {
                ("direct".to_string(), vec![model.clone()])
            } else {
                return (
                    StatusCode::NOT_FOUND,
                    Json(openai_error(
                        &format!("no route or target for model {}", model),
                        "invalid_request_error",
                    )),
                )
                    .into_response();
            }
        }
    };

    let mut candidates = Vec::new();
    let mut unknown = Vec::new();
    for target_id in &target_strings {
        match snap.registry.target_for(target_id) {
            Some(target) => candidates.push(target),
            None => unknown.push(target_id.clone()),
        }
    }

    let summary = format!("route={} embeddings", route_name);
    let mut last_err: Option<AdapterError> = None;

    'targets: for target in candidates.iter() {
        let Some(adapter) = snap.registry.adapter(&target.provider_id) else {
            continue 'targets;
        };

        let mut tried_accounts: HashSet<String> = HashSet::new();

        loop {
            match snap
                .resolver
                .resolve(&target.provider_id, &target.model, &tried_accounts)
            {
                ResolveOutcome::Selected { account_id, lease } => {
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
                            tried_accounts.insert(account_id);
                            last_err = Some(error);
                            continue;
                        }
                    };
                    let mut call_body = body.clone();
                    call_body["model"] = serde_json::Value::String(target.model.clone());

                    match adapter
                        .embeddings(call_body, target.clone(), Some(lease))
                        .await
                    {
                        Ok(value) => {
                            snap
                                .resolver
                                .report_success(&target.provider_id, &account_id);
                            tracing::info!(
                                request_id = %"embeddings",
                                model = %model,
                                target = %target.id,
                                account = %account_id,
                                status = 200u16,
                                latency_ms = started.elapsed().as_millis() as u64,
                                route = %summary
                            );
                            return with_route_header(
                                (StatusCode::OK, Json(value)).into_response(),
                                &summary,
                            );
                        }
                        Err(error) => {
                            snap.resolver.report_failure(
                                &target.provider_id,
                                &account_id,
                                &target.model,
                                error.class,
                            );
                            if error.should_fallback() {
                                tried_accounts.insert(account_id);
                                last_err = Some(error);
                                continue;
                            }

                            tracing::info!(
                                request_id = %"embeddings",
                                model = %model,
                                target = %target.id,
                                account = %account_id,
                                status = error.class.http_status(),
                                latency_ms = started.elapsed().as_millis() as u64,
                                route = %summary
                            );
                            let status = StatusCode::from_u16(error.class.http_status())
                                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                            return with_route_header(
                                (status, Json(openai_error(&error.message, "upstream_error")))
                                    .into_response(),
                                &summary,
                            );
                        }
                    }
                }
                ResolveOutcome::AllUnavailable { .. } => continue 'targets,
                ResolveOutcome::NoAccounts => continue 'targets,
            }
        }
    }

    if let Some(error) = last_err {
        let status = StatusCode::from_u16(error.class.http_status())
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        return with_route_header(
            (status, Json(openai_error(&error.message, "upstream_error"))).into_response(),
            &summary,
        );
    }

    with_route_header(
        (
            StatusCode::BAD_REQUEST,
            Json(openai_error(
                &format!("no eligible target: unknown=[{}]", unknown.join(",")),
                "invalid_request_error",
            )),
        )
            .into_response(),
        &summary,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_error_frame_is_visible_and_well_formed() {
        let frame = stream_error_frame("upstream exploded mid-stream");
        // Must be a proper SSE data frame the client can see (not a silent [DONE]).
        assert!(frame.starts_with("data: "));
        assert!(frame.ends_with("\n\n"));
        let json: serde_json::Value =
            serde_json::from_str(frame.trim_start_matches("data: ").trim()).unwrap();
        assert_eq!(json["error"]["type"], "upstream_error");
        assert_eq!(json["error"]["message"], "upstream exploded mid-stream");
    }
}
