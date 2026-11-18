use super::*;
use futures::StreamExt;
use sb_core::{AiRequest, AiStreamEvent, Config, EvaluationEventKind, Message, ResponseFormat};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

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

/// A minimal store whose `load_scorecard` succeeds exactly once (the
/// startup hydrate) and PANICS on any subsequent call — proves the route
/// path never consults the store again (outcome-routing-v1 §1: pure
/// in-memory projection after hydrate).
#[derive(Default)]
struct PanicOnSecondScorecardLoadStore {
    load_calls: AtomicUsize,
}

impl sb_store::StateStore for PanicOnSecondScorecardLoadStore {
    fn record_revision(&self, _rec: &sb_store::RevisionRecord) -> sb_store::Result<()> {
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
    fn load_scorecard(&self) -> sb_store::Result<Vec<sb_store::ScorecardRow>> {
        let calls = self.load_calls.fetch_add(1, Ordering::SeqCst);
        if calls == 0 {
            Ok(Vec::new())
        } else {
            panic!(
                "load_scorecard called {} times: the route path must never read the store after startup hydrate",
                calls + 1
            );
        }
    }
}

/// A store whose `upsert_scorecard` always fails — proves the background
/// flusher logs and retries rather than affecting request handling.
#[derive(Default)]
struct AlwaysFailUpsertStore;

impl sb_store::StateStore for AlwaysFailUpsertStore {
    fn record_revision(&self, _rec: &sb_store::RevisionRecord) -> sb_store::Result<()> {
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
    fn upsert_scorecard(&self, _rows: &[sb_store::ScorecardRow]) -> sb_store::Result<()> {
        Err(sb_store::StoreError(
            "forced upsert_scorecard failure".into(),
        ))
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

    // outcome-routing-v1 F8: the client-facing response is still fail-closed
    // (asserted above, unchanged), but the upstream call WAS a real success
    // -- finish_attempt must have run before the usage-persistence step, so
    // the scorecard still records it.
    let scorecard_cfg = engine.snapshot().config.server.scorecard.clone();
    let rows =
        engine
            .scorecard()
            .dirty_snapshot(&scorecard_cfg, Instant::now(), sb_store::now_millis());
    assert_eq!(
        rows.len(),
        1,
        "the upstream success must be recorded even though usage persistence failed"
    );
    assert_eq!(rows[0].scoreable_samples, 1);
    assert_eq!(rows[0].success_count, 1);
    assert_eq!(rows[0].target_fail_count, 0);
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

fn quality_cost_map(input: u64, output: u64, aggregator: bool) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "switchback-quality-cost-{}.json",
        sb_core::new_id("test")
    ));
    std::fs::write(
        &path,
        serde_json::json!({
            "providers": [{ "id": "mock", "aggregator": aggregator }],
            "models": [{
                "provider_id": "mock",
                "model_id": "judge",
                "input_micros_per_mtok": input,
                "output_micros_per_mtok": output
            }]
        })
        .to_string(),
    )
    .unwrap();
    path
}

fn quality_eval_validation_config(cost_map: Option<&std::path::Path>) -> Config {
    let cost_line = cost_map
        .map(|path| format!("  cost_map: {:?}\n", path.display().to_string()))
        .unwrap_or_default();
    Config::from_yaml(&format!(
        r#"
server:
  bind: "127.0.0.1:0"
  state_store: "/tmp/switchback-quality.sqlite"
{cost_line}  quality_eval:
    enabled: true
    body_allowed_targets: [mock/judge]
providers:
  - id: mock
    type: mock
routes:
  - name: judge
    match: {{ model: "auto/judge" }}
    targets: ["mock/judge"]
"#
    ))
    .unwrap()
}

#[test]
fn validate_config_accepts_a_positively_priced_private_judge_target() {
    let path = quality_cost_map(100, 200, false);
    let cfg = quality_eval_validation_config(Some(&path));

    Engine::validate_config(&cfg).expect("priced direct judge target should validate");

    std::fs::remove_file(path).unwrap();
}

#[test]
fn validate_config_rejects_unpriced_free_and_aggregator_judge_targets() {
    let unpriced = quality_eval_validation_config(None);
    let err = Engine::validate_config(&unpriced).expect_err("unpriced judge must fail closed");
    assert!(err.contains("positively priced"), "unexpected error: {err}");

    let free_path = quality_cost_map(0, 0, false);
    let free = quality_eval_validation_config(Some(&free_path));
    let err = Engine::validate_config(&free).expect_err("free judge must fail closed");
    assert!(err.contains("free"), "unexpected error: {err}");
    std::fs::remove_file(free_path).unwrap();

    let aggregator_path = quality_cost_map(100, 200, true);
    let aggregator = quality_eval_validation_config(Some(&aggregator_path));
    let err = Engine::validate_config(&aggregator).expect_err("aggregator judge must fail closed");
    assert!(err.contains("aggregator"), "unexpected error: {err}");
    std::fs::remove_file(aggregator_path).unwrap();
}

#[test]
fn validate_config_skips_judge_price_policy_when_quality_eval_is_disabled() {
    let mut cfg = quality_eval_validation_config(None);
    cfg.server.quality_eval.enabled = false;

    Engine::validate_config(&cfg).expect("disabled quality eval preserves existing validation");
}

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

#[tokio::test]
async fn execute_reports_cooldown_locked_accounts_as_provider_unavailable() {
    let cfg = Arc::new(Config::from_yaml(BASIC_CONFIG).unwrap());
    let registry = Arc::new(sb_adapters::AdapterRegistry::from_config(&cfg).unwrap());
    let resolver = Arc::new(sb_credentials::CredentialResolver::from_config(&cfg).unwrap());
    let cooldown = resolver.report_failure("mock", "a", "echo", sb_core::ErrorClass::RateLimited);
    let engine = Engine::new(
        cfg,
        registry,
        resolver,
        Arc::new(sb_ledger::UsageLedger::in_memory()),
    );

    let (_revision, outcome) = engine
        .execute(
            AiRequest::new("anything", vec![Message::user("hi")]),
            Instant::now(),
        )
        .await;

    let ExecOutcome::Error(error) = outcome else {
        panic!("cooldown-locked accounts must fail before adapter execution")
    };
    assert_eq!(error.status, 503);
    assert_eq!(error.error_type, "provider_unavailable");
    assert!(error.message.contains("temporarily unavailable"));
    assert!(error.message.contains("retry after"));
    let retry_after = error
        .retry_after
        .expect("cooldown exhaustion must preserve its retry timing");
    assert!(retry_after > Duration::ZERO);
    assert!(retry_after <= cooldown);
}

#[tokio::test]
async fn embeddings_report_cooldown_locked_accounts_as_provider_unavailable() {
    let cfg = Arc::new(Config::from_yaml(BASIC_CONFIG).unwrap());
    let registry = Arc::new(sb_adapters::AdapterRegistry::from_config(&cfg).unwrap());
    let resolver = Arc::new(sb_credentials::CredentialResolver::from_config(&cfg).unwrap());
    let cooldown = resolver.report_failure("mock", "a", "echo", sb_core::ErrorClass::RateLimited);
    let engine = Engine::new(
        cfg,
        registry,
        resolver,
        Arc::new(sb_ledger::UsageLedger::in_memory()),
    );

    let (_revision, outcome) = engine
        .execute_embeddings(
            serde_json::json!({ "model": "anything", "input": "hello" }),
            None,
            None,
            None,
            Instant::now(),
        )
        .await;

    let EmbeddingsOutcome::Error { error, .. } = outcome else {
        panic!("cooldown-locked accounts must fail before adapter execution")
    };
    assert_eq!(error.status, 503);
    assert_eq!(error.error_type, "provider_unavailable");
    assert!(error.message.contains("temporarily unavailable"));
    assert!(error.message.contains("retry after"));
    let retry_after = error
        .retry_after
        .expect("cooldown exhaustion must preserve its retry timing");
    assert!(retry_after > Duration::ZERO);
    assert!(retry_after <= cooldown);
}

#[test]
fn disabled_quality_eval_has_no_worker_or_usage_projection() {
    let engine = Arc::new(engine_from_config(Config::from_yaml(BASIC_CONFIG).unwrap()));
    assert!(engine.quality_eval_projection().is_none());
    assert!(engine.clone().spawn_quality_eval_worker().is_none());
}

#[tokio::test]
async fn scoped_execution_filters_before_fallback_and_reports_the_actual_success() {
    let config = Config::from_yaml(
        r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: mock
    type: mock
routes:
  - name: judge
    match: { model: "judge" }
    targets: ["mock/always-error", "mock/echo"]
"#,
    )
    .unwrap();
    let engine = engine_from_config(config);

    let failed = engine
        .execute_scoped(
            engine.snapshot(),
            AiRequest::new("judge", vec![Message::user("bounded material")]),
            Instant::now(),
            HashSet::from(["mock/always-error".to_string()]),
        )
        .await;
    assert!(matches!(failed.outcome, ExecOutcome::Error(_)));
    assert!(failed.success.is_none());
    let failed_trace = engine.traces().recent(1).pop().unwrap();
    assert!(failed_trace
        .attempts
        .iter()
        .all(|attempt| attempt.target_id != "mock/echo"));
    let receipt = failed_trace.decision.receipt.as_ref().unwrap();
    assert_eq!(receipt.cache.status, sb_core::CacheStatus::Bypass);
    assert!(receipt.cache.key.is_none());
    assert_eq!(receipt.job.context_fingerprint, "redacted:quality_eval");

    let succeeded = engine
        .execute_scoped(
            engine.snapshot(),
            AiRequest::new("judge", vec![Message::user("bounded material")]),
            Instant::now(),
            HashSet::from(["mock/echo".to_string()]),
        )
        .await;
    assert!(matches!(succeeded.outcome, ExecOutcome::Collected { .. }));
    assert_eq!(succeeded.success.unwrap().target_id, "mock/echo");

    let (_revision, ordinary) = engine
        .execute(
            AiRequest::new("judge", vec![Message::user("ordinary")]),
            Instant::now(),
        )
        .await;
    assert!(matches!(ordinary, ExecOutcome::Collected { .. }));
}

#[tokio::test]
async fn scoped_quality_execution_never_hedges_body_allowed_targets() {
    let config = Config::from_yaml(
        r#"
server:
  bind: "127.0.0.1:0"
  hedge:
    enabled: true
    delay_ms: 0
    max_parallel: 2
providers:
  - id: mock
    type: mock
routes:
  - name: judge
    match: { model: "judge" }
    targets: ["mock/echo", "mock/hedge-fast"]
"#,
    )
    .unwrap();
    let engine = engine_from_config(config);
    let scoped = engine
        .execute_scoped(
            engine.snapshot(),
            AiRequest::new("judge", vec![Message::user("bounded material")]),
            Instant::now(),
            HashSet::from(["mock/echo".to_string(), "mock/hedge-fast".to_string()]),
        )
        .await;

    assert!(matches!(scoped.outcome, ExecOutcome::Collected { .. }));
    assert_eq!(scoped.success.unwrap().target_id, "mock/echo");
    let trace = engine.traces().recent(1).pop().unwrap();
    assert_eq!(trace.attempts.len(), 1);
    assert_eq!(trace.attempts[0].target_id, "mock/echo");
}

#[tokio::test]
async fn execution_trace_carries_receipt_and_cache_events() {
    let engine = engine_from_config(Config::from_yaml(BASIC_CONFIG).unwrap());
    let mut req = AiRequest::new("mock/echo", vec![Message::user("hi")]);
    req.metadata.insert(
        sb_trace::NATIVE_EXECUTION_LANE_ID_META.to_string(),
        "gpt56-sol-ultra".to_string(),
    );
    req.metadata.insert(
        sb_trace::NATIVE_EXECUTION_LANE_REVISION_META.to_string(),
        format!("sha256:{}", "a".repeat(64)),
    );
    req.metadata.insert(
        sb_trace::NATIVE_EXECUTION_REQUESTED_EFFORT_META.to_string(),
        "ultra".to_string(),
    );
    req.metadata.insert(
        sb_trace::NATIVE_EXECUTION_OBSERVED_EFFORT_META.to_string(),
        "ultra".to_string(),
    );
    req.metadata.insert(
        sb_trace::NATIVE_EXECUTION_OBSERVED_PATH_META.to_string(),
        "/reasoning/effort".to_string(),
    );
    let (_revision, outcome) = engine.execute(req, Instant::now()).await;
    match outcome {
        ExecOutcome::Collected { .. } => {}
        _ => panic!("mock request should collect successfully"),
    }

    let traces = engine.traces().recent(1);
    assert_eq!(traces.len(), 1);
    let trace = &traces[0];
    let receipt = trace
        .decision
        .receipt
        .as_ref()
        .expect("route decision should carry execution receipt");
    assert_eq!(receipt.policy_version, sb_core::EXECUTION_POLICY_VERSION);
    assert_eq!(receipt.cache.status, sb_core::CacheStatus::Miss);
    assert_eq!(receipt.selected_route.as_deref(), Some("mock/echo"));
    assert!(receipt
        .candidates
        .iter()
        .any(|candidate| candidate == "mock/echo"));
    assert_eq!(trace.final_status, 200);
    assert_eq!(trace.attempts.len(), 1);
    assert_eq!(trace.attempts[0].provider_id, "mock");
    assert_eq!(trace.attempts[0].model, "echo");
    assert_eq!(
        trace.native_execution,
        Some(sb_trace::NativeExecutionObservation {
            account_attempt_id: None,
            account_session_id: None,
            account_slot: None,
            lane_id: Some("gpt56-sol-ultra".to_string()),
            lane_revision: Some(format!("sha256:{}", "a".repeat(64))),
            launch_profile: None,
            conformance_revision: None,
            harness: None,
            requested_effort: Some("ultra".to_string()),
            observed_effort: Some("ultra".to_string()),
            observed_effort_path: Some("/reasoning/effort".to_string()),
        })
    );
    assert!(trace
        .events
        .iter()
        .any(|event| event.kind == EvaluationEventKind::RunStarted));
    assert!(trace
        .events
        .iter()
        .any(|event| event.kind == EvaluationEventKind::CacheLookup));
    assert!(trace
        .events
        .iter()
        .any(|event| event.kind == EvaluationEventKind::RouteSelected));
    assert!(trace
        .events
        .iter()
        .any(|event| event.kind == EvaluationEventKind::FinalStatus));
}

#[tokio::test]
async fn execution_cache_can_be_disabled_by_config() {
    let cfg = Config::from_yaml(&BASIC_CONFIG.replace(
        "server:\n  bind: \"127.0.0.1:0\"",
        "server:\n  bind: \"127.0.0.1:0\"\n  execution_cache:\n    enabled: false",
    ))
    .unwrap();
    let engine = engine_from_config(cfg);
    let req = AiRequest::new("mock/echo", vec![Message::user("hi")]);

    let (_revision, outcome) = engine.execute(req, Instant::now()).await;
    match outcome {
        ExecOutcome::Collected { .. } => {}
        _ => panic!("mock request should collect successfully"),
    }

    let trace = engine.traces().recent(1).pop().expect("trace");
    let receipt = trace.decision.receipt.expect("execution receipt");
    assert_eq!(receipt.cache.status, sb_core::CacheStatus::Bypass);
    assert_eq!(
        receipt.cache.reason.as_deref(),
        Some("cache_policy=disabled")
    );
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
    assert!(plan.decision.receipt.is_some());
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
async fn empty_client_profile_accounts_do_not_block_execution() {
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
  - id: codex-coding
    kind: codex
    models: ["coding"]
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
        .insert("client_profile".to_string(), "codex-coding".to_string());
    req.metadata
        .insert("client_profile_source".to_string(), "header".to_string());
    req.metadata.insert(
        "client_protocol".to_string(),
        "openai_responses".to_string(),
    );
    let request_id = req.id.clone();

    let (_revision, plan) = engine.preview_route(&req).unwrap();
    assert_eq!(plan.decision.selected.unwrap().target_id, "mock/echo");

    let (_revision, outcome) = engine.execute(req, Instant::now()).await;
    let ExecOutcome::Collected { response, .. } = outcome else {
        panic!("profile without account restrictions should execute");
    };
    assert_eq!(response.message.text(), "echo: hi");
    let trace = engine.traces().get(&request_id).expect("trace");
    assert_eq!(trace.client_profile.as_deref(), Some("codex-coding"));
    assert_eq!(trace.attempts[0].account_id, "personal");
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

// ---------------------------------------------------------------------
// outcome-routing-v1 §1/§4 — commit 4: finish_attempt seam + scorecard
// wiring (spec §8 "Runtime" test list).
// ---------------------------------------------------------------------

#[tokio::test]
async fn finish_attempt_records_exactly_once_for_non_stream_success() {
    let engine = engine_from_config(Config::from_yaml(BASIC_CONFIG).unwrap());
    let req = AiRequest::new("mock/echo", vec![Message::user("hi")]);

    let (_revision, outcome) = engine.execute(req, Instant::now()).await;
    assert!(matches!(outcome, ExecOutcome::Collected { .. }));

    let scorecard_cfg = engine.snapshot().config.server.scorecard.clone();
    let rows =
        engine
            .scorecard()
            .dirty_snapshot(&scorecard_cfg, Instant::now(), sb_store::now_millis());
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].target_id, "mock/echo");
    assert_eq!(rows[0].scoreable_samples, 1);
    assert_eq!(rows[0].success_count, 1);
    assert_eq!(rows[0].target_fail_count, 0);
}

#[tokio::test]
async fn finish_attempt_records_exactly_once_for_stream_clean() {
    let engine = engine_from_config(Config::from_yaml(BASIC_CONFIG).unwrap());
    let mut req = AiRequest::new("mock/echo", vec![Message::user("hi")]);
    req.stream = true;

    let (_revision, outcome) = engine.execute(req, Instant::now()).await;
    let ExecOutcome::Stream { mut stream, .. } = outcome else {
        panic!("expected a stream outcome");
    };
    while stream.next().await.is_some() {}

    let scorecard_cfg = engine.snapshot().config.server.scorecard.clone();
    let rows =
        engine
            .scorecard()
            .dirty_snapshot(&scorecard_cfg, Instant::now(), sb_store::now_millis());
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].scoreable_samples, 1);
    assert_eq!(rows[0].success_count, 1);
}

#[tokio::test]
async fn finish_attempt_records_exactly_once_for_stream_upstream_error() {
    let cfg = Config::from_yaml(
        r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: mock
    type: mock
    accounts:
      - id: mid-stream-fail-account
        auth: { kind: api_key, inline: "k" }
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

    let (_revision, outcome) = engine.execute(req, Instant::now()).await;
    // The first event succeeds, so precommit already committed this stream
    // to the client; the failure only arrives once the client drains it.
    let ExecOutcome::Stream { mut stream, .. } = outcome else {
        panic!("mid-stream failure still precommits a stream to the client");
    };
    while stream.next().await.is_some() {}

    let scorecard_cfg = engine.snapshot().config.server.scorecard.clone();
    let rows =
        engine
            .scorecard()
            .dirty_snapshot(&scorecard_cfg, Instant::now(), sb_store::now_millis());
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].scoreable_samples, 1,
        "exactly one record for the single attempt, even after draining past the error"
    );
    assert_eq!(rows[0].target_fail_count, 1);
    assert_eq!(rows[0].success_count, 0);
}

#[tokio::test]
async fn precommit_failure_alone_is_recorded_as_target_failure() {
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

    let (_revision, outcome) = engine.execute(req, Instant::now()).await;
    assert!(
        matches!(outcome, ExecOutcome::Error(_)),
        "no fallover account configured -> the request fails outright"
    );

    let scorecard_cfg = engine.snapshot().config.server.scorecard.clone();
    let rows =
        engine
            .scorecard()
            .dirty_snapshot(&scorecard_cfg, Instant::now(), sb_store::now_millis());
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].scoreable_samples, 1);
    assert_eq!(rows[0].target_fail_count, 1);
    assert_eq!(rows[0].success_count, 0);
}

#[tokio::test]
async fn account_fallover_retry_produces_two_scorecard_records() {
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

    let (_revision, outcome) = engine.execute(req, Instant::now()).await;
    let ExecOutcome::Stream { mut stream, .. } = outcome else {
        panic!("expected fallback to commit a healthy stream");
    };
    while stream.next().await.is_some() {}

    let scorecard_cfg = engine.snapshot().config.server.scorecard.clone();
    let rows =
        engine
            .scorecard()
            .dirty_snapshot(&scorecard_cfg, Instant::now(), sb_store::now_millis());
    assert_eq!(
        rows.len(),
        1,
        "both attempts land on the same target key (account fallover, not target fallover)"
    );
    assert_eq!(rows[0].target_id, "mock/echo");
    assert_eq!(
        rows[0].scoreable_samples, 2,
        "precommit failure + fallover success = two separately-recorded attempts"
    );
    assert_eq!(rows[0].target_fail_count, 1);
    assert_eq!(rows[0].success_count, 1);
}

#[tokio::test]
async fn same_target_retries_each_record_their_own_scorecard_outcome() {
    // F5: one AttemptToken used to span the whole retry loop, so
    // timeout->timeout->success collapsed into a single recorded Success.
    // Each individual dispatch must now get its own token, finalized before
    // the next retry -- 2 failures + 1 success, three separate records.
    let cfg = Config::from_yaml(
        r#"
server:
  bind: "127.0.0.1:0"
  retry:
    max_retries: 2
    base_delay_ms: 1
    max_delay_ms: 1
providers:
  - id: mock
    type: mock
    accounts:
      - id: retry-fail-then-succeed-account
        auth: { kind: api_key, inline: "k" }
routes:
  - name: default
    match: { model: "*" }
    targets:
      - "mock/echo"
"#,
    )
    .unwrap();
    let engine = engine_from_config(cfg);
    let req = AiRequest::new("mock/echo", vec![Message::user("hi")]);

    let (_revision, outcome) = engine.execute(req, Instant::now()).await;
    assert!(
        matches!(outcome, ExecOutcome::Collected { .. }),
        "the third dispatch succeeds"
    );

    let scorecard_cfg = engine.snapshot().config.server.scorecard.clone();
    let rows =
        engine
            .scorecard()
            .dirty_snapshot(&scorecard_cfg, Instant::now(), sb_store::now_millis());
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].target_id, "mock/echo");
    assert_eq!(
        rows[0].scoreable_samples, 3,
        "2 retried failures + 1 eventual success = 3 separately-recorded attempts"
    );
    assert_eq!(rows[0].target_fail_count, 2);
    assert_eq!(rows[0].success_count, 1);
}

#[tokio::test]
async fn hedge_race_records_winner_success_and_started_loser_as_cancelled() {
    // F6: hedge racers bypassed finish_attempt entirely (winner/failures
    // updated the breaker directly; started losers were dropped with no
    // scorecard record at all). Both the winner and a started-but-canceled
    // loser must now be recorded -- winner Success, loser neutral Cancelled.
    let cfg = Config::from_yaml(
        r#"
server:
  bind: "127.0.0.1:0"
  hedge:
    enabled: true
    max_parallel: 2
    delay_ms: 0
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
      - "mock/hedge-fast"
      - "mock/hedge-slow"
"#,
    )
    .unwrap();
    let engine = engine_from_config(cfg);
    let req = AiRequest::new("hedge", vec![Message::user("hi")]);

    let (_revision, outcome) = engine.execute(req, Instant::now()).await;
    assert!(
        matches!(outcome, ExecOutcome::Collected { .. }),
        "the fast racer wins"
    );

    let scorecard_cfg = engine.snapshot().config.server.scorecard.clone();
    let rows =
        engine
            .scorecard()
            .dirty_snapshot(&scorecard_cfg, Instant::now(), sb_store::now_millis());
    assert_eq!(
        rows.len(),
        2,
        "both the winner and the canceled loser are recorded: {rows:?}"
    );
    let fast = rows
        .iter()
        .find(|r| r.target_id == "mock/hedge-fast")
        .expect("winner recorded");
    assert_eq!(fast.scoreable_samples, 1);
    assert_eq!(fast.success_count, 1);
    let slow = rows
        .iter()
        .find(|r| r.target_id == "mock/hedge-slow")
        .expect("started-but-canceled loser recorded");
    assert_eq!(
        slow.scoreable_samples, 0,
        "a canceled racer is neutral, not scoreable"
    );
    assert_eq!(slow.success_count, 0);
    assert_eq!(slow.target_fail_count, 0);
}

#[tokio::test]
async fn embeddings_success_and_failure_both_feed_the_scorecard() {
    // F7: embeddings attempts called breaker/plugins directly and never fed
    // the scorecard, although embeddings routing consumes projections.
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
"#,
    )
    .unwrap();
    let engine = engine_from_config(cfg);

    let (_revision, outcome) = engine
        .execute_embeddings(
            serde_json::json!({ "model": "mock/embed", "input": "hello" }),
            None,
            None,
            None,
            Instant::now(),
        )
        .await;
    assert!(matches!(outcome, EmbeddingsOutcome::Json { .. }));

    let scorecard_cfg = engine.snapshot().config.server.scorecard.clone();
    let rows =
        engine
            .scorecard()
            .dirty_snapshot(&scorecard_cfg, Instant::now(), sb_store::now_millis());
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].target_id, "mock/embed");
    assert_eq!(rows[0].scoreable_samples, 1);
    assert_eq!(rows[0].success_count, 1);
}

#[tokio::test]
async fn in_band_stream_error_after_commit_is_recorded_as_target_failure() {
    // F9: a first in-band Ok(AiStreamEvent::Error) accepted at precommit
    // used to end up recorded as Cancelled once the SSE stream dropped. This
    // covers the POST-commit case (ok chunk, then an in-band error) --
    // meter_stream must detect it directly and finalize as UpstreamError
    // with its class, not fall through to Aborted.
    let cfg = Config::from_yaml(
        r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: mock
    type: mock
    accounts:
      - id: mid-stream-inband-error-account
        auth: { kind: api_key, inline: "k" }
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

    let (_revision, outcome) = engine.execute(req, Instant::now()).await;
    let ExecOutcome::Stream { mut stream, .. } = outcome else {
        panic!("first event succeeds -> precommit commits this stream to the client");
    };
    while stream.next().await.is_some() {}

    let scorecard_cfg = engine.snapshot().config.server.scorecard.clone();
    let rows =
        engine
            .scorecard()
            .dirty_snapshot(&scorecard_cfg, Instant::now(), sb_store::now_millis());
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].target_fail_count, 1,
        "in-band error must be recorded as TargetFailure, not silently dropped as Cancelled"
    );
    assert_eq!(rows[0].success_count, 0);
}

#[tokio::test]
async fn mid_stream_reload_uses_dispatch_time_scorecard_config() {
    let engine = engine_from_config(Config::from_yaml(BASIC_CONFIG).unwrap());
    let mut req = AiRequest::new("mock/echo", vec![Message::user("hi")]);
    req.stream = true;

    let (_revision, outcome) = engine.execute(req, Instant::now()).await;
    let ExecOutcome::Stream { mut stream, .. } = outcome else {
        panic!("expected a stream outcome");
    };

    // Reload BEFORE draining: the live config now disables the scorecard,
    // but this attempt was already dispatched under the old (enabled)
    // config, which must be what finish_attempt uses at completion time.
    let disabled_cfg = Config::from_yaml(
        r#"
server:
  bind: "127.0.0.1:0"
  scorecard:
    enabled: false
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
    .unwrap();
    engine.reload(disabled_cfg).expect("reload should succeed");
    assert!(
        !engine.snapshot().config.server.scorecard.enabled,
        "the live snapshot now disables the scorecard"
    );

    // Drain to completion now -> triggers the finish closure.
    while stream.next().await.is_some() {}

    // dirty_snapshot doesn't gate on `enabled` (only `record`/`project` do),
    // so this directly proves whether `record()` ran at all: if
    // finish_attempt had used a freshly-read (disabled) config instead of
    // the dispatch-time one, this would be empty.
    let probe_cfg = sb_core::ScorecardConfig::default();
    let rows =
        engine
            .scorecard()
            .dirty_snapshot(&probe_cfg, Instant::now(), sb_store::now_millis());
    assert_eq!(
        rows.len(),
        1,
        "the in-flight attempt must still be scored under its dispatch-time config"
    );
    assert_eq!(rows[0].scoreable_samples, 1);
}

#[tokio::test]
async fn structural_reload_preserves_scorecard_state() {
    let engine = engine_from_config(Config::from_yaml(BASIC_CONFIG).unwrap());
    let req = AiRequest::new("mock/echo", vec![Message::user("hi")]);
    let (_revision, outcome) = engine.execute(req, Instant::now()).await;
    assert!(matches!(outcome, ExecOutcome::Collected { .. }));

    let scorecard_cfg = engine.snapshot().config.server.scorecard.clone();
    let before = engine
        .scorecard()
        .project("mock/echo", "any", &scorecard_cfg, Instant::now())
        .expect("recorded before reload");
    assert_eq!(before.samples, 1);

    let revision_before = engine.revision();
    engine
        .reload(Config::from_yaml(BASIC_CONFIG).unwrap())
        .expect("reload should succeed");
    assert_eq!(
        engine.revision(),
        revision_before + 1,
        "reload bumped the revision (a fresh Snapshot was built)"
    );

    let after = engine
        .scorecard()
        .project("mock/echo", "any", &scorecard_cfg, Instant::now())
        .expect("scorecard state survives a structural reload");
    assert_eq!(
        after.samples, 1,
        "the Engine-level scorecard field is not part of Snapshot, so it survives the swap"
    );
}

#[tokio::test]
async fn route_path_never_reads_store_after_hydrate() {
    let cfg = Arc::new(Config::from_yaml(BASIC_CONFIG).unwrap());
    let registry = Arc::new(sb_adapters::AdapterRegistry::from_config(&cfg).unwrap());
    let resolver = Arc::new(sb_credentials::CredentialResolver::from_config(&cfg).unwrap());
    let store = Arc::new(PanicOnSecondScorecardLoadStore::default());
    let engine = Engine::new(
        cfg,
        registry,
        resolver,
        Arc::new(sb_ledger::UsageLedger::in_memory()),
    )
    .with_store_policy(store, false)
    .expect("startup hydrate must not fail the engine");

    // Several requests in a row must only ever consult the in-memory
    // projection (`project`/`record`) — never re-read the store. A second
    // `load_scorecard` call would panic inside the store itself.
    for _ in 0..3 {
        let req = AiRequest::new("mock/echo", vec![Message::user("hi")]);
        let (_revision, outcome) = engine.execute(req, Instant::now()).await;
        assert!(matches!(outcome, ExecOutcome::Collected { .. }));
    }
}

#[tokio::test]
async fn flusher_failure_does_not_affect_execution() {
    let cfg = Arc::new(Config::from_yaml(BASIC_CONFIG).unwrap());
    let registry = Arc::new(sb_adapters::AdapterRegistry::from_config(&cfg).unwrap());
    let resolver = Arc::new(sb_credentials::CredentialResolver::from_config(&cfg).unwrap());
    let store = Arc::new(AlwaysFailUpsertStore);
    let engine = Engine::new(
        cfg,
        registry,
        resolver,
        Arc::new(sb_ledger::UsageLedger::in_memory()),
    )
    .with_store_policy(store, false)
    .expect("startup hydrate must not fail the engine");

    let req = AiRequest::new("mock/echo", vec![Message::user("hi")]);
    let (_revision, outcome) = engine.execute(req, Instant::now()).await;
    assert!(
        matches!(outcome, ExecOutcome::Collected { .. }),
        "request handling is unaffected by a store that can never persist the scorecard"
    );

    // The flush itself must not panic despite the store always failing.
    engine.flush_scorecard_once();

    // And the failed row must be retried on the next tick, not dropped.
    let scorecard_cfg = engine.snapshot().config.server.scorecard.clone();
    let rows =
        engine
            .scorecard()
            .dirty_snapshot(&scorecard_cfg, Instant::now(), sb_store::now_millis());
    assert_eq!(
        rows.len(),
        1,
        "a failed flush must remain (or become) dirty again for the next tick"
    );
}

// --- managed local lanes known down -------------------------------------------

/// A lane view whose listed providers are known down.
struct DownLanes(&'static [&'static str]);

impl crate::LaneStatus for DownLanes {
    fn known_down(&self, provider_id: &str) -> Option<String> {
        self.0
            .contains(&provider_id)
            .then(|| "last health probe failed".to_string())
    }
}

/// Route `[lane/m, spare/m]` where `lane` is a managed local lane.
/// `spare_tools` sets the spare's tool-calling capability; `tenant` adds a
/// tenant that may only use the lane provider.
fn lane_then_spare_config(spare_tools: bool, tenant: bool) -> Config {
    let tenants = if tenant {
        "tenants:\n  - id: lane-only\n    allowed_providers: [\"lane\"]\n"
    } else {
        ""
    };
    Config::from_yaml(&format!(
        r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: lane
    type: openai_compatible
    base_url: "http://127.0.0.1:1/v1"
    engine: llama_cpp
    accounts:
      - {{ id: a, auth: {{ kind: none }} }}
  - id: spare
    type: openai_compatible
    base_url: "http://127.0.0.1:2/v1"
    capabilities: {{ tool_calling: {spare_tools} }}
    accounts:
      - {{ id: s, auth: {{ kind: none }} }}
{tenants}local_executors:
  - name: lane
    base_url: "http://127.0.0.1:1"
    health_endpoint: "http://127.0.0.1:1/health"
    route_health: true
routes:
  - name: local-text
    match: {{ model: "local-text" }}
    targets:
      - "lane/m"
      - "spare/m"
"#
    ))
    .unwrap()
}

#[test]
fn a_lane_without_route_health_is_never_skipped() {
    // Opt-in: a lane that did not set `route_health` is routed exactly as
    // before managed text lanes, whatever the lane view says about it.
    let mut cfg = lane_then_spare_config(true, false);
    cfg.local_executors[0].route_health = false;
    let engine = engine_from_config(cfg).with_lane_status(Arc::new(DownLanes(&["lane"])));
    let req = AiRequest::new("local-text", vec![Message::user("hi")]);
    let (_revision, plan) = engine.preview_route(&req).unwrap();
    assert_eq!(plan.decision.selected.unwrap().target_id, "lane/m");
    assert!(
        !plan
            .decision
            .rejected
            .iter()
            .any(|r| r.reason.contains("local lane")),
        "{:?}",
        plan.decision.rejected
    );
    assert!(
        !plan
            .decision
            .reason
            .iter()
            .any(|r| r.contains("local lane")),
        "{:?}",
        plan.decision.reason
    );
}

#[test]
fn a_down_lane_is_skipped_when_another_candidate_can_serve() {
    let engine = engine_from_config(lane_then_spare_config(true, false))
        .with_lane_status(Arc::new(DownLanes(&["lane"])));
    let req = AiRequest::new("local-text", vec![Message::user("hi")]);
    let (_revision, plan) = engine.preview_route(&req).unwrap();
    assert_eq!(plan.decision.selected.unwrap().target_id, "spare/m");
    assert!(plan
        .decision
        .rejected
        .iter()
        .any(|r| r.target_id == "lane/m" && r.reason.contains("local lane `lane` is down")));
}

#[test]
fn a_down_lane_is_kept_when_the_other_candidates_are_filtered_out() {
    // The fail-open rule must hold after EVERY filter, not only when every
    // candidate is on a down lane: a stale probe must not become an outage.
    let tool = sb_core::ToolSpec {
        name: "lookup".to_string(),
        description: None,
        parameters: serde_json::json!({"type": "object"}),
        defer_loading: false,
    };

    // Router hard filter: the spare cannot call tools.
    let engine = engine_from_config(lane_then_spare_config(false, false))
        .with_lane_status(Arc::new(DownLanes(&["lane"])));
    let mut req = AiRequest::new("local-text", vec![Message::user("hi")]);
    req.tools.push(tool);
    let (_revision, plan) = engine.preview_route(&req).unwrap();
    assert_eq!(
        plan.decision
            .selected
            .as_ref()
            .map(|t| t.target_id.as_str()),
        Some("lane/m"),
        "{:?}",
        plan.decision
    );
    assert!(
        plan.decision
            .reason
            .iter()
            .any(|r| r == "local lane known down but kept: no other candidate can serve (lane/m)"),
        "{:?}",
        plan.decision.reason
    );
    assert!(!plan
        .decision
        .rejected
        .iter()
        .any(|r| r.target_id == "lane/m"));

    // Tenant allow-list: the tenant may only use the lane.
    let engine = engine_from_config(lane_then_spare_config(true, true))
        .with_lane_status(Arc::new(DownLanes(&["lane"])));
    let mut req = AiRequest::new("local-text", vec![Message::user("hi")]);
    req.tenant = Some("lane-only".to_string());
    let (_revision, plan) = engine.preview_route(&req).unwrap();
    assert_eq!(
        plan.decision
            .selected
            .as_ref()
            .map(|t| t.target_id.as_str()),
        Some("lane/m"),
        "{:?}",
        plan.decision
    );
    assert!(plan
        .decision
        .rejected
        .iter()
        .any(|r| r.target_id == "spare/m" && r.reason.contains("tenant policy")));
}

#[test]
fn a_reload_that_removes_a_lane_stops_skipping_its_provider() {
    let engine = engine_from_config(lane_then_spare_config(true, false))
        .with_lane_status(Arc::new(DownLanes(&["lane"])));
    let req = AiRequest::new("local-text", vec![Message::user("hi")]);
    let (_revision, plan) = engine.preview_route(&req).unwrap();
    assert_eq!(plan.decision.selected.unwrap().target_id, "spare/m");

    // The lane view lives as long as the process; the live config decides
    // which providers are managed lanes.
    let mut without_lane = lane_then_spare_config(true, false);
    without_lane.local_executors.clear();
    engine.reload(without_lane).unwrap();
    let (_revision, plan) = engine.preview_route(&req).unwrap();
    assert_eq!(plan.decision.selected.unwrap().target_id, "lane/m");
    assert!(
        !plan
            .decision
            .rejected
            .iter()
            .any(|r| r.reason.contains("local lane")),
        "{:?}",
        plan.decision.rejected
    );
}

// --- wake on request, the lane busy guard, time to first token --------------

/// A lane view for a `wake_on_request` lane `lane`: known down until a wake
/// answers `Ready`; the wake answer is scripted; busy guards are counted.
struct WakeLanes {
    down: AtomicBool,
    wake: crate::LaneWake,
    wakes: AtomicUsize,
    begun: AtomicUsize,
    active: Arc<AtomicUsize>,
    published: AtomicUsize,
}

impl WakeLanes {
    fn new(down: bool, wake: crate::LaneWake) -> Arc<Self> {
        Arc::new(Self {
            down: AtomicBool::new(down),
            wake,
            wakes: AtomicUsize::new(0),
            begun: AtomicUsize::new(0),
            active: Arc::new(AtomicUsize::new(0)),
            published: AtomicUsize::new(0),
        })
    }
}

struct Busy(Arc<AtomicUsize>);

impl Drop for Busy {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl crate::LaneStatus for WakeLanes {
    fn known_down(&self, provider_id: &str) -> Option<String> {
        (provider_id == "lane" && self.down.load(Ordering::SeqCst))
            .then(|| "last health probe failed".to_string())
    }

    fn wake_for_request<'a>(
        &'a self,
        _provider_id: &'a str,
    ) -> crate::BoxFuture<'a, crate::LaneWake> {
        self.wakes.fetch_add(1, Ordering::SeqCst);
        if matches!(self.wake, crate::LaneWake::Ready { .. }) {
            self.down.store(false, Ordering::SeqCst);
        }
        Box::pin(std::future::ready(self.wake.clone()))
    }

    fn begin_request(&self, _provider_id: &str) -> Option<crate::LaneBusy> {
        self.begun.fetch_add(1, Ordering::SeqCst);
        self.active.fetch_add(1, Ordering::SeqCst);
        Some(Box::new(Busy(self.active.clone())))
    }

    fn config_published(&self, _config: &Config) {
        self.published.fetch_add(1, Ordering::SeqCst);
    }
}

/// Route `[lane/echo, spare/echo]` over two mock providers, `lane` being a
/// managed lane with `route_health` and, when asked, `wake_on_request`.
fn wake_lane_config(wake_on_request: bool) -> Config {
    Config::from_yaml(&format!(
        r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: lane
    type: mock
  - id: spare
    type: mock
local_executors:
  - name: lane
    base_url: "http://executor-1.example:8080"
    health_endpoint: "http://executor-1.example:8080/health"
    route_health: true
    wake_on_request: {wake_on_request}
    wake_command: "ssh gateway wake-host executor-1"
routes:
  - name: local-text
    match: {{ model: "local-text" }}
    targets:
      - "lane/echo"
      - "spare/echo"
"#
    ))
    .unwrap()
}

fn ready_after_41s() -> crate::LaneWake {
    crate::LaneWake::Ready {
        detail: "woken for this request, healthy after 41 s".to_string(),
    }
}

fn wait_passed() -> crate::LaneWake {
    crate::LaneWake::NotReady {
        detail: "wake wait of 90 s passed, wake still running".to_string(),
        retry_after: Duration::from_secs(90),
    }
}

fn hello(model: &str, stream: bool) -> AiRequest {
    let mut req = AiRequest::new(model, vec![Message::user("hi")]);
    req.stream = stream;
    req
}

#[tokio::test]
async fn a_lane_woken_for_a_request_serves_it_and_the_decision_says_why() {
    let lanes = WakeLanes::new(true, ready_after_41s());
    let engine = engine_from_config(wake_lane_config(true)).with_lane_status(lanes.clone());
    let (_revision, outcome) = engine
        .execute(hello("local-text", false), Instant::now())
        .await;
    assert!(matches!(outcome, ExecOutcome::Collected { .. }));
    let trace = engine.traces().recent(1).pop().unwrap();
    assert_eq!(
        trace.decision.selected.as_ref().unwrap().target_id,
        "lane/echo"
    );
    assert!(
        trace
            .decision
            .reason
            .iter()
            .any(|r| r == "local lane `lane` woken for this request, healthy after 41 s"),
        "{:?}",
        trace.decision.reason
    );
    assert!(
        trace.decision.rejected.is_empty(),
        "{:?}",
        trace.decision.rejected
    );
    assert_eq!(trace.attempts[0].target_id, "lane/echo");
    assert_eq!(lanes.wakes.load(Ordering::SeqCst), 1);
    assert_eq!(lanes.begun.load(Ordering::SeqCst), 1, "held while served");
    assert_eq!(lanes.active.load(Ordering::SeqCst), 0, "released after");
}

#[tokio::test]
async fn a_wake_wait_that_passes_falls_through_with_the_reason() {
    let lanes = WakeLanes::new(true, wait_passed());
    let engine = engine_from_config(wake_lane_config(true)).with_lane_status(lanes.clone());
    let (_revision, outcome) = engine
        .execute(hello("local-text", false), Instant::now())
        .await;
    assert!(matches!(outcome, ExecOutcome::Collected { .. }));
    let trace = engine.traces().recent(1).pop().unwrap();
    assert_eq!(
        trace.decision.selected.as_ref().unwrap().target_id,
        "spare/echo"
    );
    let rejected = trace
        .decision
        .rejected
        .iter()
        .find(|r| r.target_id == "lane/echo")
        .expect("the lane is rejected with a reason");
    assert!(
        rejected.reason.contains("wake wait of 90 s passed"),
        "{}",
        rejected.reason
    );
    assert!(
        trace
            .decision
            .reason
            .iter()
            .any(|r| r
                == "local lane `lane` wake wait of 90 s passed, wake still running, fell through"),
        "{:?}",
        trace.decision.reason
    );
    assert_eq!(lanes.wakes.load(Ordering::SeqCst), 1);
    assert_eq!(
        lanes.begun.load(Ordering::SeqCst),
        0,
        "the lane was never dispatched"
    );
}

#[tokio::test]
async fn a_wake_wait_that_passes_with_no_other_target_is_a_retryable_503() {
    let lanes = WakeLanes::new(true, wait_passed());
    let engine = engine_from_config(wake_lane_config(true)).with_lane_status(lanes.clone());
    let (_revision, outcome) = engine
        .execute(hello("lane/echo", false), Instant::now())
        .await;
    let ExecOutcome::Error(error) = outcome else {
        panic!("expected a retryable error, not a dispatch into a lane known down");
    };
    assert_eq!(error.status, 503);
    assert_eq!(error.error_type, "local_lane_unavailable");
    assert_eq!(error.retry_after, Some(Duration::from_secs(90)));
    assert!(
        error.message.contains("wake wait of 90 s passed"),
        "{}",
        error.message
    );
    assert_eq!(lanes.begun.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn without_wake_on_request_a_down_lane_is_skipped_exactly_as_before() {
    // Opt-out (the default) equals the behaviour before this setting: no
    // wake, no busy guard, and the same decision a plain known-down view
    // produces.
    let lanes = WakeLanes::new(true, ready_after_41s());
    let engine = engine_from_config(wake_lane_config(false)).with_lane_status(lanes.clone());
    let mut req = hello("local-text", false);
    req.id = "same-request".to_string();
    engine.execute(req.clone(), Instant::now()).await;
    let with_wake_view = engine.traces().recent(1).pop().unwrap().decision;
    assert_eq!(lanes.wakes.load(Ordering::SeqCst), 0, "no wake");
    assert_eq!(lanes.begun.load(Ordering::SeqCst), 0, "no guard");

    let baseline = engine_from_config(wake_lane_config(false))
        .with_lane_status(Arc::new(DownLanes(&["lane"])));
    baseline.execute(req, Instant::now()).await;
    let before = baseline.traces().recent(1).pop().unwrap().decision;
    assert_eq!(
        with_wake_view.selected.as_ref().map(|t| &t.target_id),
        before.selected.as_ref().map(|t| &t.target_id)
    );
    assert_eq!(with_wake_view.reason, before.reason);
    assert_eq!(
        serde_json::to_value(&with_wake_view.rejected).unwrap(),
        serde_json::to_value(&before.rejected).unwrap()
    );
    assert_eq!(
        before.selected.unwrap().target_id,
        "spare/echo",
        "the down lane is skipped"
    );
}

#[tokio::test]
async fn a_stream_holds_the_lane_until_its_last_event_and_records_ttft() {
    let lanes = WakeLanes::new(false, ready_after_41s());
    let engine = engine_from_config(wake_lane_config(true)).with_lane_status(lanes.clone());
    let (_revision, outcome) = engine
        .execute(hello("local-text", true), Instant::now())
        .await;
    let ExecOutcome::Stream { mut stream, .. } = outcome else {
        panic!("expected a stream");
    };
    assert_eq!(
        lanes.active.load(Ordering::SeqCst),
        1,
        "busy while streaming"
    );
    while stream.next().await.is_some() {
        assert_eq!(lanes.active.load(Ordering::SeqCst), 1);
    }
    assert_eq!(
        lanes.active.load(Ordering::SeqCst),
        0,
        "released at the end"
    );
    drop(stream);
    let trace = engine.traces().recent(1).pop().unwrap();
    assert_eq!(trace.final_status, 200);
    assert!(trace.ttft_ms.is_some(), "streamed trace carries TTFT");
    assert!(trace.ttft_ms.unwrap() <= trace.total_latency_ms);
    assert_eq!(
        lanes.wakes.load(Ordering::SeqCst),
        0,
        "an up lane is not woken"
    );
}

#[tokio::test]
async fn a_client_that_goes_away_mid_stream_releases_the_lane() {
    let lanes = WakeLanes::new(false, ready_after_41s());
    let engine = engine_from_config(wake_lane_config(true)).with_lane_status(lanes.clone());
    let (_revision, outcome) = engine
        .execute(hello("local-text", true), Instant::now())
        .await;
    let ExecOutcome::Stream { mut stream, .. } = outcome else {
        panic!("expected a stream");
    };
    assert!(stream.next().await.is_some());
    assert_eq!(lanes.active.load(Ordering::SeqCst), 1);
    drop(stream); // the client hung up
    assert_eq!(lanes.active.load(Ordering::SeqCst), 0);
    let trace = engine.traces().recent(1).pop().unwrap();
    assert_eq!(trace.final_status, 499);
}

#[tokio::test]
async fn a_collected_response_records_ttft_and_a_failure_does_not() {
    let engine = engine_from_config(wake_lane_config(false));
    engine
        .execute(hello("spare/echo", false), Instant::now())
        .await;
    let trace = engine.traces().recent(1).pop().unwrap();
    assert_eq!(trace.final_status, 200);
    assert!(!trace.streamed);
    assert!(trace.ttft_ms.is_some(), "collected trace carries TTFT");
    let json = serde_json::to_value(&trace).unwrap();
    assert!(json["ttft_ms"].is_u64(), "{json}");

    engine
        .execute(hello("spare/always-error", false), Instant::now())
        .await;
    let trace = engine.traces().recent(1).pop().unwrap();
    assert_ne!(trace.final_status, 200);
    assert_eq!(trace.ttft_ms, None, "no token, no TTFT");
    let json = serde_json::to_value(&trace).unwrap();
    assert!(json.get("ttft_ms").is_none(), "absent, not null: {json}");
}

#[test]
fn a_reload_tells_the_lane_view() {
    let lanes = WakeLanes::new(false, ready_after_41s());
    let engine = engine_from_config(wake_lane_config(true)).with_lane_status(lanes.clone());
    engine.reload(wake_lane_config(false)).unwrap();
    assert_eq!(lanes.published.load(Ordering::SeqCst), 1);
}

#[test]
fn wake_on_request_needs_route_health() {
    let mut cfg = wake_lane_config(true);
    cfg.local_executors[0].route_health = false;
    let error = Engine::validate_config(&cfg).unwrap_err();
    assert!(
        error.contains("local_executors[0].wake_on_request needs route_health: true"),
        "{error}"
    );
    assert!(Engine::validate_config(&wake_lane_config(true)).is_ok());
}

#[test]
fn wake_on_request_needs_a_wake_command() {
    let mut cfg = wake_lane_config(true);
    cfg.local_executors[0].wake_command = None;
    let error = Engine::validate_config(&cfg).unwrap_err();
    assert!(
        error.contains("local_executors[0].wake_on_request needs a wake_command"),
        "{error}"
    );
    cfg.local_executors[0].wake_on_request = false;
    assert!(
        Engine::validate_config(&cfg).is_ok(),
        "a lane that does not wake on request may leave the wake unconfigured"
    );
}

/// Two lanes on one host `gpu-box`; only the first carries power commands.
fn shared_host_config() -> Config {
    let mut cfg = wake_lane_config(true);
    let mut second = cfg.local_executors[0].clone();
    second.name = "spare".to_string();
    second.wake_command = None;
    for lane in [&mut cfg.local_executors[0], &mut second] {
        lane.host = Some("gpu-box".to_string());
    }
    cfg.local_executors[0].poweroff_command = Some("ssh gateway poweroff-host executor-1".into());
    cfg.local_executors.push(second);
    cfg
}

#[test]
fn lanes_on_one_host_may_set_its_power_commands_once() {
    let cfg = shared_host_config();
    assert!(
        Engine::validate_config(&cfg).is_ok(),
        "the second lane wakes through the host's wake_command"
    );
    let mut identical = cfg.clone();
    identical.local_executors[1].wake_command = identical.local_executors[0].wake_command.clone();
    assert!(Engine::validate_config(&identical).is_ok());
}

#[test]
fn lanes_on_one_host_must_not_carry_different_power_commands() {
    let mut cfg = shared_host_config();
    cfg.local_executors[1].poweroff_command = Some("ssh gateway poweroff-host executor-2".into());
    let error = Engine::validate_config(&cfg).unwrap_err();
    assert!(
        error.contains(
            "local_executors[1].poweroff_command differs from the one on lane `lane` (local_executors[0]) of host `gpu-box`"
        ) && error.contains("set poweroff_command on one lane of the host"),
        "{error}"
    );
    let mut alone = shared_host_config();
    alone.local_executors[0].wake_command = None;
    let error = Engine::validate_config(&alone).unwrap_err();
    assert!(
        error.contains("local_executors[1].wake_on_request needs a wake_command"),
        "no lane of the host can wake it: {error}"
    );
}

#[test]
fn a_poweroff_guard_needs_a_poweroff_command_and_one_value_per_host() {
    const GUARD: &str = "ssh gateway check-idle executor-1";
    let mut cfg = shared_host_config();
    cfg.local_executors[1].poweroff_guard_command = Some(GUARD.into());
    assert!(
        Engine::validate_config(&cfg).is_ok(),
        "the guard may sit on a lane whose host carries the poweroff_command"
    );
    cfg.local_executors[0].poweroff_guard_command = Some("ssh gateway other-check".into());
    let error = Engine::validate_config(&cfg).unwrap_err();
    assert!(
        error.contains(
            "local_executors[1].poweroff_guard_command differs from the one on lane `lane`"
        ),
        "{error}"
    );

    let mut alone = wake_lane_config(true);
    alone.local_executors[0].poweroff_command = None;
    alone.local_executors[0].poweroff_guard_command = Some(GUARD.into());
    let error = Engine::validate_config(&alone).unwrap_err();
    assert!(
        error.contains("local_executors[0].poweroff_guard_command needs a poweroff_command"),
        "{error}"
    );
}

// --- exclusive GPU group: the first choice waits, others never ----------

/// A lane view where `lane` is in GPU group `gpu0`: the first choice's
/// bounded wait is scripted (`Some(detail)` = took it, `None` = the wait
/// ran out), and a dispatch that does not wait is refused unless allowed.
struct GroupLanes {
    took: Option<&'static str>,
    try_allowed: bool,
    acquired: AtomicUsize,
    tried: AtomicUsize,
    active: Arc<AtomicUsize>,
}

impl GroupLanes {
    fn new(took: Option<&'static str>, try_allowed: bool) -> Arc<Self> {
        Arc::new(Self {
            took,
            try_allowed,
            acquired: AtomicUsize::new(0),
            tried: AtomicUsize::new(0),
            active: Arc::new(AtomicUsize::new(0)),
        })
    }

    fn busy(&self) -> crate::LaneBusy {
        self.active.fetch_add(1, Ordering::SeqCst);
        Box::new(Busy(self.active.clone()))
    }
}

impl crate::LaneStatus for GroupLanes {
    fn known_down(&self, _provider_id: &str) -> Option<String> {
        None
    }

    fn try_begin_request(&self, provider_id: &str) -> Result<Option<crate::LaneBusy>, String> {
        assert_eq!(provider_id, "lane");
        self.tried.fetch_add(1, Ordering::SeqCst);
        if self.try_allowed {
            Ok(Some(self.busy()))
        } else {
            Err("does not hold gpu group `gpu0` (lane `comfy` does)".to_string())
        }
    }

    fn acquire_for_request<'a>(
        &'a self,
        provider_id: &'a str,
    ) -> crate::BoxFuture<'a, crate::LaneAcquire> {
        assert_eq!(provider_id, "lane");
        self.acquired.fetch_add(1, Ordering::SeqCst);
        Box::pin(std::future::ready(match self.took {
            Some(detail) => crate::LaneAcquire::Ready {
                busy: Some(self.busy()),
                detail: Some(detail.to_string()),
            },
            None => crate::LaneAcquire::NotReady {
                detail:
                    "waited 120 s for gpu group `gpu0`; lane `comfy` still holds it (1 in flight)"
                        .to_string(),
                retry_after: crate::LANE_GROUP_RETRY_AFTER,
            },
        }))
    }
}

/// [`wake_lane_config`] with the lane in GPU group `gpu0` on `gpu-box`.
fn gpu_group_lane_config(wake_on_request: bool) -> Config {
    let mut cfg = wake_lane_config(wake_on_request);
    let lane = &mut cfg.local_executors[0];
    lane.host = Some("gpu-box".to_string());
    lane.exclusive_group = Some("gpu0".to_string());
    lane.release_command = Some("ssh gateway release-gpu lane".to_string());
    cfg
}

const TOOK_FROM_COMFY: &str = "waited 12 s for gpu group `gpu0`, took it from lane `comfy`";

#[tokio::test]
async fn a_first_choice_that_waited_for_its_gpu_group_is_served_and_the_decision_says_so() {
    let lanes = GroupLanes::new(Some(TOOK_FROM_COMFY), false);
    let engine = engine_from_config(gpu_group_lane_config(false)).with_lane_status(lanes.clone());
    let (_revision, outcome) = engine
        .execute(hello("local-text", true), Instant::now())
        .await;
    let ExecOutcome::Stream { mut stream, .. } = outcome else {
        panic!("expected a stream from the lane");
    };
    assert_eq!(
        lanes.active.load(Ordering::SeqCst),
        1,
        "held while streaming"
    );
    while stream.next().await.is_some() {}
    drop(stream);
    assert_eq!(
        lanes.active.load(Ordering::SeqCst),
        0,
        "released at the end"
    );
    assert_eq!(lanes.acquired.load(Ordering::SeqCst), 1);
    assert_eq!(
        lanes.tried.load(Ordering::SeqCst),
        0,
        "the held guard is used for the dispatch"
    );
    let trace = engine.traces().recent(1).pop().unwrap();
    assert_eq!(
        trace.decision.selected.as_ref().unwrap().target_id,
        "lane/echo"
    );
    assert!(
        trace
            .decision
            .reason
            .iter()
            .any(|r| r == &format!("local lane `lane` {TOOK_FROM_COMFY}")),
        "{:?}",
        trace.decision.reason
    );
}

#[tokio::test]
async fn a_gpu_group_wait_that_runs_out_falls_through_with_the_reason() {
    let lanes = GroupLanes::new(None, false);
    let engine = engine_from_config(gpu_group_lane_config(false)).with_lane_status(lanes.clone());
    let (_revision, outcome) = engine
        .execute(hello("local-text", false), Instant::now())
        .await;
    assert!(matches!(outcome, ExecOutcome::Collected { .. }));
    let trace = engine.traces().recent(1).pop().unwrap();
    assert_eq!(
        trace.decision.selected.as_ref().unwrap().target_id,
        "spare/echo"
    );
    let rejected = trace
        .decision
        .rejected
        .iter()
        .find(|r| r.target_id == "lane/echo")
        .expect("the lane is rejected with a reason");
    assert!(
        rejected
            .reason
            .contains("waited 120 s for gpu group `gpu0`"),
        "{}",
        rejected.reason
    );
    assert_eq!(lanes.active.load(Ordering::SeqCst), 0);
    assert_eq!(lanes.tried.load(Ordering::SeqCst), 0, "never dispatched");
}

#[tokio::test]
async fn a_gpu_group_wait_that_runs_out_with_no_other_target_is_a_retryable_503() {
    let lanes = GroupLanes::new(None, false);
    let engine = engine_from_config(gpu_group_lane_config(false)).with_lane_status(lanes.clone());
    let (_revision, outcome) = engine
        .execute(hello("lane/echo", false), Instant::now())
        .await;
    let ExecOutcome::Error(error) = outcome else {
        panic!("expected a retryable error");
    };
    assert_eq!(error.status, 503);
    assert_eq!(error.error_type, "local_lane_unavailable");
    assert_eq!(error.retry_after, Some(crate::LANE_GROUP_RETRY_AFTER));
    assert!(
        error.message.contains("gpu group `gpu0`"),
        "{}",
        error.message
    );
}

#[tokio::test]
async fn a_gpu_group_lane_that_is_not_the_first_choice_never_waits() {
    // Route [spare, lane]: the spare fails, the lane is reached as a
    // fallback; it does not hold its group, so it is skipped at once.
    let mut cfg = gpu_group_lane_config(false);
    cfg.routes[0].targets = vec!["spare/always-error".to_string(), "lane/echo".to_string()];
    let lanes = GroupLanes::new(Some(TOOK_FROM_COMFY), false);
    let engine = engine_from_config(cfg).with_lane_status(lanes.clone());
    let (_revision, outcome) = engine
        .execute(hello("local-text", false), Instant::now())
        .await;
    assert!(matches!(outcome, ExecOutcome::Error(_)));
    assert_eq!(lanes.acquired.load(Ordering::SeqCst), 0, "no wait");
    assert_eq!(
        lanes.tried.load(Ordering::SeqCst),
        1,
        "refused without waiting"
    );
    let trace = engine.traces().recent(1).pop().unwrap();
    assert!(
        trace
            .warnings
            .iter()
            .any(|w| w
                .contains("target lane/echo skipped: local lane `lane` does not hold gpu group")),
        "{:?}",
        trace.warnings
    );
}

#[tokio::test]
async fn a_lane_on_a_shared_host_counts_its_requests_without_wake_on_request() {
    // The machine's power-off waits for every lane's work, so a dispatch
    // to a lane on a shared host is counted even when it does not wake.
    let mut cfg = wake_lane_config(false);
    cfg.local_executors[0].host = Some("gpu-box".to_string());
    let lanes = GroupLanes::new(None, true);
    let engine = engine_from_config(cfg).with_lane_status(lanes.clone());
    engine
        .execute(hello("local-text", false), Instant::now())
        .await;
    assert_eq!(lanes.tried.load(Ordering::SeqCst), 1, "counted");
    assert_eq!(lanes.acquired.load(Ordering::SeqCst), 0, "never waits");
    assert_eq!(
        lanes.active.load(Ordering::SeqCst),
        0,
        "released at the end"
    );
}

#[tokio::test]
async fn a_lane_outside_a_gpu_group_is_never_asked_for_one() {
    let lanes = GroupLanes::new(Some(TOOK_FROM_COMFY), true);
    let engine = engine_from_config(wake_lane_config(false)).with_lane_status(lanes.clone());
    engine
        .execute(hello("local-text", false), Instant::now())
        .await;
    assert_eq!(lanes.acquired.load(Ordering::SeqCst), 0);
    assert_eq!(
        lanes.tried.load(Ordering::SeqCst),
        0,
        "no busy guard either"
    );
}

/// A lane view whose first wait for the GPU group blocks until `gate`
/// opens; later waits take the group at once. Dispatch is never refused,
/// and records how many guards were held when it was asked.
#[derive(Default)]
struct GatedLanes {
    gate: tokio::sync::Notify,
    waits: AtomicUsize,
    tried: AtomicUsize,
    active_when_tried: AtomicUsize,
    active: Arc<AtomicUsize>,
}

impl crate::LaneStatus for GatedLanes {
    fn known_down(&self, _provider_id: &str) -> Option<String> {
        None
    }

    fn try_begin_request(&self, _provider_id: &str) -> Result<Option<crate::LaneBusy>, String> {
        self.tried.fetch_add(1, Ordering::SeqCst);
        self.active_when_tried
            .store(self.active.load(Ordering::SeqCst), Ordering::SeqCst);
        self.active.fetch_add(1, Ordering::SeqCst);
        Ok(Some(Box::new(Busy(self.active.clone()))))
    }

    fn acquire_for_request<'a>(
        &'a self,
        _provider_id: &'a str,
    ) -> crate::BoxFuture<'a, crate::LaneAcquire> {
        Box::pin(async move {
            if self.waits.fetch_add(1, Ordering::SeqCst) == 0 {
                self.gate.notified().await;
            }
            self.active.fetch_add(1, Ordering::SeqCst);
            crate::LaneAcquire::Ready {
                busy: Some(Box::new(Busy(self.active.clone()))),
                detail: Some("holds gpu group `gpu0`".to_string()),
            }
        })
    }
}

#[tokio::test]
async fn a_held_gpu_group_is_let_go_when_another_target_dispatches_first() {
    // Round robin over [lane, spare]. Request A waits for the GPU as the
    // first choice; meanwhile request B takes the cursor's turn, so A's
    // real plan dispatches `spare` first. A's held guard must go as soon as
    // `spare` is dispatched, not stay held for the whole request; `lane`,
    // reached as A's fallback, then asks for the group again.
    let cfg = Config::from_yaml(
        r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: lane
    type: mock
  - id: spare
    type: mock
local_executors:
  - name: lane
    host: gpu-box
    base_url: "http://executor-1.example:8080"
    health_endpoint: "http://executor-1.example:8080/health"
    route_health: true
    exclusive_group: gpu0
    release_command: "ssh gateway release-gpu lane"
combos:
  turns:
    strategy: round_robin
    models:
      - "lane/echo"
      - "spare/always-error"
"#,
    )
    .unwrap();
    let lanes = Arc::new(GatedLanes::default());
    let engine = engine_from_config(cfg).with_lane_status(lanes.clone());
    let (a, ()) = tokio::join!(
        engine.execute(hello("turns", false), Instant::now()),
        async {
            let (_revision, b) = engine.execute(hello("turns", false), Instant::now()).await;
            assert!(matches!(b, ExecOutcome::Collected { .. }));
            lanes.gate.notify_one();
        }
    );
    assert!(matches!(a.1, ExecOutcome::Collected { .. }));
    assert_eq!(lanes.waits.load(Ordering::SeqCst), 2);
    assert_eq!(
        lanes.tried.load(Ordering::SeqCst),
        1,
        "the fallback asked again"
    );
    assert_eq!(
        lanes.active_when_tried.load(Ordering::SeqCst),
        0,
        "the held guard went when `spare` was dispatched"
    );
    assert_eq!(lanes.active.load(Ordering::SeqCst), 0);
}

#[test]
fn a_gpu_group_needs_a_host_and_a_release_command() {
    assert!(Engine::validate_config(&gpu_group_lane_config(true)).is_ok());
    let mut cfg = gpu_group_lane_config(true);
    cfg.local_executors[0].host = None;
    cfg.local_executors[0].release_command = None;
    let error = Engine::validate_config(&cfg).unwrap_err();
    assert!(
        error.contains("local_executors[0].exclusive_group needs host"),
        "{error}"
    );
    assert!(
        error.contains("local_executors[0].exclusive_group needs a release_command"),
        "{error}"
    );
}
