use sb_core::{ExecutionTaskType, PrivacyClass};
use sb_eval::{
    ArtifactKind, CaseStore, EvalArtifactRef, EvalCaseManifest, EvalMetric, EvalOutcome,
    EvalReportQuery, EvalRunIngest, EvalStore, InMemoryEvalStore, RunStatus, Verdict,
};

fn case(case_id: &str) -> EvalCaseManifest {
    EvalCaseManifest {
        schema_version: "switchback.eval.case/v1".to_string(),
        case_id: case_id.to_string(),
        case_revision: "rev-1".to_string(),
        task_type: ExecutionTaskType::Coding,
        privacy_level: PrivacyClass::Standard,
        tags: vec!["react".to_string()],
        fixture: sb_eval::EvalFixtureRef {
            kind: "git_repo".to_string(),
            uri: "https://example.invalid/repo.git".to_string(),
            revision: Some("abc123".to_string()),
            fingerprint: Some("fixture-sha".to_string()),
        },
        prompt_ref: Some(sb_eval::PromptRef {
            kind: "sha256".to_string(),
            reference: "prompt-sha".to_string(),
            sha256: Some("prompt-sha".to_string()),
        }),
        success_criteria: vec![sb_eval::SuccessCriterion {
            id: "tests".to_string(),
            kind: "tests_pass".to_string(),
            required: true,
            params: serde_json::json!({}),
        }],
        commands: Vec::new(),
        allowed_paths: vec!["src/**".to_string()],
        forbidden_paths: vec![".env".to_string()],
    }
}

fn run(case_id: &str, source_run_id: &str, harness: &str, verdict: Verdict) -> EvalRunIngest {
    EvalRunIngest {
        schema_version: "switchback.eval.run/v1".to_string(),
        run_id: None,
        source_run_id: Some(source_run_id.to_string()),
        case_id: case_id.to_string(),
        case_revision: "rev-1".to_string(),
        harness: harness.to_string(),
        harness_version: Some("1.0.0".to_string()),
        strategy_id: "default".to_string(),
        strategy_version: Some("strategy/v1".to_string()),
        started_at_ms: Some(1000),
        finished_at_ms: Some(3000),
        job: None,
        receipt: None,
        harness_summary: None,
        status: RunStatus::Succeeded,
        outcome: EvalOutcome {
            verdict,
            confidence: None,
            checks: vec![sb_eval::CheckResult {
                id: "tests".to_string(),
                status: verdict,
                message: Some("tests normalized".to_string()),
                evidence_ref: None,
            }],
            evidence: Vec::new(),
        },
        metrics: vec![
            EvalMetric {
                name: "latency_ms".to_string(),
                value: 2000.0,
                unit: "ms".to_string(),
                source: "harness".to_string(),
            },
            EvalMetric {
                name: "cost_micros".to_string(),
                value: 42000.0,
                unit: "micros_usd".to_string(),
                source: "switchback".to_string(),
            },
        ],
        artifacts: vec![EvalArtifactRef {
            kind: ArtifactKind::Trace,
            reference: "trace:req_123".to_string(),
            sha256: None,
            privacy_level: PrivacyClass::Standard,
            metadata: serde_json::json!({}),
        }],
        retry_count: Some(1),
        cache_status: Some(sb_core::CacheStatus::Miss),
    }
}

#[test]
fn validates_case_manifest_required_fields() {
    let invalid = case("");

    let err = invalid
        .validate()
        .expect_err("empty case id must be rejected");

    assert!(err.to_string().contains("case_id must not be empty"));
}

#[test]
fn rejects_raw_prompt_response_and_inline_artifacts() {
    let mut unsafe_run = run("react-bug-001", "codex-1", "codex-cli", Verdict::Pass);
    unsafe_run.artifacts.push(EvalArtifactRef {
        kind: ArtifactKind::Diff,
        reference: "inline:diff --git a/src/App.tsx b/src/App.tsx".to_string(),
        sha256: None,
        privacy_level: PrivacyClass::Standard,
        metadata: serde_json::json!({
            "raw_prompt": "fix this bug",
            "stdout": "full tool log"
        }),
    });

    let err = unsafe_run
        .validate()
        .expect_err("unsafe raw fields must be rejected");

    assert!(err.to_string().contains("raw_prompt"));
    assert!(err.to_string().contains("stdout"));
    assert!(err.to_string().contains("inline artifact"));
}

#[test]
fn ingest_is_idempotent_by_harness_source_run_id() {
    let mut store = InMemoryEvalStore::default();
    store.put_case(case("react-bug-001")).unwrap();
    let first = store
        .ingest_run(run(
            "react-bug-001",
            "codex-session-1",
            "codex-cli",
            Verdict::Pass,
        ))
        .unwrap();
    let second = store
        .ingest_run(run(
            "react-bug-001",
            "codex-session-1",
            "codex-cli",
            Verdict::Fail,
        ))
        .unwrap();

    assert!(first.inserted);
    assert!(!second.inserted);
    assert_eq!(first.run_id, second.run_id);
    assert_eq!(store.runs().len(), 1);
}

#[test]
fn stable_run_id_uses_manifest_content_without_source_run_id() {
    let mut first = run("react-bug-001", "", "codex-cli", Verdict::Pass);
    first.source_run_id = None;
    first.started_at_ms = Some(1_000);

    let mut second = first.clone();
    second.started_at_ms = Some(2_000);

    assert_ne!(first.stable_run_id(), second.stable_run_id());
    assert_eq!(first.stable_run_id(), first.clone().stable_run_id());
}

#[test]
fn report_groups_by_harness_and_surfaces_unknowns() {
    let mut store = InMemoryEvalStore::default();
    store.put_case(case("react-bug-001")).unwrap();
    store
        .ingest_run(run("react-bug-001", "codex-1", "codex-cli", Verdict::Pass))
        .unwrap();
    store
        .ingest_run(run("react-bug-001", "codex-2", "codex-cli", Verdict::Fail))
        .unwrap();
    store
        .ingest_run(run(
            "react-bug-001",
            "claude-1",
            "claude-code",
            Verdict::Inconclusive,
        ))
        .unwrap();

    let report = store
        .report(EvalReportQuery {
            task_type: Some(ExecutionTaskType::Coding),
            tag: Some("react".to_string()),
            min_runs: 1,
        })
        .unwrap();

    let codex = report
        .rows
        .iter()
        .find(|row| row.harness == "codex-cli")
        .unwrap();
    assert_eq!(codex.runs, 2);
    assert_eq!(codex.pass_count, 1);
    assert_eq!(codex.fail_count, 1);
    assert_eq!(codex.inconclusive_count, 0);
    assert_eq!(codex.median_latency_ms, Some(2000));
    assert_eq!(codex.median_cost_micros, Some(42000));

    let claude = report
        .rows
        .iter()
        .find(|row| row.harness == "claude-code")
        .unwrap();
    assert_eq!(claude.runs, 1);
    assert_eq!(claude.inconclusive_count, 1);
}
