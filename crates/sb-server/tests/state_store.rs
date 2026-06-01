//! Durable control-plane state store (Oracle #2, first slice). A file-backed
//! SQLite store records every published config revision + an audit row per
//! bootstrap/reload/runtime-change, surfaced at `/v1/revisions` and `/v1/audit`.
//! Metadata only (revision, config hash, source, timestamp) — no config body.

use std::sync::Arc;

use serde_json::{json, Value};

fn mock_config(extra_server: &str) -> String {
    format!(
        r#"
server:
  bind: "127.0.0.1:0"
{extra_server}
providers:
  - id: mock
    type: mock
    accounts:
      - id: a
        auth: {{ kind: api_key, inline: "k" }}
routes:
  - name: default
    match: {{ model: "*" }}
    targets:
      - "mock/echo"
"#
    )
}

/// Build a switchback whose engine has a file-backed SQLite store attached —
/// the same wiring `serve` does when `server.state_store` is set.
async fn spawn_with_store(cfg_path: &std::path::Path, db_path: &str) -> String {
    let cfg = sb_core::Config::from_path(cfg_path).unwrap();
    let registry = sb_adapters::AdapterRegistry::from_config(&cfg).unwrap();
    let resolver = sb_credentials::CredentialResolver::from_config(&cfg).unwrap();
    let store = sb_store::SqliteStore::open(db_path).unwrap();
    let engine = sb_runtime::Engine::new(
        Arc::new(cfg),
        Arc::new(registry),
        Arc::new(resolver),
        Arc::new(sb_ledger::UsageLedger::in_memory()),
    )
    .with_store(Arc::new(store));
    engine.set_config_path(cfg_path.to_path_buf());
    let app = sb_server::build_app(sb_server::AppState::from_engine(engine));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

async fn get_json(url: &str) -> Value {
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
async fn revisions_and_audit_accumulate_across_publishes() {
    let cfg_path = std::env::temp_dir().join("sb_state_store_cfg.yaml");
    let db_path = std::env::temp_dir().join("sb_state_store.sqlite");
    // Start clean so revision numbering is deterministic for the assertions.
    let _ = std::fs::remove_file(&db_path);
    std::fs::write(&cfg_path, mock_config("")).unwrap();
    let db = db_path.to_string_lossy().to_string();
    let sb = spawn_with_store(&cfg_path, &db).await;
    let client = reqwest::Client::new();

    // Bootstrap recorded revision 1.
    let revs = get_json(&format!("{sb}/v1/revisions")).await;
    let revs = revs["revisions"].as_array().unwrap();
    assert_eq!(revs.len(), 1, "bootstrap recorded one revision");
    assert_eq!(revs[0]["revision"], 1);
    assert_eq!(revs[0]["source"], "bootstrap");
    let hash_v1 = revs[0]["config_hash"].as_str().unwrap().to_string();

    // A runtime-knob change → revision 2, SAME config hash (knobs aren't config).
    client
        .patch(format!("{sb}/v1/runtime"))
        .json(&json!({"cost_aware": true}))
        .send()
        .await
        .unwrap();

    // A config-file reload (different config) → revision 3, DIFFERENT hash.
    std::fs::write(&cfg_path, mock_config("  compress_tool_results: true")).unwrap();
    let reload: Value = client
        .post(format!("{sb}/v1/reload"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(reload["revision"], 3);

    // Revisions: newest first, all three sources present.
    let revs = get_json(&format!("{sb}/v1/revisions")).await;
    let revs = revs["revisions"].as_array().unwrap();
    assert_eq!(revs.len(), 3);
    assert_eq!(revs[0]["revision"], 3);
    assert_eq!(revs[0]["source"], "file_reload");
    assert_eq!(revs[1]["revision"], 2);
    assert_eq!(revs[1]["source"], "runtime_patch");
    assert_eq!(revs[2]["revision"], 1);

    // Knob change kept the config hash; reload changed it.
    assert_eq!(
        revs[1]["config_hash"].as_str().unwrap(),
        hash_v1,
        "a runtime-knob change does not change the config hash"
    );
    assert_ne!(
        revs[0]["config_hash"].as_str().unwrap(),
        hash_v1,
        "a config reload changes the config hash"
    );

    // Audit: one row per publish, newest first.
    let audit = get_json(&format!("{sb}/v1/audit")).await;
    let audit = audit["audit"].as_array().unwrap();
    assert_eq!(audit.len(), 3);
    assert_eq!(audit[0]["action"], "file_reload");
    assert_eq!(audit[0]["source"], "file_reload");
    assert_eq!(audit[0]["actor_role"], "admin");
    assert_eq!(audit[1]["source"], "runtime_patch");
    assert_eq!(audit[1]["action"], "runtime_patch");
    assert_eq!(audit[1]["actor_role"], "admin");
    assert!(
        audit[1]["detail"].as_str().unwrap().contains("cost_aware"),
        "runtime_patch audit detail records the new knob state"
    );
    assert_eq!(audit[2]["action"], "bootstrap");

    // Durable: a fresh store opened on the same file still sees all three.
    let reopened = sb_store::SqliteStore::open(&db).unwrap();
    use sb_store::StateStore;
    let persisted = reopened.list_revisions(100).unwrap();
    assert_eq!(
        persisted.len(),
        3,
        "revisions survived a store reopen (durable)"
    );
}

#[tokio::test]
async fn persistence_disabled_reports_cleanly() {
    // No store attached (AppState::new path) → endpoints return empty + disabled.
    let cfg = sb_core::Config::from_yaml(&mock_config("")).unwrap();
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

    let revs = get_json(&format!("{sb}/v1/revisions")).await;
    assert_eq!(revs["persistence"], "disabled");
    assert_eq!(revs["revisions"].as_array().unwrap().len(), 0);
}
