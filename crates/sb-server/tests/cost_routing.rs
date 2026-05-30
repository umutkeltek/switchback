//! Cost-aware routing end-to-end: a route declares the expensive provider first,
//! but with `cost_aware: true` + a cost map the router sends the request to the
//! cheapest healthy host. Proves the cost map becomes real routing behavior, and
//! that the toggle + the explainable decision reflect it.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};

#[derive(Clone)]
struct Node {
    tag: &'static str,
    hits: Arc<AtomicUsize>,
}

async fn chat(State(node): State<Node>, Json(_body): Json<Value>) -> Json<Value> {
    node.hits.fetch_add(1, Ordering::SeqCst);
    Json(json!({
        "id": "x", "object": "chat.completion",
        "choices": [{"index":0,"finish_reason":"stop","message":{"role":"assistant","content":format!("served={}", node.tag)}}],
        "usage": {"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
    }))
}

async fn spawn_node(tag: &'static str) -> (String, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/chat/completions", post(chat))
        .with_state(Node { tag, hits: hits.clone() });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), hits)
}

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

async fn served_content(base: &str) -> Value {
    let client = reqwest::Client::new();
    client
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({"model":"m","messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

#[tokio::test]
async fn cost_aware_routes_to_the_cheapest_provider_first() {
    let (cheap_url, cheap_hits) = spawn_node("cheap").await;
    let (exp_url, exp_hits) = spawn_node("expensive").await;

    // exp blended = 30/Mtok, cheap = 0.42/Mtok.
    let cost_map = std::env::temp_dir().join("sb_cost_map_e2e.json");
    std::fs::write(
        &cost_map,
        r#"{"models":[
  {"provider_id":"exp","model_id":"m","input_micros_per_mtok":5000000,"output_micros_per_mtok":25000000},
  {"provider_id":"cheap","model_id":"m","input_micros_per_mtok":140000,"output_micros_per_mtok":280000}
]}"#,
    )
    .unwrap();

    // Route declares exp FIRST; cost-aware must flip it to cheap.
    let cfg = format!(
        r#"
server:
  bind: "127.0.0.1:0"
  cost_aware: true
  cost_map: "{cost_map}"
providers:
  - id: exp
    type: openai_compatible
    base_url: "{exp_url}"
    accounts:
      - id: a
        auth: {{ kind: api_key, inline: "k" }}
  - id: cheap
    type: openai_compatible
    base_url: "{cheap_url}"
    accounts:
      - id: a
        auth: {{ kind: api_key, inline: "k" }}
routes:
  - name: default
    match: {{ model: "*" }}
    targets:
      - "exp/m"
      - "cheap/m"
"#,
        cost_map = cost_map.display()
    );
    let switchback = spawn_switchback(&cfg).await;

    let resp = served_content(&switchback).await;
    assert_eq!(
        resp["choices"][0]["message"]["content"], "served=cheap",
        "cost-aware sent the request to the cheap provider"
    );
    assert_eq!(cheap_hits.load(Ordering::SeqCst), 1);
    assert_eq!(exp_hits.load(Ordering::SeqCst), 0, "expensive provider untouched");

    // The explainable decision in the trace reflects the cost-aware choice.
    let client = reqwest::Client::new();
    let traces: Value = client
        .get(format!("{switchback}/v1/traces"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let decision = &traces["traces"][0]["decision"];
    assert_eq!(decision["strategy"], "cost_aware");
    assert_eq!(decision["selected"]["target_id"], "cheap/m");
    assert!(decision["reason"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r.as_str().unwrap().contains("cheapest=cheap/m")));
}

#[tokio::test]
async fn policy_flag_excludes_the_aggregator_lane() {
    let (agg_url, agg_hits) = spawn_node("agg").await;
    let (direct_url, direct_hits) = spawn_node("direct").await;

    // The aggregator host is far cheaper, but the cost map tags it aggregator and
    // the config disallows that lane → routing must pick the pricier direct host.
    let cost_map = std::env::temp_dir().join("sb_cost_map_policy.json");
    std::fs::write(
        &cost_map,
        r#"{
  "providers":[{"id":"agg","aggregator":true},{"id":"direct","aggregator":false}],
  "models":[
    {"provider_id":"agg","model_id":"m","input_micros_per_mtok":100000,"output_micros_per_mtok":200000},
    {"provider_id":"direct","model_id":"m","input_micros_per_mtok":5000000,"output_micros_per_mtok":5000000}
  ]
}"#,
    )
    .unwrap();

    let cfg = format!(
        r#"
server:
  bind: "127.0.0.1:0"
  cost_aware: true
  cost_map: "{cost_map}"
  cost_allow_aggregator: false
providers:
  - id: agg
    type: openai_compatible
    base_url: "{agg_url}"
    accounts:
      - id: a
        auth: {{ kind: api_key, inline: "k" }}
  - id: direct
    type: openai_compatible
    base_url: "{direct_url}"
    accounts:
      - id: a
        auth: {{ kind: api_key, inline: "k" }}
routes:
  - name: default
    match: {{ model: "*" }}
    targets:
      - "agg/m"
      - "direct/m"
"#,
        cost_map = cost_map.display()
    );
    let switchback = spawn_switchback(&cfg).await;
    let resp = served_content(&switchback).await;
    assert_eq!(
        resp["choices"][0]["message"]["content"], "served=direct",
        "the cheaper aggregator lane was excluded by policy"
    );
    assert_eq!(agg_hits.load(Ordering::SeqCst), 0, "aggregator never used");
    assert_eq!(direct_hits.load(Ordering::SeqCst), 1);

    let client = reqwest::Client::new();
    let traces: Value = client
        .get(format!("{switchback}/v1/traces"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(traces["traces"][0]["decision"]["rejected"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["target_id"] == "agg/m" && r["reason"].as_str().unwrap().contains("aggregator")));
}

#[tokio::test]
async fn live_runtime_toggle_changes_routing_without_restart() {
    let (cheap_url, _) = spawn_node("cheap").await;
    let (exp_url, _) = spawn_node("expensive").await;
    let cost_map = std::env::temp_dir().join("sb_cost_map_toggle.json");
    std::fs::write(
        &cost_map,
        r#"{"models":[
  {"provider_id":"exp","model_id":"m","input_micros_per_mtok":5000000,"output_micros_per_mtok":25000000},
  {"provider_id":"cheap","model_id":"m","input_micros_per_mtok":140000,"output_micros_per_mtok":280000}
]}"#,
    )
    .unwrap();
    // cost_aware OFF initially; the cost map is loaded so it can take effect live.
    let cfg = format!(
        r#"
server:
  bind: "127.0.0.1:0"
  cost_map: "{cost_map}"
providers:
  - id: exp
    type: openai_compatible
    base_url: "{exp_url}"
    accounts: [{{ id: a, auth: {{ kind: api_key, inline: "k" }} }}]
  - id: cheap
    type: openai_compatible
    base_url: "{cheap_url}"
    accounts: [{{ id: a, auth: {{ kind: api_key, inline: "k" }} }}]
routes:
  - name: default
    match: {{ model: "*" }}
    targets:
      - "exp/m"
      - "cheap/m"
"#,
        cost_map = cost_map.display()
    );
    let sb = spawn_switchback(&cfg).await;
    let client = reqwest::Client::new();

    // Off → declared order (expensive first).
    assert_eq!(
        served_content(&sb).await["choices"][0]["message"]["content"],
        "served=expensive"
    );

    // Flip cost_aware ON live via the control plane.
    let rt: Value = client
        .patch(format!("{sb}/v1/runtime"))
        .json(&json!({"cost_aware": true}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(rt["cost_aware"], true, "runtime reports the new value");

    // Now cost-aware → the cheap provider, no restart.
    assert_eq!(
        served_content(&sb).await["choices"][0]["message"]["content"],
        "served=cheap",
        "the live toggle changed routing"
    );
}

#[tokio::test]
async fn cost_aware_off_keeps_declared_order() {
    let (cheap_url, _cheap_hits) = spawn_node("cheap").await;
    let (exp_url, exp_hits) = spawn_node("expensive").await;

    // Same providers, cost_aware OFF (default) → declared order wins (exp first).
    let cfg = format!(
        r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: exp
    type: openai_compatible
    base_url: "{exp_url}"
    accounts:
      - id: a
        auth: {{ kind: api_key, inline: "k" }}
  - id: cheap
    type: openai_compatible
    base_url: "{cheap_url}"
    accounts:
      - id: a
        auth: {{ kind: api_key, inline: "k" }}
routes:
  - name: default
    match: {{ model: "*" }}
    targets:
      - "exp/m"
      - "cheap/m"
"#
    );
    let switchback = spawn_switchback(&cfg).await;
    let resp = served_content(&switchback).await;
    assert_eq!(
        resp["choices"][0]["message"]["content"], "served=expensive",
        "with cost_aware off, the declared-first provider is used"
    );
    assert_eq!(exp_hits.load(Ordering::SeqCst), 1);
}
