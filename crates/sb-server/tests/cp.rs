//! The /cp/v1 declarative control plane: resource envelopes, route-preview, and
//! the draft → validate → publish lifecycle (with optimistic concurrency).

use std::sync::Arc;

use serde_json::{json, Value};

#[derive(Default)]
struct DraftWriteFailStore;

impl sb_store::StateStore for DraftWriteFailStore {
    fn record_revision(&self, _rec: &sb_store::RevisionRecord) -> sb_store::Result<()> {
        Ok(())
    }

    fn record_revision_and_audit(
        &self,
        _revision: &sb_store::RevisionRecord,
        _audit: &sb_store::AuditEntry,
    ) -> sb_store::Result<()> {
        Ok(())
    }

    fn list_revisions(&self, _limit: usize) -> sb_store::Result<Vec<sb_store::RevisionRecord>> {
        Ok(Vec::new())
    }

    fn get_revision(&self, _revision: u64) -> sb_store::Result<Option<sb_store::RevisionRecord>> {
        Ok(None)
    }

    fn record_audit(&self, _entry: &sb_store::AuditEntry) -> sb_store::Result<()> {
        Ok(())
    }

    fn list_audit(&self, _limit: usize) -> sb_store::Result<Vec<sb_store::AuditEntry>> {
        Ok(Vec::new())
    }

    fn record_usage(
        &self,
        _event: &sb_store::UsageEvent,
    ) -> sb_store::Result<sb_store::UsageWriteOutcome> {
        Ok(sb_store::UsageWriteOutcome::Inserted)
    }

    fn usage_rollup(&self) -> sb_store::Result<sb_store::UsageRollup> {
        Ok(sb_store::UsageRollup::default())
    }

    fn recent_usage(&self, _limit: usize) -> sb_store::Result<Vec<sb_store::UsageEvent>> {
        Ok(Vec::new())
    }

    fn idempotency_get(&self, _key: &str) -> sb_store::Result<Option<sb_store::IdempotencyRecord>> {
        Ok(None)
    }

    fn idempotency_put(&self, _rec: &sb_store::IdempotencyRecord) -> sb_store::Result<bool> {
        Ok(true)
    }

    fn put_draft(&self, _rec: &sb_store::DraftRecord) -> sb_store::Result<()> {
        Err(sb_store::StoreError("forced draft write failure".into()))
    }

    fn get_draft(&self, _id: &str) -> sb_store::Result<Option<sb_store::DraftRecord>> {
        Ok(None)
    }

    fn list_drafts(&self) -> sb_store::Result<Vec<sb_store::DraftRecord>> {
        Ok(Vec::new())
    }

    fn delete_draft(&self, _id: &str) -> sb_store::Result<()> {
        Ok(())
    }
}

fn config_yaml_with_server(extra_server: &str, extra_provider: &str) -> String {
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
{extra_provider}
routes:
  - name: default
    match: {{ model: "*" }}
    targets:
      - "mock/echo"
"#
    )
}

fn config_yaml(extra_provider: &str) -> String {
    config_yaml_with_server("", extra_provider)
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

async fn spawn_with_eval_evidence(yaml: &str) -> String {
    let cfg = sb_core::Config::from_yaml(yaml).unwrap();
    let registry = sb_adapters::AdapterRegistry::from_config(&cfg).unwrap();
    let resolver = sb_credentials::CredentialResolver::from_config(&cfg).unwrap();
    let eval_snapshot = sb_eval::EvalEvidenceSnapshot::from_report(
        &sb_eval::EvalReportQuery {
            task_type: Some(sb_core::ExecutionTaskType::Coding),
            min_runs: 1,
            ..Default::default()
        },
        sb_eval::EvalReport {
            rows: vec![sb_eval::EvalReportRow {
                harness: "codex-cli".to_string(),
                harness_version: Some("1.0.0".to_string()),
                strategy_id: Some("default".to_string()),
                runs: 1,
                pass_count: 1,
                success_rate: Some(1.0),
                ..Default::default()
            }],
        },
        42,
    );
    let state = sb_server::AppState::new(
        Arc::new(cfg),
        Arc::new(registry),
        Arc::new(resolver),
        Arc::new(sb_ledger::UsageLedger::in_memory()),
    )
    .with_eval_evidence(Arc::new(eval_snapshot));
    let app = sb_server::build_app(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

async fn spawn_with_locked_account(yaml: &str) -> String {
    let cfg = sb_core::Config::from_yaml(yaml).unwrap();
    let registry = sb_adapters::AdapterRegistry::from_config(&cfg).unwrap();
    let resolver = Arc::new(sb_credentials::CredentialResolver::from_config(&cfg).unwrap());
    resolver.report_failure("mock", "a", "echo", sb_core::ErrorClass::RateLimited);
    let state = sb_server::AppState::new(
        Arc::new(cfg),
        Arc::new(registry),
        resolver,
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

#[tokio::test]
async fn runtime_state_can_reset_account_model_lockout() {
    let sb = spawn_with_locked_account(&config_yaml("")).await;
    let client = reqwest::Client::new();

    let before = get(&format!("{sb}/cp/v1/runtime-state")).await;
    assert_eq!(
        before["spec"]["providers"][0]["accounts"][0]["locks"][0]["model"],
        "echo"
    );

    let reset: Value = client
        .post(format!("{sb}/cp/v1/runtime-state/reset-lockout"))
        .json(&json!({"provider":"mock","account":"a","model":"echo"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(reset["ok"], true);
    assert_eq!(reset["cleared"], true);

    let after = get(&format!("{sb}/cp/v1/runtime-state")).await;
    assert!(after["spec"]["providers"][0]["accounts"][0]["locks"]
        .as_array()
        .unwrap()
        .is_empty());
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

async fn spawn_with_state_store(
    yaml: &str,
    store: Arc<dyn sb_store::StateStore>,
    required: bool,
) -> String {
    let cfg = sb_core::Config::from_yaml(yaml).unwrap();
    let registry = sb_adapters::AdapterRegistry::from_config(&cfg).unwrap();
    let resolver = sb_credentials::CredentialResolver::from_config(&cfg).unwrap();
    let engine = sb_runtime::Engine::new(
        Arc::new(cfg),
        Arc::new(registry),
        Arc::new(resolver),
        Arc::new(sb_ledger::UsageLedger::in_memory()),
    )
    .with_store_policy(store, required)
    .unwrap();
    let app = sb_server::build_app(sb_server::AppState::from_engine(engine));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn durable_drafts_reject_inline_secrets_by_default() {
    let db = std::env::temp_dir().join("sb_cp_drafts_privacy.sqlite");
    let _ = std::fs::remove_file(&db);
    let dbs = db.to_string_lossy().to_string();
    let body = serde_json::to_value(sb_core::Config::from_yaml(&config_yaml("")).unwrap()).unwrap();
    let client = reqwest::Client::new();

    let sb = spawn_with_store(&config_yaml(""), &dbs).await;
    let created = client
        .post(format!("{sb}/cp/v1/drafts"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(
        created.status(),
        422,
        "durable draft persistence must not store inline secrets by default"
    );
    let body: Value = created.json().await.unwrap();
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("inline secrets"),
        "operator error should explain the privacy guard: {body}"
    );
}

#[tokio::test]
async fn required_store_draft_write_failure_returns_500() {
    let cfg = config_yaml_with_server("  persist_secret_bearing_drafts: true", "");
    let body = serde_json::to_value(sb_core::Config::from_yaml(&cfg).unwrap()).unwrap();
    let sb = spawn_with_state_store(&cfg, Arc::new(DraftWriteFailStore), true).await;

    let res = reqwest::Client::new()
        .post(format!("{sb}/cp/v1/drafts"))
        .json(&body)
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), 500);
    let body: Value = res.json().await.unwrap();
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("draft store write failed"),
        "operator should see required-store persistence failure: {body}"
    );
}

#[tokio::test]
async fn drafts_are_durable_across_a_restart() {
    let db = std::env::temp_dir().join("sb_cp_drafts.sqlite");
    let _ = std::fs::remove_file(&db);
    let dbs = db.to_string_lossy().to_string();
    let cfg = config_yaml_with_server("  persist_secret_bearing_drafts: true", "");
    let body = serde_json::to_value(sb_core::Config::from_yaml(&cfg).unwrap()).unwrap();
    let client = reqwest::Client::new();

    // First process: stage a draft.
    let id = {
        let sb = spawn_with_store(&cfg, &dbs).await;
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
    let sb2 = spawn_with_store(&cfg, &dbs).await;
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
async fn draft_publish_audit_source_is_draft_publish() {
    let db = std::env::temp_dir().join("sb_cp_draft_publish_audit.sqlite");
    let _ = std::fs::remove_file(&db);
    let dbs = db.to_string_lossy().to_string();
    let yaml = config_yaml_with_server("  persist_secret_bearing_drafts: true", "");
    let sb = spawn_with_store(&yaml, &dbs).await;
    let client = reqwest::Client::new();
    let body = serde_json::to_value(sb_core::Config::from_yaml(&yaml).unwrap()).unwrap();

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

    let published: Value = client
        .post(format!("{sb}/cp/v1/drafts/{id}/publish"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(published["ok"], true);

    let audit: Value = client
        .get(format!("{sb}/v1/audit"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let first = &audit["audit"][0];
    assert_eq!(first["source"], "draft_publish");
    assert_eq!(first["object_id"], id);
    assert_eq!(first["actor_role"], "admin");
}

#[tokio::test]
async fn resources_and_route_preview() {
    let yaml = format!(
        "{}{}",
        config_yaml(""),
        r#"
harnesses:
  - name: codex-cli
    version: "contract/v1"
    capabilities:
      streaming_events: true
      artifacts: true
      tool_logs: true
      latency_metadata: true
    supported_task_types: [chat]
    required_tools: ["shell"]
    input_contract: "execution-job/v1"
    output_contract: "harness-run-summary/v1"
"#
    );
    let sb = spawn(&yaml).await;
    let client = reqwest::Client::new();

    // Discovery root advertises the kinds + verbs.
    let root = get(&format!("{sb}/cp/v1")).await;
    assert_eq!(root["apiVersion"], "cp.switchback.dev/v1");
    assert!(root["kinds"]
        .as_array()
        .unwrap()
        .iter()
        .any(|k| k["name"] == "ProviderEndpoint"));
    assert!(root["kinds"]
        .as_array()
        .unwrap()
        .iter()
        .any(|k| k["name"] == "HarnessAdapter"));

    // The provider is projected as a declarative resource with the envelope.
    let list = get(&format!("{sb}/cp/v1/resources/providers")).await;
    assert_eq!(list["kind"], "ProviderEndpoint");
    assert_eq!(list["items"].as_array().unwrap().len(), 1);
    let one = get(&format!("{sb}/cp/v1/resources/providers/mock")).await;
    assert_eq!(one["kind"], "ProviderEndpoint");
    assert_eq!(one["metadata"]["name"], "mock");
    assert_eq!(one["metadata"]["etag"], "W/\"rev-1\"");
    assert_eq!(one["spec"]["id"], "mock");

    let harnesses = get(&format!("{sb}/cp/v1/resources/harnesses")).await;
    assert_eq!(harnesses["kind"], "HarnessAdapter");
    assert_eq!(harnesses["items"][0]["metadata"]["name"], "codex-cli");

    // runtime-state exposes the live non-secret operator state as a CP resource.
    let state = get(&format!("{sb}/cp/v1/runtime-state")).await;
    assert_eq!(state["apiVersion"], "cp.switchback.dev/v1");
    assert_eq!(state["kind"], "RuntimeState");
    assert_eq!(state["metadata"]["name"], "current");
    assert_eq!(state["metadata"]["revision"], 1);
    assert_eq!(state["spec"]["providers"][0]["id"], "mock");
    assert_eq!(state["spec"]["providers"][0]["accounts_total"], 1);
    assert_eq!(state["spec"]["providers"][0]["accounts_healthy"], 1);
    assert_eq!(state["spec"]["providers"][0]["accounts"][0]["id"], "a");
    assert_eq!(state["spec"]["admission"]["max_concurrency"], Value::Null);
    assert_eq!(state["spec"]["runtime"]["cost_aware"], false);

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
    assert_eq!(preview["harness_candidates"][0]["name"], "codex-cli");
    assert_eq!(
        preview["harness_candidates"][0]["input_contract"],
        "execution-job/v1"
    );

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
async fn route_preview_attaches_principal_and_session_context() {
    let yaml = config_yaml_with_server(
        "",
        r#"
tenants:
  - id: acme
api_keys:
  - key: "sk-operator"
    tenant: acme
    project: api
    role: operator
"#,
    );
    let sb = spawn(&yaml).await;

    let preview: Value = reqwest::Client::new()
        .post(format!("{sb}/cp/v1/route-preview"))
        .header("authorization", "Bearer sk-operator")
        .header("x-switchback-session-id", "sess-123")
        .json(&json!({"model":"mock/echo","messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(preview["principal"]["tenant"], "acme");
    assert_eq!(preview["principal"]["project"], "api");
    assert_eq!(preview["principal"]["session_id"], "sess-123");
    assert_eq!(preview["decision"]["selected"]["target_id"], "mock/echo");
}

#[tokio::test]
async fn route_preview_includes_eval_evidence_when_available() {
    let yaml = format!(
        "{}{}",
        config_yaml(""),
        r#"
harnesses:
- name: codex-cli
  version: "contract/v1"
  capabilities:
    artifacts: true
    latency_metadata: true
  supported_task_types: [coding]
  required_tools: ["shell"]
  input_contract: "execution-job/v1"
  output_contract: "harness-run-summary/v1"
"#
    );
    let sb = spawn_with_eval_evidence(&yaml).await;
    let preview: Value = reqwest::Client::new()
        .post(format!("{sb}/cp/v1/route-preview"))
        .json(&json!({"model":"auto/coding","messages":[{"role":"user","content":"fix this bug"}]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(preview["decision"]["selected"]["target_id"], "mock/echo");
    let evidence = preview["eval_evidence"].as_array().unwrap();
    assert_eq!(evidence.len(), 1);
    assert_eq!(evidence[0]["harness"], "codex-cli");
    assert_eq!(evidence[0]["runs"], 1);
    assert_eq!(evidence[0]["pass_count"], 1);
}

#[tokio::test]
async fn tenant_operator_control_plane_views_are_scoped() {
    let yaml = r#"
server:
  bind: "127.0.0.1:0"
tenants:
  - id: acme
    allowed_routes: ["default"]
    allowed_providers: ["mock"]
    allowed_accounts: ["mock/team"]
  - id: beta
api_keys:
  - key: "sk-acme-operator"
    tenant: acme
    role: operator
providers:
  - id: mock
    type: mock
    accounts:
      - id: team
        auth: { kind: api_key, inline: "team-key" }
      - id: shared
        auth: { kind: api_key, inline: "shared-key" }
  - id: shadow
    type: mock
    accounts:
      - id: beta
        auth: { kind: api_key, inline: "beta-key" }
routes:
  - name: default
    match: { model: "*" }
    targets:
      - "mock/echo"
      - "shadow/echo"
  - name: private
    match: { model: "private" }
    targets:
      - "shadow/echo"
"#;
    let sb = spawn(yaml).await;
    let client = reqwest::Client::new();
    let auth = "Bearer sk-acme-operator";

    let config: Value = client
        .get(format!("{sb}/v1/config"))
        .header("authorization", auth)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let config_text = serde_json::to_string(&config).unwrap();
    assert_eq!(config["scope"]["tenant"], "acme");
    assert!(config_text.contains("mock"));
    assert!(config_text.contains("team"));
    assert!(!config_text.contains("shadow"));
    assert!(!config_text.contains("shared"));
    assert!(!config_text.contains("beta"));

    let providers: Value = client
        .get(format!("{sb}/v1/providers"))
        .header("authorization", auth)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(providers["providers"].as_array().unwrap().len(), 1);
    assert_eq!(providers["providers"][0]["id"], "mock");
    assert_eq!(providers["providers"][0]["accounts"], json!(["team"]));

    let models: Value = client
        .get(format!("{sb}/v1/models"))
        .header("authorization", auth)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let model_text = serde_json::to_string(&models).unwrap();
    assert!(model_text.contains("mock/echo"));
    assert!(!model_text.contains("shadow/echo"));

    let health: Value = client
        .get(format!("{sb}/v1/health"))
        .header("authorization", auth)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health["providers"].as_array().unwrap().len(), 1);
    assert_eq!(health["providers"][0]["id"], "mock");
    assert_eq!(health["providers"][0]["accounts"][0]["id"], "team");

    let cp_providers: Value = client
        .get(format!("{sb}/cp/v1/resources/providers"))
        .header("authorization", auth)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(cp_providers["items"].as_array().unwrap().len(), 1);
    assert_eq!(cp_providers["items"][0]["metadata"]["name"], "mock");

    let shadow = client
        .get(format!("{sb}/cp/v1/resources/providers/shadow"))
        .header("authorization", auth)
        .send()
        .await
        .unwrap();
    assert_eq!(shadow.status(), 404);

    let routes: Value = client
        .get(format!("{sb}/cp/v1/resources/routes"))
        .header("authorization", auth)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(routes["items"].as_array().unwrap().len(), 1);
    assert_eq!(routes["items"][0]["metadata"]["name"], "default");
    assert_eq!(routes["items"][0]["spec"]["targets"], json!(["mock/echo"]));

    let runtime_state: Value = client
        .get(format!("{sb}/cp/v1/runtime-state"))
        .header("authorization", auth)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        runtime_state["spec"]["providers"].as_array().unwrap().len(),
        1
    );
    assert_eq!(runtime_state["spec"]["providers"][0]["id"], "mock");
    assert_eq!(
        runtime_state["spec"]["providers"][0]["accounts"][0]["id"],
        "team"
    );

    let drafts = client
        .get(format!("{sb}/cp/v1/drafts"))
        .header("authorization", auth)
        .send()
        .await
        .unwrap();
    assert_eq!(drafts.status(), 403);
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
    role: operator
  - key: "sk-open"
    tenant: open
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
