use std::sync::Arc;

const CFG: &str = r#"
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
"#;

const MODEL_LIST_CFG: &str = r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: mock
    type: mock
combos:
  coder_combo:
    models:
      - "mock/echo"
routes:
  - name: coder
    match:
      model: "coder"
    targets:
      - "mock/echo"
  - name: default
    match:
      model: "*"
    targets:
      - "mock/echo"
"#;

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
async fn mock_path_end_to_end() {
    let switchback = spawn_switchback(CFG).await;
    let client = reqwest::Client::new();

    let health: serde_json::Value = client
        .get(format!("{switchback}/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health["ok"], serde_json::json!(true));

    let body = serde_json::json!({"model":"mock/echo","messages":[{"role":"user","content":"hi"}]});
    let resp: serde_json::Value = client
        .post(format!("{switchback}/v1/chat/completions"))
        .json(&body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let content = resp["choices"][0]["message"]["content"].as_str().unwrap();
    assert!(content.contains("echo:"), "content was: {content}");

    let sbody = serde_json::json!({"model":"mock/echo","stream":true,"messages":[{"role":"user","content":"hi"}]});
    let text = client
        .post(format!("{switchback}/v1/chat/completions"))
        .json(&sbody)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(text.contains("data:"), "stream body: {text}");
    assert!(text.contains("[DONE]"), "stream body: {text}");
}

#[tokio::test]
async fn models_endpoint_lists_usable_virtual_model_contracts() {
    let switchback = spawn_switchback(MODEL_LIST_CFG).await;

    let models: serde_json::Value = reqwest::Client::new()
        .get(format!("{switchback}/v1/models"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ids = models["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|model| model["id"].as_str())
        .collect::<Vec<_>>();
    assert!(
        models["models"].as_array().is_some(),
        "Codex-compatible models field missing: {models}"
    );

    assert!(ids.contains(&"coder"), "ids: {ids:?}");
    assert!(ids.contains(&"coder_combo"), "ids: {ids:?}");
    assert!(ids.contains(&"auto/cheap"), "ids: {ids:?}");
    assert!(ids.contains(&"auto/coding"), "ids: {ids:?}");
    assert!(ids.contains(&"mock/echo"), "ids: {ids:?}");
}

#[tokio::test]
async fn responses_ingress_accepts_native_sized_payloads() {
    let switchback = spawn_switchback(CFG).await;
    let large_input = "x".repeat(3 * 1024 * 1024);
    let body = serde_json::json!({
        "model": "mock/echo",
        "input": large_input,
        "max_output_tokens": 16
    });

    let resp = reqwest::Client::new()
        .post(format!("{switchback}/v1/responses"))
        .json(&body)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let value: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(value["object"], "response");
}
