//! Admission control + bounded backpressure (Oracle #8). A global in-flight cap
//! queues bursts (bounded wait) and sheds with 503 past the admission timeout;
//! the collect path refuses to buffer an over-cap non-streaming response.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};

#[derive(Clone)]
struct Node {
    hits: Arc<AtomicUsize>,
    delay_ms: u64,
    content: String,
}

async fn upstream(State(node): State<Node>, Json(_b): Json<Value>) -> Json<Value> {
    node.hits.fetch_add(1, Ordering::SeqCst);
    if node.delay_ms > 0 {
        tokio::time::sleep(Duration::from_millis(node.delay_ms)).await;
    }
    Json(json!({
        "id": "x", "object": "chat.completion",
        "choices": [{"index":0,"finish_reason":"stop","message":{"role":"assistant","content":node.content}}],
        "usage": {"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
    }))
}

async fn spawn_node(delay_ms: u64, content: &str) -> (String, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/chat/completions", post(upstream))
        .with_state(Node {
            hits: hits.clone(),
            delay_ms,
            content: content.to_string(),
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), hits)
}

async fn spawn_switchback(up: &str, server_extra: &str) -> String {
    let cfg_yaml = format!(
        r#"
server:
  bind: "127.0.0.1:0"
{server_extra}
providers:
  - id: up
    type: openai_compatible
    base_url: "{up}"
    accounts:
      - id: a
        auth: {{ kind: api_key, inline: "k" }}
routes:
  - name: default
    match: {{ model: "*" }}
    targets:
      - "up/m"
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

async fn spawn_switchback_with_store(
    up: &str,
    server_extra: &str,
    store: Arc<dyn sb_store::StateStore>,
) -> String {
    let cfg_yaml = format!(
        r#"
server:
  bind: "127.0.0.1:0"
{server_extra}
providers:
  - id: up
    type: openai_compatible
    base_url: "{up}"
    accounts:
      - id: a
        auth: {{ kind: api_key, inline: "k" }}
routes:
  - name: default
    match: {{ model: "*" }}
    targets:
      - "up/m"
"#
    );
    let cfg = sb_core::Config::from_yaml(&cfg_yaml).unwrap();
    let registry = sb_adapters::AdapterRegistry::from_config(&cfg).unwrap();
    let resolver = sb_credentials::CredentialResolver::from_config(&cfg).unwrap();
    let engine = sb_runtime::Engine::new(
        Arc::new(cfg),
        Arc::new(registry),
        Arc::new(resolver),
        Arc::new(sb_ledger::UsageLedger::in_memory().with_store(store.clone())),
    )
    .with_store(store);
    let state = sb_server::AppState::from_engine(engine);
    let app = sb_server::build_app(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

fn chat(base: &str) -> reqwest::RequestBuilder {
    reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({"model":"m","messages":[{"role":"user","content":"hi"}]}))
}

#[tokio::test]
async fn over_capacity_requests_are_shed_with_503() {
    // One global slot, a 50ms admission timeout, a 400ms upstream.
    let (up, hits) = spawn_node(400, "ok").await;
    let sb = spawn_switchback(&up, "  max_concurrency: 1\n  admission_timeout_ms: 50").await;

    // A grabs the only slot and holds it for ~400ms.
    let sb_a = sb.clone();
    let a = tokio::spawn(async move { chat(&sb_a).send().await.unwrap() });
    tokio::time::sleep(Duration::from_millis(100)).await;

    // B waits 50ms for a slot, never gets one → 503 (shed).
    let b = chat(&sb).send().await.unwrap();
    assert_eq!(b.status(), 503);
    let body: Value = b.json().await.unwrap();
    assert_eq!(body["error"]["type"], "overloaded");

    // A still completes; only A reached the upstream.
    assert_eq!(a.await.unwrap().status(), 200);
    assert_eq!(hits.load(Ordering::SeqCst), 1, "B was shed before dispatch");
}

#[tokio::test]
async fn global_admission_limit_is_coordinated_across_store_backed_nodes() {
    let (up, hits) = spawn_node(400, "ok").await;
    let store: Arc<dyn sb_store::StateStore> =
        Arc::new(sb_store::SqliteStore::in_memory().unwrap());
    let extra = "  max_concurrency: 1\n  admission_timeout_ms: 50\n";
    let sb_a = spawn_switchback_with_store(&up, extra, store.clone()).await;
    let sb_b = spawn_switchback_with_store(&up, extra, store).await;

    let a = tokio::spawn(async move { chat(&sb_a).send().await.unwrap() });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let b = chat(&sb_b).send().await.unwrap();
    assert_eq!(
        b.status(),
        503,
        "second node should observe the first node's durable admission slot"
    );
    let body: Value = b.json().await.unwrap();
    assert_eq!(body["error"]["type"], "overloaded");

    assert_eq!(a.await.unwrap().status(), 200);
    assert_eq!(hits.load(Ordering::SeqCst), 1, "B was shed before dispatch");
}

#[tokio::test]
async fn durable_global_admission_slot_is_renewed_past_original_ttl() {
    let (up, hits) = spawn_node(600, "ok").await;
    let store: Arc<dyn sb_store::StateStore> =
        Arc::new(sb_store::SqliteStore::in_memory().unwrap());
    let extra = "  max_concurrency: 1\n  admission_timeout_ms: 50\n  admission_slot_ttl_ms: 100\n";
    let sb_a = spawn_switchback_with_store(&up, extra, store.clone()).await;
    let sb_b = spawn_switchback_with_store(&up, extra, store).await;

    let a = tokio::spawn(async move { chat(&sb_a).send().await.unwrap() });
    tokio::time::sleep(Duration::from_millis(250)).await;

    let b = chat(&sb_b).send().await.unwrap();
    assert_eq!(
        b.status(),
        503,
        "active durable admission slots should renew instead of expiring mid-request"
    );

    assert_eq!(a.await.unwrap().status(), 200);
    assert_eq!(hits.load(Ordering::SeqCst), 1, "B was shed before dispatch");
}

#[tokio::test]
async fn a_queued_request_proceeds_and_reports_its_wait() {
    // One slot, a generous 5s timeout, a 200ms upstream — B queues behind A,
    // then proceeds once A releases the slot.
    let (up, hits) = spawn_node(200, "ok").await;
    let sb = spawn_switchback(&up, "  max_concurrency: 1\n  admission_timeout_ms: 5000").await;

    let sb_a = sb.clone();
    let a = tokio::spawn(async move { chat(&sb_a).send().await.unwrap() });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let b = chat(&sb).send().await.unwrap();
    assert_eq!(b.status(), 200, "B queued, then was admitted");
    let queued: u64 = b
        .headers()
        .get("x-switchback-queue-ms")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    assert!(
        queued > 0,
        "B reports a non-zero admission queue wait (got {queued}ms)"
    );

    assert_eq!(a.await.unwrap().status(), 200);
    assert_eq!(hits.load(Ordering::SeqCst), 2, "both eventually dispatched");
}

#[tokio::test]
async fn cross_node_global_admission_queue_waits_for_shared_slot() {
    let (up, hits) = spawn_node(200, "ok").await;
    let store: Arc<dyn sb_store::StateStore> =
        Arc::new(sb_store::SqliteStore::in_memory().unwrap());
    let extra = "  max_concurrency: 1\n  admission_timeout_ms: 5000\n";
    let sb_a = spawn_switchback_with_store(&up, extra, store.clone()).await;
    let sb_b = spawn_switchback_with_store(&up, extra, store).await;

    let a = tokio::spawn(async move { chat(&sb_a).send().await.unwrap() });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let b = chat(&sb_b).send().await.unwrap();
    assert_eq!(b.status(), 200, "B queued on the shared store slot");
    let queued: u64 = b
        .headers()
        .get("x-switchback-queue-ms")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    assert!(
        queued > 0,
        "B reports a non-zero cross-node admission wait (got {queued}ms)"
    );

    assert_eq!(a.await.unwrap().status(), 200);
    assert_eq!(hits.load(Ordering::SeqCst), 2, "both eventually dispatched");
}

#[tokio::test]
async fn collect_path_refuses_an_over_cap_response() {
    // Upstream returns 200 chars; the cap is 10 bytes → the collect path aborts.
    let big = "x".repeat(200);
    let (up, hits) = spawn_node(0, &big).await;
    let sb = spawn_switchback(&up, "  max_response_bytes: 10").await;

    let resp = chat(&sb).send().await.unwrap();
    assert_eq!(
        resp.status(),
        502,
        "over-cap response is aborted, not buffered"
    );
    let body: Value = resp.json().await.unwrap();
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("max_response_bytes"));
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn no_limits_means_no_admission_overhead() {
    let (up, _hits) = spawn_node(0, "ok").await;
    let sb = spawn_switchback(&up, "").await;
    let resp = chat(&sb).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    // No queue header when there's no global cap.
    assert!(resp.headers().get("x-switchback-queue-ms").is_none());
}
