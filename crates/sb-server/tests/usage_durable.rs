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

fn mock_config_with_project_key() -> String {
    r#"
server:
  bind: "127.0.0.1:0"
tenants:
  - id: acme
api_keys:
  - key: "tenant-key"
    tenant: acme
    project: api
    role: operator
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

async fn chat_with_key(base: &str, key: &str) {
    reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .bearer_auth(key)
        .json(&json!({"model":"mock/echo","messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
}

async fn chat_with_session(base: &str, session_id: &str) -> String {
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .header("x-switchback-session-id", session_id)
        .json(&json!({"model":"mock/echo","messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .unwrap();
    let req_id = resp
        .headers()
        .get("x-switchback-request-id")
        .expect("response must carry request id")
        .to_str()
        .unwrap()
        .to_string();
    let _ = resp.json::<Value>().await.unwrap();
    req_id
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

async fn get_with_key(url: &str, key: &str) -> Value {
    reqwest::Client::new()
        .get(url)
        .bearer_auth(key)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

fn unique_db(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "{name}-{}-{}.sqlite",
        std::process::id(),
        sb_store::now_millis()
    ))
}

#[tokio::test]
async fn usage_is_durable_across_a_restart() {
    let db = unique_db("sb_usage_durable");
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
async fn usage_events_include_api_key_project_dimension() {
    let store: Arc<dyn sb_store::StateStore> =
        Arc::new(sb_store::SqliteStore::in_memory().unwrap());
    let sb = spawn(&mock_config_with_project_key(), store).await;

    chat_with_key(&sb, "tenant-key").await;

    let events = get_with_key(&format!("{sb}/v1/usage/events"), "tenant-key").await;
    assert_eq!(events["events"].as_array().unwrap().len(), 1);
    assert_eq!(events["events"][0]["tenant"], "acme");
    assert_eq!(events["events"][0]["project"], "api");
    assert_eq!(events["events"][0]["provider_id"], "mock");
    assert_eq!(events["events"][0]["model"], "echo");
}

#[tokio::test]
async fn traces_and_sessions_are_durable_across_a_restart() {
    let db = unique_db("sb_trace_durable");
    let _ = std::fs::remove_file(&db);
    let db_str = db.to_string_lossy().to_string();
    let req_id;

    {
        let store: Arc<dyn sb_store::StateStore> =
            Arc::new(sb_store::SqliteStore::open(&db_str).unwrap());
        let sb = spawn(&mock_config(), store).await;
        req_id = chat_with_session(&sb, "sess-durable").await;

        let traces = get(&format!(
            "{sb}/v1/traces?session_id=sess-durable&model=mock/echo&status=200"
        ))
        .await;
        assert_eq!(traces["source"]["kind"], "state_store");
        assert_eq!(traces["count"], 1);
        assert_eq!(traces["traces"][0]["request_id"], req_id);
        assert_eq!(traces["traces"][0]["session_id"], "sess-durable");

        let preview = get(&format!("{sb}/v1/traces/{req_id}/route-preview")).await;
        assert_eq!(preview["source_request_id"], req_id);
        assert_eq!(preview["diff"]["selected_changed"], false);
        assert_eq!(preview["diff"]["original_selected"], "mock/echo");
        assert_eq!(preview["diff"]["current_selected"], "mock/echo");
    }

    {
        let store: Arc<dyn sb_store::StateStore> =
            Arc::new(sb_store::SqliteStore::open(&db_str).unwrap());
        let sb = spawn(&mock_config(), store).await;

        let session = get(&format!("{sb}/v1/sessions/sess-durable")).await;
        assert_eq!(session["source"]["kind"], "state_store");
        assert_eq!(session["session"]["session_id"], "sess-durable");
        assert_eq!(session["session"]["request_count"], 1);
        assert_eq!(session["session"]["models"], json!(["mock/echo"]));
        assert_eq!(session["traces"][0]["request_id"], req_id);

        let traces = get(&format!("{sb}/v1/sessions/sess-durable/traces")).await;
        assert_eq!(traces["source"]["kind"], "state_store");
        assert_eq!(traces["count"], 1);
        assert_eq!(traces["traces"][0]["request_id"], req_id);
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
