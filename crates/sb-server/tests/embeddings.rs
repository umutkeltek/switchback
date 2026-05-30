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
      - "mock/anything"
"#;

#[tokio::test]
async fn mock_embeddings_end_to_end() {
    let cfg = sb_core::Config::from_yaml(CFG).unwrap();
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

    let client = reqwest::Client::new();
    let body = serde_json::json!({"model":"mock/anything","input":["hello","world"]});
    let response = client
        .post(format!("http://{addr}/v1/embeddings"))
        .json(&body)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), reqwest::StatusCode::OK);

    let resp: serde_json::Value = response.json().await.unwrap();
    assert_eq!(resp["object"], serde_json::json!("list"));
    assert_eq!(resp["data"].as_array().unwrap().len(), 2);
    assert!(!resp["data"][0]["embedding"].as_array().unwrap().is_empty());
}
