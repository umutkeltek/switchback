//! Latency-aware routing end-to-end: one upstream is slow, one fast. After the
//! tracker has sampled both (cold targets are explored first), subsequent
//! requests converge on the fast host. Proves observed latency feeds routing.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};

#[derive(Clone)]
struct Node {
    tag: &'static str,
    delay_ms: u64,
    hits: Arc<AtomicUsize>,
}

async fn chat(State(node): State<Node>, Json(_body): Json<Value>) -> Json<Value> {
    if node.delay_ms > 0 {
        tokio::time::sleep(Duration::from_millis(node.delay_ms)).await;
    }
    node.hits.fetch_add(1, Ordering::SeqCst);
    Json(json!({
        "id": "x", "object": "chat.completion",
        "choices": [{"index":0,"finish_reason":"stop","message":{"role":"assistant","content":format!("served={}", node.tag)}}],
        "usage": {"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
    }))
}

async fn spawn_node(tag: &'static str, delay_ms: u64) -> (String, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/chat/completions", post(chat))
        .with_state(Node {
            tag,
            delay_ms,
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

async fn served(base: &str, client: &reqwest::Client) -> String {
    let resp: Value = client
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({"model":"m","messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    resp["choices"][0]["message"]["content"]
        .as_str()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn latency_aware_converges_to_the_fast_host() {
    let (slow_url, _) = spawn_node("slow", 200).await;
    let (fast_url, _) = spawn_node("fast", 0).await;

    // Route declares slow FIRST; latency-aware must converge to fast once both
    // are measured. fill_first is irrelevant here — these are distinct targets.
    let cfg = format!(
        r#"
server:
  bind: "127.0.0.1:0"
  latency_aware: true
providers:
  - id: slow
    type: openai_compatible
    base_url: "{slow_url}"
    accounts:
      - id: a
        auth: {{ kind: api_key, inline: "k" }}
  - id: fast
    type: openai_compatible
    base_url: "{fast_url}"
    accounts:
      - id: a
        auth: {{ kind: api_key, inline: "k" }}
routes:
  - name: default
    match: {{ model: "*" }}
    targets:
      - "slow/m"
      - "fast/m"
"#
    );
    let switchback = spawn_switchback(&cfg).await;
    let client = reqwest::Client::new();

    // Warm-up: cold targets are explored first, so two requests sample both
    // (slow then fast). After that the EWMA strongly favors fast.
    let _ = served(&switchback, &client).await;
    let _ = served(&switchback, &client).await;

    // Now several requests in a row should all land on the fast host.
    for _ in 0..3 {
        assert_eq!(
            served(&switchback, &client).await,
            "served=fast",
            "latency-aware should route to the fast host after warm-up"
        );
    }
}
