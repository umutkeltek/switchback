//! Durable usage events (Oracle #2, second slice). With a SQLite state store
//! attached, every request's usage is persisted; `/v1/usage` reflects history
//! across a "restart" (a fresh ledger hydrates its totals from the store), and
//! `/v1/usage/events` exposes the per-event detail.

use std::sync::Arc;

use serde_json::{json, Value};

fn mock_config() -> String {
    r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: mock
    type: mock
    accounts:
      - id: a
        auth: { kind: api_key, inline: "k" }
routes:
  - name: default
    match: { model: "*" }
    targets:
      - "mock/echo"
"#
    .to_string()
}

/// Spawn a switchback whose ledger AND engine share one SQLite store, exactly as
/// `serve` wires it. Returns the base URL.
async fn spawn(cfg_yaml: &str, store: Arc<dyn sb_store::StateStore>) -> String {
    let cfg = sb_core::Config::from_yaml(cfg_yaml).unwrap();
    let registry = sb_adapters::AdapterRegistry::from_config(&cfg).unwrap();
    let resolver = sb_credentials::CredentialResolver::from_config(&cfg).unwrap();
    let ledger = sb_ledger::UsageLedger::in_memory().with_store(store.clone());
    let engine = sb_runtime::Engine::new(
        Arc::new(cfg),
        Arc::new(registry),
        Arc::new(resolver),
        Arc::new(ledger),
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

async fn chat(base: &str) {
    reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({"model":"mock/echo","messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
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
async fn usage_is_durable_across_a_restart() {
    let db = std::env::temp_dir().join("sb_usage_durable.sqlite");
    let _ = std::fs::remove_file(&db);
    let db_str = db.to_string_lossy().to_string();

    // First process: two requests, dual-written to memory + the SQLite store.
    {
        let store: Arc<dyn sb_store::StateStore> =
            Arc::new(sb_store::SqliteStore::open(&db_str).unwrap());
        let sb = spawn(&mock_config(), store).await;
        chat(&sb).await;
        chat(&sb).await;

        let usage = get(&format!("{sb}/v1/usage")).await;
        assert_eq!(usage["requests"], 2, "summary counts both requests");
        // mock provider, two events.
        assert_eq!(usage["by_provider"]["mock"][0], 2);
        assert_eq!(usage["durability"]["status"], "durable");
        assert_eq!(usage["durability"]["store_configured"], true);
        assert_eq!(usage["durability"]["persisted_writes"], 2);
        assert_eq!(usage["durability"]["failed_writes"], 0);

        let reconcile = get(&format!("{sb}/v1/usage/reconcile")).await;
        assert_eq!(reconcile["status"], "ok");
        assert_eq!(reconcile["billing_grade"], true);
        assert_eq!(reconcile["durable"]["requests"], 2);
        assert_eq!(reconcile["ledger"]["requests"], 2);
        assert_eq!(reconcile["memory_fallback"]["requests"], 0);
        assert_eq!(reconcile["delta"]["unexplained_requests"], 0);

        let events = get(&format!("{sb}/v1/usage/events")).await;
        assert_eq!(events["events"].as_array().unwrap().len(), 2);
        assert_eq!(events["events"][0]["provider_id"], "mock");
    }

    // Second process: a brand-new server on the SAME db file. Its ledger hydrates
    // the historical total from the store before serving a single request.
    {
        let store: Arc<dyn sb_store::StateStore> =
            Arc::new(sb_store::SqliteStore::open(&db_str).unwrap());
        let sb = spawn(&mock_config(), store).await;

        let usage = get(&format!("{sb}/v1/usage")).await;
        assert_eq!(
            usage["requests"], 2,
            "the restarted process sees the durable historical total"
        );

        // One more request → 3 total, no double-counting.
        chat(&sb).await;
        let usage = get(&format!("{sb}/v1/usage")).await;
        assert_eq!(usage["requests"], 3);
        assert_eq!(usage["by_provider"]["mock"][0], 3);

        let events = get(&format!("{sb}/v1/usage/events")).await;
        assert_eq!(
            events["events"].as_array().unwrap().len(),
            3,
            "all three events persisted across the restart"
        );
    }
}

#[tokio::test]
async fn usage_events_disabled_without_a_store() {
    // No store attached → /v1/usage/events reports disabled, /v1/usage still works.
    let cfg = sb_core::Config::from_yaml(&mock_config()).unwrap();
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
    let sb = format!("http://{addr}");

    let events = get(&format!("{sb}/v1/usage/events")).await;
    assert_eq!(events["persistence"], "disabled");
    let usage = get(&format!("{sb}/v1/usage")).await;
    assert_eq!(usage["durability"]["status"], "memory_only");
    assert_eq!(usage["durability"]["store_configured"], false);
    let reconcile = get(&format!("{sb}/v1/usage/reconcile")).await;
    assert_eq!(reconcile["status"], "degraded");
    assert_eq!(reconcile["billing_grade"], false);
    assert_eq!(reconcile["issues"][0], "state_store_disabled");
}
