//! Execution profiles: `auto/*` is user-facing model UX, but it compiles into
//! the same route planner and explainable decision path as ordinary routes.

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
        "id": "x",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "finish_reason": "stop",
            "message": {
                "role": "assistant",
                "content": format!("served={}", node.tag)
            }
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    }))
}

async fn spawn_node(tag: &'static str) -> (String, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/chat/completions", post(chat))
        .with_state(Node {
            tag,
            hits: hits.clone(),
        });
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

async fn chat_content(base: &str, model: &str) -> Value {
    reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({"model": model, "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn traces(base: &str) -> Value {
    reqwest::Client::new()
        .get(format!("{base}/v1/traces"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

#[tokio::test]
async fn auto_cheap_uses_cost_ordering_over_the_default_route() {
    let (cheap_url, cheap_hits) = spawn_node("cheap").await;
    let (exp_url, exp_hits) = spawn_node("expensive").await;

    let cost_map = std::env::temp_dir().join(format!("sb_auto_cheap_{}.json", std::process::id()));
    std::fs::write(
        &cost_map,
        r#"{"models":[
  {"provider_id":"exp","model_id":"m","input_micros_per_mtok":5000000,"output_micros_per_mtok":25000000},
  {"provider_id":"cheap","model_id":"m","input_micros_per_mtok":140000,"output_micros_per_mtok":280000}
]}"#,
    )
    .unwrap();

    let cfg = format!(
        r#"
server:
  bind: "127.0.0.1:0"
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

    let resp = chat_content(&switchback, "auto/cheap").await;
    assert_eq!(resp["choices"][0]["message"]["content"], "served=cheap");
    assert_eq!(cheap_hits.load(Ordering::SeqCst), 1);
    assert_eq!(exp_hits.load(Ordering::SeqCst), 0);

    let trace_body = traces(&switchback).await;
    let decision = &trace_body["traces"][0]["decision"];
    assert_eq!(decision["profile"], "auto/cheap");
    assert_eq!(decision["strategy"], "auto/cheap");
    assert_eq!(decision["selected"]["target_id"], "cheap/m");
}

#[tokio::test]
async fn auto_coding_prefers_catalog_tagged_models() {
    let (general_url, general_hits) = spawn_node("general").await;
    let (coder_url, coder_hits) = spawn_node("coder").await;

    let cfg = format!(
        r#"
server:
  bind: "127.0.0.1:0"
catalog:
  models:
    - id: general
      provider_id: general
    - id: sonnet
      provider_id: coder
      tags: ["coding"]
providers:
  - id: general
    type: openai_compatible
    base_url: "{general_url}"
    accounts:
      - id: a
        auth: {{ kind: api_key, inline: "k" }}
  - id: coder
    type: openai_compatible
    base_url: "{coder_url}"
    accounts:
      - id: a
        auth: {{ kind: api_key, inline: "k" }}
routes:
  - name: default
    match: {{ model: "*" }}
    targets:
      - "general/general"
      - "coder/sonnet"
"#
    );
    let switchback = spawn_switchback(&cfg).await;

    let resp = chat_content(&switchback, "auto/coding").await;
    assert_eq!(resp["choices"][0]["message"]["content"], "served=coder");
    assert_eq!(general_hits.load(Ordering::SeqCst), 0);
    assert_eq!(coder_hits.load(Ordering::SeqCst), 1);

    let trace_body = traces(&switchback).await;
    let decision = &trace_body["traces"][0]["decision"];
    assert_eq!(decision["profile"], "auto/coding");
    assert_eq!(decision["selected"]["target_id"], "coder/sonnet");
}

#[tokio::test]
async fn auto_private_rejects_aggregator_lanes_without_global_cost_routing() {
    let (agg_url, agg_hits) = spawn_node("agg").await;
    let (direct_url, direct_hits) = spawn_node("direct").await;

    let cost_map =
        std::env::temp_dir().join(format!("sb_auto_private_{}.json", std::process::id()));
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
  cost_map: "{cost_map}"
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

    let resp = chat_content(&switchback, "auto/private").await;
    assert_eq!(resp["choices"][0]["message"]["content"], "served=direct");
    assert_eq!(agg_hits.load(Ordering::SeqCst), 0);
    assert_eq!(direct_hits.load(Ordering::SeqCst), 1);

    let trace_body = traces(&switchback).await;
    let decision = &trace_body["traces"][0]["decision"];
    assert_eq!(decision["profile"], "auto/private");
    assert_eq!(decision["selected"]["target_id"], "direct/m");
    assert!(decision["rejected"]
        .as_array()
        .unwrap()
        .iter()
        .any(|rejected| rejected["target_id"] == "agg/m"));
}
