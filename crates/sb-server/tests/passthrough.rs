//! Adaptive model pass-through: a model that matches no route and isn't a
//! configured `provider/model` is forwarded VERBATIM to `server.default_provider`
//! — so a brand-new model works with no per-model config and no rebuild.

use std::sync::Arc;

use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};

/// Fake OpenAI-compatible upstream that echoes the model it was asked to serve.
async fn fake_openai(Json(body): Json<Value>) -> Json<Value> {
    let model = body["model"].as_str().unwrap_or("?").to_string();
    Json(json!({
        "id": "chatcmpl-x",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "finish_reason": "stop",
            "message": { "role": "assistant", "content": format!("served model={model}") }
        }],
        "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
    }))
}

async fn spawn_fake() -> String {
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
async fn unknown_model_is_forwarded_to_default_provider_verbatim() {
    let upstream = spawn_fake().await;
    // No routes at all — the default provider catches everything.
    let cfg = format!(
        r#"
server:
  bind: "127.0.0.1:0"
  default_provider: "pool"
providers:
  - id: pool
    type: openai_compatible
    base_url: "{upstream}"
    accounts:
      - id: a
        auth: {{ kind: api_key, inline: "k" }}
"#
    );
    let switchback = spawn_switchback(&cfg).await;
    let client = reqwest::Client::new();

    // A model the gateway has never heard of — added by simply requesting it.
    let resp: Value = client
        .post(format!("{switchback}/v1/chat/completions"))
        .json(&json!({"model":"brand-new-model-2099","messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        resp["choices"][0]["message"]["content"], "served model=brand-new-model-2099",
        "unknown model not forwarded verbatim"
    );

    // An OpenRouter-style author/model id (contains a slash) is forwarded whole.
    let resp: Value = client
        .post(format!("{switchback}/v1/chat/completions"))
        .json(
            &json!({"model":"some-vendor/some-model","messages":[{"role":"user","content":"hi"}]}),
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        resp["choices"][0]["message"]["content"], "served model=some-vendor/some-model",
        "slashed model id not forwarded verbatim"
    );
}
