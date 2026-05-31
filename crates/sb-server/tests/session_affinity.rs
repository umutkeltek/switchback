//! Session affinity: a coding-agent session can stay on the same provider
//! account while other sessions still use the provider's normal selector.

use std::sync::Arc;

use serde_json::{json, Value};

const CFG: &str = r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: mock
    type: mock
    selection: round_robin
    sticky: 1
    accounts:
      - id: a
        auth: { kind: none }
      - id: b
        auth: { kind: none }
routes:
  - name: default
    match: { model: "*" }
    targets:
      - "mock/echo"
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

async fn chat(base: &str, session: &str) {
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .header("x-switchback-session-id", session)
        .json(&json!({"model":"mock/echo","messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn session_header_sticks_to_the_same_account() {
    let base = spawn().await;

    chat(&base, "s1").await;
    chat(&base, "s1").await;
    chat(&base, "s2").await;

    let traces: Value = reqwest::Client::new()
        .get(format!("{base}/v1/traces"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let traces = traces["traces"].as_array().unwrap();
    let newest_first = traces
        .iter()
        .map(|trace| trace["attempts"][0]["account_id"].as_str().unwrap())
        .collect::<Vec<_>>();

    assert_eq!(
        newest_first,
        vec!["b", "a", "a"],
        "s1 stayed on account a, while s2 advanced to b"
    );
}
