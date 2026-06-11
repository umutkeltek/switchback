use super::*;
use futures::StreamExt;
use sb_core::{AiRequest, AiStreamEvent, Config, Message, ResponseFormat};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

#[derive(Default)]
struct FailingAfterBootstrapStore {
    revision_writes: AtomicUsize,
    fail_usage: AtomicBool,
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

    fn record_usage(
        &self,
        _event: &sb_store::UsageEvent,
    ) -> sb_store::Result<sb_store::UsageWriteOutcome> {
        if self.fail_usage.load(Ordering::SeqCst) {
            Err(sb_store::StoreError("forced usage write failure".into()))
        } else {
            Ok(sb_store::UsageWriteOutcome::Inserted)
        }
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

#[tokio::test]
async fn required_store_usage_failure_fails_non_streaming_request() {
    let cfg = Arc::new(
        Config::from_yaml(
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
"#,
        )
        .unwrap(),
    );
    let registry = Arc::new(sb_adapters::AdapterRegistry::from_config(&cfg).unwrap());
    let resolver = Arc::new(sb_credentials::CredentialResolver::from_config(&cfg).unwrap());
    let store = Arc::new(FailingAfterBootstrapStore {
        revision_writes: AtomicUsize::new(0),
        fail_usage: AtomicBool::new(true),
    });
    let ledger = Arc::new(sb_ledger::UsageLedger::in_memory().with_store(store.clone()));
    let engine = Engine::new(cfg, registry, resolver, ledger)
        .with_store_policy(store, true)
        .unwrap();
    let req = AiRequest::new("mock/echo", vec![Message::user("hi")]);

    let (_revision, outcome) = engine.execute(req, Instant::now()).await;

    match outcome {
        ExecOutcome::Error(error) => {
            assert_eq!(error.status, 500);
            assert_eq!(error.error_type, "usage_persistence_failed");
        }
        _ => panic!("required usage store failure must fail closed"),
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

#[test]
fn publish_if_match_is_monotonic_and_rejects_stale() {
    let engine = engine_from_config(Config::from_yaml(BASIC_CONFIG).unwrap());
    let cfg = || Config::from_yaml(&BASIC_CONFIG.replace("mock/echo", "mock/next")).unwrap();
    // Wrong expected revision → Conflict.
    assert!(matches!(
        engine.publish_with_audit(cfg(), crate::AuditContext::new("t", "x"), Some(999)),
        Err(crate::PublishError::Conflict {
            expected: 999,
            current: 1
        })
    ));
    // Correct expected → swaps, revision 1 → 2.
    assert_eq!(
        engine
            .publish_with_audit(cfg(), crate::AuditContext::new("t", "x"), Some(1))
            .unwrap(),
        2
    );
    // The now-stale expected → Conflict (no silent overwrite).
    assert!(matches!(
        engine.publish_with_audit(cfg(), crate::AuditContext::new("t", "x"), Some(1)),
        Err(crate::PublishError::Conflict {
            expected: 1,
            current: 2
        })
    ));
    // No If-Match → always swaps.
    assert_eq!(
        engine
            .publish_with_audit(cfg(), crate::AuditContext::new("t", "x"), None)
            .unwrap(),
        3
    );
}

#[test]
fn concurrent_publish_with_same_if_match_lets_only_one_win() {
    // Two publishers race with the same expected revision. The reload lock makes
    // the check-and-swap atomic, so exactly one wins and the other sees the new
    // revision (Conflict) — no lost update, no duplicate revision number.
    let engine = Arc::new(engine_from_config(Config::from_yaml(BASIC_CONFIG).unwrap()));
    let base = engine.revision();
    let e1 = engine.clone();
    let e2 = engine.clone();
    let cfg1 = Config::from_yaml(&BASIC_CONFIG.replace("mock/echo", "mock/one")).unwrap();
    let cfg2 = Config::from_yaml(&BASIC_CONFIG.replace("mock/echo", "mock/two")).unwrap();
    let h1 = std::thread::spawn(move || {
        e1.publish_with_audit(cfg1, crate::AuditContext::new("t", "p1"), Some(base))
    });
    let h2 = std::thread::spawn(move || {
        e2.publish_with_audit(cfg2, crate::AuditContext::new("t", "p2"), Some(base))
    });
    let r1 = h1.join().unwrap();
    let r2 = h2.join().unwrap();

    let wins = [&r1, &r2].iter().filter(|r| r.is_ok()).count();
    let conflicts = [&r1, &r2]
        .iter()
        .filter(|r| matches!(r, Err(crate::PublishError::Conflict { .. })))
        .count();
    assert_eq!(wins, 1, "exactly one concurrent publisher should win");
    assert_eq!(
        conflicts, 1,
        "the loser must get a Conflict, not a silent overwrite"
    );
    assert_eq!(
        engine.revision(),
        base + 1,
        "revision advances by exactly one (no duplicate-revision lost update)"
    );
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

#[tokio::test]
async fn client_profile_pins_account_during_execution() {
    let cfg = Config::from_yaml(
        r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: mock
    type: mock
    selection: fill_first
    accounts:
      - id: personal
        auth: { kind: api_key, inline: "personal" }
        priority: 0
      - id: work
        auth: { kind: api_key, inline: "work" }
        priority: 1
client_profiles:
  - id: codex-work
    kind: codex
    models: ["coding"]
    accounts: ["mock/work"]
routes:
  - name: coding
    match: { model: "coding" }
    targets:
      - "mock/echo"
"#,
    )
    .unwrap();
    let engine = engine_from_config(cfg);
    let mut req = AiRequest::new("coding", vec![Message::user("hi")]);
    req.metadata
        .insert("client_profile".to_string(), "codex-work".to_string());
    req.metadata
        .insert("client_profile_source".to_string(), "header".to_string());
    req.metadata.insert(
        "client_protocol".to_string(),
        "openai_responses".to_string(),
    );
    let request_id = req.id.clone();

    let (_revision, plan) = engine.preview_route(&req).unwrap();
    assert!(plan
        .decision
        .reason
        .iter()
        .any(|reason| reason == "client_profile=codex-work"));

    let (_revision, outcome) = engine.execute(req, Instant::now()).await;
    let ExecOutcome::Collected { response, .. } = outcome else {
        panic!("profile-pinned request should execute");
    };
    assert_eq!(response.message.text(), "echo: hi");
    let trace = engine.traces().get(&request_id).expect("trace");
    assert_eq!(trace.client_profile.as_deref(), Some("codex-work"));
    assert_eq!(trace.attempts[0].account_id, "work");
}

#[tokio::test]
async fn header_selected_unknown_client_profile_fails_closed() {
    let cfg = Config::from_yaml(BASIC_CONFIG).unwrap();
    let engine = engine_from_config(cfg);
    let mut req = AiRequest::new("mock/echo", vec![Message::user("hi")]);
    req.metadata
        .insert("client_profile".to_string(), "codex-missing".to_string());
    req.metadata
        .insert("client_profile_source".to_string(), "header".to_string());
    req.metadata.insert(
        "client_protocol".to_string(),
        "openai_responses".to_string(),
    );

    let (_revision, outcome) = engine.execute(req, Instant::now()).await;
    let ExecOutcome::Error(error) = outcome else {
        panic!("unknown header-selected profile must fail closed");
    };
    assert_eq!(error.status, 422);
    assert!(error.message.contains("codex-missing"));
}

#[tokio::test]
async fn client_profile_infers_from_model_when_default_profile_is_not_configured() {
    let cfg = Config::from_yaml(
        r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: mock
    type: mock
    accounts:
      - id: personal
        auth: { kind: api_key, inline: "personal" }
      - id: work
        auth: { kind: api_key, inline: "work" }
client_profiles:
  - id: codex-work
    kind: codex
    models: ["codex/work"]
    accounts: ["mock/work"]
routes:
  - name: codex-work
    match: { model: "codex/work" }
    targets:
      - "mock/echo"
"#,
    )
    .unwrap();
    let engine = engine_from_config(cfg);
    let mut req = AiRequest::new("codex/work", vec![Message::user("hi")]);
    req.metadata
        .insert("client_profile".to_string(), "codex".to_string());
    req.metadata
        .insert("client_profile_source".to_string(), "default".to_string());
    req.metadata.insert(
        "client_protocol".to_string(),
        "openai_responses".to_string(),
    );
    let request_id = req.id.clone();

    let (_revision, outcome) = engine.execute(req, Instant::now()).await;
    let ExecOutcome::Collected { .. } = outcome else {
        panic!("model-inferred profile should execute");
    };
    let trace = engine.traces().get(&request_id).expect("trace");
    assert_eq!(trace.client_profile.as_deref(), Some("codex-work"));
    assert_eq!(trace.attempts[0].account_id, "work");
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
fn plugin_pre_route_denies_blocked_model_in_preview() {
    let cfg = Config::from_yaml(
        r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: mock
    type: mock
plugins:
  - type: model_blocklist
    models: ["codex-native"]
routes:
  - name: default
    match: { model: "*" }
    targets: ["mock/echo"]
"#,
    )
    .unwrap();
    let engine = engine_from_config(cfg);
    let req = AiRequest::new("codex-native", vec![Message::user("hi")]);

    let err = match engine.preview_route(&req) {
        Ok(_) => panic!("blocked model should not fall through to wildcard preview"),
        Err(err) => err,
    };

    assert_eq!(err.status, 403);
    assert_eq!(err.error_type, "plugin_rejected");
    assert!(err.message.contains("codex-native"));
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
fn validate_config_accepts_codex_native_relay_adapter() {
    let cfg = Config::from_yaml(
        r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: codex-relay
    type: codex_native_relay
routes:
  - name: default
    match: { model: "coding" }
    targets:
      - "codex-relay/coding"
"#,
    )
    .unwrap();

    Engine::validate_config(&cfg).expect("codex native relay should compile");
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
