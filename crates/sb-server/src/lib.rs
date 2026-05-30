use std::collections::{BTreeMap, HashSet, VecDeque};
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::State;
use axum::http::{header::AUTHORIZATION, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use clap::{Parser, Subcommand};
use futures::StreamExt;
use sb_adapter::{AdapterError, EventStream, PreparedRequest};
use sb_core::{
    AiRequest, AiResponse, AiStreamEvent, Config, ContentPart, ErrorClass, FinishReason, Message,
    ProviderKind, Role, RouteRequire, Usage,
};
use sb_credentials::ResolveOutcome;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub registry: Arc<sb_adapters::AdapterRegistry>,
    pub resolver: Arc<sb_credentials::CredentialResolver>,
    pub ledger: Arc<sb_ledger::UsageLedger>,
    pub traces: Arc<sb_trace::TraceLog>,
}

impl AppState {
    /// Build state with the core dependencies; the trace log defaults to an
    /// in-memory ring. Use this over a struct literal so adding observability
    /// fields (here, and the egress pool later) doesn't churn every call site.
    pub fn new(
        config: Arc<Config>,
        registry: Arc<sb_adapters::AdapterRegistry>,
        resolver: Arc<sb_credentials::CredentialResolver>,
        ledger: Arc<sb_ledger::UsageLedger>,
    ) -> Self {
        AppState {
            config,
            registry,
            resolver,
            ledger,
            traces: Arc::new(sb_trace::TraceLog::default()),
        }
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

async fn async_run() -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    match Cli::parse().cmd {
        Cmd::Serve { config, bind } => {
            let cfg = Config::from_path(&config)?;
            let registry =
                sb_adapters::AdapterRegistry::from_config(&cfg).map_err(|e| anyhow::anyhow!(e))?;
            let resolver = sb_credentials::CredentialResolver::from_config(&cfg)
                .map_err(|e| anyhow::anyhow!(e))?;
            let ledger = match &cfg.server.usage_log {
                Some(path) => sb_ledger::UsageLedger::with_sink(path),
                None => sb_ledger::UsageLedger::in_memory(),
            };
            let ring = cfg.server.trace_ring_size;
            let traces = match &cfg.server.trace_log {
                Some(path) => sb_trace::TraceLog::with_sink(ring, path),
                None => sb_trace::TraceLog::in_memory(ring),
            };
            let bind = bind.unwrap_or_else(|| cfg.server.bind.clone());
            let state = AppState {
                config: Arc::new(cfg),
                registry: Arc::new(registry),
                resolver: Arc::new(resolver),
                ledger: Arc::new(ledger),
                traces: Arc::new(traces),
            };
            let app = build_app(state);
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
                }
            }

            for route in &cfg.routes {
                println!("route {} targets={}", route.name, route.targets.join(","));
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
    }

    Ok(())
}

pub fn build_app(state: AppState) -> Router {
    Router::new()
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
    let mut seen = HashSet::new();
    let mut ids = Vec::new();

    for route in &state.config.routes {
        for target in &route.targets {
            if seen.insert(target.clone()) {
                ids.push(target.clone());
            }
        }
    }

    for provider_id in state.registry.provider_ids() {
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

async fn collect_response(
    mut stream: EventStream,
    req_id: String,
    model: String,
) -> Result<AiResponse, AdapterError> {
    let mut content = String::new();
    let mut tool_uses: BTreeMap<u32, (String, String, String)> = BTreeMap::new();
    let mut finish_reason = None;
    let mut usage = Usage::default();

    while let Some(item) = stream.next().await {
        match item? {
            AiStreamEvent::TextDelta { text } => content.push_str(&text),
            AiStreamEvent::ToolCallStart(start) => {
                tool_uses.insert(start.index, (start.id, start.name, String::new()));
            }
            AiStreamEvent::ToolCallArgsDelta { index, json } => {
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

/// Inbound API-key gate, shared by chat/responses.
fn check_api_key(state: &AppState, headers: &HeaderMap) -> Option<Response> {
    let expected = state.config.server.api_key.as_deref()?;
    let expected = format!("Bearer {expected}");
    let authorized = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .map(|value| value == expected)
        .unwrap_or(false);
    if authorized {
        None
    } else {
        Some(
            (
                StatusCode::UNAUTHORIZED,
                Json(openai_error(
                    "missing or invalid api key",
                    "invalid_request_error",
                )),
            )
                .into_response(),
        )
    }
}

fn error_response(error: &AdapterError, summary: &str) -> Response {
    let status = StatusCode::from_u16(error.class.http_status())
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    with_route_header(
        (status, Json(openai_error(&error.message, "upstream_error"))).into_response(),
        summary,
    )
}

/// Committed result of the shared execution core: a live stream (client wants
/// streaming), a collected response (non-streaming), or an error Response.
enum ExecOutcome {
    Stream {
        stream: EventStream,
        summary: String,
    },
    Collected {
        response: AiResponse,
        summary: String,
    },
    Error(Response),
}

/// Append a usage/cost record for a completed (non-streamed) request.
#[allow(clippy::too_many_arguments)]
fn record_usage(
    state: &AppState,
    request_id: &str,
    provider_id: &str,
    model: &str,
    account_id: &str,
    usage: Usage,
    started: Instant,
    streamed: bool,
) {
    let empty = sb_core::Catalog::default();
    let catalog = state.config.catalog.as_ref().unwrap_or(&empty);
    state.ledger.record(sb_ledger::UsageRecord::new(
        request_id,
        provider_id,
        model,
        Some(account_id.to_string()),
        usage,
        started.elapsed().as_millis() as u64,
        streamed,
        catalog,
    ));
}

/// Wrap a streamed response so the final usage is recorded when the stream
/// completes (every adapter emits a terminal `UsageDelta`). If the client
/// disconnects early the stream is dropped before completion and nothing is
/// recorded — correct, there was no final usage.
fn meter_stream<F>(stream: EventStream, on_complete: F) -> EventStream
where
    F: FnOnce(Usage) + Send + 'static,
{
    let init = (stream, Usage::default(), Some(on_complete));
    futures::stream::unfold(
        init,
        |(mut stream, mut usage, mut on_complete)| async move {
            match stream.next().await {
                Some(item) => {
                    if let Ok(AiStreamEvent::UsageDelta { usage: latest }) = &item {
                        usage = latest.clone();
                    }
                    Some((item, (stream, usage, on_complete)))
                }
                None => {
                    if let Some(callback) = on_complete.take() {
                        callback(usage);
                    }
                    None
                }
            }
        },
    )
    .boxed()
}

/// The shared execution core — route resolution + two-level (target × account)
/// fallback. Format-agnostic: `/v1/chat/completions` and `/v1/responses` both
/// call this, then render the committed result in their own wire format. (One
/// loop, not two — the 9router duplication trap avoided.)
async fn execute_request(state: &AppState, mut req: AiRequest, started: Instant) -> ExecOutcome {
    // RTK-style tool-result compression (opt-in): shrink bulky tool outputs in
    // the prompt before dispatch. Fail-safe (never-grow/never-empty), so the
    // worst case is a no-op. Metadata-only log, never the content.
    if state.config.server.compress_tool_results {
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

    // Resolve the request's model to candidate targets. Precedence: a matching
    // route → an explicit `provider/model` → the default pass-through provider
    // (forwarding the model verbatim) → 404. The default-provider path is what
    // makes adding a model a runtime/data concern, not a code change.
    let (route_name, require, candidates, unknown): (
        String,
        RouteRequire,
        Vec<sb_core::ExecutionTarget>,
        Vec<String>,
    ) = if let Some(route) = state.config.route_for(&req.model) {
        let mut candidates = Vec::new();
        let mut unknown = Vec::new();
        for target_id in &route.targets {
            match state.registry.target_for(target_id) {
                Some(target) => candidates.push(target),
                None => unknown.push(target_id.clone()),
            }
        }
        (route.name.clone(), route.require.clone(), candidates, unknown)
    } else if let Some(target) = state.registry.target_for(&req.model) {
        ("direct".to_string(), RouteRequire::default(), vec![target], Vec::new())
    } else if let Some(provider) = state.config.server.default_provider.as_deref() {
        match state.registry.target_for_provider_model(provider, &req.model) {
            Some(target) => (
                format!("default:{provider}"),
                RouteRequire::default(),
                vec![target],
                Vec::new(),
            ),
            None => {
                return ExecOutcome::Error(
                    (
                        StatusCode::NOT_FOUND,
                        Json(openai_error(
                            &format!("default_provider `{provider}` is not a configured provider"),
                            "invalid_request_error",
                        )),
                    )
                        .into_response(),
                );
            }
        }
    } else {
        return ExecOutcome::Error(
            (
                StatusCode::NOT_FOUND,
                Json(openai_error(
                    &format!(
                        "no route or target for model `{}` — add a route, use `provider/model`, or set server.default_provider",
                        req.model
                    ),
                    "invalid_request_error",
                )),
            )
                .into_response(),
        );
    };

    let plan = sb_router::plan_route(&req, &route_name, &require, &candidates);
    let summary = plan.decision.summary();
    let mut last_err: Option<AdapterError> = None;

    // One trace per request: the route decision + every attempt + outcome + cost.
    // Metadata only (sb-trace upholds the no-secrets invariant). `egress` stays
    // "direct" until the egress layer lands (phase 3).
    let mut trace = sb_trace::RequestTrace::start(
        req.id.clone(),
        req.model.clone(),
        route_name.clone(),
        plan.decision.clone(),
    );

    'targets: for target in plan.candidates.iter() {
        let Some(adapter) = state.registry.adapter(&target.provider_id) else {
            continue 'targets;
        };

        let mut tried_accounts: HashSet<String> = HashSet::new();

        loop {
            match state
                .resolver
                .resolve(&target.provider_id, &target.model, &tried_accounts)
            {
                ResolveOutcome::Selected { account_id, lease } => {
                    let attempt_started = Instant::now();
                    // Upgrade an OAuth account's lease to a freshly-refreshed
                    // token (no-op for api-key accounts). A refresh failure is
                    // an auth failure on this account → fall over like any other.
                    let lease = match state
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
                            state.resolver.report_failure(
                                &target.provider_id,
                                &account_id,
                                &target.model,
                                error.class,
                            );
                            trace.attempt(sb_trace::Attempt::failed(
                                &target.id, &target.provider_id, &target.model,
                                &account_id, "direct",
                                attempt_started.elapsed().as_millis() as u64,
                                error.class.as_str(), true,
                            ));
                            tried_accounts.insert(account_id);
                            last_err = Some(error);
                            continue;
                        }
                    };
                    let prepared = PreparedRequest::new(req.clone(), target.clone(), Some(lease));

                    match adapter.execute(prepared).await {
                        Ok(stream) => {
                            state
                                .resolver
                                .report_success(&target.provider_id, &account_id);

                            if req.stream {
                                tracing::info!(
                                    request_id = %req.id, model = %req.model, target = %target.id,
                                    account = %account_id, status = 200u16,
                                    latency_ms = started.elapsed().as_millis() as u64, route = %summary
                                );
                                trace.attempt(sb_trace::Attempt::success(
                                    &target.id, &target.provider_id, &target.model,
                                    &account_id, "direct",
                                    attempt_started.elapsed().as_millis() as u64,
                                ));
                                // Meter the stream: record usage/cost AND finalize
                                // the trace when it completes (the terminal
                                // UsageDelta is known only after the client drains
                                // the stream). One callback does both.
                                let ledger = state.ledger.clone();
                                let traces = state.traces.clone();
                                let catalog = state.config.catalog.clone().unwrap_or_default();
                                let (rid, pid, mdl, acct) = (
                                    req.id.clone(),
                                    target.provider_id.clone(),
                                    target.model.clone(),
                                    account_id.clone(),
                                );
                                let metered = meter_stream(stream, move |usage| {
                                    let latency = started.elapsed().as_millis() as u64;
                                    let cost = sb_ledger::compute_cost_micros(&catalog, &mdl, &usage);
                                    ledger.record(sb_ledger::UsageRecord::new(
                                        rid,
                                        pid,
                                        mdl,
                                        Some(acct),
                                        usage.clone(),
                                        latency,
                                        true,
                                        &catalog,
                                    ));
                                    trace.set_usage(usage, cost);
                                    traces.record(trace.finish(200, latency, true));
                                });
                                return ExecOutcome::Stream {
                                    stream: metered,
                                    summary,
                                };
                            }

                            match collect_response(stream, req.id.clone(), req.model.clone()).await
                            {
                                Ok(response) => {
                                    tracing::info!(
                                        request_id = %req.id, model = %req.model, target = %target.id,
                                        account = %account_id, status = 200u16,
                                        latency_ms = started.elapsed().as_millis() as u64, route = %summary
                                    );
                                    record_usage(
                                        state,
                                        &req.id,
                                        &target.provider_id,
                                        &target.model,
                                        &account_id,
                                        response.usage.clone(),
                                        started,
                                        false,
                                    );
                                    trace.attempt(sb_trace::Attempt::success(
                                        &target.id, &target.provider_id, &target.model,
                                        &account_id, "direct",
                                        attempt_started.elapsed().as_millis() as u64,
                                    ));
                                    let cost = trace_cost(state, &target.model, &response.usage);
                                    trace.set_usage(response.usage.clone(), cost);
                                    state.traces.record(trace.finish(
                                        200,
                                        started.elapsed().as_millis() as u64,
                                        false,
                                    ));
                                    return ExecOutcome::Collected { response, summary };
                                }
                                Err(error) => {
                                    state.resolver.report_failure(
                                        &target.provider_id,
                                        &account_id,
                                        &target.model,
                                        error.class,
                                    );
                                    let fell_over = error.should_fallback();
                                    trace.attempt(sb_trace::Attempt::failed(
                                        &target.id, &target.provider_id, &target.model,
                                        &account_id, "direct",
                                        attempt_started.elapsed().as_millis() as u64,
                                        error.class.as_str(), fell_over,
                                    ));
                                    if fell_over {
                                        tried_accounts.insert(account_id);
                                        last_err = Some(error);
                                        continue;
                                    }
                                    state.traces.record(trace.finish(
                                        error.class.http_status(),
                                        started.elapsed().as_millis() as u64,
                                        false,
                                    ));
                                    return ExecOutcome::Error(error_response(&error, &summary));
                                }
                            }
                        }
                        Err(error) => {
                            state.resolver.report_failure(
                                &target.provider_id,
                                &account_id,
                                &target.model,
                                error.class,
                            );
                            let fell_over = error.should_fallback();
                            trace.attempt(sb_trace::Attempt::failed(
                                &target.id, &target.provider_id, &target.model,
                                &account_id, "direct",
                                attempt_started.elapsed().as_millis() as u64,
                                error.class.as_str(), fell_over,
                            ));
                            if fell_over {
                                tried_accounts.insert(account_id);
                                last_err = Some(error);
                                continue;
                            }
                            state.traces.record(trace.finish(
                                error.class.http_status(),
                                started.elapsed().as_millis() as u64,
                                false,
                            ));
                            return ExecOutcome::Error(error_response(&error, &summary));
                        }
                    }
                }
                ResolveOutcome::AllUnavailable { .. } => continue 'targets,
                ResolveOutcome::NoAccounts => continue 'targets,
            }
        }
    }

    if let Some(error) = last_err {
        state.traces.record(trace.finish(
            error.class.http_status(),
            started.elapsed().as_millis() as u64,
            false,
        ));
        return ExecOutcome::Error(error_response(&error, &summary));
    }

    let rejected = plan
        .decision
        .rejected
        .iter()
        .map(|rejected| format!("{}:{}", rejected.target_id, rejected.reason))
        .collect::<Vec<_>>()
        .join(",");
    state
        .traces
        .record(trace.finish(400, started.elapsed().as_millis() as u64, false));
    ExecOutcome::Error(with_route_header(
        (
            StatusCode::BAD_REQUEST,
            Json(openai_error(
                &format!(
                    "no eligible target: rejected={} unknown=[{}]",
                    rejected,
                    unknown.join(",")
                ),
                "invalid_request_error",
            )),
        )
            .into_response(),
        &summary,
    ))
}

/// Attributed cost (micro-USD) of `usage` for `model` at the catalog's current
/// prices — the same computation the usage ledger uses, for the trace record.
fn trace_cost(state: &AppState, model: &str, usage: &Usage) -> u64 {
    let empty = sb_core::Catalog::default();
    let catalog = state.config.catalog.as_ref().unwrap_or(&empty);
    sb_ledger::compute_cost_micros(catalog, model, usage)
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
    if let Some(resp) = check_api_key(&state, &headers) {
        return resp;
    }
    let req = match sb_protocols::openai::request_from_openai_chat(&body) {
        Ok(request) => request,
        Err(message) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(openai_error(&message, "invalid_request_error")),
            )
                .into_response();
        }
    };
    let (req_id, req_model) = (req.id.clone(), req.model.clone());
    let trace_id = req.id.clone();
    let response = match execute_request(&state, req, started).await {
        ExecOutcome::Stream { stream, summary } => {
            let mut encoder = sb_protocols::openai::OpenAiStreamEncoder::new(req_id, req_model);
            let body = sse_body(
                stream,
                move |event| encoder.encode(event),
                stream_error_frame,
                Some("data: [DONE]\n\n".to_string()),
            );
            sse_response(body, &summary)
        }
        ExecOutcome::Collected { response, summary } => with_route_header(
            (
                StatusCode::OK,
                Json(sb_protocols::openai::response_to_openai_chat(&response)),
            )
                .into_response(),
            &summary,
        ),
        ExecOutcome::Error(resp) => resp,
    };
    with_request_id(response, &trace_id)
}

async fn responses(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let started = Instant::now();
    if let Some(resp) = check_api_key(&state, &headers) {
        return resp;
    }
    let req = match sb_protocols::responses::request_from_openai_responses(&body) {
        Ok(request) => request,
        Err(message) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(openai_error(&message, "invalid_request_error")),
            )
                .into_response();
        }
    };
    let (req_id, req_model) = (req.id.clone(), req.model.clone());
    let trace_id = req.id.clone();
    let response = match execute_request(&state, req, started).await {
        ExecOutcome::Stream { stream, summary } => {
            let mut encoder =
                sb_protocols::responses::OpenAiResponsesStreamEncoder::new(req_id, req_model);
            let body = sse_body(
                stream,
                move |event| encoder.encode(event),
                responses_error_frame,
                None,
            );
            sse_response(body, &summary)
        }
        ExecOutcome::Collected { response, summary } => with_route_header(
            (
                StatusCode::OK,
                Json(sb_protocols::responses::response_to_openai_responses(
                    &response,
                )),
            )
                .into_response(),
            &summary,
        ),
        ExecOutcome::Error(resp) => resp,
    };
    with_request_id(response, &trace_id)
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
    if let Some(resp) = check_api_key(&state, &headers) {
        return resp;
    }
    let req = match sb_protocols::anthropic::request_from_anthropic(&body) {
        Ok(request) => request,
        Err(message) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(openai_error(&message, "invalid_request_error")),
            )
                .into_response();
        }
    };
    let (req_id, req_model) = (req.id.clone(), req.model.clone());
    let trace_id = req.id.clone();
    let response = match execute_request(&state, req, started).await {
        ExecOutcome::Stream { stream, summary } => {
            let mut encoder =
                sb_protocols::anthropic::AnthropicStreamEncoder::new(req_id, req_model);
            let body = sse_body(
                stream,
                move |event| encoder.encode(event),
                anthropic_error_frame,
                None,
            );
            sse_response(body, &summary)
        }
        ExecOutcome::Collected { response, summary } => with_route_header(
            (
                StatusCode::OK,
                Json(sb_protocols::anthropic::response_to_anthropic(&response)),
            )
                .into_response(),
            &summary,
        ),
        ExecOutcome::Error(resp) => resp,
    };
    with_request_id(response, &trace_id)
}

/// Anthropic `/v1/messages/count_tokens`. Returns an approximate `input_tokens`
/// (chars/4 heuristic) — the shape Claude Code expects for context budgeting.
async fn count_tokens(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    if let Some(resp) = check_api_key(&state, &headers) {
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

    if let Some(expected) = state.config.server.api_key.as_deref() {
        let expected = format!("Bearer {expected}");
        let authorized = headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .map(|value| value == expected)
            .unwrap_or(false);

        if !authorized {
            return (
                StatusCode::UNAUTHORIZED,
                Json(openai_error(
                    "missing or invalid api key",
                    "invalid_request_error",
                )),
            )
                .into_response();
        }
    }

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

    let (route_name, target_strings) = match state.config.route_for(&model) {
        Some(route) => (route.name.clone(), route.targets.clone()),
        None => {
            if state.registry.target_for(&model).is_some() {
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
        match state.registry.target_for(target_id) {
            Some(target) => candidates.push(target),
            None => unknown.push(target_id.clone()),
        }
    }

    let summary = format!("route={} embeddings", route_name);
    let mut last_err: Option<AdapterError> = None;

    'targets: for target in candidates.iter() {
        let Some(adapter) = state.registry.adapter(&target.provider_id) else {
            continue 'targets;
        };

        let mut tried_accounts: HashSet<String> = HashSet::new();

        loop {
            match state
                .resolver
                .resolve(&target.provider_id, &target.model, &tried_accounts)
            {
                ResolveOutcome::Selected { account_id, lease } => {
                    let lease = match state
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
                            state.resolver.report_failure(
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
                            state
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
                            state.resolver.report_failure(
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
