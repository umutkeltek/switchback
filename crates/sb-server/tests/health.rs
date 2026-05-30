//! Account-pool health visible to routing (Oracle #3). When a provider's only
//! account locks (its upstream keeps auth-failing), the router demotes that
//! target below one that can actually execute — proven by the second request
//! never touching the failed provider — and `/v1/health` reports it degraded.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};

#[derive(Clone)]
struct Node {
    tag: &'static str,
    auth_fail: bool,
    hits: Arc<AtomicUsize>,
}

async fn chat(State(node): State<Node>, Json(_b): Json<Value>) -> (StatusCode, Json<Value>) {
    node.hits.fetch_add(1, Ordering::SeqCst);
    if node.auth_fail {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": {"message": "bad key", "type": "auth"}})),
        );
    }
    (
        StatusCode::OK,
        Json(json!({
            "id": "x", "object": "chat.completion",
            "choices": [{"index":0,"finish_reason":"stop","message":{"role":"assistant","content":format!("served={}", node.tag)}}],
            "usage": {"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
        })),
    )
}

async fn spawn_node(tag: &'static str, auth_fail: bool) -> (String, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/chat/completions", post(chat))
        .with_state(Node {
            tag,
            auth_fail,
            hits: hits.clone(),
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), hits)
}

async fn spawn_switchback(p1: &str, p2: &str) -> String {
    let cfg_yaml = format!(
        r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: p1
    type: openai_compatible
    base_url: "{p1}"
    accounts:
      - id: a
        auth: {{ kind: api_key, inline: "k" }}
  - id: p2
    type: openai_compatible
    base_url: "{p2}"
    accounts:
      - id: a
        auth: {{ kind: api_key, inline: "k" }}
routes:
  - name: default
    match: {{ model: "*" }}
    targets:
      - "p1/m"
      - "p2/m"
"#
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
    format!("http://{addr}")
}

async fn chat_once(base: &str) -> Value {
    reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({"model":"m","messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn get(url: &str) -> Value {
    reqwest::Client::new()
        .get(url)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

#[tokio::test]
async fn a_locked_account_pool_demotes_its_target_in_routing() {
    let (p1_url, p1_hits) = spawn_node("p1", true).await; // always auth-fails
    let (p2_url, p2_hits) = spawn_node("p2", false).await; // healthy
    let sb = spawn_switchback(&p1_url, &p2_url).await;

    // Request 1: p1 is tried first (declared order), auth-fails (locks its only
    // account account-wide), falls over to p2.
    let r1 = chat_once(&sb).await;
    assert_eq!(r1["choices"][0]["message"]["content"], "served=p2");
    assert_eq!(p1_hits.load(Ordering::SeqCst), 1, "p1 attempted once");
    assert_eq!(p2_hits.load(Ordering::SeqCst), 1);

    // /v1/health now shows p1 degraded (0 usable accounts), p2 healthy.
    let health = get(&format!("{sb}/v1/health")).await;
    let p1h = health["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == "p1")
        .unwrap();
    assert_eq!(p1h["accounts_healthy"], 0);
    assert_eq!(p1h["status"], "degraded");

    // Request 2: the router sees p1 has no healthy accounts → demotes it → p2 is
    // selected first and serves, WITHOUT p1 being attempted again.
    let r2 = chat_once(&sb).await;
    assert_eq!(r2["choices"][0]["message"]["content"], "served=p2");
    assert_eq!(
        p1_hits.load(Ordering::SeqCst),
        1,
        "p1 was demoted — not attempted on the second request"
    );
    assert_eq!(p2_hits.load(Ordering::SeqCst), 2);

    // The explainable decision records the demotion.
    let traces = get(&format!("{sb}/v1/traces")).await;
    let decision = &traces["traces"][0]["decision"];
    assert_eq!(decision["selected"]["target_id"], "p2/m");
    assert!(decision["reason"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r.as_str().unwrap().contains("no healthy accounts")));
}

#[tokio::test]
async fn health_endpoint_reports_all_healthy_at_rest() {
    let (p1_url, _) = spawn_node("p1", false).await;
    let (p2_url, _) = spawn_node("p2", false).await;
    let sb = spawn_switchback(&p1_url, &p2_url).await;

    let health = get(&format!("{sb}/v1/health")).await;
    assert_eq!(health["summary"]["providers"], 2);
    assert_eq!(health["summary"]["healthy"], 2);
    for p in health["providers"].as_array().unwrap() {
        assert_eq!(p["status"], "healthy");
        assert_eq!(p["accounts_healthy"], 1);
    }
}
