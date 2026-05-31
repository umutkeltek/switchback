use std::collections::{HashSet, VecDeque};
use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use clap::{Parser, Subcommand, ValueEnum};
use futures::StreamExt;
use sb_adapter::EventStream;
use sb_core::{AiStreamEvent, Config, ExecutionProfile, FinishReason, ProviderKind, Usage};
use sb_runtime::{EmbeddingsOutcome, Engine, ExecError, ExecOutcome, Runtime, Snapshot};
use serde::Serialize;

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
    /// Create a starter local config that works with no provider credentials.
    Init {
        #[arg(long, default_value = "switchback.yaml")]
        config: PathBuf,
        /// Replace the config file if it already exists.
        #[arg(long)]
        force: bool,
    },
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
    /// List routes and combo profiles (name + targets).
    Routes,
}

#[derive(Subcommand)]
enum ProviderCmd {
    /// Append or replace a provider entry. Secrets are referenced by env var only.
    Add {
        preset: ProviderPreset,
        /// Override the provider id written to config.
        #[arg(long)]
        id: Option<String>,
        /// Override the upstream base URL.
        #[arg(long)]
        base_url: Option<String>,
        /// Override the API-key env var name. Empty value is treated as no auth.
        #[arg(long)]
        api_key_env: Option<String>,
        /// Optional upstream model id to add as an exact route target.
        #[arg(long)]
        model: Option<String>,
        /// Optional inbound route/alias for --model. Defaults to provider/model.
        #[arg(long)]
        route: Option<String>,
        /// Replace an existing provider or exact route with the same id/alias.
        #[arg(long)]
        force: bool,
    },
    /// Execute a tiny request against one configured provider/model.
    Test {
        provider: String,
        /// Upstream model id to test. Defaults to the first discoverable model.
        #[arg(long)]
        model: Option<String>,
        /// Exercise the provider's streaming path.
        #[arg(long)]
        stream: bool,
    },
    /// List upstream models visible to one configured provider/account.
    Models { provider: String },
    /// Discover upstream models and add exact provider/model routes.
    SyncRoutes {
        provider: String,
        /// Optional local route prefix. Defaults to the provider id.
        #[arg(long)]
        prefix: Option<String>,
        /// Replace existing routes with the same local model id.
        #[arg(long)]
        force: bool,
    },
    /// Run model discovery, route preview, chat, stream, and embeddings checks.
    Doctor {
        provider: String,
        /// Upstream model id to test. Defaults to the first discoverable model.
        #[arg(long)]
        model: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum ProviderPreset {
    Openai,
    Openrouter,
    Anthropic,
    Gemini,
    Ollama,
    Deepseek,
    Groq,
    Mistral,
    Together,
    Fireworks,
    Cerebras,
    Xai,
    Nvidia,
    Vllm,
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

const STARTER_CONFIG: &str = include_str!("../../../config/quickstart.yaml");

fn init_config_file(path: &Path, force: bool) -> anyhow::Result<()> {
    let cfg = Config::from_yaml(STARTER_CONFIG)?;
    if let Err(e) = Engine::validate_config(&cfg) {
        anyhow::bail!("bundled starter config is invalid: {e}");
    }
    if path.exists() && !force {
        anyhow::bail!(
            "{} already exists; pass --force to replace it",
            path.display()
        );
    }
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, STARTER_CONFIG)?;
    Ok(())
}

#[derive(Debug)]
struct ProviderAddSummary {
    provider_id: String,
    api_key_env: Option<String>,
    route_model: Option<String>,
    target: Option<String>,
}

struct ProviderAddRequest {
    preset: ProviderPreset,
    id: Option<String>,
    base_url: Option<String>,
    api_key_env: Option<String>,
    model: Option<String>,
    route: Option<String>,
    force: bool,
}

#[derive(Debug, Serialize)]
struct ProviderTestSummary {
    ok: bool,
    revision: u64,
    provider_id: String,
    model: String,
    target: String,
    stream: bool,
    summary: String,
    output_chars: usize,
    event_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    finish_reason: Option<FinishReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    usage: Option<Usage>,
}

#[derive(Debug, Serialize)]
struct ProviderModelSummary {
    id: String,
    switchback_model: String,
}

#[derive(Debug, Serialize)]
struct ProviderModelsSummary {
    ok: bool,
    revision: u64,
    provider_id: String,
    models: Vec<ProviderModelSummary>,
}

#[derive(Debug, Serialize)]
struct ProviderSyncRoutesSummary {
    ok: bool,
    provider_id: String,
    prefix: String,
    discovered: usize,
    added: usize,
    skipped: usize,
    replaced: usize,
}

#[derive(Debug, Serialize)]
struct ProviderDoctorCheck {
    name: String,
    ok: bool,
    required: bool,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

#[derive(Debug, Serialize)]
struct ProviderDoctorSummary {
    ok: bool,
    revision: u64,
    provider_id: String,
    model: String,
    target: String,
    checks: Vec<ProviderDoctorCheck>,
}

fn preset_defaults(
    preset: ProviderPreset,
) -> (
    &'static str,
    &'static str,
    Option<&'static str>,
    Option<&'static str>,
) {
    match preset {
        ProviderPreset::Openai => (
            "openai",
            "openai_compatible",
            Some("https://api.openai.com/v1"),
            Some("OPENAI_API_KEY"),
        ),
        ProviderPreset::Openrouter => (
            "openrouter",
            "openai_compatible",
            Some("https://openrouter.ai/api/v1"),
            Some("OPENROUTER_API_KEY"),
        ),
        ProviderPreset::Anthropic => ("anthropic", "anthropic", None, Some("ANTHROPIC_API_KEY")),
        ProviderPreset::Gemini => ("gemini", "gemini", None, Some("GEMINI_API_KEY")),
        ProviderPreset::Ollama => ("ollama", "openai_compatible", None, None),
        ProviderPreset::Deepseek => (
            "deepseek",
            "openai_compatible",
            Some("https://api.deepseek.com"),
            Some("DEEPSEEK_API_KEY"),
        ),
        ProviderPreset::Groq => (
            "groq",
            "openai_compatible",
            Some("https://api.groq.com/openai/v1"),
            Some("GROQ_API_KEY"),
        ),
        ProviderPreset::Mistral => (
            "mistral",
            "openai_compatible",
            Some("https://api.mistral.ai/v1"),
            Some("MISTRAL_API_KEY"),
        ),
        ProviderPreset::Together => (
            "together",
            "openai_compatible",
            Some("https://api.together.ai/v1"),
            Some("TOGETHER_API_KEY"),
        ),
        ProviderPreset::Fireworks => (
            "fireworks",
            "openai_compatible",
            Some("https://api.fireworks.ai/inference/v1"),
            Some("FIREWORKS_API_KEY"),
        ),
        ProviderPreset::Cerebras => (
            "cerebras",
            "openai_compatible",
            Some("https://api.cerebras.ai/v1"),
            Some("CEREBRAS_API_KEY"),
        ),
        ProviderPreset::Xai => (
            "xai",
            "openai_compatible",
            Some("https://api.x.ai/v1"),
            Some("XAI_API_KEY"),
        ),
        ProviderPreset::Nvidia => (
            "nvidia",
            "openai_compatible",
            Some("https://integrate.api.nvidia.com/v1"),
            Some("NVIDIA_API_KEY"),
        ),
        ProviderPreset::Vllm => ("vllm", "openai_compatible", None, None),
    }
}

fn yaml_key(key: &str) -> serde_yaml::Value {
    serde_yaml::Value::String(key.to_string())
}

fn yaml_string(value: impl Into<String>) -> serde_yaml::Value {
    serde_yaml::Value::String(value.into())
}

fn mapping_str<'a>(mapping: &'a serde_yaml::Mapping, key: &str) -> Option<&'a str> {
    mapping
        .get(yaml_key(key))
        .and_then(serde_yaml::Value::as_str)
}

fn clean_optional(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn provider_mapping(
    preset: ProviderPreset,
    id: &str,
    base_url: Option<String>,
    api_key_env: Option<String>,
) -> serde_yaml::Value {
    let (_default_id, kind, _default_base_url, _default_api_key_env) = preset_defaults(preset);
    let mut provider = serde_yaml::Mapping::new();
    provider.insert(yaml_key("id"), yaml_string(id));
    provider.insert(yaml_key("type"), yaml_string(kind));
    if let Some(base_url) = base_url {
        provider.insert(yaml_key("base_url"), yaml_string(base_url));
    }
    if let Some(api_key_env) = api_key_env {
        provider.insert(yaml_key("api_key_env"), yaml_string(api_key_env));
    }
    serde_yaml::Value::Mapping(provider)
}

fn exact_route_mapping(route_model: &str, target: &str) -> serde_yaml::Value {
    let mut match_mapping = serde_yaml::Mapping::new();
    match_mapping.insert(yaml_key("model"), yaml_string(route_model));

    let mut route = serde_yaml::Mapping::new();
    route.insert(yaml_key("name"), yaml_string(route_model));
    route.insert(yaml_key("match"), serde_yaml::Value::Mapping(match_mapping));
    route.insert(
        yaml_key("targets"),
        serde_yaml::Value::Sequence(vec![yaml_string(target)]),
    );
    serde_yaml::Value::Mapping(route)
}

fn ensure_sequence<'a>(
    root: &'a mut serde_yaml::Mapping,
    key: &str,
) -> anyhow::Result<&'a mut Vec<serde_yaml::Value>> {
    let yaml_key = yaml_key(key);
    if !root.contains_key(&yaml_key) {
        root.insert(yaml_key.clone(), serde_yaml::Value::Sequence(Vec::new()));
    }
    root.get_mut(&yaml_key)
        .and_then(serde_yaml::Value::as_sequence_mut)
        .ok_or_else(|| anyhow::anyhow!("top-level `{key}` must be a YAML sequence"))
}

fn provider_add_config_file(
    path: &Path,
    request: ProviderAddRequest,
) -> anyhow::Result<ProviderAddSummary> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("read {}: {e}", path.display()))?;
    let mut value: serde_yaml::Value = serde_yaml::from_str(&text)
        .map_err(|e| anyhow::anyhow!("parse {} as YAML: {e}", path.display()))?;
    let root = value
        .as_mapping_mut()
        .ok_or_else(|| anyhow::anyhow!("{} must contain a YAML mapping", path.display()))?;

    let (default_id, _kind, default_base_url, default_api_key_env) =
        preset_defaults(request.preset);
    let provider_id = clean_optional(request.id).unwrap_or_else(|| default_id.to_string());
    let base_url = clean_optional(request.base_url)
        .or_else(|| default_base_url.map(ToString::to_string))
        .or_else(|| {
            (request.preset == ProviderPreset::Ollama)
                .then(|| format!("{}://{}:{}/v1", "http", "localhost", 11434))
        })
        .or_else(|| {
            (request.preset == ProviderPreset::Vllm)
                .then(|| format!("{}://{}:{}/v1", "http", "localhost", 8000))
        });
    let api_key_env = match request.api_key_env {
        Some(value) => clean_optional(Some(value)),
        None => default_api_key_env.map(ToString::to_string),
    };
    let provider = provider_mapping(request.preset, &provider_id, base_url, api_key_env.clone());
    let providers = ensure_sequence(root, "providers")?;
    match providers.iter().position(|entry| {
        entry
            .as_mapping()
            .and_then(|mapping| mapping_str(mapping, "id"))
            == Some(provider_id.as_str())
    }) {
        Some(index) if request.force => providers[index] = provider,
        Some(_) => {
            anyhow::bail!(
                "provider `{provider_id}` already exists in {}; pass --force to replace it",
                path.display()
            );
        }
        None => providers.push(provider),
    }

    let model = clean_optional(request.model);
    let mut route_model = None;
    let mut target = None;
    if let Some(model) = model {
        let target_id = format!("{provider_id}/{model}");
        let inbound = clean_optional(request.route).unwrap_or_else(|| target_id.clone());
        let routes = ensure_sequence(root, "routes")?;
        let route_entry = exact_route_mapping(&inbound, &target_id);
        match routes.iter().position(|entry| {
            entry
                .as_mapping()
                .and_then(|mapping| mapping.get(yaml_key("match")))
                .and_then(serde_yaml::Value::as_mapping)
                .and_then(|mapping| mapping_str(mapping, "model"))
                == Some(inbound.as_str())
        }) {
            Some(index) if request.force => routes[index] = route_entry,
            Some(_) => {
                anyhow::bail!(
                    "route `{inbound}` already exists in {}; pass --force to replace it",
                    path.display()
                );
            }
            None => routes.push(route_entry),
        }
        route_model = Some(inbound);
        target = Some(target_id);
    }

    let rendered = serde_yaml::to_string(&value)?;
    let cfg = Config::from_yaml(&rendered)?;
    let problems = cfg.semantic_problems();
    if !problems.is_empty() {
        anyhow::bail!("config would be invalid: {}", problems.join("; "));
    }
    std::fs::write(path, rendered)?;
    Ok(ProviderAddSummary {
        provider_id,
        api_key_env,
        route_model,
        target,
    })
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

async fn provider_test_config_file(
    path: &Path,
    provider_id: &str,
    model: Option<&str>,
    stream: bool,
) -> anyhow::Result<ProviderTestSummary> {
    let resolved_model = match model.map(str::trim).filter(|value| !value.is_empty()) {
        Some(model) => model.to_string(),
        None => {
            let discovered = provider_models_config_file(path, provider_id).await?;
            discovered
                .models
                .first()
                .map(|model| model.id.clone())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "provider `{provider_id}` did not report any models; pass --model"
                    )
                })?
        }
    };
    let cfg = Config::from_path(path)?;
    let engine = engine_from_config(cfg)?;
    let target_model = format!("{provider_id}/{resolved_model}");
    let mut req = sb_core::AiRequest::new(
        target_model.clone(),
        vec![sb_core::Message::user(
            "Switchback provider test. Reply briefly.",
        )],
    );
    req.max_output_tokens = Some(32);
    req.temperature = Some(0.0);
    req.stream = stream;

    let (_preview_revision, plan) = engine
        .preview_route(&req)
        .map_err(|e| anyhow::anyhow!(e.message))?;
    let selected = plan
        .decision
        .selected
        .as_ref()
        .map(|target| target.target_id.clone())
        .ok_or_else(|| anyhow::anyhow!("no selected target for `{target_model}`"))?;
    if selected != target_model {
        anyhow::bail!(
            "provider test selected `{selected}`, not requested `{target_model}`; check routes"
        );
    }

    let (revision, outcome) = engine.execute(req, Instant::now()).await;
    match outcome {
        ExecOutcome::Collected { response, summary } => Ok(ProviderTestSummary {
            ok: true,
            revision,
            provider_id: provider_id.to_string(),
            model: resolved_model.clone(),
            target: selected,
            stream: false,
            summary,
            output_chars: response.message.text().chars().count(),
            event_count: 0,
            response_id: Some(response.id),
            finish_reason: Some(response.finish_reason),
            usage: Some(response.usage),
        }),
        ExecOutcome::Stream {
            mut stream,
            summary,
        } => {
            let mut event_count = 0usize;
            let mut output_chars = 0usize;
            let mut response_id = None;
            let mut finish_reason = None;
            let mut usage = None;
            while let Some(item) = stream.next().await {
                let event = item.map_err(|e| anyhow::anyhow!(e.message))?;
                event_count += 1;
                match event {
                    AiStreamEvent::MessageStart { id, .. } => {
                        response_id.get_or_insert(id);
                    }
                    AiStreamEvent::TextDelta { text } | AiStreamEvent::ReasoningDelta { text } => {
                        output_chars += text.chars().count();
                    }
                    AiStreamEvent::UsageDelta { usage: u } => usage = Some(u),
                    AiStreamEvent::MessageEnd { finish_reason: f } => finish_reason = Some(f),
                    AiStreamEvent::Error { message, .. } => anyhow::bail!(message),
                    AiStreamEvent::ToolCallStart(_)
                    | AiStreamEvent::ToolCallArgsDelta { .. }
                    | AiStreamEvent::ToolCallEnd { .. } => {}
                }
            }
            Ok(ProviderTestSummary {
                ok: true,
                revision,
                provider_id: provider_id.to_string(),
                model: resolved_model,
                target: selected,
                stream: true,
                summary,
                output_chars,
                event_count,
                response_id,
                finish_reason,
                usage,
            })
        }
        ExecOutcome::Error(e) => Err(anyhow::anyhow!(e.message)),
    }
}

async fn provider_models_config_file(
    path: &Path,
    provider_id: &str,
) -> anyhow::Result<ProviderModelsSummary> {
    let cfg = Config::from_path(path)?;
    let engine = engine_from_config(cfg)?;
    let snap = engine.snapshot();
    let adapter = snap
        .registry
        .adapter(provider_id)
        .ok_or_else(|| anyhow::anyhow!("provider `{provider_id}` is not configured"))?;

    let (account_id, lease) = match snap.resolver.resolve(provider_id, "", &HashSet::new()) {
        sb_credentials::ResolveOutcome::Selected { account_id, lease } => (account_id, lease),
        sb_credentials::ResolveOutcome::AllUnavailable { retry_after } => {
            let suffix = retry_after
                .map(|duration| format!("; retry after {}ms", duration.as_millis()))
                .unwrap_or_default();
            anyhow::bail!("provider `{provider_id}` has no available accounts{suffix}");
        }
        sb_credentials::ResolveOutcome::NoAccounts => {
            anyhow::bail!("provider `{provider_id}` has no accounts");
        }
    };
    let lease = snap
        .resolver
        .fresh_lease(provider_id, &account_id, lease)
        .await
        .map_err(|e| anyhow::anyhow!("refresh credential for `{provider_id}`: {e}"))?;
    let upstream_models = adapter
        .list_models(Some(lease), None)
        .await
        .map_err(|e| anyhow::anyhow!(e.message))?;

    let mut seen = HashSet::new();
    let models = upstream_models
        .into_iter()
        .filter(|id| seen.insert(id.clone()))
        .map(|id| ProviderModelSummary {
            switchback_model: format!("{provider_id}/{id}"),
            id,
        })
        .collect();

    Ok(ProviderModelsSummary {
        ok: true,
        revision: snap.revision,
        provider_id: provider_id.to_string(),
        models,
    })
}

async fn provider_sync_routes_config_file(
    path: &Path,
    provider_id: &str,
    prefix: Option<&str>,
    force: bool,
) -> anyhow::Result<ProviderSyncRoutesSummary> {
    let discovered = provider_models_config_file(path, provider_id).await?;
    let prefix = prefix
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(provider_id)
        .trim_end_matches('/')
        .to_string();
    let text = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("read {}: {e}", path.display()))?;
    let mut value: serde_yaml::Value = serde_yaml::from_str(&text)
        .map_err(|e| anyhow::anyhow!("parse {} as YAML: {e}", path.display()))?;
    let root = value
        .as_mapping_mut()
        .ok_or_else(|| anyhow::anyhow!("{} must contain a YAML mapping", path.display()))?;
    let routes = ensure_sequence(root, "routes")?;

    let mut added = 0usize;
    let mut skipped = 0usize;
    let mut replaced = 0usize;
    for model in &discovered.models {
        let route_model = format!("{prefix}/{}", model.id);
        let route_entry = exact_route_mapping(&route_model, &model.switchback_model);
        match routes.iter().position(|entry| {
            entry
                .as_mapping()
                .and_then(|mapping| mapping.get(yaml_key("match")))
                .and_then(serde_yaml::Value::as_mapping)
                .and_then(|mapping| mapping_str(mapping, "model"))
                == Some(route_model.as_str())
        }) {
            Some(index) if force => {
                routes[index] = route_entry;
                replaced += 1;
            }
            Some(_) => skipped += 1,
            None => {
                routes.push(route_entry);
                added += 1;
            }
        }
    }

    let rendered = serde_yaml::to_string(&value)?;
    let cfg = Config::from_yaml(&rendered)?;
    let problems = cfg.semantic_problems();
    if !problems.is_empty() {
        anyhow::bail!("config would be invalid: {}", problems.join("; "));
    }
    std::fs::write(path, rendered)?;

    Ok(ProviderSyncRoutesSummary {
        ok: true,
        provider_id: provider_id.to_string(),
        prefix,
        discovered: discovered.models.len(),
        added,
        skipped,
        replaced,
    })
}

fn provider_doctor_check(
    name: &str,
    ok: bool,
    required: bool,
    status: &str,
    detail: Option<String>,
) -> ProviderDoctorCheck {
    ProviderDoctorCheck {
        name: name.to_string(),
        ok,
        required,
        status: status.to_string(),
        detail,
    }
}

fn provider_doctor_ok(
    name: &str,
    required: bool,
    detail: impl Into<Option<String>>,
) -> ProviderDoctorCheck {
    provider_doctor_check(name, true, required, "ok", detail.into())
}

fn provider_doctor_failed(
    name: &str,
    required: bool,
    detail: impl Into<String>,
) -> ProviderDoctorCheck {
    provider_doctor_check(name, false, required, "failed", Some(detail.into()))
}

fn provider_doctor_unsupported(name: &str, detail: impl Into<String>) -> ProviderDoctorCheck {
    provider_doctor_check(name, false, false, "unsupported", Some(detail.into()))
}

async fn provider_doctor_embeddings_check(
    engine: &Engine,
    target_model: &str,
) -> ProviderDoctorCheck {
    let body = serde_json::json!({
        "model": target_model,
        "input": "Switchback provider doctor"
    });
    let (_revision, outcome) = engine
        .execute_embeddings(body, None, None, None, Instant::now())
        .await;
    match outcome {
        EmbeddingsOutcome::Json { value, summary, .. } => {
            let rows = value
                .get("data")
                .and_then(serde_json::Value::as_array)
                .map(Vec::len)
                .unwrap_or_default();
            provider_doctor_ok(
                "embeddings",
                false,
                Some(format!("{summary}; embeddings={rows}")),
            )
        }
        EmbeddingsOutcome::Error { error, .. }
            if error.status == 422
                || error
                    .message
                    .to_ascii_lowercase()
                    .contains("embeddings not supported") =>
        {
            provider_doctor_unsupported("embeddings", error.message)
        }
        EmbeddingsOutcome::Error { error, .. } => provider_doctor_failed(
            "embeddings",
            false,
            format!("{}: {}", error.error_type, error.message),
        ),
    }
}

async fn provider_doctor_config_file(
    path: &Path,
    provider_id: &str,
    model: Option<&str>,
) -> anyhow::Result<ProviderDoctorSummary> {
    let cfg = Config::from_path(path)?;
    let engine = engine_from_config(cfg)?;
    let revision = engine.revision();
    let mut checks = Vec::new();
    checks.push(provider_doctor_ok(
        "config",
        true,
        Some(format!("revision {revision}")),
    ));

    let explicit_model = model
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string);
    let models_required = explicit_model.is_none();
    let discovered = provider_models_config_file(path, provider_id).await;
    let resolved_model = match (&explicit_model, &discovered) {
        (Some(model), Ok(summary)) => {
            checks.push(provider_doctor_ok(
                "models",
                false,
                Some(format!("{} model(s) discoverable", summary.models.len())),
            ));
            model.clone()
        }
        (Some(model), Err(e)) => {
            checks.push(provider_doctor_failed("models", false, e.to_string()));
            model.clone()
        }
        (None, Ok(summary)) => {
            checks.push(provider_doctor_ok(
                "models",
                models_required,
                Some(format!("{} model(s) discoverable", summary.models.len())),
            ));
            summary
                .models
                .first()
                .map(|model| model.id.clone())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "provider `{provider_id}` did not report any models; pass --model"
                    )
                })?
        }
        (None, Err(e)) => {
            checks.push(provider_doctor_failed(
                "models",
                models_required,
                e.to_string(),
            ));
            anyhow::bail!("model discovery failed for `{provider_id}`; pass --model: {e}");
        }
    };
    let target_model = format!("{provider_id}/{resolved_model}");

    let mut req = sb_core::AiRequest::new(
        target_model.clone(),
        vec![sb_core::Message::user("Switchback provider doctor")],
    );
    req.max_output_tokens = Some(32);
    req.temperature = Some(0.0);
    match engine.preview_route(&req) {
        Ok((_preview_revision, plan)) => {
            let selected = plan
                .decision
                .selected
                .as_ref()
                .map(|target| target.target_id.as_str());
            if selected == Some(target_model.as_str()) {
                checks.push(provider_doctor_ok(
                    "route_preview",
                    true,
                    Some(plan.decision.summary()),
                ));
            } else {
                checks.push(provider_doctor_failed(
                    "route_preview",
                    true,
                    format!(
                        "selected `{}`, expected `{target_model}`",
                        selected.unwrap_or("<none>")
                    ),
                ));
            }
        }
        Err(e) => checks.push(provider_doctor_failed("route_preview", true, e.message)),
    }

    match provider_test_config_file(path, provider_id, Some(&resolved_model), false).await {
        Ok(summary) => checks.push(provider_doctor_ok(
            "chat_non_stream",
            true,
            Some(format!(
                "{}; output_chars={}",
                summary.summary, summary.output_chars
            )),
        )),
        Err(e) => checks.push(provider_doctor_failed(
            "chat_non_stream",
            true,
            e.to_string(),
        )),
    }

    match provider_test_config_file(path, provider_id, Some(&resolved_model), true).await {
        Ok(summary) => checks.push(provider_doctor_ok(
            "chat_stream",
            true,
            Some(format!(
                "{}; events={}; output_chars={}",
                summary.summary, summary.event_count, summary.output_chars
            )),
        )),
        Err(e) => checks.push(provider_doctor_failed("chat_stream", true, e.to_string())),
    }

    checks.push(provider_doctor_embeddings_check(&engine, &target_model).await);
    let ok = checks
        .iter()
        .filter(|check| check.required)
        .all(|check| check.ok);

    Ok(ProviderDoctorSummary {
        ok,
        revision,
        provider_id: provider_id.to_string(),
        model: resolved_model,
        target: target_model,
        checks,
    })
}

async fn async_run() -> anyhow::Result<()> {
    let cli = Cli::parse();
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
            println!("created {}", config.display());
            println!("next: switchback serve --config {}", config.display());
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
            // (config revisions + audit). A failed open disables persistence rather
            // than refusing to start — the gateway still serves from memory.
            let store: Option<Arc<dyn sb_store::StateStore>> = match cfg
                .server
                .state_store
                .as_deref()
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
        Cmd::RoutePreview {
            config,
            model,
            stream,
        } => {
            let cfg = Config::from_path(&config)?;
            let engine = engine_from_config(cfg)?;
            let mut req = sb_core::AiRequest::new(model, vec![sb_core::Message::user("preview")]);
            req.stream = stream;
            let (revision, plan) = engine
                .preview_route(&req)
                .map_err(|e| anyhow::anyhow!(e.message))?;
            println!(
                "{}",
                to_pretty(&serde_json::json!({
                    "revision": revision,
                    "decision": plan.decision,
                    "candidates": plan.candidates.iter().map(|c| &c.id).collect::<Vec<_>>(),
                }))
            );
        }
        Cmd::Provider { action, config } => match action {
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
                if let (Some(route_model), Some(target)) = (summary.route_model, summary.target) {
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
        },
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
                    // Use the same semantic + compile validation as draft publish.
                    if let Err(e) = sb_runtime::Engine::validate_config(&cfg) {
                        let problems: Vec<&str> = e.split("; ").collect();
                        println!(
                            "{}",
                            to_pretty(&serde_json::json!({"ok": false, "problems": problems}))
                        );
                        std::process::exit(1);
                    } else {
                        println!("{}", to_pretty(&serde_json::json!({"ok": true})));
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
                    println!(
                        "{}",
                        to_pretty(&serde_json::json!({ "providers": providers }))
                    );
                }
                ConfigCmd::Routes => {
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
        .route("/cp/v1/runtime-state", get(cp::runtime_state))
        .route(
            "/cp/v1/runtime-state/reset-lockout",
            post(cp::reset_lockout),
        )
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
    let ids = model_ids_for_snapshot(&snap);

    let data: Vec<serde_json::Value> = ids
        .into_iter()
        .map(|id| serde_json::json!({"id": id, "object": "model", "owned_by": "switchback"}))
        .collect();

    Json(serde_json::json!({"object": "list", "data": data}))
}

fn push_model_id(ids: &mut Vec<String>, seen: &mut HashSet<String>, id: impl Into<String>) {
    let id = id.into();
    if seen.insert(id.clone()) {
        ids.push(id);
    }
}

fn model_ids_for_snapshot(snap: &Snapshot) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut ids = Vec::new();

    if snap.config.wildcard_route().is_some() {
        for profile in [
            ExecutionProfile::Auto,
            ExecutionProfile::Cheap,
            ExecutionProfile::Fast,
            ExecutionProfile::Coding,
            ExecutionProfile::Private,
            ExecutionProfile::LargeContext,
        ] {
            push_model_id(&mut ids, &mut seen, profile.id());
        }
    }

    for route in &snap.config.routes {
        if let Some(model) = route.match_.model.as_deref().filter(|model| *model != "*") {
            push_model_id(&mut ids, &mut seen, model);
        }
        for target in &route.targets {
            push_model_id(&mut ids, &mut seen, target.clone());
        }
    }

    for name in snap.config.combos.keys() {
        push_model_id(&mut ids, &mut seen, name.clone());
    }

    if let Some(catalog) = &snap.config.catalog {
        for model in &catalog.models {
            push_model_id(
                &mut ids,
                &mut seen,
                format!("{}/{}", model.provider_id, model.id),
            );
        }
    }

    for provider_id in snap.registry.provider_ids() {
        push_model_id(&mut ids, &mut seen, provider_id);
    }

    ids
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
        response
            .headers_mut()
            .insert("x-switchback-revision", value);
    }
    response
}

/// Stamp how long the request queued for a global admission slot (only when it
/// actually waited), so backpressure is visible to clients and operators.
fn with_queue_header(mut response: Response, queue_ms: u64) -> Response {
    if queue_ms > 0 {
        if let Ok(value) = HeaderValue::from_str(&queue_ms.to_string()) {
            response
                .headers_mut()
                .insert("x-switchback-queue-ms", value);
        }
    }
    response
}

/// Render a runtime [`ExecError`] as an HTTP response in the OpenAI error shape
/// (the wire format all three ingress handlers already used for execution
/// errors), re-stamping the route summary when the failure happened after a
/// routing decision was made.
fn render_exec_error(error: &ExecError) -> Response {
    let status = StatusCode::from_u16(error.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
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
    use std::time::{SystemTime, UNIX_EPOCH};

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

    #[test]
    fn starter_config_is_valid() {
        let cfg = Config::from_yaml(STARTER_CONFIG).unwrap();
        Engine::validate_config(&cfg).unwrap();
        assert_eq!(cfg.providers[0].id, "mock");
    }

    #[test]
    fn init_config_writes_parent_dirs_and_refuses_overwrite() {
        let root = std::env::temp_dir().join(format!(
            "switchback-init-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
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
        std::fs::write(&path, STARTER_CONFIG).unwrap();

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
            ProviderKind::OpenaiCompatible {
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
            assert_eq!(mapping_str(mapping, "id"), Some(id));
            assert_eq!(mapping_str(mapping, "base_url"), Some(base_url));
            assert_eq!(mapping_str(mapping, "api_key_env"), Some(env));
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
        std::fs::write(&path, STARTER_CONFIG).unwrap();

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
            ProviderKind::OpenaiCompatible { api_key_env, .. } => {
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
