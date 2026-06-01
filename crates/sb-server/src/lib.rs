use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use clap::{Parser, Subcommand};
use sb_core::Config;
use sb_runtime::{Engine, ExecOutcome, Runtime, Snapshot};
use serde::Serialize;

mod admission;
mod app;
mod auth;
mod config_cli;
mod controlplane;
mod cp;
mod doctor_cli;
mod handlers;
mod http_response;
mod idempotency;
mod mcp_cli;
mod provider_cli;
mod provider_preset;
mod schema_cli;
mod sse;
mod tenancy;
mod vault_cli;

use config_cli::{
    config_format_file, config_patch_file, config_set_file, config_unset_file,
    config_validate_json, init_config_file, ConfigCmd,
};
use doctor_cli::{doctor_report, print_doctor_text};
use http_response::{
    openai_error, render_exec_error, sse_response, with_queue_header, with_request_id,
    with_revision_header, with_route_header,
};
use mcp_cli::run_mcp_stdio;
use provider_cli::{
    provider_add_config_file, provider_certify_config_file, provider_doctor_config_file,
    provider_matrix_config_file, provider_models_config_file, provider_sync_routes_config_file,
    provider_test_config_file, ProviderAddRequest, ProviderCmd,
};
use provider_preset::provider_presets_json;
use schema_cli::{schema_json, SchemaCmd};
use vault_cli::{run_vault_cmd, VaultCmd};

pub use app::build_app;

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
    /// Emit machine-readable JSON for commands that otherwise default to text.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a starter local config that works with no provider credentials.
    Init {
        #[arg(long, default_value = "switchback.yaml")]
        config: PathBuf,
        /// Replace the config file if it already exists.
        #[arg(long)]
        force: bool,
    },
    /// Serve the Switchback HTTP gateway.
    Serve {
        #[arg(long, default_value = "config/switchback.example.yaml")]
        config: PathBuf,
        #[arg(long)]
        bind: Option<String>,
    },
    /// Inspect config, provider auth envs, egress reachability, and catalog health.
    Doctor {
        #[arg(long, default_value = "config/switchback.example.yaml")]
        config: PathBuf,
    },
    /// Preview the route decision for a model without starting the server.
    RoutePreview {
        #[arg(long, default_value = "config/switchback.example.yaml")]
        config: PathBuf,
        /// Inbound model/profile/combo to preview.
        #[arg(long)]
        model: String,
        /// Simulate a streaming request.
        #[arg(long)]
        stream: bool,
    },
    /// Print machine-readable command/config/MCP schemas for agents.
    Schema {
        #[command(subcommand)]
        action: SchemaCmd,
    },
    /// Run a minimal stdio MCP server over local Switchback control tools.
    Mcp {
        #[arg(long, default_value = "config/switchback.example.yaml")]
        config: PathBuf,
    },
    /// Add provider config for a supported official/provider-compatible API.
    Provider {
        #[command(subcommand)]
        action: ProviderCmd,
        #[arg(long, global = true, default_value = "switchback.yaml")]
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
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE)
        .with_writer(std::io::stderr);

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
            eprintln!("otel_endpoint is set but this binary was built without the `otel` feature");
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

fn engine_from_config(cfg: Config) -> anyhow::Result<Engine> {
    if let Err(e) = Engine::validate_config(&cfg) {
        anyhow::bail!("config validation failed: {e}");
    }
    let registry =
        sb_adapters::AdapterRegistry::from_config(&cfg).map_err(|e| anyhow::anyhow!(e))?;
    let resolver =
        sb_credentials::CredentialResolver::from_config(&cfg).map_err(|e| anyhow::anyhow!(e))?;
    Ok(Engine::new(
        Arc::new(cfg),
        Arc::new(registry),
        Arc::new(resolver),
        Arc::new(sb_ledger::UsageLedger::in_memory()),
    ))
}

pub(crate) fn route_preview_json(
    path: &Path,
    model: &str,
    stream: bool,
) -> anyhow::Result<serde_json::Value> {
    let cfg = Config::from_path(path)?;
    let engine = engine_from_config(cfg)?;
    let mut req =
        sb_core::AiRequest::new(model.to_string(), vec![sb_core::Message::user("preview")]);
    req.stream = stream;
    let (revision, plan) = engine
        .preview_route(&req)
        .map_err(|e| anyhow::anyhow!(e.message))?;
    Ok(serde_json::json!({
        "revision": revision,
        "decision": plan.decision,
        "candidates": plan.candidates.iter().map(|c| &c.id).collect::<Vec<_>>(),
    }))
}

async fn async_run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let json = cli.json;
    // Pre-load the serve config so tracing init can wire the OTLP exporter from
    // `server.otel_endpoint` before any spans are emitted.
    let serve_cfg = match &cli.cmd {
        Cmd::Serve { config, .. } => Some(Config::from_path(config)?),
        _ => None,
    };
    init_tracing(
        serve_cfg
            .as_ref()
            .and_then(|c| c.server.otel_endpoint.as_deref()),
    );

    match cli.cmd {
        Cmd::Init { config, force } => {
            init_config_file(&config, force)?;
            if json {
                print_json(&serde_json::json!({
                    "ok": true,
                    "config": config,
                    "next": format!("switchback serve --config {}", config.display()),
                }))?;
            } else {
                println!("created {}", config.display());
                println!("next: switchback serve --config {}", config.display());
            }
        }
        Cmd::Serve { bind, config } => {
            let cfg = serve_cfg.expect("serve config pre-loaded above");
            if let Err(e) = Engine::validate_config(&cfg) {
                anyhow::bail!("config validation failed: {e}");
            }
            let registry =
                sb_adapters::AdapterRegistry::from_config(&cfg).map_err(|e| anyhow::anyhow!(e))?;
            let resolver = sb_credentials::CredentialResolver::from_config(&cfg)
                .map_err(|e| anyhow::anyhow!(e))?;
            // Durable control-plane + usage state (opt-in via `server.state_store`).
            // Opened once and shared by the ledger (usage events) and the engine
            // (config revisions + audit). Optional stores degrade to memory on
            // open failure; `required: true` fails startup.
            let store = open_state_store(&cfg)?;
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
        Cmd::Vault { action, config } => run_vault_cmd(action, &config, json)?,
        Cmd::Doctor { config } => {
            let cfg = Config::from_path(&config)?;
            let report = doctor_report(&cfg).await;
            if json {
                print_json(&report)?;
            } else {
                print_doctor_text(&report);
            }
        }
        Cmd::RoutePreview {
            config,
            model,
            stream,
        } => {
            print_json(&route_preview_json(&config, &model, stream)?)?;
        }
        Cmd::Schema { action } => print_json(&schema_json(action))?,
        Cmd::Mcp { config } => {
            run_mcp_stdio(&config)?;
        }
        Cmd::Provider { action, config } => match action {
            ProviderCmd::Presets => {
                print_json(&provider_presets_json())?;
            }
            ProviderCmd::Add {
                preset,
                id,
                base_url,
                api_key_env,
                model,
                route,
                force,
            } => {
                let summary = provider_add_config_file(
                    &config,
                    ProviderAddRequest {
                        preset,
                        id,
                        base_url,
                        api_key_env,
                        model,
                        route,
                        force,
                    },
                )?;
                if json {
                    print_json(&serde_json::json!({
                        "ok": true,
                        "config": config,
                        "provider_id": summary.provider_id,
                        "api_key_env": summary.api_key_env,
                        "route_model": summary.route_model,
                        "target": summary.target,
                    }))?;
                } else {
                    println!(
                        "added provider `{}` to {}",
                        summary.provider_id,
                        config.display()
                    );
                    if let Some(env) = summary.api_key_env.as_deref() {
                        if std::env::var(env).is_err() {
                            println!("set {env} before serve/route-preview");
                        }
                    }
                    if let (Some(route_model), Some(target)) = (summary.route_model, summary.target)
                    {
                        println!("added route `{route_model}` -> `{target}`");
                        match summary.api_key_env.as_deref() {
                            Some(env) if std::env::var(env).is_err() => {}
                            _ => println!(
                                "preview: switchback route-preview --config {} --model {}",
                                config.display(),
                                route_model
                            ),
                        }
                    } else {
                        println!(
                            "next: add a route with --model, or request an explicit provider/model"
                        );
                    }
                }
            }
            ProviderCmd::Test {
                provider,
                model,
                stream,
            } => {
                let summary =
                    provider_test_config_file(&config, &provider, model.as_deref(), stream).await?;
                println!("{}", to_pretty(&serde_json::to_value(summary)?));
            }
            ProviderCmd::Models { provider } => {
                let summary = provider_models_config_file(&config, &provider).await?;
                println!("{}", to_pretty(&serde_json::to_value(summary)?));
            }
            ProviderCmd::SyncRoutes {
                provider,
                prefix,
                force,
            } => {
                let summary =
                    provider_sync_routes_config_file(&config, &provider, prefix.as_deref(), force)
                        .await?;
                println!("{}", to_pretty(&serde_json::to_value(summary)?));
            }
            ProviderCmd::Doctor { provider, model } => {
                let summary =
                    provider_doctor_config_file(&config, &provider, model.as_deref()).await?;
                println!("{}", to_pretty(&serde_json::to_value(summary)?));
            }
            ProviderCmd::Certify { provider, model } => {
                let summary =
                    provider_certify_config_file(&config, &provider, model.as_deref()).await?;
                println!("{}", to_pretty(&serde_json::to_value(summary)?));
            }
            ProviderCmd::Matrix => {
                let summary = provider_matrix_config_file(&config).await?;
                println!("{}", to_pretty(&serde_json::to_value(summary)?));
            }
        },
        Cmd::Config { action, config } => match action {
            ConfigCmd::Show => {
                let cfg = Config::from_path(&config)?;
                println!("{}", to_pretty(&controlplane::redact_config(&cfg)));
            }
            ConfigCmd::Get { pointer } => {
                let cfg = Config::from_path(&config)?;
                let v = controlplane::redact_config(&cfg);
                match controlplane::pointer_get(&v, &pointer) {
                    Some(found) => println!("{}", to_pretty(found)),
                    None => {
                        eprintln!("no value at `{pointer}`");
                        std::process::exit(1);
                    }
                }
            }
            ConfigCmd::Set { pointer, value } => {
                let parsed = config_set_file(&config, &pointer, &value)?;
                print_json(&serde_json::json!({
                    "ok": true,
                    "config": config,
                    "path": pointer,
                    "value": parsed,
                }))?;
            }
            ConfigCmd::Unset { pointer } => {
                let removed = config_unset_file(&config, &pointer)?;
                print_json(&serde_json::json!({
                    "ok": true,
                    "config": config,
                    "path": pointer,
                    "removed": removed,
                }))?;
            }
            ConfigCmd::Patch { from_file } => {
                config_patch_file(&config, &from_file)?;
                print_json(&serde_json::json!({
                    "ok": true,
                    "config": config,
                    "patch": from_file,
                }))?;
            }
            ConfigCmd::Format => {
                config_format_file(&config)?;
                print_json(&serde_json::json!({
                    "ok": true,
                    "config": config,
                }))?;
            }
            ConfigCmd::Validate => {
                let report = config_validate_json(&config)?;
                let ok = report
                    .get("ok")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                println!("{}", to_pretty(&report));
                if !ok {
                    std::process::exit(1);
                }
            }
            ConfigCmd::Providers => {
                let cfg = Config::from_path(&config)?;
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
                println!(
                    "{}",
                    to_pretty(&serde_json::json!({ "providers": providers }))
                );
            }
            ConfigCmd::Routes => {
                let cfg = Config::from_path(&config)?;
                let routes: Vec<serde_json::Value> = cfg
                    .routes
                    .iter()
                    .map(|r| serde_json::json!({ "name": r.name, "targets": r.targets }))
                    .collect();
                let combos: Vec<serde_json::Value> = cfg
                    .combos
                    .iter()
                    .map(|(name, combo)| {
                        serde_json::json!({
                            "name": name,
                            "strategy": combo.strategy.as_str(),
                            "targets": combo.models.clone(),
                        })
                    })
                    .collect();
                println!(
                    "{}",
                    to_pretty(&serde_json::json!({ "routes": routes, "combos": combos }))
                );
            }
        },
    }

    Ok(())
}

/// Pretty JSON for CLI output (falls back to compact on the impossible error).
fn to_pretty(value: &serde_json::Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

fn print_json(value: &impl Serialize) -> anyhow::Result<()> {
    println!("{}", to_pretty(&serde_json::to_value(value)?));
    Ok(())
}

fn open_state_store(config: &Config) -> anyhow::Result<Option<Arc<dyn sb_store::StateStore>>> {
    let Some(state_store) = config.server.state_store.as_ref() else {
        return Ok(None);
    };
    let path = state_store.path();
    match sb_store::SqliteStore::open(path) {
        Ok(store) => {
            tracing::info!(%path, "state store enabled (revisions + audit + usage)");
            Ok(Some(Arc::new(store)))
        }
        Err(error) if state_store.required() => Err(anyhow::anyhow!(
            "state store `{path}` is required but could not be opened: {error}"
        )),
        Err(error) => {
            tracing::warn!(error = %error, %path, "state store disabled: open failed");
            Ok(None)
        }
    }
}

fn session_id_from_headers(headers: &HeaderMap) -> Option<String> {
    [
        "x-switchback-session-id",
        "x-codex-session-id",
        "x-session-id",
    ]
    .iter()
    .find_map(|name| {
        headers
            .get(*name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    })
}

fn attach_session_metadata(req: &mut sb_core::AiRequest, headers: &HeaderMap) {
    if req.metadata.contains_key("session_id") {
        return;
    }
    if let Some(session_id) = session_id_from_headers(headers) {
        req.metadata.insert("session_id".to_string(), session_id);
    }
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
    let idem_scope = idem
        .as_deref()
        .map(|key| idempotency::scoped_key(key, &principal, "/v1/chat/completions"));
    let idem_fp = idem_scope.as_ref().map(|_| idempotency::fingerprint(&body));
    if let (Some(key), Some(fp)) = (idem_scope.as_deref(), idem_fp.as_deref()) {
        if let Some(resp) = idempotency::precheck(&state, key, fp) {
            return resp;
        }
    }
    let _guard = match idem_scope.as_deref() {
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
    attach_session_metadata(&mut req, &headers);
    let (req_id, req_model) = (req.id.clone(), req.model.clone());
    let trace_id = req.id.clone();
    let (revision, outcome) = state.engine.execute(req, started).await;
    let response = match outcome {
        ExecOutcome::Stream { stream, summary } => {
            let mut encoder = sb_protocols::openai::OpenAiStreamEncoder::new(req_id, req_model);
            let body = sse::body(
                stream,
                // Hold the single-flight + concurrency guards for the stream's life.
                move |event| {
                    let _hold = (&_guard, &_conc, &_admit);
                    encoder.encode(event)
                },
                sse::openai_error_frame,
                Some("data: [DONE]\n\n".to_string()),
            );
            sse_response(body, &summary)
        }
        ExecOutcome::Collected { response, summary } => {
            let value = sb_protocols::openai::response_to_openai_chat(&response);
            if let (Some(key), Some(fp)) = (idem_scope.as_deref(), idem_fp.as_deref()) {
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
    let idem_scope = idem
        .as_deref()
        .map(|key| idempotency::scoped_key(key, &principal, "/v1/responses"));
    let idem_fp = idem_scope.as_ref().map(|_| idempotency::fingerprint(&body));
    if let (Some(key), Some(fp)) = (idem_scope.as_deref(), idem_fp.as_deref()) {
        if let Some(resp) = idempotency::precheck(&state, key, fp) {
            return resp;
        }
    }
    let _guard = match idem_scope.as_deref() {
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
    attach_session_metadata(&mut req, &headers);
    let (req_id, req_model) = (req.id.clone(), req.model.clone());
    let trace_id = req.id.clone();
    let (revision, outcome) = state.engine.execute(req, started).await;
    let response = match outcome {
        ExecOutcome::Stream { stream, summary } => {
            let mut encoder =
                sb_protocols::responses::OpenAiResponsesStreamEncoder::new(req_id, req_model);
            let body = sse::body(
                stream,
                move |event| {
                    let _hold = (&_guard, &_conc, &_admit);
                    encoder.encode(event)
                },
                sse::responses_error_frame,
                None,
            );
            sse_response(body, &summary)
        }
        ExecOutcome::Collected { response, summary } => {
            let value = sb_protocols::responses::response_to_openai_responses(&response);
            if let (Some(key), Some(fp)) = (idem_scope.as_deref(), idem_fp.as_deref()) {
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
    let idem_scope = idem
        .as_deref()
        .map(|key| idempotency::scoped_key(key, &principal, "/v1/messages"));
    let idem_fp = idem_scope.as_ref().map(|_| idempotency::fingerprint(&body));
    if let (Some(key), Some(fp)) = (idem_scope.as_deref(), idem_fp.as_deref()) {
        if let Some(resp) = idempotency::precheck(&state, key, fp) {
            return resp;
        }
    }
    let _guard = match idem_scope.as_deref() {
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
    attach_session_metadata(&mut req, &headers);
    let (req_id, req_model) = (req.id.clone(), req.model.clone());
    let trace_id = req.id.clone();
    let (revision, outcome) = state.engine.execute(req, started).await;
    let response = match outcome {
        ExecOutcome::Stream { stream, summary } => {
            let mut encoder =
                sb_protocols::anthropic::AnthropicStreamEncoder::new(req_id, req_model);
            let body = sse::body(
                stream,
                move |event| {
                    let _hold = (&_guard, &_conc, &_admit);
                    encoder.encode(event)
                },
                sse::anthropic_error_frame,
                None,
            );
            sse_response(body, &summary)
        }
        ExecOutcome::Collected { response, summary } => {
            let value = sb_protocols::anthropic::response_to_anthropic(&response);
            if let (Some(key), Some(fp)) = (idem_scope.as_deref(), idem_fp.as_deref()) {
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
    let principal = match tenancy::authenticate(&state, &headers) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let (_admit, queue_ms) = match state.admission.acquire().await {
        Ok(slot) => slot,
        Err(resp) => return resp,
    };
    let _conc = match tenancy::admit_concurrency(&state, &principal) {
        Ok(guard) => guard,
        Err(resp) => return resp,
    };

    let (revision, outcome) = state
        .engine
        .execute_embeddings(
            body,
            principal.tenant,
            principal.project,
            session_id_from_headers(&headers),
            started,
        )
        .await;
    let (response, request_id) = match outcome {
        sb_runtime::EmbeddingsOutcome::Json {
            value,
            summary,
            request_id,
        } => (
            with_route_header((StatusCode::OK, Json(value)).into_response(), &summary),
            request_id,
        ),
        sb_runtime::EmbeddingsOutcome::Error { error, request_id } => {
            (render_exec_error(&error), request_id)
        }
    };
    with_queue_header(
        with_revision_header(with_request_id(response, &request_id), revision),
        queue_ms,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider_cli::provider_mapping;
    use crate::provider_preset::{preset_defaults, ProviderPreset};
    use axum::routing::{get, post};
    use axum::Router;
    use clap::ValueEnum;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_name(prefix: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "{prefix}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn config_with_state_store(state_store: &str) -> Config {
        Config::from_yaml(&format!(
            r#"
server:
  bind: "127.0.0.1:0"
{state_store}
providers:
  - id: mock
    type: mock
routes:
  - name: default
    match:
      model: "*"
    targets:
      - "mock/echo"
"#
        ))
        .unwrap()
    }

    #[test]
    fn starter_config_is_valid() {
        let cfg = Config::from_yaml(config_cli::STARTER_CONFIG).unwrap();
        Engine::validate_config(&cfg).unwrap();
        assert_eq!(cfg.providers[0].id, "mock");
    }

    #[test]
    fn state_store_open_failure_degrades_when_optional() {
        let missing_parent = temp_name("switchback-optional-state-store").join("missing");
        let db_path = missing_parent.join("state.sqlite");
        let cfg = config_with_state_store(&format!("  state_store: \"{}\"", db_path.display()));

        let store = open_state_store(&cfg).unwrap();

        assert!(store.is_none());
        assert!(!missing_parent.exists());
    }

    #[test]
    fn state_store_open_failure_fails_when_required() {
        let missing_parent = temp_name("switchback-required-state-store").join("missing");
        let db_path = missing_parent.join("state.sqlite");
        let cfg = config_with_state_store(&format!(
            "  state_store:\n    path: \"{}\"\n    required: true",
            db_path.display()
        ));

        let error = match open_state_store(&cfg) {
            Ok(_) => panic!("required state store should fail when its path cannot be opened"),
            Err(error) => error.to_string(),
        };

        assert!(error.contains("state store"));
        assert!(error.contains("required"));
        assert!(error.contains("could not be opened"));
        assert!(!missing_parent.exists());
    }

    #[test]
    fn init_config_writes_parent_dirs_and_refuses_overwrite() {
        let root = temp_name("switchback-init-test");
        let path = root.join("nested").join("switchback.yaml");

        init_config_file(&path, false).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("mock/echo"));

        let err = init_config_file(&path, false).unwrap_err().to_string();
        assert!(err.contains("already exists"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), written);

        init_config_file(&path, true).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn provider_add_appends_env_key_provider_and_optional_route() {
        let root = std::env::temp_dir().join(format!(
            "switchback-provider-add-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("switchback.yaml");
        std::fs::write(&path, config_cli::STARTER_CONFIG).unwrap();

        let summary = provider_add_config_file(
            &path,
            ProviderAddRequest {
                preset: ProviderPreset::Openai,
                id: None,
                base_url: None,
                api_key_env: None,
                model: Some("gpt-test".to_string()),
                route: Some("openai/test".to_string()),
                force: false,
            },
        )
        .unwrap();
        assert_eq!(summary.provider_id, "openai");
        assert_eq!(summary.api_key_env.as_deref(), Some("OPENAI_API_KEY"));
        assert_eq!(summary.route_model.as_deref(), Some("openai/test"));
        assert_eq!(summary.target.as_deref(), Some("openai/gpt-test"));

        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("OPENAI_API_KEY"));
        assert!(!written.contains("api_key:"));

        let cfg = Config::from_yaml(&written).unwrap();
        let provider = cfg.providers.iter().find(|p| p.id == "openai").unwrap();
        match &provider.kind {
            sb_core::ProviderKind::OpenaiCompatible {
                base_url,
                api_key_env,
                api_key,
                ..
            } => {
                assert_eq!(base_url, "https://api.openai.com/v1");
                assert_eq!(api_key_env.as_deref(), Some("OPENAI_API_KEY"));
                assert!(api_key.is_none());
            }
            _ => panic!("expected openai-compatible provider"),
        }
        let route = cfg.exact_route_for("openai/test").unwrap();
        assert_eq!(route.targets, vec!["openai/gpt-test"]);

        let err = provider_add_config_file(
            &path,
            ProviderAddRequest {
                preset: ProviderPreset::Openai,
                id: None,
                base_url: None,
                api_key_env: None,
                model: None,
                route: None,
                force: false,
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("already exists"));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn provider_presets_cover_common_official_api_hosts() {
        let expected = [
            (
                "deepseek",
                "deepseek",
                "https://api.deepseek.com",
                "DEEPSEEK_API_KEY",
            ),
            (
                "groq",
                "groq",
                "https://api.groq.com/openai/v1",
                "GROQ_API_KEY",
            ),
            (
                "mistral",
                "mistral",
                "https://api.mistral.ai/v1",
                "MISTRAL_API_KEY",
            ),
            (
                "together",
                "together",
                "https://api.together.ai/v1",
                "TOGETHER_API_KEY",
            ),
            (
                "fireworks",
                "fireworks",
                "https://api.fireworks.ai/inference/v1",
                "FIREWORKS_API_KEY",
            ),
            (
                "cerebras",
                "cerebras",
                "https://api.cerebras.ai/v1",
                "CEREBRAS_API_KEY",
            ),
            ("xai", "xai", "https://api.x.ai/v1", "XAI_API_KEY"),
            (
                "nvidia",
                "nvidia",
                "https://integrate.api.nvidia.com/v1",
                "NVIDIA_API_KEY",
            ),
        ];

        for (cli, id, base_url, env) in expected {
            let preset = ProviderPreset::from_str(cli, true).unwrap();
            let (_default_id, _kind, default_base_url, default_api_key_env) =
                preset_defaults(preset);
            let value = provider_mapping(
                preset,
                id,
                default_base_url.map(ToString::to_string),
                default_api_key_env.map(ToString::to_string),
            );
            let mapping = value.as_mapping().unwrap();
            assert_eq!(config_cli::mapping_str(mapping, "id"), Some(id));
            assert_eq!(config_cli::mapping_str(mapping, "base_url"), Some(base_url));
            assert_eq!(config_cli::mapping_str(mapping, "api_key_env"), Some(env));
        }
    }

    #[test]
    fn provider_add_empty_api_key_env_disables_auth_default() {
        let root = std::env::temp_dir().join(format!(
            "switchback-provider-no-auth-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("switchback.yaml");
        std::fs::write(&path, config_cli::STARTER_CONFIG).unwrap();

        provider_add_config_file(
            &path,
            ProviderAddRequest {
                preset: ProviderPreset::Openai,
                id: Some("local-openai".to_string()),
                base_url: Some(format!("{}://{}:{}/v1", "http", "localhost", 9999)),
                api_key_env: Some(String::new()),
                model: None,
                route: None,
                force: false,
            },
        )
        .unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        let cfg = Config::from_yaml(&written).unwrap();
        let provider = cfg
            .providers
            .iter()
            .find(|p| p.id == "local-openai")
            .unwrap();
        match &provider.kind {
            sb_core::ProviderKind::OpenaiCompatible { api_key_env, .. } => {
                assert!(api_key_env.is_none());
            }
            _ => panic!("expected openai-compatible provider"),
        }
        Engine::validate_config(&cfg).unwrap();

        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn provider_test_executes_the_selected_direct_target() {
        let root = std::env::temp_dir().join(format!(
            "switchback-provider-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("switchback.yaml");
        std::fs::write(
            &path,
            r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: mock
    type: mock
    accounts:
      - id: a
        auth: { kind: api_key, inline: "k" }
  - id: alt
    type: mock
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

        let summary = provider_test_config_file(&path, "alt", Some("echo"), false)
            .await
            .unwrap();

        assert_eq!(summary.target, "alt/echo");
        assert!(!summary.stream);
        assert!(summary.output_chars > 0);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn provider_test_uses_first_discovered_model_when_model_is_omitted() {
        let root = std::env::temp_dir().join(format!(
            "switchback-provider-test-discovery-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("switchback.yaml");
        std::fs::write(
            &path,
            r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: mock
    type: mock
"#,
        )
        .unwrap();

        let summary = provider_test_config_file(&path, "mock", None, false)
            .await
            .unwrap();

        assert_eq!(summary.model, "echo");
        assert_eq!(summary.target, "mock/echo");

        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn provider_doctor_reports_core_provider_checks() {
        let root = std::env::temp_dir().join(format!(
            "switchback-provider-doctor-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("switchback.yaml");
        std::fs::write(
            &path,
            r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: mock
    type: mock
"#,
        )
        .unwrap();

        let summary = provider_doctor_config_file(&path, "mock", None)
            .await
            .unwrap();

        assert!(summary.ok);
        assert_eq!(summary.provider_id, "mock");
        assert_eq!(summary.model, "echo");
        assert_eq!(summary.target, "mock/echo");
        for name in [
            "config",
            "models",
            "route_preview",
            "chat_non_stream",
            "chat_stream",
            "embeddings",
        ] {
            assert!(
                summary
                    .checks
                    .iter()
                    .any(|check| check.name == name && check.status == "ok"),
                "missing ok check {name}: {:?}",
                summary.checks
            );
        }

        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn provider_matrix_skips_missing_env_and_checks_available_providers() {
        let missing_env = format!(
            "SB_MATRIX_MISSING_{}_{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let root = std::env::temp_dir().join(format!(
            "switchback-provider-matrix-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("switchback.yaml");
        std::fs::write(
            &path,
            format!(
                r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: mock
    type: mock
  - id: remote
    type: openai_compatible
    base_url: "https://example.invalid/v1"
    api_key_env: "{missing_env}"
"#
            ),
        )
        .unwrap();

        let summary = provider_matrix_config_file(&path).await.unwrap();

        assert_eq!(summary.schema, "switchback/provider-matrix@1");
        assert!(summary.ok);
        assert_eq!(summary.total, 2);
        assert_eq!(summary.checked, 1);
        assert_eq!(summary.skipped, 1);
        assert_eq!(summary.failed, 0);
        let mock = summary
            .providers
            .iter()
            .find(|provider| provider.provider_id == "mock")
            .unwrap();
        assert_eq!(mock.status, "ok");
        assert_eq!(mock.doctor.as_ref().unwrap().target, "mock/echo");
        let remote = summary
            .providers
            .iter()
            .find(|provider| provider.provider_id == "remote")
            .unwrap();
        assert_eq!(remote.status, "skipped");
        assert_eq!(remote.missing_env, vec![missing_env]);
        assert!(remote.doctor.is_none());

        std::fs::remove_dir_all(root).unwrap();
    }

    async fn fake_openai_chat_without_models(Json(body): Json<serde_json::Value>) -> Response {
        let model = body
            .get("model")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("missing-model");
        if body
            .get("stream")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            let sse = format!(
                "data: {{\"id\":\"chatcmpl-hint\",\"object\":\"chat.completion.chunk\",\"choices\":[{{\"index\":0,\"delta\":{{\"role\":\"assistant\"}},\"finish_reason\":null}}]}}\n\n\
data: {{\"id\":\"chatcmpl-hint\",\"object\":\"chat.completion.chunk\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"model={model}\"}},\"finish_reason\":null}}]}}\n\n\
data: {{\"id\":\"chatcmpl-hint\",\"object\":\"chat.completion.chunk\",\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}],\"usage\":{{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}}}\n\n\
data: [DONE]\n\n"
            );
            return ([("content-type", "text/event-stream")], sse).into_response();
        }

        Json(serde_json::json!({
            "id": "chatcmpl-hint",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "finish_reason": "stop",
                "message": {
                    "role": "assistant",
                    "content": format!("model={model}")
                }
            }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
        }))
        .into_response()
    }

    async fn spawn_fake_openai_without_models() -> String {
        let app = Router::new().route("/chat/completions", post(fake_openai_chat_without_models));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn provider_doctor_uses_model_hint_when_models_endpoint_is_unavailable() {
        let upstream = spawn_fake_openai_without_models().await;
        let root = std::env::temp_dir().join(format!(
            "switchback-provider-hint-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("switchback.yaml");
        std::fs::write(
            &path,
            format!(
                r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: hinted
    type: openai_compatible
    base_url: "{upstream}"
    model_hint: "hint-model"
"#
            ),
        )
        .unwrap();

        let summary = provider_doctor_config_file(&path, "hinted", None)
            .await
            .unwrap();

        assert!(summary.ok);
        assert_eq!(summary.model, "hint-model");
        assert_eq!(summary.target, "hinted/hint-model");
        assert!(
            summary
                .checks
                .iter()
                .any(|check| check.name == "model_hint" && check.status == "ok"),
            "missing model hint check: {:?}",
            summary.checks
        );
        assert!(
            summary
                .checks
                .iter()
                .any(|check| check.name == "models" && !check.required),
            "model discovery should be optional when a hint is configured: {:?}",
            summary.checks
        );

        let test_summary = provider_test_config_file(&path, "hinted", None, false)
            .await
            .unwrap();
        assert_eq!(test_summary.model, "hint-model");
        assert_eq!(test_summary.target, "hinted/hint-model");

        let matrix = provider_matrix_config_file(&path).await.unwrap();
        assert_eq!(matrix.checked, 1);
        assert_eq!(matrix.failed, 0);
        assert_eq!(matrix.providers[0].status, "ok");
        assert_eq!(
            matrix.providers[0].doctor.as_ref().unwrap().model,
            "hint-model"
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    async fn fake_openai_models(headers: HeaderMap) -> Json<serde_json::Value> {
        let auth = headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("absent");
        Json(serde_json::json!({
            "object": "list",
            "data": [
                {
                    "id": "model-a",
                    "object": "model",
                    "owned_by": auth
                },
                {
                    "id": "owner/model-b",
                    "object": "model",
                    "owned_by": "test"
                }
            ]
        }))
    }

    async fn spawn_fake_openai_models() -> String {
        let app = Router::new().route("/models", get(fake_openai_models));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn provider_models_lists_upstream_models_with_switchback_ids() {
        let upstream = spawn_fake_openai_models().await;
        let root = std::env::temp_dir().join(format!(
            "switchback-provider-models-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("switchback.yaml");
        std::fs::write(
            &path,
            format!(
                r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: upstream
    type: openai_compatible
    base_url: "{upstream}"
    accounts:
      - id: a
        auth: {{ kind: api_key, inline: "secret-xyz" }}
"#
            ),
        )
        .unwrap();

        let summary = provider_models_config_file(&path, "upstream")
            .await
            .unwrap();

        assert_eq!(summary.provider_id, "upstream");
        assert_eq!(summary.models.len(), 2);
        assert_eq!(summary.models[0].id, "model-a");
        assert_eq!(summary.models[0].switchback_model, "upstream/model-a");
        assert_eq!(summary.models[1].id, "owner/model-b");
        assert_eq!(summary.models[1].switchback_model, "upstream/owner/model-b");

        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn provider_sync_routes_imports_discovered_models() {
        let upstream = spawn_fake_openai_models().await;
        let root = std::env::temp_dir().join(format!(
            "switchback-provider-sync-routes-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("switchback.yaml");
        std::fs::write(
            &path,
            format!(
                r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: upstream
    type: openai_compatible
    base_url: "{upstream}"
    accounts:
      - id: a
        auth: {{ kind: api_key, inline: "secret-xyz" }}
routes:
  - name: existing
    match: {{ model: "upstream/model-a" }}
    targets:
      - "upstream/old-model"
"#
            ),
        )
        .unwrap();

        let skipped = provider_sync_routes_config_file(&path, "upstream", None, false)
            .await
            .unwrap();
        assert_eq!(skipped.added, 1);
        assert_eq!(skipped.skipped, 1);
        assert_eq!(skipped.replaced, 0);

        let cfg = Config::from_path(&path).unwrap();
        assert_eq!(
            cfg.exact_route_for("upstream/model-a").unwrap().targets,
            vec!["upstream/old-model"]
        );
        assert_eq!(
            cfg.exact_route_for("upstream/owner/model-b")
                .unwrap()
                .targets,
            vec!["upstream/owner/model-b"]
        );

        let forced = provider_sync_routes_config_file(&path, "upstream", Some("local"), true)
            .await
            .unwrap();
        assert_eq!(forced.added, 2);
        assert_eq!(forced.skipped, 0);
        assert_eq!(forced.replaced, 0);

        let cfg = Config::from_path(&path).unwrap();
        assert_eq!(
            cfg.exact_route_for("local/model-a").unwrap().targets,
            vec!["upstream/model-a"]
        );
        assert_eq!(
            cfg.exact_route_for("local/owner/model-b").unwrap().targets,
            vec!["upstream/owner/model-b"]
        );

        std::fs::remove_dir_all(root).unwrap();
    }
}
