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
    AiResponse, AiStreamEvent, Config, ContentPart, CredentialLease, FinishReason, Message,
    ProviderKind, Role, RouteRequire, Usage,
};

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub registry: Arc<sb_adapters::AdapterRegistry>,
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
            let registry = sb_adapters::AdapterRegistry::from_config(&cfg)
                .map_err(|e| anyhow::anyhow!(e))?;
            let bind = bind.unwrap_or_else(|| cfg.server.bind.clone());
            let state = AppState {
                config: Arc::new(cfg),
                registry: Arc::new(registry),
            };
            let app = build_app(state);
            let listener = tokio::net::TcpListener::bind(&bind).await?;
            tracing::info!(%bind, "switchback listening");
            axum::serve(listener, app).await?;
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
                }
            }

            for route in &cfg.routes {
                println!("route {} targets={}", route.name, route.targets.join(","));
            }
        }
    }

    Ok(())
}

pub fn build_app(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(chat_completions))
        .with_state(state)
}

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": true }))
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
        response
            .headers_mut()
            .insert("x-switchback-route", value);
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
            AiStreamEvent::MessageEnd { finish_reason: finish } => {
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

async fn chat_completions(
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
                Json(openai_error("missing or invalid api key", "invalid_request_error")),
            )
                .into_response();
        }
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

    let (route_name, require, target_strings) = match state.config.route_for(&req.model) {
        Some(route) => (
            route.name.clone(),
            route.require.clone(),
            route.targets.clone(),
        ),
        None => {
            if state.registry.target_for(&req.model).is_some() {
                (
                    "direct".to_string(),
                    RouteRequire::default(),
                    vec![req.model.clone()],
                )
            } else {
                return (
                    StatusCode::NOT_FOUND,
                    Json(openai_error(
                        &format!("no route or target for model {}", req.model),
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

    let plan = sb_router::plan_route(&req, &route_name, &require, &candidates);
    let summary = plan.decision.summary();
    let mut last_err = None;

    for (index, target) in plan.candidates.iter().enumerate() {
        let Some(adapter) = state.registry.adapter(&target.provider_id) else {
            continue;
        };

        let lease: Option<CredentialLease> = state.registry.lease(&target.provider_id);
        let prepared = PreparedRequest::new(req.clone(), target.clone(), lease);
        let is_last = index + 1 == plan.candidates.len();

        match adapter.execute(prepared).await {
            Ok(stream) => {
                if req.stream {
                    let encoder =
                        sb_protocols::openai::OpenAiStreamEncoder::new(req.id.clone(), req.model.clone());
                    let sse_stream = futures::stream::unfold(
                        (stream, encoder, VecDeque::<String>::new(), false, false),
                        |(mut stream, mut encoder, mut pending, done_sent, finished)| async move {
                            let mut done_sent = done_sent;
                            let mut finished = finished;

                            loop {
                                if let Some(frame) = pending.pop_front() {
                                    return Some((
                                        Ok::<String, Infallible>(frame),
                                        (stream, encoder, pending, done_sent, finished),
                                    ));
                                }

                                if finished {
                                    if !done_sent {
                                        done_sent = true;
                                        return Some((
                                            Ok::<String, Infallible>(encoder.done()),
                                            (stream, encoder, pending, done_sent, finished),
                                        ));
                                    }
                                    return None;
                                }

                                // Invariant: once we are emitting bytes we are COMMITTED
                                // to this target — fallback is only legal before the first
                                // byte (handled by the execute() error path above). A
                                // mid-stream failure must be made VISIBLE, never swallowed
                                // into a clean [DONE] (the 9router silent-failure anti-pattern).
                                match stream.next().await {
                                    Some(Ok(AiStreamEvent::Error { message, .. })) => {
                                        pending.push_back(stream_error_frame(&message));
                                        finished = true;
                                    }
                                    Some(Ok(event)) => {
                                        pending.extend(encoder.encode(&event));
                                    }
                                    Some(Err(error)) => {
                                        pending.push_back(stream_error_frame(&error.message));
                                        finished = true;
                                    }
                                    None => {
                                        finished = true;
                                    }
                                }
                            }
                        },
                    );

                    let body = axum::body::Body::from_stream(sse_stream);
                    tracing::info!(
                        request_id = %req.id,
                        model = %req.model,
                        target = %target.id,
                        status = 200u16,
                        latency_ms = started.elapsed().as_millis() as u64,
                        route = %summary
                    );

                    let response = match Response::builder()
                        .status(StatusCode::OK)
                        .header("content-type", "text/event-stream")
                        .body(body)
                    {
                        Ok(response) => response,
                        Err(_) => {
                            return with_route_header(
                                (
                                    StatusCode::INTERNAL_SERVER_ERROR,
                                    Json(openai_error("failed to build stream response", "upstream_error")),
                                )
                                    .into_response(),
                                &summary,
                            );
                        }
                    };

                    return with_route_header(response, &summary);
                }

                match collect_response(stream, req.id.clone(), req.model.clone()).await {
                    Ok(response) => {
                        tracing::info!(
                            request_id = %req.id,
                            model = %req.model,
                            target = %target.id,
                            status = 200u16,
                            latency_ms = started.elapsed().as_millis() as u64,
                            route = %summary
                        );
                        return with_route_header(
                            (
                                StatusCode::OK,
                                Json(sb_protocols::openai::response_to_openai_chat(&response)),
                            )
                                .into_response(),
                            &summary,
                        );
                    }
                    Err(error) => {
                        if error.should_fallback() && !is_last {
                            last_err = Some(error);
                            continue;
                        }

                        tracing::info!(
                            request_id = %req.id,
                            model = %req.model,
                            target = %target.id,
                            status = error.class.http_status(),
                            latency_ms = started.elapsed().as_millis() as u64,
                            route = %summary
                        );
                        let status = StatusCode::from_u16(error.class.http_status())
                            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                        return with_route_header(
                            (
                                status,
                                Json(openai_error(&error.message, "upstream_error")),
                            )
                                .into_response(),
                            &summary,
                        );
                    }
                }
            }
            Err(error) => {
                if error.should_fallback() && !is_last {
                    last_err = Some(error);
                    continue;
                }

                tracing::info!(
                    request_id = %req.id,
                    model = %req.model,
                    target = %target.id,
                    status = error.class.http_status(),
                    latency_ms = started.elapsed().as_millis() as u64,
                    route = %summary
                );
                let status = StatusCode::from_u16(error.class.http_status())
                    .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                return with_route_header(
                    (
                        status,
                        Json(openai_error(&error.message, "upstream_error")),
                    )
                        .into_response(),
                    &summary,
                );
            }
        }
    }

    if let Some(error) = last_err {
        let status =
            StatusCode::from_u16(error.class.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        return with_route_header(
            (
                status,
                Json(openai_error(&error.message, "upstream_error")),
            )
                .into_response(),
            &summary,
        );
    }

    let rejected = plan
        .decision
        .rejected
        .iter()
        .map(|rejected| format!("{}:{}", rejected.target_id, rejected.reason))
        .collect::<Vec<_>>()
        .join(",");

    with_route_header(
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
