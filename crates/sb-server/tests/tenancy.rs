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

async fn spawn_switchback_with_store(
    cfg_yaml: &str,
    store: Arc<dyn sb_store::StateStore>,
) -> String {
    let cfg = sb_core::Config::from_yaml(cfg_yaml).unwrap();
    let registry = sb_adapters::AdapterRegistry::from_config(&cfg).unwrap();
    let resolver = sb_credentials::CredentialResolver::from_config(&cfg).unwrap();
    let engine = sb_runtime::Engine::new(
        Arc::new(cfg),
        Arc::new(registry),
        Arc::new(resolver),
        Arc::new(sb_ledger::UsageLedger::in_memory().with_store(store.clone())),
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

fn config(up: &str, tenant_extra: &str) -> String {
    config_with_server(up, "", tenant_extra)
}

fn config_with_server(up: &str, server_extra: &str, tenant_extra: &str) -> String {
    format!(
        r#"
server:
  bind: "127.0.0.1:0"
{server_extra}
tenants:
  - id: acme
{tenant_extra}
api_keys:
  - key: "sk-acme"
    tenant: acme
    project: web
    role: operator
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

fn two_tenant_config(up: &str) -> String {
    format!(
        r#"
server:
  bind: "127.0.0.1:0"
tenants:
  - id: acme
  - id: beta
api_keys:
  - key: "sk-acme"
    tenant: acme
    project: web
    role: operator
  - key: "sk-beta"
    tenant: beta
    project: web
    role: operator
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
async fn tenant_operator_observability_is_scoped_to_own_tenant() {
    let (up, _hits) = spawn_node(0).await;
    let sb = spawn_switchback(&two_tenant_config(&up)).await;

    let acme_resp = chat(&sb, Some("sk-acme")).send().await.unwrap();
    assert_eq!(acme_resp.status(), 200);
    let acme_req = acme_resp
        .headers()
        .get("x-switchback-request-id")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let beta_resp = chat(&sb, Some("sk-beta")).send().await.unwrap();
    assert_eq!(beta_resp.status(), 200);
    let beta_req = beta_resp
        .headers()
        .get("x-switchback-request-id")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    let client = reqwest::Client::new();
    let usage: Value = client
        .get(format!("{sb}/v1/usage"))
        .header("authorization", "Bearer sk-acme")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(usage["requests"], 1);
    assert!(usage["by_tenant"].get("acme").is_some());
    assert!(usage["by_tenant"].get("beta").is_none());

    let traces: Value = client
        .get(format!("{sb}/v1/traces"))
        .header("authorization", "Bearer sk-acme")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(traces["count"], 1);
    assert_eq!(traces["traces"][0]["tenant"], "acme");
    assert_eq!(traces["traces"][0]["request_id"], acme_req);

    let beta_trace = client
        .get(format!("{sb}/v1/traces/{beta_req}"))
        .header("authorization", "Bearer sk-acme")
        .send()
        .await
        .unwrap();
    assert_eq!(beta_trace.status(), 404);
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

#[tokio::test]
async fn tenant_concurrency_limit_is_coordinated_across_store_backed_nodes() {
    let (up, hits) = spawn_node(400).await;
    let yaml = config(
        &up,
        r#"    max_concurrency: 1
"#,
    );
    let store: Arc<dyn sb_store::StateStore> =
        Arc::new(sb_store::SqliteStore::in_memory().unwrap());
    let sb_a = spawn_switchback_with_store(&yaml, store.clone()).await;
    let sb_b = spawn_switchback_with_store(&yaml, store).await;

    let a = tokio::spawn(async move { chat(&sb_a, Some("sk-acme")).send().await.unwrap() });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let b = chat(&sb_b, Some("sk-acme")).send().await.unwrap();
    assert_eq!(
        b.status(),
        429,
        "second node should observe the first node's durable tenant slot"
    );

    assert_eq!(a.await.unwrap().status(), 200);
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn durable_tenant_slot_is_renewed_past_original_ttl() {
    let (up, hits) = spawn_node(600).await;
    let yaml = config_with_server(
        &up,
        r#"  tenant_concurrency_ttl_ms: 100
"#,
        r#"    max_concurrency: 1
"#,
    );
    let store: Arc<dyn sb_store::StateStore> =
        Arc::new(sb_store::SqliteStore::in_memory().unwrap());
    let sb_a = spawn_switchback_with_store(&yaml, store.clone()).await;
    let sb_b = spawn_switchback_with_store(&yaml, store).await;

    let a = tokio::spawn(async move { chat(&sb_a, Some("sk-acme")).send().await.unwrap() });
    tokio::time::sleep(Duration::from_millis(250)).await;

    let b = chat(&sb_b, Some("sk-acme")).send().await.unwrap();
    assert_eq!(
        b.status(),
        429,
        "active durable tenant slots should renew instead of expiring mid-request"
    );

    assert_eq!(a.await.unwrap().status(), 200);
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn tenant_budget_reads_live_durable_usage_from_store() {
    let (up, hits) = spawn_node(0).await;
    let yaml = config(
        &up,
        r#"    budget_usd: 0.50
"#,
    );
    let store: Arc<dyn sb_store::StateStore> =
        Arc::new(sb_store::SqliteStore::in_memory().unwrap());
    let sb = spawn_switchback_with_store(&yaml, store.clone()).await;

    store
        .record_usage(&sb_store::UsageEvent {
            request_id: "seed".into(),
            provider_id: "up".into(),
            model: "m".into(),
            account_id: Some("a".into()),
            tenant: Some("acme".into()),
            cost_micros: 1_000_000,
            input_tokens: 0,
            output_tokens: 0,
            latency_ms: 0,
            streamed: false,
            created_at_ms: sb_store::now_millis(),
        })
        .unwrap();

    let resp = chat(&sb, Some("sk-acme")).send().await.unwrap();
    assert_eq!(resp.status(), 402);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["type"], "tenant_budget_exceeded");
    assert_eq!(
        hits.load(Ordering::SeqCst),
        0,
        "budget denial should read the shared durable store before dispatch"
    );
}
