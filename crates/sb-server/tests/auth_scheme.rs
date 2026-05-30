//! "Auth as data": an OpenAI-shaped provider configured with a non-bearer
//! `auth_scheme` attaches its key the way the config says — no new adapter. The
//! fake upstream reports which auth it received; we assert the key rode as
//! `x-api-key`, not `Authorization: Bearer`.

use std::sync::Arc;

use axum::http::HeaderMap;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};

/// Fake OpenAI `/chat/completions` that echoes the auth it saw into the message.
async fn fake_openai(headers: HeaderMap) -> Json<Value> {
    let x_api_key = headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("absent")
        .to_string();
    let authorization = if headers.contains_key("authorization") {
        "present"
    } else {
        "absent"
    };
    Json(json!({
        "id": "chatcmpl-fake",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "finish_reason": "stop",
            "message": {
                "role": "assistant",
                "content": format!("x-api-key={x_api_key} authorization={authorization}")
            }
        }],
        "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
    }))
}

async fn spawn_fake_openai() -> String {
    let app = Router::new().route("/chat/completions", post(fake_openai));
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

#[tokio::test]
async fn openai_shaped_provider_with_header_auth_is_pure_config() {
    let upstream = spawn_fake_openai().await;
    // OpenAI wire format, but authenticates with x-api-key — declared as DATA.
    let cfg = format!(
        r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: weird
    type: openai_compatible
    base_url: "{upstream}"
    auth_scheme: {{ kind: header, name: x-api-key }}
    accounts:
      - id: a
        auth: {{ kind: api_key, inline: "secret-xyz" }}
routes:
  - name: default
    match:
      model: "*"
    targets:
      - "weird/some-model"
"#
    );
    let switchback = spawn_switchback(&cfg).await;

    let resp: Value = reqwest::Client::new()
        .post(format!("{switchback}/v1/chat/completions"))
        .json(&json!({"model":"some-model","messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    // The key rode as x-api-key, and NOT as Authorization: Bearer — proving the
    // auth scheme is composed from config, not hardcoded bearer.
    let content = resp["choices"][0]["message"]["content"].as_str().unwrap();
    assert_eq!(content, "x-api-key=secret-xyz authorization=absent", "got: {content}");
}
