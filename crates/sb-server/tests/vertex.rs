//! Vertex on the `WireCodec × AuthScheme` seam: a new cloud provider as data.
//! The fake upstream verifies switchback hit the project-scoped Vertex URL with
//! an `Authorization: Bearer` access token, and returns the Gemini wire format —
//! which switchback translates back to OpenAI. No new adapter; just a codec.

use std::sync::Arc;

use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, Uri};
use axum::response::{IntoResponse, Response};
use axum::Router;
use serde_json::{json, Value};

/// Fake Vertex: reports whether it saw a Bearer token and the right project path.
async fn fake_vertex(uri: Uri, headers: HeaderMap) -> Response {
    let bearer = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.starts_with("Bearer "))
        .unwrap_or(false);
    let path_ok = uri
        .path()
        .contains("/projects/my-proj/locations/us-central1/publishers/google/models/gemini-2.0-flash");

    if uri.path().contains("streamGenerateContent") {
        let sse = "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"streamed\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":1,\"candidatesTokenCount\":1}}\n\n";
        ([(CONTENT_TYPE, "text/event-stream")], sse).into_response()
    } else {
        axum::Json(json!({
            "candidates": [{
                "content": { "role": "model", "parts": [{ "text": format!("bearer={bearer} path_ok={path_ok}") }] },
                "finishReason": "STOP"
            }],
            "usageMetadata": { "promptTokenCount": 3, "candidatesTokenCount": 4 },
            "modelVersion": "gemini-2.0-flash"
        }))
        .into_response()
    }
}

async fn spawn_fake_vertex() -> String {
    let app = Router::new().fallback(fake_vertex);
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
        ledger: Arc::new(sb_ledger::UsageLedger::in_memory()),
    };
    let app = sb_server::build_app(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn vertex_is_a_codec_plus_bearer_token_no_new_adapter() {
    let upstream = spawn_fake_vertex().await;
    let cfg = format!(
        r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: vertex
    type: vertex
    project: my-proj
    region: us-central1
    base_url: "{upstream}"
    accounts:
      - id: t
        auth: {{ kind: api_key, inline: "fake-vertex-access-token" }}
routes:
  - name: default
    match:
      model: "*"
    targets:
      - "vertex/gemini-2.0-flash"
"#
    );
    let switchback = spawn_switchback(&cfg).await;

    let resp: Value = reqwest::Client::new()
        .post(format!("{switchback}/v1/chat/completions"))
        .json(&json!({"model":"gemini-2.0-flash","messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    // The fake confirms switchback hit the project-scoped URL with a Bearer token,
    // and its Gemini-format reply came back as an OpenAI chat.completion.
    assert_eq!(resp["object"], "chat.completion");
    assert_eq!(
        resp["choices"][0]["message"]["content"],
        "bearer=true path_ok=true"
    );
    assert_eq!(resp["usage"]["prompt_tokens"], 3);
}
