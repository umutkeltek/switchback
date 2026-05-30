//! Multi-tenancy + quotas (Oracle #4). An API key resolves to a tenant; usage is
//! attributed per tenant; a tenant's hard limits reject before upstream dispatch
//! (budget → 402, concurrency → 429).

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

async fn upstream(State(node): State<Node>, Json(_b): Json<Value>) -> Json<Value> {
    node.hits.fetch_add(1, Ordering::SeqCst);
    if node.delay_ms > 0 {
        tokio::time::sleep(Duration::from_millis(node.delay_ms)).await;
    }
    Json(json!({
        "id": "x", "object": "chat.completion",
        "choices": [{"index":0,"finish_reason":"stop","message":{"role":"assistant","content":"ok"}}],
        "usage": {"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
    }))
}

async fn spawn_node(delay_ms: u64) -> (String, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/chat/completions", post(upstream))
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

fn config(up: &str, tenant_extra: &str) -> String {
    format!(
        r#"
server:
  bind: "127.0.0.1:0"
tenants:
  - id: acme
{tenant_extra}
api_keys:
  - key: "sk-acme"
    tenant: acme
    project: web
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
    )
}

fn chat(base: &str, key: Option<&str>) -> reqwest::RequestBuilder {
    let rb = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({"model":"m","messages":[{"role":"user","content":"hi"}]}));
    match key {
        Some(k) => rb.header("authorization", format!("Bearer {k}")),
        None => rb,
    }
}

#[tokio::test]
async fn api_key_resolves_a_tenant_and_attributes_usage() {
    let (up, _hits) = spawn_node(0).await;
    let sb = spawn_switchback(&config(&up, "")).await;

    // Valid key → 200; usage is attributed to the tenant.
    let ok = chat(&sb, Some("sk-acme")).send().await.unwrap();
    assert_eq!(ok.status(), 200);

    // Read endpoints require the key too once api_keys is configured.
    let auth_get = |url: String| async move {
        reqwest::Client::new()
            .get(url)
            .header("authorization", "Bearer sk-acme")
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap()
    };
    let usage = auth_get(format!("{sb}/v1/usage")).await;
    assert_eq!(
        usage["by_tenant"]["acme"][0], 1,
        "request attributed to tenant acme"
    );

    let tenants = auth_get(format!("{sb}/v1/tenants")).await;
    let acme = &tenants["tenants"][0];
    assert_eq!(acme["id"], "acme");
    assert_eq!(tenants["keys"], 1);

    // Wrong key and no key are both 401 (api_keys is the authoritative list).
    assert_eq!(
        chat(&sb, Some("sk-wrong")).send().await.unwrap().status(),
        401
    );
    assert_eq!(chat(&sb, None).send().await.unwrap().status(), 401);
}

#[tokio::test]
async fn a_tenant_over_budget_is_rejected_before_dispatch() {
    let (up, hits) = spawn_node(0).await;
    // budget_usd: 0 → spent (0) already meets the cap, so every request is a hard
    // 402 BEFORE the upstream is touched.
    let sb = spawn_switchback(&config(&up, "    budget_usd: 0.0")).await;

    let resp = chat(&sb, Some("sk-acme")).send().await.unwrap();
    assert_eq!(resp.status(), 402);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["type"], "tenant_budget_exceeded");
    assert_eq!(
        hits.load(Ordering::SeqCst),
        0,
        "the hard budget cap rejected before any upstream dispatch"
    );
}

#[tokio::test]
async fn tenant_concurrency_limit_returns_429() {
    let (up, hits) = spawn_node(400).await; // slow, so A holds its slot
    let sb = spawn_switchback(&config(&up, "    max_concurrency: 1")).await;

    // A claims the single slot and blocks on the slow upstream.
    let sb_a = sb.clone();
    let a = tokio::spawn(async move { chat(&sb_a, Some("sk-acme")).send().await.unwrap() });
    tokio::time::sleep(Duration::from_millis(100)).await;

    // B, same tenant, exceeds max_concurrency=1 → 429 before dispatch.
    let b = chat(&sb, Some("sk-acme")).send().await.unwrap();
    assert_eq!(b.status(), 429);
    let body: Value = b.json().await.unwrap();
    assert_eq!(body["error"]["type"], "rate_limit_error");

    // A still succeeds; only A reached the upstream.
    assert_eq!(a.await.unwrap().status(), 200);
    assert_eq!(hits.load(Ordering::SeqCst), 1);

    // Slot released after A → a fresh request succeeds.
    assert_eq!(
        chat(&sb, Some("sk-acme")).send().await.unwrap().status(),
        200
    );
}
