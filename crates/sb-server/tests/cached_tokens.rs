//! Cached prompt tokens are charged once. OpenAI counts cached tokens inside
//! `prompt_tokens` (and the Responses API inside `input_tokens`); the cost
//! code assumes the canonical `input_tokens` excludes them. Golden costs and
//! ledger rows for a plain and a streamed chat response, priced both by
//! `server.cost_map` and by the typed catalog, and the client still sees
//! the upstream's own prompt count.
use std::sync::Arc;

use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};

/// 1000 prompt tokens, 800 of them cached, 40 generated.
fn usage() -> Value {
    json!({"prompt_tokens": 1000, "completion_tokens": 40, "total_tokens": 1040,
           "prompt_tokens_details": {"cached_tokens": 800}})
}

async fn chat(Json(body): Json<Value>) -> Response {
    let model = body["model"].as_str().unwrap_or("").to_string();
    if model == "timed" {
        // A llama.cpp-style engine: the cache shows only in its timings.
        return Json(json!({
            "id": "x", "object": "chat.completion", "model": model,
            "choices": [{"index": 0, "finish_reason": "stop",
                         "message": {"role": "assistant", "content": "hi"}}],
            "usage": {"prompt_tokens": 1000, "completion_tokens": 40, "total_tokens": 1040},
            "timings": {"prompt_n": 200, "prompt_ms": 20.0, "cache_n": 800,
                        "predicted_n": 40, "predicted_ms": 400.0}
        }))
        .into_response();
    }
    if body["stream"].as_bool() == Some(true) {
        let first = json!({"id": "x", "model": model, "choices": [{"index": 0,
            "delta": {"content": "hi"}, "finish_reason": null}]});
        let last = json!({"id": "x", "model": model,
            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
            "usage": usage()});
        let body = format!("data: {first}\n\ndata: {last}\n\ndata: [DONE]\n\n");
        return ([("content-type", "text/event-stream")], body).into_response();
    }
    Json(json!({
        "id": "x", "object": "chat.completion", "model": model,
        "choices": [{"index": 0, "finish_reason": "stop",
                     "message": {"role": "assistant", "content": "hi"}}],
        "usage": usage()
    }))
    .into_response()
}

async fn spawn_upstream() -> String {
    let app = Router::new().route("/chat/completions", post(chat));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

struct Gateway {
    base: String,
    ledger: Arc<sb_ledger::UsageLedger>,
    _cost_map: CostMapFile,
}

/// Removes the cost-map file when the test ends.
struct CostMapFile(std::path::PathBuf);

impl Drop for CostMapFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

async fn spawn_gateway(name: &str) -> Gateway {
    let upstream = spawn_upstream().await;
    // `up/mapped`: cost map, $1/Mtok input (cached tokens at the same
    // rate), $1/Mtok output. `up/cataloged`: catalog, $1/Mtok input,
    // $0.10/Mtok cached input, $2/Mtok output.
    let cost_map = std::env::temp_dir().join(format!(
        "sb_cached_tokens_{name}_{}.json",
        std::process::id()
    ));
    std::fs::write(
        &cost_map,
        r#"{"models":[{"provider_id":"up","model_id":"mapped","input_micros_per_mtok":1000000,"output_micros_per_mtok":1000000}]}"#,
    )
    .unwrap();
    let cfg_yaml = format!(
        r#"
server:
  bind: "127.0.0.1:0"
  cost_map: "{cost_map}"
providers:
  - id: up
    type: openai_compatible
    base_url: "{upstream}"
    accounts:
      - id: a
        auth: {{ kind: api_key, inline: "k" }}
routes:
  - name: mapped
    match: {{ model: "mapped" }}
    targets: ["up/mapped"]
  - name: cataloged
    match: {{ model: "cataloged" }}
    targets: ["up/cataloged"]
  - name: timed
    match: {{ model: "timed" }}
    targets: ["up/timed"]
catalog:
  prices:
    - {{ model_id: cataloged, token_kind: input, unit_price_micros_per_mtok: 1000000, effective_from: "2025-01-01T00:00:00Z" }}
    - {{ model_id: cataloged, token_kind: cached_input, unit_price_micros_per_mtok: 100000, effective_from: "2025-01-01T00:00:00Z" }}
    - {{ model_id: cataloged, token_kind: output, unit_price_micros_per_mtok: 2000000, effective_from: "2025-01-01T00:00:00Z" }}
    - {{ model_id: timed, token_kind: input, unit_price_micros_per_mtok: 1000000, effective_from: "2025-01-01T00:00:00Z" }}
    - {{ model_id: timed, token_kind: cached_input, unit_price_micros_per_mtok: 100000, effective_from: "2025-01-01T00:00:00Z" }}
    - {{ model_id: timed, token_kind: output, unit_price_micros_per_mtok: 2000000, effective_from: "2025-01-01T00:00:00Z" }}
"#,
        cost_map = cost_map.display()
    );
    let cfg = sb_core::Config::from_yaml(&cfg_yaml).unwrap();
    let registry = sb_adapters::AdapterRegistry::from_config(&cfg).unwrap();
    let resolver = sb_credentials::CredentialResolver::from_config(&cfg).unwrap();
    let ledger = Arc::new(sb_ledger::UsageLedger::in_memory());
    let state = sb_server::AppState::new(
        Arc::new(cfg),
        Arc::new(registry),
        Arc::new(resolver),
        ledger.clone(),
    );
    let app = sb_server::build_app(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Gateway {
        base: format!("http://{addr}"),
        ledger,
        _cost_map: CostMapFile(cost_map),
    }
}

/// Send one chat request; returns the usage object the client saw.
async fn ask(base: &str, model: &str, stream: bool) -> Value {
    let mut body = json!({"model": model, "messages": [{"role": "user", "content": "hi"}]});
    if stream {
        body["stream"] = json!(true);
        body["stream_options"] = json!({"include_usage": true});
    }
    let response = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success(), "{}", response.status());
    let text = response.text().await.unwrap();
    if !stream {
        let value: Value = serde_json::from_str(&text).unwrap();
        return value["usage"].clone();
    }
    text.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|data| serde_json::from_str::<Value>(data).ok())
        .find_map(|chunk| chunk.get("usage").filter(|u| !u.is_null()).cloned())
        .unwrap_or_else(|| panic!("no usage chunk in {text}"))
}

/// The one ledger row for `model`, waiting briefly for a streamed request's
/// row (written when the stream finishes).
async fn row(ledger: &sb_ledger::UsageLedger, model: &str) -> sb_ledger::UsageRecord {
    for _ in 0..100 {
        if let Some(row) = ledger.snapshot().into_iter().find(|r| r.model == model) {
            return row;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("no ledger row for {model}");
}

async fn check(stream: bool) {
    let gateway = spawn_gateway(if stream { "stream" } else { "plain" }).await;
    for model in ["mapped", "cataloged"] {
        let seen = ask(&gateway.base, model, stream).await;
        // The client sees OpenAI's own convention, unchanged.
        assert_eq!(seen["prompt_tokens"], 1000, "{seen}");
        assert_eq!(seen["completion_tokens"], 40, "{seen}");
        assert_eq!(seen["total_tokens"], 1040, "{seen}");
    }
    let mapped = row(&gateway.ledger, "mapped").await;
    assert_eq!(
        (mapped.usage.input_tokens, mapped.usage.cached_input_tokens),
        (200, 800),
        "input_tokens excludes the cached tokens"
    );
    // Cost map: every prompt token once at the input rate, plus output.
    assert_eq!(mapped.cost_micros, Some(1000 + 40));
    let cataloged = row(&gateway.ledger, "cataloged").await;
    assert_eq!(
        (
            cataloged.usage.input_tokens,
            cataloged.usage.cached_input_tokens
        ),
        (200, 800)
    );
    // Catalog: 200 uncached at $1, 800 cached at $0.10, 40 out at $2.
    assert_eq!(cataloged.cost_micros, Some(200 + 80 + 80));
    // Realized savings: 800 cached tokens at ($1 - $0.10).
    assert_eq!(cataloged.cache_savings_micros, Some(720));
    assert_eq!(gateway.ledger.summary().total_cost_micros, 1040 + 360);
}

#[tokio::test]
async fn cached_tokens_are_charged_once_on_a_plain_response() {
    check(false).await;
}

#[tokio::test]
async fn cached_tokens_are_charged_once_on_a_streamed_response() {
    check(true).await;
}

// REREVIEW: an Anthropic Messages client in front of an OpenAI upstream.
// On main it saw input_tokens 1000 (the whole prompt). Now it sees only
// the uncached part and nothing for the cached 800.
#[tokio::test]
async fn rereview_anthropic_client_sees_the_whole_prompt() {
    let gateway = spawn_gateway("anthropic_client").await;
    let response = reqwest::Client::new()
        .post(format!("{}/v1/messages", gateway.base))
        .json(&json!({"model": "mapped", "max_tokens": 64,
                      "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success(), "{}", response.status());
    let value: Value = response.json().await.unwrap();
    let usage = &value["usage"];
    let input = usage["input_tokens"].as_u64().unwrap_or(0);
    let cache_read = usage["cache_read_input_tokens"].as_u64().unwrap_or(0);
    assert_eq!(input + cache_read, 1000, "Anthropic client usage: {usage}");
}

// A plain answer from an engine that reports its cache only in its timings
// is priced after the cache is filled in: the trace's cost (and the
// scorecard's) matches the ledger's, as on a stream.
#[tokio::test]
async fn a_cache_known_only_from_timings_is_priced_as_cached_on_a_plain_answer() {
    let gateway = spawn_gateway("timed").await;
    let seen = ask(&gateway.base, "timed", false).await;
    assert_eq!(seen["prompt_tokens"], 1000, "{seen}");
    let timed = row(&gateway.ledger, "timed").await;
    assert_eq!(
        (timed.usage.input_tokens, timed.usage.cached_input_tokens),
        (200, 800)
    );
    // 200 uncached at $1, 800 cached at $0.10, 40 out at $2.
    assert_eq!(timed.cost_micros, Some(200 + 80 + 80));
    let traces: Value = reqwest::get(format!("{}/v1/traces", gateway.base))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let trace = traces["traces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t.to_string().contains("up/timed"))
        .unwrap_or_else(|| panic!("no trace for up/timed: {traces}"));
    assert_eq!(trace["cost_micros"], 360, "{trace}");
}
