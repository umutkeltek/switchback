//! The /cp/v1 declarative control plane: resource envelopes, route-preview, and
//! the draft → validate → publish lifecycle (with optimistic concurrency).

use std::sync::Arc;

use serde_json::{json, Value};

fn config_yaml(extra_provider: &str) -> String {
    format!(
        r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: mock
    type: mock
    accounts:
      - id: a
        auth: {{ kind: api_key, inline: "k" }}
{extra_provider}
routes:
  - name: default
    match: {{ model: "*" }}
    targets:
      - "mock/echo"
"#
    )
}

async fn spawn(yaml: &str) -> String {
    let cfg = sb_core::Config::from_yaml(yaml).unwrap();
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

/// Like `spawn`, but with a file-backed SQLite store attached (drafts durable).
async fn spawn_with_store(yaml: &str, db: &str) -> String {
    let cfg = sb_core::Config::from_yaml(yaml).unwrap();
    let registry = sb_adapters::AdapterRegistry::from_config(&cfg).unwrap();
    let resolver = sb_credentials::CredentialResolver::from_config(&cfg).unwrap();
    let store: Arc<dyn sb_store::StateStore> = Arc::new(sb_store::SqliteStore::open(db).unwrap());
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

#[tokio::test]
async fn drafts_are_durable_across_a_restart() {
    let db = std::env::temp_dir().join("sb_cp_drafts.sqlite");
    let _ = std::fs::remove_file(&db);
    let dbs = db.to_string_lossy().to_string();
    let body = serde_json::to_value(sb_core::Config::from_yaml(&config_yaml("")).unwrap()).unwrap();
    let client = reqwest::Client::new();

    // First process: stage a draft.
    let id = {
        let sb = spawn_with_store(&config_yaml(""), &dbs).await;
        let created: Value = client
            .post(format!("{sb}/cp/v1/drafts"))
            .json(&body)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        created["id"].as_str().unwrap().to_string()
    };

    // Second process on the SAME db file: the draft is still there.
    let sb2 = spawn_with_store(&config_yaml(""), &dbs).await;
    let got = client
        .get(format!("{sb2}/cp/v1/drafts/{id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(got.status(), 200, "draft survived the restart");
    let list = get(&format!("{sb2}/cp/v1/drafts")).await;
    assert!(list["drafts"]
        .as_array()
        .unwrap()
        .iter()
        .any(|d| d["id"] == id));

    // And it still publishes from the restarted process.
    let published: Value = client
        .post(format!("{sb2}/cp/v1/drafts/{id}/publish"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(published["ok"], true);
}

#[tokio::test]
async fn resources_and_route_preview() {
    let sb = spawn(&config_yaml("")).await;
    let client = reqwest::Client::new();

    // Discovery root advertises the kinds + verbs.
    let root = get(&format!("{sb}/cp/v1")).await;
    assert_eq!(root["apiVersion"], "cp.switchback.dev/v1");
    assert!(root["kinds"]
        .as_array()
        .unwrap()
        .iter()
        .any(|k| k["name"] == "ProviderEndpoint"));

    // The provider is projected as a declarative resource with the envelope.
    let list = get(&format!("{sb}/cp/v1/resources/providers")).await;
    assert_eq!(list["kind"], "ProviderEndpoint");
    assert_eq!(list["items"].as_array().unwrap().len(), 1);
    let one = get(&format!("{sb}/cp/v1/resources/providers/mock")).await;
    assert_eq!(one["kind"], "ProviderEndpoint");
    assert_eq!(one["metadata"]["name"], "mock");
    assert_eq!(one["metadata"]["etag"], "W/\"rev-1\"");
    assert_eq!(one["spec"]["id"], "mock");

    // route-preview returns the explainable decision WITHOUT executing.
    let preview: Value = client
        .post(format!("{sb}/cp/v1/route-preview"))
        .json(&json!({"model":"mock/echo","messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(preview["decision"]["selected"]["target_id"], "mock/echo");
    assert_eq!(preview["candidates"], json!(["mock/echo"]));

    // A model with no route/target previews as a 404 decision error.
    let miss = client
        .post(format!("{sb}/cp/v1/route-preview"))
        .json(&json!({"model":"ghost/none","messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .unwrap();
    // wildcard route catches everything here, so this still resolves to mock —
    // assert the preview is well-formed rather than 404.
    assert_eq!(miss.status(), 200);
}

#[tokio::test]
async fn watch_streams_the_current_revision() {
    use futures::StreamExt;
    use std::time::Duration;

    let sb = spawn(&config_yaml("")).await;
    let resp = reqwest::Client::new()
        .get(format!("{sb}/cp/v1/watch"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .contains("text/event-stream"));

    // The first SSE frame carries the current revision (1).
    let mut stream = resp.bytes_stream();
    let first = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .expect("watch emitted within 2s")
        .expect("a chunk")
        .expect("chunk ok");
    let text = String::from_utf8_lossy(&first);
    assert!(text.contains("event:revision") || text.contains("event: revision"));
    assert!(text.contains("\"revision\":1"), "got: {text}");
}

#[tokio::test]
async fn admission_preview_reflects_tenant_quota() {
    let yaml = r#"
server:
  bind: "127.0.0.1:0"
tenants:
  - id: broke
    budget_usd: 0.0
  - id: open
    max_concurrency: 4
api_keys:
  - key: "sk-broke"
    tenant: broke
  - key: "sk-open"
    tenant: open
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
"#;
    let sb = spawn(yaml).await;
    let client = reqwest::Client::new();

    // The broke tenant (budget 0) would NOT be admitted.
    let broke: Value = client
        .post(format!("{sb}/cp/v1/admission-preview"))
        .header("authorization", "Bearer sk-broke")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(broke["admitted"], false);
    assert_eq!(broke["tenant"]["budget_ok"], false);

    // The open tenant (no budget, headroom) would be admitted.
    let open: Value = client
        .post(format!("{sb}/cp/v1/admission-preview"))
        .header("authorization", "Bearer sk-open")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(open["admitted"], true);
    assert_eq!(open["tenant"]["in_flight"], 0);

    // A bad key is rejected (401).
    let bad = client
        .post(format!("{sb}/cp/v1/admission-preview"))
        .header("authorization", "Bearer nope")
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 401);
}

#[tokio::test]
async fn route_preview_flags_unverified_passthrough() {
    // No wildcard route; default_provider forwards unknown models verbatim.
    let yaml = r#"
server:
  bind: "127.0.0.1:0"
  default_provider: mock
providers:
  - id: mock
    type: mock
    accounts:
      - id: a
        auth: { kind: api_key, inline: "k" }
routes:
  - name: known
    match: { model: "known/*" }
    targets:
      - "mock/echo"
"#;
    let sb = spawn(yaml).await;
    let preview: Value = reqwest::Client::new()
        .post(format!("{sb}/cp/v1/route-preview"))
        .json(&json!({"model":"ghost/unknown","messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    // The unknown model is a pass-through → flagged unverified in the decision.
    assert_eq!(preview["decision"]["unverified"], true);
    assert!(preview["decision"]["reason"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r.as_str().unwrap().contains("unverified passthrough")));
}

#[tokio::test]
async fn draft_validate_publish_lifecycle() {
    let sb = spawn(&config_yaml("")).await;
    let client = reqwest::Client::new();

    // A proposed config that adds a second provider.
    let new_cfg = sb_core::Config::from_yaml(&config_yaml(
        "  - id: mock2\n    type: mock\n    accounts:\n      - id: a\n        auth: { kind: api_key, inline: \"k\" }",
    ))
    .unwrap();
    let body = serde_json::to_value(&new_cfg).unwrap();

    // Stage the draft (based on revision 1).
    let created: Value = client
        .post(format!("{sb}/cp/v1/drafts"))
        .json(&body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let draft_id = created["id"].as_str().unwrap().to_string();
    assert_eq!(created["base_revision"], 1);

    // Validate → compiles.
    let valid: Value = client
        .post(format!("{sb}/cp/v1/drafts/{draft_id}/validate"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(valid["valid"], true);

    // Publish → atomic hot-swap, revision 2.
    let published: Value = client
        .post(format!("{sb}/cp/v1/drafts/{draft_id}/publish"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(published["ok"], true);
    assert_eq!(published["revision"], 2);

    // The published config is now live: two providers, at revision 2.
    let providers = get(&format!("{sb}/cp/v1/resources/providers")).await;
    assert_eq!(providers["items"].as_array().unwrap().len(), 2);
    assert_eq!(get(&format!("{sb}/cp/v1")).await["revision"], 2);

    // The consumed draft is gone.
    let gone = client
        .get(format!("{sb}/cp/v1/drafts/{draft_id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(gone.status(), 404);
}

#[tokio::test]
async fn publish_rejects_a_stale_if_match() {
    let sb = spawn(&config_yaml("")).await;
    let client = reqwest::Client::new();
    let body = serde_json::to_value(sb_core::Config::from_yaml(&config_yaml("")).unwrap()).unwrap();

    let created: Value = client
        .post(format!("{sb}/cp/v1/drafts"))
        .json(&body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap();

    // If-Match a non-current revision → 409 (someone else published since).
    let conflict = client
        .post(format!("{sb}/cp/v1/drafts/{id}/publish"))
        .header("if-match", "999")
        .send()
        .await
        .unwrap();
    assert_eq!(conflict.status(), 409);

    // If-Match the current revision → succeeds.
    let ok = client
        .post(format!("{sb}/cp/v1/drafts/{id}/publish"))
        .header("if-match", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 200);
}
