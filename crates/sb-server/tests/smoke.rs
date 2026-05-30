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

#[tokio::test]
async fn mock_path_end_to_end() {
    let cfg = sb_core::Config::from_yaml(CFG).unwrap();
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

    let client = reqwest::Client::new();

    let health: serde_json::Value = client
        .get(format!("http://{addr}/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health["ok"], serde_json::json!(true));

    let body = serde_json::json!({"model":"mock/echo","messages":[{"role":"user","content":"hi"}]});
    let resp: serde_json::Value = client
        .post(format!("http://{addr}/v1/chat/completions"))
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
        .post(format!("http://{addr}/v1/chat/completions"))
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
