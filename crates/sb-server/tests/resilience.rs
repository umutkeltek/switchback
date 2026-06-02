//! Resilience features end-to-end: same-target retry, the provider circuit
//! breaker, and spend-cap budgets — each against a controllable fake upstream.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

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
    assert_eq!(
        resp.status(),
        200,
        "retry should have recovered the request"
    );
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["choices"][0]["message"]["content"], "served=up");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "2 failed + 1 successful attempt"
    );
}

#[derive(Clone)]
struct Delayed {
    tag: &'static str,
    delay_ms: u64,
}

async fn delayed_chat(State(d): State<Delayed>, Json(_b): Json<Value>) -> Json<Value> {
    if d.delay_ms > 0 {
        tokio::time::sleep(Duration::from_millis(d.delay_ms)).await;
    }
    Json(ok_body(d.tag))
}

async fn spawn_delayed(tag: &'static str, delay_ms: u64) -> String {
    let app = Router::new()
        .route("/chat/completions", post(delayed_chat))
        .with_state(Delayed { tag, delay_ms });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn hedge_returns_the_fast_providers_response() {
    // Route declares the slow provider first; with hedging both are raced and
    // the fast one's response wins.
    let slow = spawn_delayed("slow", 300).await;
    let fast = spawn_delayed("fast", 0).await;
    let cfg = format!(
        r#"
server:
  bind: "127.0.0.1:0"
  hedge: {{ enabled: true, delay_ms: 10, max_parallel: 2 }}
providers:
  - id: slow
    type: openai_compatible
    base_url: "{slow}"
    accounts:
      - id: a
        auth: {{ kind: api_key, inline: "k" }}
  - id: fast
    type: openai_compatible
    base_url: "{fast}"
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
    let sb = spawn_switchback(&cfg).await;
    let client = reqwest::Client::new();
    let response = client
        .post(format!("{sb}/v1/chat/completions"))
        .json(&json!({"model":"m","messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .unwrap();
    let request_id = response
        .headers()
        .get("x-switchback-request-id")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let resp: Value = response.json().await.unwrap();
    assert_eq!(
        resp["choices"][0]["message"]["content"], "served=fast",
        "hedge should return the fast provider's response, not wait for the slow one"
    );

    let trace: Value = client
        .get(format!("{sb}/v1/traces/{request_id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let attempts = trace["attempts"].as_array().unwrap();
    assert_eq!(
        attempts.len(),
        2,
        "hedge traces must include the winner and the canceled loser"
    );
    assert!(
        attempts
            .iter()
            .any(|attempt| attempt["class"] == "hedge_cancelled"),
        "pending hedge losers should be explicitly visible"
    );
}

#[tokio::test]
async fn failed_hedge_attempt_locks_its_account() {
    // One hedge candidate always 503s, the other succeeds. The failing attempt
    // must lock its account (and record the breaker) instead of silently
    // dropping the error — otherwise a later sequential fallback would re-pick
    // the known-bad account. The healthy candidate still wins the race.
    let (bad, _bad_calls) = spawn_flaky(usize::MAX).await;
    let good = spawn_delayed("good", 20).await;
    let cfg = format!(
        r#"
server:
  bind: "127.0.0.1:0"
  hedge: {{ enabled: true, delay_ms: 10, max_parallel: 2 }}
providers:
  - id: bad
    type: openai_compatible
    base_url: "{bad}"
    accounts:
      - id: a
        auth: {{ kind: api_key, inline: "k" }}
  - id: good
    type: openai_compatible
    base_url: "{good}"
    accounts:
      - id: a
        auth: {{ kind: api_key, inline: "k" }}
routes:
  - name: default
    match: {{ model: "*" }}
    targets:
      - "bad/m"
      - "good/m"
"#
    );
    let sb = spawn_switchback(&cfg).await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{sb}/v1/chat/completions"))
        .json(&json!({"model":"m","messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "the healthy hedge candidate should win");
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["choices"][0]["message"]["content"], "served=good");

    let health: Value = client
        .get(format!("{sb}/v1/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let bad_provider = health["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == "bad")
        .expect("bad provider present in health view");
    let locks = bad_provider["accounts"][0]["locks"].as_array().unwrap();
    assert!(
        !locks.is_empty(),
        "a failed hedge attempt must lock its account, got: {bad_provider}"
    );
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
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "exactly one attempt, no retry"
    );
}
