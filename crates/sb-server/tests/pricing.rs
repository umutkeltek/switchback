//! Routing cost and ledger cost are priced from ONE source (audit #5). With a
//! `cost_map` and NO catalog, the ledger/trace cost is still non-zero (priced
//! from the same index the router routes on) and matches the request's usage.

use std::sync::Arc;

use serde_json::{json, Value};

#[tokio::test]
async fn ledger_prices_from_the_router_cost_index() {
    // $1/Mtok input + $1/Mtok output → cost_micros = input_tokens + output_tokens.
    let cost_map = std::env::temp_dir().join("sb_pricing_test.json");
    std::fs::write(
        &cost_map,
        r#"{"models":[{"provider_id":"mock","model_id":"echo","input_micros_per_mtok":1000000,"output_micros_per_mtok":1000000}]}"#,
    )
    .unwrap();

    // No `catalog:` at all — the OLD ledger path would have priced this at 0.
    let cfg_yaml = format!(
        r#"
server:
  bind: "127.0.0.1:0"
  cost_map: "{}"
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
"#,
        cost_map.display()
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
    let sb = format!("http://{addr}");
    let client = reqwest::Client::new();

    let chat: Value = client
        .post(format!("{sb}/v1/chat/completions"))
        .json(&json!({"model":"mock/echo","messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let usage = &chat["usage"];
    let expected = usage["prompt_tokens"].as_u64().unwrap() + usage["completion_tokens"].as_u64().unwrap();
    assert!(expected > 0);

    // Ledger cost is priced from the cost_map (non-zero despite no catalog) and
    // equals input+output tokens at $1/Mtok each.
    let summary: Value = client
        .get(format!("{sb}/v1/usage"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(summary["total_cost_micros"].as_u64().unwrap(), expected);

    // The trace records the SAME cost — route-side and ledger-side agree.
    let traces: Value = client
        .get(format!("{sb}/v1/traces"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        traces["traces"][0]["cost_micros"].as_u64().unwrap(),
        expected,
        "trace cost must match the ledger cost (one price source)"
    );
}
