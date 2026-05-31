//! Gateway-boundary idempotency (Oracle #2 / #7). With `Idempotency-Key`:
//!   - a duplicate non-streaming request replays the EXACT first response
//!     (proven by a per-call counter in the upstream — replay must show call=1),
//!   - a reused key with a different body is a 422,
//!   - a concurrent duplicate (still in flight) is a 409 (single-flight).

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
}

async fn upstream_chat(State(node): State<Node>, Json(_body): Json<Value>) -> Json<Value> {
    let n = node.hits.fetch_add(1, Ordering::SeqCst) + 1;
    if node.delay_ms > 0 {
        tokio::time::sleep(Duration::from_millis(node.delay_ms)).await;
    }
    Json(json!({
        "id": "x", "object": "chat.completion",
        "choices": [{"index":0,"finish_reason":"stop","message":{"role":"assistant","content":format!("call={n}")}}],
        "usage": {"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
    }))
}

async fn spawn_node(delay_ms: u64) -> (String, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/chat/completions", post(upstream_chat))
        .with_state(Node {
            hits: hits.clone(),
            delay_ms,
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), hits)
}

/// Switchback with an in-memory store attached, pointed at `upstream_url`.
async fn spawn_switchback_with_body_persistence(
    upstream_url: &str,
    persist_response_bodies: bool,
) -> String {
    let cfg_yaml = format!(
        r#"
server:
  bind: "127.0.0.1:0"
  idempotency:
    persist_response_bodies: {persist_response_bodies}
providers:
  - id: up
    type: openai_compatible
    base_url: "{upstream_url}"
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
    let store: Arc<dyn sb_store::StateStore> =
        Arc::new(sb_store::SqliteStore::in_memory().unwrap());
    let engine = sb_runtime::Engine::new(
        Arc::new(cfg),
        Arc::new(registry),
        Arc::new(resolver),
        Arc::new(sb_ledger::UsageLedger::in_memory()),
    )
    .with_store(store);
    let app = sb_server::build_app(sb_server::AppState::from_engine(engine));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

/// Switchback with durable idempotency response-body persistence explicitly on.
async fn spawn_switchback(upstream_url: &str) -> String {
    spawn_switchback_with_body_persistence(upstream_url, true).await
}

async fn spawn_switchback_with_tenants(upstream_url: &str) -> String {
    let cfg_yaml = format!(
        r#"
server:
  bind: "127.0.0.1:0"
  idempotency:
    persist_response_bodies: true
providers:
  - id: up
    type: openai_compatible
    base_url: "{upstream_url}"
    accounts:
      - id: a
        auth: {{ kind: api_key, inline: "k" }}
routes:
  - name: default
    match: {{ model: "*" }}
    targets:
      - "up/m"
tenants:
  - id: acme
  - id: globex
api_keys:
  - key: sk-acme
    tenant: acme
  - key: sk-globex
    tenant: globex
"#
    );
    let cfg = sb_core::Config::from_yaml(&cfg_yaml).unwrap();
    let registry = sb_adapters::AdapterRegistry::from_config(&cfg).unwrap();
    let resolver = sb_credentials::CredentialResolver::from_config(&cfg).unwrap();
    let store: Arc<dyn sb_store::StateStore> =
        Arc::new(sb_store::SqliteStore::in_memory().unwrap());
    let engine = sb_runtime::Engine::new(
        Arc::new(cfg),
        Arc::new(registry),
        Arc::new(resolver),
        Arc::new(sb_ledger::UsageLedger::in_memory()),
    )
    .with_store(store);
    let app = sb_server::build_app(sb_server::AppState::from_engine(engine));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

fn req(base: &str, key: &str, content: &str) -> reqwest::RequestBuilder {
    reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .header("idempotency-key", key)
        .json(&json!({"model":"m","messages":[{"role":"user","content":content}]}))
}

fn tenant_req(base: &str, tenant_key: &str) -> reqwest::RequestBuilder {
    reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .header("authorization", format!("Bearer {tenant_key}"))
        .header("idempotency-key", "shared-key")
        .json(&json!({"model":"m","messages":[{"role":"user","content":"same"}]}))
}

#[tokio::test]
async fn durable_replay_requires_explicit_response_body_persistence() {
    let (up, hits) = spawn_node(0).await;
    let sb = spawn_switchback_with_body_persistence(&up, false).await;

    let r1 = req(&sb, "key-no-body-store", "hi").send().await.unwrap();
    assert_eq!(r1.status(), 200);
    assert!(r1.headers().get("idempotent-replayed").is_none());
    let b1: Value = r1.json().await.unwrap();
    assert_eq!(b1["choices"][0]["message"]["content"], "call=1");

    let r2 = req(&sb, "key-no-body-store", "hi").send().await.unwrap();
    assert_eq!(r2.status(), 200);
    assert!(
        r2.headers().get("idempotent-replayed").is_none(),
        "response bodies are not replayed unless the operator explicitly enables persistence"
    );
    let b2: Value = r2.json().await.unwrap();
    assert_eq!(b2["choices"][0]["message"]["content"], "call=2");
    assert_eq!(hits.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn duplicate_non_streaming_request_replays_the_first_response() {
    let (up, hits) = spawn_node(0).await;
    let sb = spawn_switchback(&up).await;

    // First call → executes upstream once, content "call=1".
    let r1 = req(&sb, "key-abc", "hi").send().await.unwrap();
    assert!(r1.headers().get("idempotent-replayed").is_none());
    let b1: Value = r1.json().await.unwrap();
    assert_eq!(b1["choices"][0]["message"]["content"], "call=1");
    assert_eq!(hits.load(Ordering::SeqCst), 1);

    // Duplicate (same key + body) → replays the stored bytes, upstream NOT hit.
    let r2 = req(&sb, "key-abc", "hi").send().await.unwrap();
    assert_eq!(
        r2.headers()
            .get("idempotent-replayed")
            .map(|v| v.to_str().unwrap()),
        Some("true"),
        "replay is flagged"
    );
    let b2: Value = r2.json().await.unwrap();
    assert_eq!(
        b2["choices"][0]["message"]["content"], "call=1",
        "the FIRST response replays — not a re-execution (which would be call=2)"
    );
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "upstream never called a second time"
    );
}

#[tokio::test]
async fn reused_key_with_different_body_is_rejected() {
    let (up, _hits) = spawn_node(0).await;
    let sb = spawn_switchback(&up).await;

    let r1 = req(&sb, "key-xyz", "first").send().await.unwrap();
    assert_eq!(r1.status(), 200);

    // Same key, different body → 422 (Stripe's rule).
    let r2 = req(&sb, "key-xyz", "DIFFERENT").send().await.unwrap();
    assert_eq!(r2.status(), 422);
    let b: Value = r2.json().await.unwrap();
    assert_eq!(b["error"]["type"], "idempotency_error");
}

#[tokio::test]
async fn same_idempotency_key_is_isolated_by_tenant() {
    let (up, hits) = spawn_node(0).await;
    let sb = spawn_switchback_with_tenants(&up).await;

    let acme_first = tenant_req(&sb, "sk-acme").send().await.unwrap();
    assert_eq!(acme_first.status(), 200);
    assert!(acme_first.headers().get("idempotent-replayed").is_none());
    let acme_body: Value = acme_first.json().await.unwrap();
    assert_eq!(acme_body["choices"][0]["message"]["content"], "call=1");

    let globex_first = tenant_req(&sb, "sk-globex").send().await.unwrap();
    assert_eq!(globex_first.status(), 200);
    assert!(globex_first.headers().get("idempotent-replayed").is_none());
    let globex_body: Value = globex_first.json().await.unwrap();
    assert_eq!(
        globex_body["choices"][0]["message"]["content"], "call=2",
        "different tenants get independent first executions"
    );

    let acme_replay = tenant_req(&sb, "sk-acme").send().await.unwrap();
    assert_eq!(
        acme_replay
            .headers()
            .get("idempotent-replayed")
            .map(|v| v.to_str().unwrap()),
        Some("true")
    );
    let acme_replay_body: Value = acme_replay.json().await.unwrap();
    assert_eq!(
        acme_replay_body["choices"][0]["message"]["content"],
        "call=1"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn concurrent_duplicate_is_single_flighted() {
    // Slow upstream so the first request is still in flight when the second lands.
    let (up, hits) = spawn_node(400).await;
    let sb = spawn_switchback(&up).await;

    // Fire A; it claims the key and blocks on the slow upstream.
    let sb_a = sb.clone();
    let a = tokio::spawn(async move { req(&sb_a, "key-race", "hi").send().await.unwrap() });

    // Let A claim the key, then fire B with the same key.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let b = req(&sb, "key-race", "hi").send().await.unwrap();
    assert_eq!(
        b.status(),
        409,
        "concurrent duplicate is rejected while in flight"
    );

    // A still succeeds; upstream was hit exactly once (B never reached it).
    let ra = a.await.unwrap();
    assert_eq!(ra.status(), 200);
    let ba: Value = ra.json().await.unwrap();
    assert_eq!(ba["choices"][0]["message"]["content"], "call=1");
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn no_key_means_no_idempotency() {
    let (up, hits) = spawn_node(0).await;
    let sb = spawn_switchback(&up).await;
    let plain = || {
        reqwest::Client::new()
            .post(format!("{sb}/v1/chat/completions"))
            .json(&json!({"model":"m","messages":[{"role":"user","content":"hi"}]}))
            .send()
    };
    let b1: Value = plain().await.unwrap().json().await.unwrap();
    let b2: Value = plain().await.unwrap().json().await.unwrap();
    // Without a key, each request executes independently (call=1 then call=2).
    assert_eq!(b1["choices"][0]["message"]["content"], "call=1");
    assert_eq!(b2["choices"][0]["message"]["content"], "call=2");
    assert_eq!(hits.load(Ordering::SeqCst), 2);
}
