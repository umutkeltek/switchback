//! End-to-end proof of the Anthropic adapter through the real server stack:
//! OpenAI ingress -> canonical IR -> Anthropic Messages wire -> upstream ->
//! Anthropic SSE -> canonical -> OpenAI egress. The first two tests run a fake
//! Anthropic upstream so they're deterministic and CI-safe; the third hits the
//! real API and self-skips unless `ANTHROPIC_API_KEY` is set.

use std::sync::Arc;

use axum::http::header::CONTENT_TYPE;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};

/// Canned Anthropic Messages SSE — a `Hello` text generation, end_turn.
const FAKE_SSE: &str = "event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_fake\",\"model\":\"claude-test\",\"usage\":{\"input_tokens\":5,\"output_tokens\":1}}}\n\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}\n\n\
event: message_stop\n\
data: {\"type\":\"message_stop\"}\n\n";

/// Fake `/v1/messages`: streamed SSE or a single JSON message, per `stream`.
async fn fake_messages(Json(body): Json<Value>) -> Response {
    let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    if stream {
        ([(CONTENT_TYPE, "text/event-stream")], FAKE_SSE).into_response()
    } else {
        Json(json!({
            "id": "msg_fake",
            "type": "message",
            "role": "assistant",
            "model": "claude-test",
            "content": [{ "type": "text", "text": "Hello" }],
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 5, "output_tokens": 3 }
        }))
        .into_response()
    }
}

/// Spawn the fake Anthropic upstream; returns its `http://host:port` base.
async fn spawn_fake_anthropic() -> String {
    let app = Router::new().route("/v1/messages", post(fake_messages));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

/// Boot a switchback instance from a YAML config string; returns its address.
async fn spawn_switchback(cfg_yaml: &str) -> String {
    let cfg = sb_core::Config::from_yaml(cfg_yaml).unwrap();
    let registry = sb_adapters::AdapterRegistry::from_config(&cfg).unwrap();
    let resolver = sb_credentials::CredentialResolver::from_config(&cfg).unwrap();
    let state = sb_server::AppState::new(
        Arc::new(cfg),
        Arc::new(registry),
        Arc::new(resolver),
        Arc::new(sb_ledger::UsageLedger::in_memory()),
    );
    let app = sb_server::build_app(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

fn config_pointing_at(base_url: &str) -> String {
    format!(
        r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: anthropic
    type: anthropic
    base_url: "{base_url}"
    accounts:
      - id: test
        auth: {{ kind: api_key, inline: "test-key" }}
routes:
  - name: default
    match:
      model: "*"
    targets:
      - "anthropic/claude-test"
"#
    )
}

#[tokio::test]
async fn non_stream_round_trips_anthropic_through_canonical() {
    let upstream = spawn_fake_anthropic().await;
    let switchback = spawn_switchback(&config_pointing_at(&upstream)).await;

    let resp: Value = reqwest::Client::new()
        .post(format!("{switchback}/v1/chat/completions"))
        .json(&json!({"model":"claude","messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(resp["object"], "chat.completion");
    assert_eq!(resp["choices"][0]["message"]["content"], "Hello");
    assert_eq!(resp["choices"][0]["finish_reason"], "stop");
    // usage survived the Anthropic -> canonical -> OpenAI translation.
    assert_eq!(resp["usage"]["prompt_tokens"], 5);
    assert_eq!(resp["usage"]["completion_tokens"], 3);
}

#[tokio::test]
async fn stream_round_trips_anthropic_sse_to_openai_sse() {
    let upstream = spawn_fake_anthropic().await;
    let switchback = spawn_switchback(&config_pointing_at(&upstream)).await;

    let body = reqwest::Client::new()
        .post(format!("{switchback}/v1/chat/completions"))
        .json(&json!({"model":"claude","stream":true,"messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    // The OpenAI-encoded stream should carry the "Hello" delta, a stop finish,
    // and terminate with [DONE].
    assert!(
        body.contains("\"content\":\"Hello\""),
        "stream missing text delta: {body}"
    );
    assert!(
        body.contains("\"finish_reason\":\"stop\""),
        "stream missing stop: {body}"
    );
    assert!(body.trim_end().ends_with("data: [DONE]"), "stream not terminated: {body}");
}

/// Hits the REAL Anthropic API. Self-skips unless `ANTHROPIC_API_KEY` is set, so
/// it's safe in CI but proves wire-format fidelity against the actual service
/// for anyone with a key (`ANTHROPIC_API_KEY=... cargo test -p sb-server`).
#[tokio::test]
async fn live_real_anthropic_when_key_present() {
    if std::env::var("ANTHROPIC_API_KEY").is_err() {
        eprintln!("skipping live_real_anthropic_when_key_present: ANTHROPIC_API_KEY not set");
        return;
    }

    let cfg = r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: anthropic
    type: anthropic
    api_key_env: "ANTHROPIC_API_KEY"
routes:
  - name: default
    match:
      model: "*"
    targets:
      - "anthropic/claude-3-5-haiku-latest"
"#;
    let switchback = spawn_switchback(cfg).await;

    let resp = reqwest::Client::new()
        .post(format!("{switchback}/v1/chat/completions"))
        .json(&json!({
            "model": "claude",
            "max_tokens": 16,
            "messages": [{"role":"user","content":"Reply with the single word: pong"}]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), reqwest::StatusCode::OK, "live anthropic call failed");
    let value: Value = resp.json().await.unwrap();
    let content = value["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or_default();
    assert!(!content.is_empty(), "live anthropic returned empty content: {value}");
}

// ---------------------------------------------------------------------------
// Anthropic INGRESS: a Claude-shaped client hits /v1/messages and is routed to
// a non-Anthropic provider (mock), proving the gateway translates both ways:
// Anthropic in -> canonical -> mock out -> canonical -> Anthropic out.
// ---------------------------------------------------------------------------

fn mock_config() -> String {
    r#"
server:
  bind: "127.0.0.1:0"
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
    .to_string()
}

#[tokio::test]
async fn anthropic_ingress_non_stream_routes_to_mock() {
    let switchback = spawn_switchback(&mock_config()).await;

    let resp: Value = reqwest::Client::new()
        .post(format!("{switchback}/v1/messages"))
        .json(&json!({
            "model": "mock/echo",
            "max_tokens": 100,
            "messages": [{"role":"user","content":"hi"}]
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    // Rendered as an Anthropic Messages response, not OpenAI.
    assert_eq!(resp["type"], "message");
    assert_eq!(resp["role"], "assistant");
    assert_eq!(resp["content"][0]["type"], "text");
    assert!(resp["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("echo: hi"));
    assert_eq!(resp["stop_reason"], "end_turn");
}

#[tokio::test]
async fn anthropic_ingress_streams_anthropic_sse() {
    let switchback = spawn_switchback(&mock_config()).await;

    let body = reqwest::Client::new()
        .post(format!("{switchback}/v1/messages"))
        .json(&json!({
            "model": "mock/echo",
            "max_tokens": 100,
            "stream": true,
            "messages": [{"role":"user","content":"hi"}]
        }))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    assert!(
        body.contains("event: message_start"),
        "missing message_start: {body}"
    );
    assert!(
        body.contains("event: content_block_start"),
        "missing content_block_start: {body}"
    );
    assert!(body.contains("\"text_delta\""), "missing text_delta: {body}");
    assert!(body.contains("echo"), "missing echoed text: {body}");
    assert!(
        body.contains("event: message_stop"),
        "missing message_stop: {body}"
    );
}

#[tokio::test]
async fn anthropic_count_tokens_returns_estimate() {
    let switchback = spawn_switchback(&mock_config()).await;

    let resp: Value = reqwest::Client::new()
        .post(format!("{switchback}/v1/messages/count_tokens"))
        .json(&json!({
            "model": "mock/echo",
            "messages": [{"role":"user","content":"hello world this is a token count test"}]
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert!(
        resp["input_tokens"].as_u64().unwrap() > 0,
        "expected nonzero estimate: {resp}"
    );
}
