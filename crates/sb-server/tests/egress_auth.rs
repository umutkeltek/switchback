//! An egress identity selects a network path; it must never set or override
//! credentials. A configured egress `Authorization` header is refused, so the
//! upstream always receives the lease's real key (audit P0/P1).

use std::sync::Arc;

use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};

async fn echo_headers(headers: HeaderMap, State(()): State<()>, Json(_b): Json<Value>) -> Json<Value> {
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("<none>")
        .to_string();
    let xcustom = headers
        .get("x-custom")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("<none>")
        .to_string();
    Json(json!({
        "id": "x", "object": "chat.completion",
        "choices": [{"index":0,"finish_reason":"stop","message":{
            "role":"assistant","content": format!("auth=[{auth}] xcustom=[{xcustom}]")
        }}],
        "usage": {"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
    }))
}

async fn spawn_node() -> String {
    let app = Router::new()
        .route("/chat/completions", post(echo_headers))
        .with_state(());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn egress_identity_cannot_override_authorization() {
    let up = spawn_node().await;
    let cfg_yaml = format!(
        r#"
server:
  bind: "127.0.0.1:0"
egress:
  - id: tagged
    kind: direct
    headers:
      authorization: "Bearer EVIL-OVERRIDE"
      x-custom: "applied"
providers:
  - id: up
    type: openai_compatible
    base_url: "{up}"
    accounts:
      - id: a
        egress: tagged
        auth: {{ kind: api_key, inline: "realkey" }}
routes:
  - name: default
    match: {{ model: "*" }}
    targets:
      - "up/m"
"#
    );
    let cfg = sb_core::Config::from_yaml(&cfg_yaml).unwrap();
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
    let sb = format!("http://{addr}");

    let body: Value = reqwest::Client::new()
        .post(format!("{sb}/v1/chat/completions"))
        .json(&json!({"model":"up/m","messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let content = body["choices"][0]["message"]["content"].as_str().unwrap();
    // The upstream saw the LEASE's real key, not the egress override.
    assert!(content.contains("auth=[Bearer realkey]"), "got: {content}");
    assert!(!content.contains("EVIL-OVERRIDE"), "egress overrode auth: {content}");
    // A non-auth identity header IS still applied.
    assert!(content.contains("xcustom=[applied]"), "got: {content}");
}
