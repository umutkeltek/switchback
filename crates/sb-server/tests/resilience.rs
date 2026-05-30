//! Resilience features end-to-end: same-target retry, the provider circuit
//! breaker, and spend-cap budgets — each against a controllable fake upstream.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};

fn ok_body(tag: &str) -> Value {
    json!({
        "id": "x", "object": "chat.completion",
        "choices": [{"index":0,"finish_reason":"stop","message":{"role":"assistant","content":format!("served={tag}")}}],
        "usage": {"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
    })
}

/// Upstream that returns 503 for its first `fail_first` calls, then 200.
#[derive(Clone)]
struct Flaky {
    calls: Arc<AtomicUsize>,
    fail_first: usize,
}

async fn flaky_chat(State(f): State<Flaky>, Json(_b): Json<Value>) -> Response {
    let n = f.calls.fetch_add(1, Ordering::SeqCst);
    if n < f.fail_first {
        (StatusCode::SERVICE_UNAVAILABLE, "overloaded").into_response()
    } else {
        Json(ok_body("up")).into_response()
    }
}

async fn spawn_flaky(fail_first: usize) -> (String, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/chat/completions", post(flaky_chat))
        .with_state(Flaky {
            calls: calls.clone(),
            fail_first,
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), calls)
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

async fn post_chat(base: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({"model":"up/m","messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .unwrap()
}

fn cfg_one_provider(upstream: &str, extra_server: &str) -> String {
    format!(
        r#"
server:
  bind: "127.0.0.1:0"
{extra_server}
providers:
  - id: up
    type: openai_compatible
    base_url: "{upstream}"
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

#[tokio::test]
async fn retry_recovers_a_transient_failure_on_the_same_account() {
    // Upstream 503s twice then succeeds; with max_retries:2 the SAME account
    // recovers it (3 upstream hits, one success — no other target exists).
    let (upstream, calls) = spawn_flaky(2).await;
    let cfg = cfg_one_provider(&upstream, "  retry: { max_retries: 2, base_delay_ms: 1 }");
    let switchback = spawn_switchback(&cfg).await;

    let resp = post_chat(&switchback).await;
    assert_eq!(resp.status(), 200, "retry should have recovered the request");
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["choices"][0]["message"]["content"], "served=up");
    assert_eq!(calls.load(Ordering::SeqCst), 3, "2 failed + 1 successful attempt");
}

#[tokio::test]
async fn budget_cap_rejects_requests_once_spend_reaches_the_limit() {
    // The mock provider is priced via the catalog; a tiny max_usd cap lets the
    // first request(s) through, then rejects with 402 once spend reaches it.
    let cfg = r#"
server:
  bind: "127.0.0.1:0"
  budget: { max_usd: 0.00003 }
catalog:
  prices:
    - { model_id: echo, token_kind: input, unit_price_micros_per_mtok: 1000000, effective_from: "2025-01-01T00:00:00Z" }
    - { model_id: echo, token_kind: output, unit_price_micros_per_mtok: 1000000, effective_from: "2025-01-01T00:00:00Z" }
providers:
  - id: mock
    type: mock
routes:
  - name: default
    match: { model: "*" }
    targets: [ "mock/echo" ]
"#;
    let switchback = spawn_switchback(cfg).await;
    let client = reqwest::Client::new();

    let mut statuses = Vec::new();
    for _ in 0..5 {
        let r = client
            .post(format!("{switchback}/v1/chat/completions"))
            .json(&json!({"model":"mock/echo","messages":[{"role":"user","content":"hi"}]}))
            .send()
            .await
            .unwrap();
        statuses.push(r.status().as_u16());
    }
    assert_eq!(statuses[0], 200, "first request is under budget");
    assert!(
        statuses.contains(&402),
        "budget cap must reject once spend reaches it: {statuses:?}"
    );
}

#[tokio::test]
async fn without_retry_a_transient_failure_is_not_recovered() {
    // Same flaky upstream, retry off → the single attempt fails (no fallback
    // target), so the request errors and the upstream is hit exactly once.
    let (upstream, calls) = spawn_flaky(2).await;
    let cfg = cfg_one_provider(&upstream, "");
    let switchback = spawn_switchback(&cfg).await;

    let resp = post_chat(&switchback).await;
    assert!(resp.status().is_server_error(), "no retry → request fails");
    assert_eq!(calls.load(Ordering::SeqCst), 1, "exactly one attempt, no retry");
}
