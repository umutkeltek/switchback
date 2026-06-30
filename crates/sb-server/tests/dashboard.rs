use std::sync::Arc;

const CFG: &str = r#"
server:
  bind: "127.0.0.1:0"
  api_key: "sk-test"
providers:
  - id: mock
    type: mock
routes:
  - name: mock
    match: { model: "coding" }
    targets: ["mock/echo"]
"#;

async fn spawn() -> String {
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
    format!("http://{addr}")
}

#[tokio::test]
async fn dashboard_serves_graphite_switchboard_scaffold() {
    let base = spawn().await;
    let body = reqwest::Client::new()
        .get(format!("{base}/"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    assert!(body.contains("Graphite switchboard"));
    assert!(body.contains("/v1/client-profiles"));
    assert!(body.contains("/cp/v1/route-preview"));
    assert!(body.contains("/v1/workflows"));
    assert!(body.contains("/v1/jobs"));
    assert!(body.contains("/v1/images/generations"));
    assert!(body.contains("Workflows"));
    assert!(body.contains("Artifacts"));
    assert!(body.contains("Resolve setup blocker"));
    assert!(body.contains("Native auth stores are read only by explicit OAuth account sources"));
    assert!(body.contains("switchback setup native --config switchback.yaml"));
    assert!(body
        .contains("switchback setup pack install native-token-adapter --config switchback.yaml"));
    assert!(
        !body.contains(">Add Anthropic account<"),
        "dashboard should derive account blockers contextually instead of hardcoding an Anthropic add button"
    );
}
