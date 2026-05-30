//! End-to-end proof of the Gemini adapter through the real server stack: an
//! OpenAI-shaped client hits switchback, which routes to a fake Gemini upstream
//! and translates GenerateContent back to OpenAI — proving the canonical IR
//! generalizes to a third wire format. Deterministic (fake upstream), CI-safe.

use std::sync::Arc;

use axum::http::header::CONTENT_TYPE;
use axum::http::Uri;
use axum::response::{IntoResponse, Response};
use axum::Router;
use serde_json::{json, Value};

/// Two Gemini SSE chunks: "Hello" + " world", final carries finishReason+usage.
const FAKE_SSE: &str = "data: {\"responseId\":\"r1\",\"modelVersion\":\"gemini-test\",\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"Hello\"}]}}]}\n\n\
data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\" world\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":5,\"candidatesTokenCount\":2}}\n\n";

/// Fake Gemini upstream: streamed SSE for `:streamGenerateContent`, else a
/// single JSON `generateContent` response. Distinguishes by the URL path.
async fn fake_gemini(uri: Uri) -> Response {
    if uri.path().contains("streamGenerateContent") {
        ([(CONTENT_TYPE, "text/event-stream")], FAKE_SSE).into_response()
    } else {
        axum::Json(json!({
            "candidates": [{
                "content": { "role": "model", "parts": [{ "text": "Hello" }] },
                "finishReason": "STOP"
            }],
            "usageMetadata": { "promptTokenCount": 5, "candidatesTokenCount": 3 },
            "modelVersion": "gemini-test"
        }))
        .into_response()
    }
}

async fn spawn_fake_gemini() -> String {
    let app = Router::new().fallback(fake_gemini);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

async fn spawn_switchback(cfg_yaml: &str) -> String {
    let cfg = sb_core::Config::from_yaml(cfg_yaml).unwrap();
    let registry = sb_adapters::AdapterRegistry::from_config(&cfg).unwrap();
    let resolver = sb_credentials::CredentialResolver::from_config(&cfg).unwrap();
    let state = sb_server::AppState {
        config: Arc::new(cfg),
        registry: Arc::new(registry),
        resolver: Arc::new(resolver),
    };
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
  - id: gemini
    type: gemini
    base_url: "{base_url}"
    accounts:
      - id: test
        auth: {{ kind: api_key, inline: "test-key" }}
routes:
  - name: default
    match:
      model: "*"
    targets:
      - "gemini/gemini-test"
"#
    )
}

#[tokio::test]
async fn non_stream_round_trips_gemini_through_canonical() {
    let upstream = spawn_fake_gemini().await;
    let switchback = spawn_switchback(&config_pointing_at(&upstream)).await;

    let resp: Value = reqwest::Client::new()
        .post(format!("{switchback}/v1/chat/completions"))
        .json(&json!({"model":"gemini-test","messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(resp["object"], "chat.completion");
    assert_eq!(resp["choices"][0]["message"]["content"], "Hello");
    // Gemini's promptTokenCount/candidatesTokenCount -> OpenAI usage.
    assert_eq!(resp["usage"]["prompt_tokens"], 5);
    assert_eq!(resp["usage"]["completion_tokens"], 3);
}

#[tokio::test]
async fn stream_round_trips_gemini_sse_to_openai_sse() {
    let upstream = spawn_fake_gemini().await;
    let switchback = spawn_switchback(&config_pointing_at(&upstream)).await;

    let body = reqwest::Client::new()
        .post(format!("{switchback}/v1/chat/completions"))
        .json(&json!({"model":"gemini-test","stream":true,"messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    // The two Gemini text chunks reassemble into "Hello world" on the OpenAI SSE.
    assert!(body.contains("\"content\":\"Hello\""), "missing first delta: {body}");
    assert!(body.contains("\"content\":\" world\""), "missing second delta: {body}");
    assert!(body.contains("\"finish_reason\":\"stop\""), "missing stop: {body}");
    assert!(body.trim_end().ends_with("data: [DONE]"), "not terminated: {body}");
}
