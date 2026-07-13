use super::*;
use futures::StreamExt;
use sb_core::{AiRequest, AiStreamEvent, Config, EvaluationEventKind, Message, ResponseFormat};
use std::collections::HashSet;
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
    let req = AiRequest::new("mock/echo", vec![Message::user("hi")]);
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
