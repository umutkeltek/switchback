//! Built-in plugins (Oracle #6, tier 1) wired into the hot path. A `pre_route`
//! plugin can reject a request before routing; the active chain is introspectable.

use std::sync::Arc;

use serde_json::{json, Value};

async fn spawn(plugins_yaml: &str) -> String {
    let cfg_yaml = format!(
        r#"
server:
  bind: "127.0.0.1:0"
{plugins_yaml}
providers:
  - id: mock
    type: mock
    accounts:
      - id: a
        auth: {{ kind: api_key, inline: "k" }}
routes:
  - name: default
    match: {{ model: "*" }}
    targets:
      - "mock/echo"
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
    format!("http://{addr}")
}

async fn chat(base: &str, model: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({"model": model, "messages": [{"role":"user","content":"hi"}]}))
        .send()
        .await
        .unwrap()
}

async fn get(url: &str) -> Value {
    reqwest::Client::new().get(url).send().await.unwrap().json().await.unwrap()
}

#[tokio::test]
async fn model_blocklist_plugin_rejects_before_routing() {
    let sb = spawn(
        r#"plugins:
  - type: model_blocklist
    models: ["blocked/*"]
  - type: request_tag
    tags: { env: test }"#,
    )
    .await;

    // A blocked model is rejected (403) by the pre_route plugin.
    let blocked = chat(&sb, "blocked/anything").await;
    assert_eq!(blocked.status(), 403);
    let body: Value = blocked.json().await.unwrap();
    assert_eq!(body["error"]["type"], "plugin_rejected");
    assert!(body["error"]["message"].as_str().unwrap().contains("blocked"));

    // An allowed model passes through to the mock and succeeds.
    let ok = chat(&sb, "mock/echo").await;
    assert_eq!(ok.status(), 200);

    // The active plugin chain is introspectable, in run order.
    let plugins = get(&format!("{sb}/v1/plugins")).await;
    assert_eq!(
        plugins["plugins"],
        json!(["model_blocklist", "request_tag"])
    );
}

#[tokio::test]
async fn no_plugins_configured_is_a_clean_passthrough() {
    let sb = spawn("").await;
    assert_eq!(chat(&sb, "mock/echo").await.status(), 200);
    let plugins = get(&format!("{sb}/v1/plugins")).await;
    assert_eq!(plugins["plugins"], json!([]));
}
