//! Simple combo UX: named model lists compile into normal route decisions.

use std::sync::Arc;

use serde_json::{json, Value};

async fn spawn(yaml: &str) -> String {
    let cfg = sb_core::Config::from_yaml(yaml).unwrap();
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

fn yaml(strategy: &str) -> String {
    format!(
        r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: mock
    type: mock
combos:
  coder:
    strategy: {strategy}
    models:
      - "mock/alpha"
      - "mock/beta"
"#
    )
}

async fn chat(sb: &str, model: &str) -> String {
    let resp = reqwest::Client::new()
        .post(format!("{sb}/v1/chat/completions"))
        .json(&json!({"model":model,"messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    resp.headers()
        .get("x-switchback-route")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string()
}

#[tokio::test]
async fn combo_fallback_previews_as_a_normal_route_decision() {
    let sb = spawn(&yaml("fallback")).await;
    let preview: Value = reqwest::Client::new()
        .post(format!("{sb}/cp/v1/route-preview"))
        .json(&json!({"model":"coder","messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(preview["decision"]["strategy"], "combo_fallback");
    assert_eq!(preview["decision"]["selected"]["target_id"], "mock/alpha");
    assert_eq!(
        preview["decision"]["fallbacks"][0]["target_id"],
        "mock/beta"
    );
    assert!(preview["decision"]["reason"]
        .as_array()
        .unwrap()
        .iter()
        .any(|reason| reason == "combo=coder"));

    let resource: Value = reqwest::Client::new()
        .get(format!("{sb}/cp/v1/resources/combos/coder"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resource["kind"], "ComboProfile");
    assert_eq!(resource["metadata"]["name"], "coder");
    assert_eq!(resource["spec"]["models"][0], "mock/alpha");
}

#[tokio::test]
async fn combo_round_robin_rotates_candidate_order() {
    let sb = spawn(&yaml("round_robin")).await;

    let first = chat(&sb, "coder").await;
    let second = chat(&sb, "coder").await;
    let third = chat(&sb, "coder").await;

    assert!(first.contains("strategy=combo_round_robin"));
    assert!(first.contains("selected=mock/alpha"), "route was: {first}");
    assert!(second.contains("selected=mock/beta"), "route was: {second}");
    assert!(third.contains("selected=mock/alpha"), "route was: {third}");
}
