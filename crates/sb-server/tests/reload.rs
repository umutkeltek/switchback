//! Compiled-snapshot hot-reload end-to-end. `POST /v1/reload` re-reads the
//! config file, recompiles a fresh immutable snapshot (registry + resolver +
//! runtime), bumps the revision, and swaps it atomically. Two guarantees:
//!   1. A reload changes routing without a restart and bumps `x-switchback-revision`.
//!   2. A request that pinned an older snapshot keeps serving against it even if a
//!      reload swaps the snapshot mid-flight (per-request revision pinning).

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

/// Build a switchback that knows its config file, so `POST /v1/reload` works.
async fn spawn_switchback_from_file(cfg_path: &std::path::Path) -> String {
    let cfg = sb_core::Config::from_path(cfg_path).unwrap();
    let registry = sb_adapters::AdapterRegistry::from_config(&cfg).unwrap();
    let resolver = sb_credentials::CredentialResolver::from_config(&cfg).unwrap();
    let state = sb_server::AppState::new(
        Arc::new(cfg),
        Arc::new(registry),
        Arc::new(resolver),
        Arc::new(sb_ledger::UsageLedger::in_memory()),
    )
    .with_config_path(cfg_path.to_path_buf());
    let app = sb_server::build_app(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

fn config_pointing_at(target_url: &str) -> String {
    format!(
        r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: up
    type: openai_compatible
    base_url: "{target_url}"
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

async fn served(base: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({"model":"m","messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn reload_swaps_config_and_bumps_revision() {
    let (alpha_url, alpha_hits) = spawn_node("alpha", 0).await;
    let (beta_url, beta_hits) = spawn_node("beta", 0).await;

    // Start pointed at alpha. Write to a unique file so the reload re-reads it.
    let cfg_path = std::env::temp_dir().join("sb_reload_swap.yaml");
    std::fs::write(&cfg_path, config_pointing_at(&alpha_url)).unwrap();
    let sb = spawn_switchback_from_file(&cfg_path).await;

    // Revision 1 serves alpha.
    let resp = served(&sb).await;
    assert_eq!(
        resp.headers().get("x-switchback-revision").unwrap(),
        "1",
        "first request is pinned to revision 1"
    );
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["choices"][0]["message"]["content"], "served=alpha");

    // Rewrite the file to point at beta, then hot-reload.
    std::fs::write(&cfg_path, config_pointing_at(&beta_url)).unwrap();
    let reload: Value = reqwest::Client::new()
        .post(format!("{sb}/v1/reload"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(reload["ok"], true);
    assert_eq!(reload["revision"], 2, "reload bumps the revision");

    // Revision 2 serves beta — no restart.
    let resp = served(&sb).await;
    assert_eq!(
        resp.headers().get("x-switchback-revision").unwrap(),
        "2",
        "post-reload request is pinned to revision 2"
    );
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body["choices"][0]["message"]["content"], "served=beta",
        "the reloaded config changed routing live"
    );

    assert_eq!(
        alpha_hits.load(Ordering::SeqCst),
        1,
        "alpha served only the pre-reload request"
    );
    assert_eq!(
        beta_hits.load(Ordering::SeqCst),
        1,
        "beta served only the post-reload request"
    );

    // GET /v1/runtime now reports the new revision too.
    let rt: Value = reqwest::Client::new()
        .get(format!("{sb}/v1/runtime"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(rt["revision"], 2);
}

#[tokio::test]
async fn in_flight_request_pins_its_snapshot_across_a_reload() {
    // alpha is SLOW so a request that pins revision 1 is still mid-flight when we
    // reload to revision 2 (which points at beta). The pinned request must finish
    // against alpha — per-request snapshot pinning, not a torn read.
    let (alpha_url, alpha_hits) = spawn_node("alpha", 400).await;
    let (beta_url, beta_hits) = spawn_node("beta", 0).await;

    let cfg_path = std::env::temp_dir().join("sb_reload_inflight.yaml");
    std::fs::write(&cfg_path, config_pointing_at(&alpha_url)).unwrap();
    let sb = spawn_switchback_from_file(&cfg_path).await;

    // Fire the slow request; it pins revision 1 (alpha) and blocks on the upstream.
    let sb_for_task = sb.clone();
    let inflight = tokio::spawn(async move { served(&sb_for_task).await });

    // Let it pin + start the upstream call, then reload to beta underneath it.
    tokio::time::sleep(Duration::from_millis(100)).await;
    std::fs::write(&cfg_path, config_pointing_at(&beta_url)).unwrap();
    let reload: Value = reqwest::Client::new()
        .post(format!("{sb}/v1/reload"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(reload["revision"], 2);

    // The in-flight request still completes against alpha at revision 1.
    let resp = inflight.await.unwrap();
    assert_eq!(
        resp.headers().get("x-switchback-revision").unwrap(),
        "1",
        "the in-flight request kept its pinned revision through the reload"
    );
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body["choices"][0]["message"]["content"], "served=alpha",
        "the in-flight request was served by the snapshot it pinned, not the reloaded one"
    );

    // A fresh request now lands on beta at revision 2.
    let resp = served(&sb).await;
    assert_eq!(resp.headers().get("x-switchback-revision").unwrap(), "2");
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["choices"][0]["message"]["content"], "served=beta");

    assert_eq!(
        alpha_hits.load(Ordering::SeqCst),
        1,
        "alpha served the in-flight request"
    );
    assert_eq!(
        beta_hits.load(Ordering::SeqCst),
        1,
        "beta served the post-reload request"
    );
}
