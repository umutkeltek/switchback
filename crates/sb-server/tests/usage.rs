//! End-to-end proof of the usage/cost ledger: a request through the real server
//! stack is recorded with usage and an attributed cost (priced from the catalog
//! ledger), and surfaced at `GET /v1/usage`.

use std::sync::Arc;

use serde_json::{json, Value};

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

/// mock provider + a catalog that prices model `echo` ($1/Mtok in, $2/Mtok out).
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
catalog:
  prices:
    - { model_id: echo, token_kind: input, unit_price_micros_per_mtok: 1000000, effective_from: "2025-01-01T00:00:00Z" }
    - { model_id: echo, token_kind: output, unit_price_micros_per_mtok: 2000000, effective_from: "2025-01-01T00:00:00Z" }
"#;

#[tokio::test]
async fn request_is_recorded_in_the_usage_ledger_with_attributed_cost() {
    let switchback = spawn_switchback(CFG).await;
    let client = reqwest::Client::new();

    // Before any request: empty ledger.
    let before: Value = client
        .get(format!("{switchback}/v1/usage"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(before["requests"], 0);

    // One non-streaming request through the stack.
    let resp: Value = client
        .post(format!("{switchback}/v1/chat/completions"))
        .json(&json!({"model":"mock/echo","messages":[{"role":"user","content":"hello there"}]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let input = resp["usage"]["prompt_tokens"].as_u64().unwrap();
    let output = resp["usage"]["completion_tokens"].as_u64().unwrap();

    let after: Value = client
        .get(format!("{switchback}/v1/usage"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(after["requests"], 1);
    // cost = input*$1/Mtok + output*$2/Mtok, in micro-USD.
    let expected = input + output * 2;
    assert_eq!(after["total_cost_micros"].as_u64().unwrap(), expected);
    assert!(after["total_cost_micros"].as_u64().unwrap() > 0, "{after}");
    // attributed to the mock provider.
    assert_eq!(after["by_provider"]["mock"][0], 1);
}
