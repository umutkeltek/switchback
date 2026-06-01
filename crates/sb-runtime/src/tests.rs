use super::*;
use futures::StreamExt;
use sb_core::{AiRequest, AiStreamEvent, Config, Message, ResponseFormat};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

#[derive(Default)]
struct FailingAfterBootstrapStore {
    revision_writes: AtomicUsize,
}

impl sb_store::StateStore for FailingAfterBootstrapStore {
    fn record_revision(&self, _rec: &sb_store::RevisionRecord) -> sb_store::Result<()> {
        Ok(())
    }

    fn record_revision_and_audit(
        &self,
        _revision: &sb_store::RevisionRecord,
        _audit: &sb_store::AuditEntry,
    ) -> sb_store::Result<()> {
        if self.revision_writes.fetch_add(1, Ordering::SeqCst) == 0 {
            Ok(())
        } else {
            Err(sb_store::StoreError("forced revision write failure".into()))
        }
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

    fn record_usage(&self, _event: &sb_store::UsageEvent) -> sb_store::Result<()> {
        Ok(())
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
        Ok(())
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

const BASIC_CONFIG: &str = r#"
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
"#;

#[test]
fn validate_config_rejects_api_keys_for_unknown_tenants() {
    let cfg = Config::from_yaml(&format!(
        "{BASIC_CONFIG}\napi_keys:\n  - key: sk-live\n    tenant: missing\n"
    ))
    .unwrap();

    let err = Engine::validate_config(&cfg).expect_err("unknown tenant must be rejected");

    assert!(
        err.contains("api_keys[0].tenant"),
        "error should name the broken reference: {err}"
    );
}

#[test]
fn validate_config_rejects_closed_wasm_plugin_that_cannot_activate() {
    let cfg = Config::from_yaml(&format!(
            "{BASIC_CONFIG}\nplugins:\n  - type: wasm\n    path: \"/tmp/switchback-missing-policy.wasm\"\n    failure_mode: closed\n"
        ))
        .unwrap();

    let err = Engine::validate_config(&cfg)
        .expect_err("fail-closed wasm activation must reject config validation");

    assert!(
        err.contains("plugins:"),
        "error should mention plugins: {err}"
    );
    assert!(
        err.contains("plugins[0]"),
        "error should name the plugin: {err}"
    );
}

#[test]
fn engine_try_new_rejects_fail_closed_broken_plugin() {
    let cfg = Arc::new(
            Config::from_yaml(&format!(
                "{BASIC_CONFIG}\nplugins:\n  - type: wasm\n    path: \"/tmp/switchback-missing-policy.wasm\"\n    failure_mode: closed\n"
            ))
            .unwrap(),
        );
    let registry = Arc::new(sb_adapters::AdapterRegistry::from_config(&cfg).unwrap());
    let resolver = Arc::new(sb_credentials::CredentialResolver::from_config(&cfg).unwrap());

    let err = match Engine::try_new(
        cfg,
        registry,
        resolver,
        Arc::new(sb_ledger::UsageLedger::in_memory()),
    ) {
        Ok(_) => panic!("try_new must not silently disable fail-closed plugins"),
        Err(err) => err,
    };

    assert!(
        err.contains("plugins[0]"),
        "error should name plugin: {err}"
    );
}

#[test]
fn config_hash_is_stable_sha256() {
    let cfg = Config::from_yaml(BASIC_CONFIG).unwrap();

    let first = config_hash(&cfg);
    let second = config_hash(&cfg);

    assert_eq!(first, second);
    assert_eq!(first.len(), 64);
}

#[test]
fn config_hash_changes_when_route_changes() {
    let first = Config::from_yaml(BASIC_CONFIG).unwrap();
    let second = Config::from_yaml(&BASIC_CONFIG.replace("mock/echo", "mock/other")).unwrap();

    assert_ne!(config_hash(&first), config_hash(&second));
}

fn engine_from_config(config: Config) -> Engine {
    let cfg = Arc::new(config);
    let registry = Arc::new(sb_adapters::AdapterRegistry::from_config(&cfg).unwrap());
    let resolver = Arc::new(sb_credentials::CredentialResolver::from_config(&cfg).unwrap());
    Engine::new(
        cfg,
        registry,
        resolver,
        Arc::new(sb_ledger::UsageLedger::in_memory()),
    )
}

#[test]
fn required_store_reload_failure_does_not_swap_runtime() {
    let engine = engine_from_config(Config::from_yaml(BASIC_CONFIG).unwrap())
        .with_store_policy(Arc::new(FailingAfterBootstrapStore::default()), true)
        .unwrap();
    let replacement =
        Config::from_yaml(&BASIC_CONFIG.replace("mock/echo", "mock/replacement")).unwrap();

    let err = engine
        .reload(replacement)
        .expect_err("required store failure must reject reload");

    assert!(err.contains("state store persistence failed"));
    assert_eq!(engine.revision(), 1);
    let req = AiRequest::new("mock/echo", vec![Message::user("hi")]);
    let (_revision, plan) = engine.preview_route(&req).unwrap();
    assert_eq!(plan.candidates[0].id, "mock/echo");
}

#[test]
fn required_store_runtime_patch_failure_does_not_swap_runtime() {
    let engine = engine_from_config(Config::from_yaml(BASIC_CONFIG).unwrap())
        .with_store_policy(Arc::new(FailingAfterBootstrapStore::default()), true)
        .unwrap();

    let err = engine
        .update_runtime(|runtime| runtime.cost_aware = true)
        .expect_err("required store failure must reject runtime patch");

    assert!(err.contains("state store persistence failed"));
    assert_eq!(engine.revision(), 1);
    assert!(!engine.snapshot().runtime.cost_aware);
}

#[tokio::test]
async fn streaming_precommit_error_falls_over_before_client_commit() {
    let cfg = Config::from_yaml(
        r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: mock
    type: mock
    accounts:
      - id: stream-fail-account
        auth: { kind: api_key, inline: "bad" }
        priority: 0
      - id: good-account
        auth: { kind: api_key, inline: "good" }
        priority: 1
routes:
  - name: default
    match: { model: "*" }
    targets:
      - "mock/echo"
"#,
    )
    .unwrap();
    let engine = engine_from_config(cfg);
    let mut req = AiRequest::new("mock/echo", vec![Message::user("hi")]);
    req.stream = true;
    let request_id = req.id.clone();

    let (_revision, outcome) = engine.execute(req, Instant::now()).await;
    let ExecOutcome::Stream { mut stream, .. } = outcome else {
        panic!("expected fallback to commit a healthy stream");
    };

    let mut text = String::new();
    while let Some(item) = stream.next().await {
        if let AiStreamEvent::TextDelta { text: delta } = item.unwrap() {
            text.push_str(&delta);
        }
    }

    assert!(text.contains("echo: hi"));
    let trace = engine.traces().get(&request_id).expect("stream trace");
    assert_eq!(trace.revision, 1);
    assert_eq!(trace.final_status, 200);
    assert_eq!(trace.attempts.len(), 2);
    assert_eq!(trace.attempts[0].account_id, "stream-fail-account");
    assert_eq!(trace.attempts[1].account_id, "good-account");
}

#[tokio::test]
async fn strict_schema_downlevel_rejects_high_lossiness_before_dispatch() {
    let cfg = Config::from_yaml(
        r#"
server:
  bind: "127.0.0.1:0"
  strict_schema_downlevel: true
providers:
  - id: gemini
    type: gemini
    base_url: "http://127.0.0.1:1"
    accounts:
      - id: a
        auth: { kind: api_key, inline: "k" }
routes:
  - name: default
    match: { model: "*" }
    targets:
      - "gemini/g"
"#,
    )
    .unwrap();
    let engine = engine_from_config(cfg);
    let mut req = AiRequest::new("x", vec![Message::user("hi")]);
    req.response_format = Some(ResponseFormat::JsonSchema {
        name: "out".into(),
        schema: serde_json::json!({
            "type": "object",
            "properties": {
                "a": { "anyOf": [{ "type": "null" }, { "type": "string" }] }
            }
        }),
        strict: true,
    });
    let request_id = req.id.clone();

    let (_revision, outcome) = engine.execute(req, Instant::now()).await;

    let ExecOutcome::Error(error) = outcome else {
        panic!("strict high-lossiness downlevel must reject");
    };
    assert_eq!(error.status, 422);
    assert_eq!(error.error_type, "schema_downlevel_rejected");
    assert!(error.message.contains("schema_downlevel:high"));

    let trace = engine.traces().get(&request_id).expect("trace");
    assert_eq!(trace.final_status, 422);
    assert_eq!(trace.attempts.len(), 1);
    assert!(matches!(
        &trace.attempts[0].outcome,
        sb_trace::AttemptOutcome::Failed {
            class,
            fell_over: false
        } if class == "unsupported_capability"
    ));
    assert!(trace
        .warnings
        .iter()
        .any(|warning| warning.contains("schema_downlevel:high")));
}

#[tokio::test]
async fn tenant_policy_filters_providers_and_accounts() {
    let cfg = Config::from_yaml(
        r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: openai
    type: openai_compatible
    base_url: "http://127.0.0.1:1/v1"
    accounts:
      - id: a
        auth: { kind: api_key, inline: "k" }
  - id: mock
    type: mock
    accounts:
      - id: personal
        auth: { kind: api_key, inline: "personal" }
        priority: 0
      - id: team
        auth: { kind: api_key, inline: "team" }
        priority: 1
tenants:
  - id: acme
    allowed_routes: ["default"]
    allowed_providers: ["mock"]
    allowed_accounts: ["mock/team"]
routes:
  - name: default
    match: { model: "*" }
    targets:
      - "openai/gpt-test"
      - "mock/echo"
"#,
    )
    .unwrap();
    let engine = engine_from_config(cfg);
    let mut req = AiRequest::new("x", vec![Message::user("hi")]);
    req.tenant = Some("acme".into());
    let request_id = req.id.clone();

    let (_revision, plan) = engine.preview_route(&req).unwrap();
    assert_eq!(plan.decision.selected.unwrap().target_id, "mock/echo");
    assert!(plan.decision.rejected.iter().any(|rejected| {
        rejected.target_id == "openai/gpt-test" && rejected.reason.contains("provider")
    }));

    let (_revision, outcome) = engine.execute(req, Instant::now()).await;
    let ExecOutcome::Collected { response, .. } = outcome else {
        panic!("tenant-allowed mock target should execute");
    };
    assert_eq!(response.message.text(), "echo: hi");
    let trace = engine.traces().get(&request_id).expect("trace");
    assert_eq!(trace.attempts[0].account_id, "team");
}

#[test]
fn tenant_policy_denies_disallowed_route_in_preview() {
    let cfg = Config::from_yaml(
        r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: mock
    type: mock
    accounts:
      - id: a
        auth: { kind: api_key, inline: "k" }
tenants:
  - id: acme
    allowed_routes: ["safe"]
routes:
  - name: safe
    match: { model: "safe" }
    targets: ["mock/echo"]
  - name: blocked
    match: { model: "blocked" }
    targets: ["mock/echo"]
"#,
    )
    .unwrap();
    let engine = engine_from_config(cfg);
    let mut req = AiRequest::new("blocked", vec![Message::user("hi")]);
    req.tenant = Some("acme".into());

    let err = match engine.preview_route(&req) {
        Ok(_) => panic!("tenant should not preview disallowed route"),
        Err(err) => err,
    };

    assert_eq!(err.status, 403);
    assert_eq!(err.error_type, "tenant_policy_denied");
}

#[test]
fn validate_config_rejects_route_targets_with_unknown_providers() {
    let cfg = Config::from_yaml(
        r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: mock
    type: mock
routes:
  - name: default
    match: { model: "*" }
    targets:
      - "ghost/echo"
"#,
    )
    .unwrap();

    let err = Engine::validate_config(&cfg).expect_err("dangling target must be rejected");

    assert!(
        err.contains("routes[0].targets[0]"),
        "error should name the broken target: {err}"
    );
}

#[test]
fn explicit_provider_model_previews_before_wildcard_route() {
    let cfg = Config::from_yaml(
        r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: mock
    type: mock
    accounts:
      - id: a
        auth: { kind: api_key, inline: "k" }
  - id: openai
    type: openai_compatible
    base_url: "http://127.0.0.1:1/v1"
    accounts:
      - id: a
        auth: { kind: api_key, inline: "k" }
routes:
  - name: default
    match: { model: "*" }
    targets:
      - "mock/echo"
"#,
    )
    .unwrap();
    let cfg = Arc::new(cfg);
    let registry = Arc::new(sb_adapters::AdapterRegistry::from_config(&cfg).unwrap());
    let resolver = Arc::new(sb_credentials::CredentialResolver::from_config(&cfg).unwrap());
    let engine = Engine::new(
        cfg,
        registry,
        resolver,
        Arc::new(sb_ledger::UsageLedger::in_memory()),
    );
    let req = AiRequest::new("openai/gpt-test", vec![Message::user("hi")]);

    let (_revision, plan) = engine.preview_route(&req).unwrap();

    assert_eq!(plan.decision.selected.unwrap().target_id, "openai/gpt-test");
    assert_eq!(plan.candidates[0].id, "openai/gpt-test");
}
