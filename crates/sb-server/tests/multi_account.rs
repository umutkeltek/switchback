use std::sync::Arc;

const CFG: &str = r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: mock
    type: mock
    selection: fill_first
    accounts:
      - id: fail-account
        auth: { kind: none }
        priority: 0
      - id: good-account
        auth: { kind: none }
        priority: 1
routes:
  - name: default
    match:
      model: "*"
    targets:
      - "mock/echo"
"#;

#[tokio::test]
async fn falls_back_across_accounts_within_a_target() {
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
    let body = serde_json::json!({"model":"mock/echo","messages":[{"role":"user","content":"hi"}]});

    let first: serde_json::Value = client
        .post(format!("http://{addr}/v1/chat/completions"))
        .json(&body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let first_content = first["choices"][0]["message"]["content"].as_str().unwrap();
    assert!(
        first_content.contains("echo:"),
        "first response content was: {first_content}"
    );

    let second: serde_json::Value = client
        .post(format!("http://{addr}/v1/chat/completions"))
        .json(&body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let second_content = second["choices"][0]["message"]["content"].as_str().unwrap();
    assert!(
        second_content.contains("echo:"),
        "second response content was: {second_content}"
    );
}
