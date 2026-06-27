use sb_core::{ExecutionTaskType, PrivacyClass};
use sb_eval::{
    normalize_mechanical_checks, ArtifactKind, CaseStore, EvalArtifactRef, EvalCaseManifest,
    EvalEvidenceSnapshot, EvalMetric, EvalOutcome, EvalReportQuery, EvalRunIngest, EvalStore,
    HarnessConversion, HarnessKind, InMemoryEvalStore, MechanicalCheckKind, MechanicalCheckSummary,
    RunStatus, Verdict,
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
            ..EvalReportQuery::default()
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

#[test]
fn codex_cli_converter_produces_sanitized_eval_run() {
    let raw = serde_json::json!({
        "session_id": "codex-session-1",
        "status": "succeeded",
        "version": "0.12.3",
        "duration_ms": 3210,
        "total_cost_usd": 0.0123,
        "artifacts": [
            {
                "kind": "trace",
                "reference": "trace:codex-session-1",
                "sha256": "trace-sha",
                "privacy_level": "standard",
                "metadata": { "trace_id": "codex-session-1" }
            }
        ]
    });

    let run = HarnessConversion {
        kind: HarnessKind::CodexCli,
        case_id: "react-bug-001".to_string(),
        case_revision: "rev-1".to_string(),
        strategy_id: Some("default".to_string()),
        verdict: Some(Verdict::Pass),
        status: None,
        input: raw,
    }
    .convert()
    .unwrap();

    assert_eq!(run.harness, "codex-cli");
    assert_eq!(run.harness_version.as_deref(), Some("0.12.3"));
    assert_eq!(run.source_run_id.as_deref(), Some("codex-session-1"));
    assert_eq!(run.status, RunStatus::Succeeded);
    assert_eq!(run.outcome.verdict, Verdict::Pass);
    assert_eq!(run.latency_ms(), Some(3210));
    assert_eq!(run.cost_micros(), Some(12_300));
    assert_eq!(run.artifacts[0].reference, "trace:codex-session-1");
}

#[test]
fn claude_code_and_aider_converters_use_their_native_ids() {
    let claude = HarnessConversion {
        kind: HarnessKind::ClaudeCode,
        case_id: "react-bug-001".to_string(),
        case_revision: "rev-1".to_string(),
        strategy_id: None,
        verdict: None,
        status: None,
        input: serde_json::json!({
            "conversation_id": "claude-conv-1",
            "status": "completed",
            "elapsed_ms": 900,
            "cost_micros": 5000
        }),
    }
    .convert()
    .unwrap();

    assert_eq!(claude.harness, "claude-code");
    assert_eq!(claude.source_run_id.as_deref(), Some("claude-conv-1"));
    assert_eq!(claude.outcome.verdict, Verdict::NotEvaluated);

    let aider = HarnessConversion {
        kind: HarnessKind::Aider,
        case_id: "react-bug-001".to_string(),
        case_revision: "rev-1".to_string(),
        strategy_id: None,
        verdict: Some(Verdict::Fail),
        status: None,
        input: serde_json::json!({
            "chat_history_id": "aider-chat-1",
            "exit_status": 1,
            "duration_ms": 1200
        }),
    }
    .convert()
    .unwrap();

    assert_eq!(aider.harness, "aider");
    assert_eq!(aider.source_run_id.as_deref(), Some("aider-chat-1"));
    assert_eq!(aider.status, RunStatus::Failed);
    assert_eq!(aider.outcome.verdict, Verdict::Fail);
}

#[test]
fn converter_rejects_raw_prompt_fields() {
    let err = HarnessConversion {
        kind: HarnessKind::CodexCli,
        case_id: "react-bug-001".to_string(),
        case_revision: "rev-1".to_string(),
        strategy_id: None,
        verdict: None,
        status: None,
        input: serde_json::json!({
            "session_id": "codex-session-1",
            "raw_prompt": "fix this secret thing"
        }),
    }
    .convert()
    .expect_err("raw prompt fields must be rejected");

    assert!(err.to_string().contains("raw_prompt"));
}

#[test]
fn report_filters_strategy_version_cache_hits_and_time_window() {
    let mut store = InMemoryEvalStore::default();
    store.put_case(case("react-bug-001")).unwrap();

    let mut cache_hit = run("react-bug-001", "codex-1", "codex-cli", Verdict::Pass);
    cache_hit.harness_version = Some("1.0.0".to_string());
    cache_hit.strategy_id = "default".to_string();
    cache_hit.started_at_ms = Some(1_000);
    cache_hit.cache_status = Some(sb_core::CacheStatus::Hit);
    store.ingest_run(cache_hit).unwrap();

    let mut matched = run("react-bug-001", "codex-2", "codex-cli", Verdict::Fail);
    matched.harness_version = Some("2.0.0".to_string());
    matched.strategy_id = "repair".to_string();
    matched.started_at_ms = Some(2_000);
    matched.cache_status = Some(sb_core::CacheStatus::Miss);
    store.ingest_run(matched).unwrap();

    let mut out_of_window = run("react-bug-001", "codex-3", "codex-cli", Verdict::Pass);
    out_of_window.harness_version = Some("2.0.0".to_string());
    out_of_window.strategy_id = "repair".to_string();
    out_of_window.started_at_ms = Some(9_000);
    out_of_window.finished_at_ms = Some(11_000);
    out_of_window.cache_status = Some(sb_core::CacheStatus::Miss);
    store.ingest_run(out_of_window).unwrap();

    let report = store
        .report(EvalReportQuery {
            task_type: Some(ExecutionTaskType::Coding),
            tag: Some("react".to_string()),
            min_runs: 1,
            harness: Some("codex-cli".to_string()),
            harness_version: Some("2.0.0".to_string()),
            strategy_id: Some("repair".to_string()),
            exclude_cache_hits: true,
            since_ms: Some(1_500),
            until_ms: Some(3_000),
            group_by_strategy: true,
            group_by_harness_version: true,
        })
        .unwrap();

    assert_eq!(report.rows.len(), 1);
    let row = &report.rows[0];
    assert_eq!(row.harness, "codex-cli");
    assert_eq!(row.harness_version.as_deref(), Some("2.0.0"));
    assert_eq!(row.strategy_id.as_deref(), Some("repair"));
    assert_eq!(row.runs, 1);
    assert_eq!(row.fail_count, 1);
}

#[test]
fn golden_harness_converter_fixtures_stay_sanitized_and_stable() {
    let fixtures = [
        (
            HarnessKind::CodexCli,
            include_str!("fixtures/codex_cli_sanitized.json"),
            "codex-cli",
            "codex-session-golden",
            "0.13.0",
            RunStatus::Succeeded,
            Verdict::Pass,
        ),
        (
            HarnessKind::ClaudeCode,
            include_str!("fixtures/claude_code_sanitized.json"),
            "claude-code",
            "claude-conversation-golden",
            "2.1.0",
            RunStatus::Succeeded,
            Verdict::Pass,
        ),
        (
            HarnessKind::Aider,
            include_str!("fixtures/aider_sanitized.json"),
            "aider",
            "aider-chat-golden",
            "0.86.1",
            RunStatus::Failed,
            Verdict::Fail,
        ),
    ];

    for (kind, raw, harness, source_run_id, version, status, verdict) in fixtures {
        let input: serde_json::Value = serde_json::from_str(raw).unwrap();
        let run = HarnessConversion {
            kind,
            case_id: "react-bug-001".to_string(),
            case_revision: "rev-1".to_string(),
            strategy_id: None,
            verdict: None,
            status: None,
            input,
        }
        .convert()
        .unwrap();

        assert_eq!(run.harness, harness);
        assert_eq!(run.source_run_id.as_deref(), Some(source_run_id));
        assert_eq!(run.harness_version.as_deref(), Some(version));
        assert_eq!(run.status, status);
        assert_eq!(run.outcome.verdict, verdict);
        assert!(run.latency_ms().is_some());
        assert!(run.cost_micros().is_some());
        if harness == "codex-cli" {
            assert_eq!(run.outcome.checks.len(), 1);
            assert_eq!(run.outcome.checks[0].id, "tests");
            assert_eq!(run.outcome.checks[0].status, Verdict::Pass);
        }
        assert!(run.artifacts.iter().all(|artifact| {
            !artifact.reference.starts_with("inline:") && !artifact.reference.starts_with('/')
        }));
    }
}

#[test]
fn mechanical_summaries_normalize_to_outcome_checks_without_bodies() {
    let checks = normalize_mechanical_checks(&[
        MechanicalCheckSummary {
            id: "tests".to_string(),
            kind: MechanicalCheckKind::TestsPass,
            passed: Some(true),
            exit_code: Some(0),
            evidence_ref: Some("artifact:test-log-sha".to_string()),
            message: Some("12 passed".to_string()),
        },
        MechanicalCheckSummary {
            id: "build".to_string(),
            kind: MechanicalCheckKind::BuildPass,
            passed: Some(false),
            exit_code: Some(1),
            evidence_ref: Some("artifact:build-log-sha".to_string()),
            message: None,
        },
        MechanicalCheckSummary {
            id: "diff-scope".to_string(),
            kind: MechanicalCheckKind::DiffScope,
            passed: None,
            exit_code: None,
            evidence_ref: None,
            message: None,
        },
    ])
    .unwrap();

    assert_eq!(checks.len(), 3);
    assert_eq!(checks[0].id, "tests");
    assert_eq!(checks[0].status, Verdict::Pass);
    assert_eq!(
        checks[0].evidence_ref.as_deref(),
        Some("artifact:test-log-sha")
    );
    assert_eq!(checks[1].id, "build");
    assert_eq!(checks[1].status, Verdict::Fail);
    assert_eq!(checks[2].id, "diff-scope");
    assert_eq!(checks[2].status, Verdict::Inconclusive);
}

#[test]
fn eval_evidence_snapshot_filters_by_task_and_harness_candidates() {
    let report = sb_eval::EvalReport {
        rows: vec![
            sb_eval::EvalReportRow {
                harness: "codex-cli".to_string(),
                runs: 3,
                pass_count: 2,
                success_rate: Some(2.0 / 3.0),
                ..Default::default()
            },
            sb_eval::EvalReportRow {
                harness: "claude-code".to_string(),
                runs: 2,
                pass_count: 2,
                success_rate: Some(1.0),
                ..Default::default()
            },
        ],
    };
    let snapshot = EvalEvidenceSnapshot::from_report(
        &EvalReportQuery {
            task_type: Some(ExecutionTaskType::Coding),
            tag: Some("react".to_string()),
            min_runs: 2,
            ..Default::default()
        },
        report,
        42,
    );

    let matched = snapshot.matching_rows(
        Some(ExecutionTaskType::Coding),
        ["codex-cli".to_string()].into_iter().collect(),
    );

    assert_eq!(
        snapshot.schema_version,
        "switchback.eval.evidence_snapshot/v1"
    );
    assert_eq!(matched.len(), 1);
    assert_eq!(matched[0].task_type, Some(ExecutionTaskType::Coding));
    assert_eq!(matched[0].tag.as_deref(), Some("react"));
    assert_eq!(matched[0].harness, "codex-cli");
    assert_eq!(matched[0].runs, 3);
}
