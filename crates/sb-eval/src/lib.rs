//! Switchback execution evaluation contracts.
//!
//! This crate is intentionally offline/control-plane shaped. It defines the
//! sanitized case/run envelopes, validation, and report aggregation used to
//! compare harness outcomes. It does not execute harnesses and does not depend
//! on the runtime/router hot path.

use sb_core::{
    CacheLookupReceipt, CacheStatus, ExecutionJob, ExecutionReceipt, ExecutionTaskType,
    HarnessRunSummary, PrivacyClass,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub const CASE_SCHEMA_VERSION: &str = "switchback.eval.case/v1";
pub const RUN_SCHEMA_VERSION: &str = "switchback.eval.run/v1";
pub const EVIDENCE_SNAPSHOT_SCHEMA_VERSION: &str = "switchback.eval.evidence_snapshot/v1";
const MILLIS_PER_DAY: u64 = 24 * 60 * 60 * 1_000;

const FORBIDDEN_METADATA_KEYS: &[&str] = &[
    "raw_prompt",
    "prompt",
    "raw_response",
    "response",
    "stdout",
    "stderr",
    "raw_log",
    "log",
    "raw_diff",
    "diff",
    "secret",
    "token",
    "api_key",
    "password",
];

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
#[error("{0}")]
pub struct EvalStoreError(pub String);

pub type Result<T> = std::result::Result<T, EvalStoreError>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvalCaseManifest {
    pub schema_version: String,
    pub case_id: String,
    pub case_revision: String,
    pub task_type: ExecutionTaskType,
    pub privacy_level: PrivacyClass,
    #[serde(default)]
    pub tags: Vec<String>,
    pub fixture: EvalFixtureRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_ref: Option<PromptRef>,
    #[serde(default)]
    pub success_criteria: Vec<SuccessCriterion>,
    #[serde(default)]
    pub commands: Vec<EvalCommand>,
    #[serde(default)]
    pub allowed_paths: Vec<String>,
    #[serde(default)]
    pub forbidden_paths: Vec<String>,
}

impl EvalCaseManifest {
    pub fn validate(&self) -> Result<()> {
        let mut problems = Vec::new();
        if self.schema_version != CASE_SCHEMA_VERSION {
            problems.push(format!(
                "schema_version must be {CASE_SCHEMA_VERSION}, got `{}`",
                self.schema_version
            ));
        }
        require_non_empty("case_id", &self.case_id, &mut problems);
        require_non_empty("case_revision", &self.case_revision, &mut problems);
        if self.task_type == ExecutionTaskType::Unknown {
            problems.push("task_type must not be unknown".to_string());
        }
        self.fixture.validate(&mut problems);
        if let Some(prompt_ref) = &self.prompt_ref {
            prompt_ref.validate(&mut problems);
        }
        let mut criterion_ids = BTreeSet::new();
        for (i, criterion) in self.success_criteria.iter().enumerate() {
            criterion.validate(i, &mut criterion_ids, &mut problems);
        }
        for (i, command) in self.commands.iter().enumerate() {
            command.validate(i, &mut problems);
        }
        finish_validation(problems)
    }

    pub fn has_tag(&self, tag: &str) -> bool {
        self.tags.iter().any(|candidate| candidate == tag)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EvalFixtureRef {
    pub kind: String,
    pub uri: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
}

impl EvalFixtureRef {
    fn validate(&self, problems: &mut Vec<String>) {
        require_non_empty("fixture.kind", &self.kind, problems);
        require_non_empty("fixture.uri", &self.uri, problems);
        if self
            .fingerprint
            .as_ref()
            .is_some_and(|fingerprint| fingerprint.trim().is_empty())
        {
            problems.push("fixture.fingerprint must not be empty".to_string());
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PromptRef {
    pub kind: String,
    pub reference: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

impl PromptRef {
    fn validate(&self, problems: &mut Vec<String>) {
        require_non_empty("prompt_ref.kind", &self.kind, problems);
        require_non_empty("prompt_ref.reference", &self.reference, problems);
        if self
            .sha256
            .as_ref()
            .is_some_and(|sha| sha.trim().is_empty())
        {
            problems.push("prompt_ref.sha256 must not be empty".to_string());
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SuccessCriterion {
    pub id: String,
    pub kind: String,
    pub required: bool,
    #[serde(default)]
    pub params: serde_json::Value,
}

impl SuccessCriterion {
    fn validate(&self, index: usize, seen: &mut BTreeSet<String>, problems: &mut Vec<String>) {
        if self.id.trim().is_empty() {
            problems.push(format!("success_criteria[{index}].id must not be empty"));
        } else if !seen.insert(self.id.clone()) {
            problems.push(format!(
                "success_criteria[{index}].id duplicates `{}`",
                self.id
            ));
        }
        require_non_empty(
            &format!("success_criteria[{index}].kind"),
            &self.kind,
            problems,
        );
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EvalCommand {
    pub id: String,
    #[serde(default)]
    pub command: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

impl EvalCommand {
    fn validate(&self, index: usize, problems: &mut Vec<String>) {
        require_non_empty(&format!("commands[{index}].id"), &self.id, problems);
        if self.command.is_empty() {
            problems.push(format!("commands[{index}].command must not be empty"));
        }
        for (arg_i, arg) in self.command.iter().enumerate() {
            if arg.trim().is_empty() {
                problems.push(format!(
                    "commands[{index}].command[{arg_i}] must not be empty"
                ));
            }
        }
        if self.timeout_ms == Some(0) {
            problems.push(format!("commands[{index}].timeout_ms must be positive"));
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvalRunIngest {
    pub schema_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_run_id: Option<String>,
    pub case_id: String,
    pub case_revision: String,
    pub harness: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_version: Option<String>,
    pub strategy_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strategy_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<ExecutionJob>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt: Option<ExecutionReceipt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_summary: Option<HarnessRunSummary>,
    pub status: RunStatus,
    pub outcome: EvalOutcome,
    #[serde(default)]
    pub metrics: Vec<EvalMetric>,
    #[serde(default)]
    pub artifacts: Vec<EvalArtifactRef>,
    #[serde(default)]
    pub human_outcomes: Vec<HumanOutcomeSignal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_count: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_status: Option<CacheStatus>,
}

impl EvalRunIngest {
    pub fn validate(&self) -> Result<()> {
        let mut problems = Vec::new();
        if self.schema_version != RUN_SCHEMA_VERSION {
            problems.push(format!(
                "schema_version must be {RUN_SCHEMA_VERSION}, got `{}`",
                self.schema_version
            ));
        }
        require_non_empty("case_id", &self.case_id, &mut problems);
        require_non_empty("case_revision", &self.case_revision, &mut problems);
        require_non_empty("harness", &self.harness, &mut problems);
        require_non_empty("strategy_id", &self.strategy_id, &mut problems);
        if let (Some(started), Some(finished)) = (self.started_at_ms, self.finished_at_ms) {
            if finished < started {
                problems.push("finished_at_ms must be >= started_at_ms".to_string());
            }
        }
        self.outcome.validate(&mut problems);
        for (i, metric) in self.metrics.iter().enumerate() {
            metric.validate(i, &mut problems);
        }
        for (i, artifact) in self.artifacts.iter().enumerate() {
            artifact.validate(i, &mut problems);
        }
        for (i, outcome) in self.human_outcomes.iter().enumerate() {
            outcome.validate(i, &mut problems);
        }
        finish_validation(problems)
    }

    pub fn stable_run_id(&self) -> String {
        if let Some(run_id) = self.run_id.as_ref().filter(|id| !id.trim().is_empty()) {
            return run_id.clone();
        }
        let mut hasher = Sha256::new();
        hasher.update(self.case_id.as_bytes());
        hasher.update(b"\0");
        hasher.update(self.case_revision.as_bytes());
        hasher.update(b"\0");
        hasher.update(self.harness.as_bytes());
        hasher.update(b"\0");
        if let Some(source_run_id) = self
            .source_run_id
            .as_ref()
            .filter(|id| !id.trim().is_empty())
        {
            hasher.update(source_run_id.as_bytes());
        } else if let Ok(run_json) = serde_json::to_vec(self) {
            hasher.update(&run_json);
        }
        format!("eval_run_{:x}", hasher.finalize())
    }

    pub fn latency_ms(&self) -> Option<u64> {
        metric_as_u64(&self.metrics, "latency_ms").or_else(|| {
            self.started_at_ms
                .zip(self.finished_at_ms)
                .map(|(started, finished)| finished.saturating_sub(started))
        })
    }

    pub fn cost_micros(&self) -> Option<u64> {
        metric_as_u64(&self.metrics, "cost_micros")
            .or_else(|| metric_as_u64(&self.metrics, "estimated_cost_micros"))
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Succeeded,
    Failed,
    Cancelled,
    TimedOut,
    Inconclusive,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum HumanOutcomeKind {
    Accepted,
    Edited,
    Retried,
    Abandoned,
    RolledBack,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HumanOutcomeSignal {
    pub kind: HumanOutcomeKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occurred_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl HumanOutcomeSignal {
    fn validate(&self, index: usize, problems: &mut Vec<String>) {
        if self
            .source
            .as_ref()
            .is_some_and(|source| source.trim().is_empty())
        {
            problems.push(format!("human_outcomes[{index}].source must not be empty"));
        }
        if let Some(evidence_ref) = self.evidence_ref.as_ref() {
            require_non_empty(
                &format!("human_outcomes[{index}].evidence_ref"),
                evidence_ref,
                problems,
            );
            if evidence_ref.trim_start().starts_with("inline:") {
                problems.push(format!(
                    "human_outcomes[{index}].evidence_ref inline evidence is not allowed"
                ));
            }
            if looks_like_absolute_path(evidence_ref) {
                problems.push(format!(
                    "human_outcomes[{index}].evidence_ref must be redacted, relative, or stable id"
                ));
            }
        }
        if self.note.as_ref().is_some_and(|note| note.len() > 512) {
            problems.push(format!("human_outcomes[{index}].note must be <= 512 bytes"));
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessKind {
    CodexCli,
    ClaudeCode,
    Aider,
}

impl HarnessKind {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "codex-cli" | "codex" => Some(Self::CodexCli),
            "claude-code" | "claude" => Some(Self::ClaudeCode),
            "aider" => Some(Self::Aider),
            _ => None,
        }
    }

    pub fn harness_id(self) -> &'static str {
        match self {
            Self::CodexCli => "codex-cli",
            Self::ClaudeCode => "claude-code",
            Self::Aider => "aider",
        }
    }

    fn source_id_keys(self) -> &'static [&'static str] {
        match self {
            Self::CodexCli => &["session_id", "run_id", "id"],
            Self::ClaudeCode => &["conversation_id", "session_id", "run_id", "id"],
            Self::Aider => &["chat_history_id", "session_id", "run_id", "id"],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HarnessConversion {
    pub kind: HarnessKind,
    pub case_id: String,
    pub case_revision: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strategy_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<Verdict>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<RunStatus>,
    pub input: serde_json::Value,
}

impl HarnessConversion {
    pub fn convert(&self) -> Result<EvalRunIngest> {
        let mut problems = Vec::new();
        require_non_empty("case_id", &self.case_id, &mut problems);
        require_non_empty("case_revision", &self.case_revision, &mut problems);
        collect_forbidden_metadata_keys(&self.input, "input", &mut problems);
        finish_validation(problems)?;

        let source_run_id = first_string(&self.input, self.kind.source_id_keys());
        let harness_version = first_string(&self.input, &["harness_version", "version"]);
        let strategy_id = self
            .strategy_id
            .clone()
            .or_else(|| first_string(&self.input, &["strategy_id", "strategy"]))
            .unwrap_or_else(|| "default".to_string());
        let status = self
            .status
            .or_else(|| parse_run_status_from_input(self.kind, &self.input))
            .unwrap_or(RunStatus::Inconclusive);
        let verdict = self
            .verdict
            .or_else(|| parse_verdict_from_input(&self.input))
            .unwrap_or(Verdict::NotEvaluated);
        let mut metrics = Vec::new();
        push_metric_from_first_number(
            &mut metrics,
            "latency_ms",
            "ms",
            &self.input,
            &["latency_ms", "duration_ms", "elapsed_ms"],
        );
        if let Some(cost_micros) =
            first_number(&self.input, &["cost_micros", "estimated_cost_micros"])
        {
            metrics.push(EvalMetric {
                name: "cost_micros".to_string(),
                value: cost_micros,
                unit: "micros_usd".to_string(),
                source: "harness".to_string(),
            });
        } else if let Some(cost_usd) = first_number(&self.input, &["total_cost_usd", "cost_usd"]) {
            metrics.push(EvalMetric {
                name: "cost_micros".to_string(),
                value: (cost_usd * 1_000_000.0).round(),
                unit: "micros_usd".to_string(),
                source: "harness".to_string(),
            });
        }
        push_metric_from_first_number(
            &mut metrics,
            "input_tokens",
            "count",
            &self.input,
            &["input_tokens", "tokens_in"],
        );
        push_metric_from_first_number(
            &mut metrics,
            "output_tokens",
            "count",
            &self.input,
            &["output_tokens", "tokens_out"],
        );

        let artifacts = parse_artifacts(&self.input)?;
        let checks = parse_mechanical_checks(&self.input)?;
        let started_at_ms = first_u64(&self.input, &["started_at_ms", "start_ms"]);
        let finished_at_ms = first_u64(&self.input, &["finished_at_ms", "end_ms"]);
        let retry_count = first_u64(&self.input, &["retry_count", "retries"]).map(|v| v as u32);
        let cache_status = first_string(&self.input, &["cache_status"])
            .and_then(|value| parse_cache_status(&value));

        let run = EvalRunIngest {
            schema_version: RUN_SCHEMA_VERSION.to_string(),
            run_id: first_string(&self.input, &["eval_run_id"]),
            source_run_id,
            case_id: self.case_id.clone(),
            case_revision: self.case_revision.clone(),
            harness: self.kind.harness_id().to_string(),
            harness_version,
            strategy_id,
            strategy_version: first_string(&self.input, &["strategy_version"]),
            started_at_ms,
            finished_at_ms,
            job: None,
            receipt: None,
            harness_summary: None,
            status,
            outcome: EvalOutcome {
                verdict,
                confidence: first_number(&self.input, &["confidence"]).map(|v| v as f32),
                checks,
                evidence: Vec::new(),
            },
            metrics,
            artifacts,
            human_outcomes: Vec::new(),
            retry_count,
            cache_status,
        };
        run.validate()?;
        Ok(run)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvalOutcome {
    pub verdict: Verdict,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
    #[serde(default)]
    pub checks: Vec<CheckResult>,
    #[serde(default)]
    pub evidence: Vec<EvidenceRef>,
}

impl EvalOutcome {
    fn validate(&self, problems: &mut Vec<String>) {
        if let Some(confidence) = self.confidence {
            if !(0.0..=1.0).contains(&confidence) {
                problems.push("outcome.confidence must be between 0 and 1".to_string());
            }
        }
        let mut check_ids = BTreeSet::new();
        for (i, check) in self.checks.iter().enumerate() {
            check.validate(i, &mut check_ids, problems);
        }
        for (i, evidence) in self.evidence.iter().enumerate() {
            evidence.validate(i, problems);
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Pass,
    Fail,
    Partial,
    Inconclusive,
    NotEvaluated,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CheckResult {
    pub id: String,
    pub status: Verdict,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_ref: Option<String>,
}

impl CheckResult {
    fn validate(&self, index: usize, seen: &mut BTreeSet<String>, problems: &mut Vec<String>) {
        if self.id.trim().is_empty() {
            problems.push(format!("outcome.checks[{index}].id must not be empty"));
        } else if !seen.insert(self.id.clone()) {
            problems.push(format!(
                "outcome.checks[{index}].id duplicates `{}`",
                self.id
            ));
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MechanicalCheckKind {
    TestsPass,
    BuildPass,
    LintPass,
    DiffScope,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MechanicalCheckSummary {
    pub id: String,
    pub kind: MechanicalCheckKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

pub fn normalize_mechanical_checks(
    summaries: &[MechanicalCheckSummary],
) -> Result<Vec<CheckResult>> {
    let mut problems = Vec::new();
    let mut seen = BTreeSet::new();
    let mut checks = Vec::with_capacity(summaries.len());
    for (index, summary) in summaries.iter().enumerate() {
        if summary.id.trim().is_empty() {
            problems.push(format!("mechanical_checks[{index}].id must not be empty"));
        } else if !seen.insert(summary.id.clone()) {
            problems.push(format!(
                "mechanical_checks[{index}].id duplicates `{}`",
                summary.id
            ));
        }
        if let Some(evidence_ref) = &summary.evidence_ref {
            if evidence_ref.trim().is_empty() {
                problems.push(format!(
                    "mechanical_checks[{index}].evidence_ref must not be empty"
                ));
            }
            if evidence_ref.trim_start().starts_with("inline:")
                || looks_like_absolute_path(evidence_ref)
            {
                problems.push(format!(
                    "mechanical_checks[{index}].evidence_ref must be a redacted reference"
                ));
            }
        }
        if summary
            .message
            .as_ref()
            .is_some_and(|message| message.len() > 256 || message.contains('\n'))
        {
            problems.push(format!(
                "mechanical_checks[{index}].message must be a short single-line summary"
            ));
        }
        checks.push(CheckResult {
            id: summary.id.clone(),
            status: mechanical_status(summary),
            message: summary.message.clone(),
            evidence_ref: summary.evidence_ref.clone(),
        });
    }
    finish_validation(problems)?;
    Ok(checks)
}

fn mechanical_status(summary: &MechanicalCheckSummary) -> Verdict {
    match (summary.passed, summary.exit_code) {
        (Some(true), _) | (None, Some(0)) => Verdict::Pass,
        (Some(false), _) | (None, Some(_)) => Verdict::Fail,
        (None, None) => Verdict::Inconclusive,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EvidenceRef {
    pub kind: String,
    pub reference: String,
}

impl EvidenceRef {
    fn validate(&self, index: usize, problems: &mut Vec<String>) {
        require_non_empty(
            &format!("outcome.evidence[{index}].kind"),
            &self.kind,
            problems,
        );
        require_non_empty(
            &format!("outcome.evidence[{index}].reference"),
            &self.reference,
            problems,
        );
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvalMetric {
    pub name: String,
    pub value: f64,
    pub unit: String,
    pub source: String,
}

impl EvalMetric {
    fn validate(&self, index: usize, problems: &mut Vec<String>) {
        require_non_empty(&format!("metrics[{index}].name"), &self.name, problems);
        require_non_empty(&format!("metrics[{index}].unit"), &self.unit, problems);
        require_non_empty(&format!("metrics[{index}].source"), &self.source, problems);
        if !self.value.is_finite() {
            problems.push(format!("metrics[{index}].value must be finite"));
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvalArtifactRef {
    pub kind: ArtifactKind,
    pub reference: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    pub privacy_level: PrivacyClass,
    #[serde(default)]
    pub metadata: serde_json::Value,
}

impl EvalArtifactRef {
    fn validate(&self, index: usize, problems: &mut Vec<String>) {
        require_non_empty(
            &format!("artifacts[{index}].reference"),
            &self.reference,
            problems,
        );
        if self.reference.trim_start().starts_with("inline:") {
            problems.push(format!(
                "artifacts[{index}] inline artifact content is not allowed"
            ));
        }
        if looks_like_absolute_path(&self.reference) {
            problems.push(format!(
                "artifacts[{index}].reference must be redacted, relative, or a stable id"
            ));
        }
        if self
            .sha256
            .as_ref()
            .is_some_and(|sha| sha.trim().is_empty())
        {
            problems.push(format!("artifacts[{index}].sha256 must not be empty"));
        }
        collect_forbidden_metadata_keys(
            &self.metadata,
            &format!("artifacts[{index}].metadata"),
            problems,
        );
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Patch,
    Diff,
    TestLog,
    BuildLog,
    LintLog,
    Trace,
    Summary,
    Other,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct EvalIngestReceipt {
    pub run_id: String,
    pub inserted: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct EvalReportQuery {
    pub task_type: Option<ExecutionTaskType>,
    pub tag: Option<String>,
    pub min_runs: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strategy_id: Option<String>,
    #[serde(default)]
    pub exclude_cache_hits: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until_ms: Option<u64>,
    #[serde(default)]
    pub group_by_strategy: bool,
    #[serde(default)]
    pub group_by_harness_version: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct EvalReport {
    pub rows: Vec<EvalReportRow>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct EvalEvidenceGatePolicy {
    pub min_preview_runs_per_candidate: u64,
    pub min_preview_distinct_cases: u64,
    pub min_routing_runs_per_candidate: u64,
    pub min_routing_distinct_cases: u64,
    pub max_inconclusive_rate: f64,
    pub max_rolled_back_rate: f64,
    pub max_age_days: u64,
    pub require_task_type_match: bool,
    pub require_tag_overlap: bool,
    pub require_version_compatible: bool,
}

impl Default for EvalEvidenceGatePolicy {
    fn default() -> Self {
        Self {
            min_preview_runs_per_candidate: 5,
            min_preview_distinct_cases: 3,
            min_routing_runs_per_candidate: 20,
            min_routing_distinct_cases: 8,
            max_inconclusive_rate: 0.20,
            max_rolled_back_rate: 0.05,
            max_age_days: 60,
            require_task_type_match: true,
            require_tag_overlap: true,
            require_version_compatible: true,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct EvalReportRow {
    pub harness: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strategy_id: Option<String>,
    pub runs: u64,
    pub distinct_cases: u64,
    pub pass_count: u64,
    pub fail_count: u64,
    pub partial_count: u64,
    pub inconclusive_count: u64,
    pub not_evaluated_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub success_rate: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inconclusive_rate: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub median_latency_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub median_cost_micros: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_rate: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_hit_rate: Option<f64>,
    pub human_accepted_count: u64,
    pub human_edited_count: u64,
    pub human_retried_count: u64,
    pub human_abandoned_count: u64,
    pub human_rolled_back_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub human_acceptance_rate: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub human_edited_rate: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub human_retried_rate: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub human_abandoned_rate: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub human_rolled_back_rate: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_run_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_run_at_ms: Option<u64>,
    pub insufficient_sample: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvalEvidenceSnapshot {
    pub schema_version: String,
    pub snapshot_id: String,
    pub generated_at_ms: u64,
    pub rows: Vec<EvalEvidenceRow>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct EvalEvidenceRow {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_type: Option<ExecutionTaskType>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    pub harness: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strategy_id: Option<String>,
    pub runs: u64,
    pub distinct_cases: u64,
    pub pass_count: u64,
    pub fail_count: u64,
    pub partial_count: u64,
    pub inconclusive_count: u64,
    pub not_evaluated_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub success_rate: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inconclusive_rate: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub median_latency_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub median_cost_micros: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_rate: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_hit_rate: Option<f64>,
    pub human_accepted_count: u64,
    pub human_edited_count: u64,
    pub human_retried_count: u64,
    pub human_abandoned_count: u64,
    pub human_rolled_back_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub human_acceptance_rate: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub human_edited_rate: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub human_retried_rate: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub human_abandoned_rate: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub human_rolled_back_rate: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_run_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_run_at_ms: Option<u64>,
    pub preview_eligible: bool,
    pub routing_eligible: bool,
    #[serde(default)]
    pub ineligible_reasons: Vec<String>,
    pub insufficient_sample: bool,
}

impl EvalEvidenceSnapshot {
    pub fn from_report(query: &EvalReportQuery, report: EvalReport, generated_at_ms: u64) -> Self {
        Self::from_report_with_policy(
            query,
            report,
            generated_at_ms,
            EvalEvidenceGatePolicy::default(),
        )
    }

    pub fn from_report_with_policy(
        query: &EvalReportQuery,
        report: EvalReport,
        generated_at_ms: u64,
        policy: EvalEvidenceGatePolicy,
    ) -> Self {
        let rows = report
            .rows
            .into_iter()
            .map(|row| EvalEvidenceRow::from_report_row(query, row, generated_at_ms, policy))
            .collect::<Vec<_>>();
        let snapshot_id = evidence_snapshot_id(query, &rows);
        Self {
            schema_version: EVIDENCE_SNAPSHOT_SCHEMA_VERSION.to_string(),
            snapshot_id,
            generated_at_ms,
            rows,
        }
    }

    pub fn matching_rows(
        &self,
        task_type: Option<ExecutionTaskType>,
        candidate_harnesses: BTreeSet<String>,
    ) -> Vec<EvalEvidenceRow> {
        self.rows
            .iter()
            .filter(|row| candidate_harnesses.contains(&row.harness))
            .filter(|row| match (task_type, row.task_type) {
                (Some(wanted), Some(row_task_type)) => wanted == row_task_type,
                (Some(_), None) | (None, _) => true,
            })
            .cloned()
            .collect()
    }
}

impl EvalEvidenceRow {
    fn from_report_row(
        query: &EvalReportQuery,
        row: EvalReportRow,
        generated_at_ms: u64,
        policy: EvalEvidenceGatePolicy,
    ) -> Self {
        let mut evidence = Self {
            task_type: query.task_type,
            tag: query.tag.clone(),
            harness: row.harness,
            harness_version: row.harness_version,
            strategy_id: row.strategy_id,
            runs: row.runs,
            distinct_cases: row.distinct_cases,
            pass_count: row.pass_count,
            fail_count: row.fail_count,
            partial_count: row.partial_count,
            inconclusive_count: row.inconclusive_count,
            not_evaluated_count: row.not_evaluated_count,
            success_rate: row.success_rate,
            inconclusive_rate: row.inconclusive_rate,
            median_latency_ms: row.median_latency_ms,
            median_cost_micros: row.median_cost_micros,
            retry_rate: row.retry_rate,
            cache_hit_rate: row.cache_hit_rate,
            human_accepted_count: row.human_accepted_count,
            human_edited_count: row.human_edited_count,
            human_retried_count: row.human_retried_count,
            human_abandoned_count: row.human_abandoned_count,
            human_rolled_back_count: row.human_rolled_back_count,
            human_acceptance_rate: row.human_acceptance_rate,
            human_edited_rate: row.human_edited_rate,
            human_retried_rate: row.human_retried_rate,
            human_abandoned_rate: row.human_abandoned_rate,
            human_rolled_back_rate: row.human_rolled_back_rate,
            first_run_at_ms: row.first_run_at_ms,
            latest_run_at_ms: row.latest_run_at_ms,
            preview_eligible: false,
            routing_eligible: false,
            ineligible_reasons: Vec::new(),
            insufficient_sample: row.insufficient_sample,
        };
        evidence.apply_gate_policy(generated_at_ms, policy);
        evidence
    }

    fn apply_gate_policy(&mut self, generated_at_ms: u64, policy: EvalEvidenceGatePolicy) {
        if self.runs < policy.min_preview_runs_per_candidate {
            self.ineligible_reasons
                .push("preview_min_runs_not_met".to_string());
        }
        if self.distinct_cases < policy.min_preview_distinct_cases {
            self.ineligible_reasons
                .push("preview_min_distinct_cases_not_met".to_string());
        }
        self.preview_eligible = self.runs >= policy.min_preview_runs_per_candidate
            && self.distinct_cases >= policy.min_preview_distinct_cases;

        if self.runs < policy.min_routing_runs_per_candidate {
            self.ineligible_reasons
                .push("routing_min_runs_not_met".to_string());
        }
        if self.distinct_cases < policy.min_routing_distinct_cases {
            self.ineligible_reasons
                .push("routing_min_distinct_cases_not_met".to_string());
        }
        if policy.require_task_type_match && self.task_type.is_none() {
            self.ineligible_reasons
                .push("task_type_missing".to_string());
        }
        if policy.require_tag_overlap && self.tag.is_none() {
            self.ineligible_reasons.push("tag_missing".to_string());
        }
        if policy.require_version_compatible && self.harness_version.is_none() {
            self.ineligible_reasons
                .push("harness_version_missing".to_string());
        }
        if self
            .inconclusive_rate
            .is_some_and(|rate| rate > policy.max_inconclusive_rate)
        {
            self.ineligible_reasons
                .push("inconclusive_rate_exceeded".to_string());
        }
        if self
            .human_rolled_back_rate
            .is_some_and(|rate| rate > policy.max_rolled_back_rate)
        {
            self.ineligible_reasons
                .push("rolled_back_rate_exceeded".to_string());
        }
        match self.latest_run_at_ms {
            Some(latest)
                if generated_at_ms.saturating_sub(latest)
                    > policy.max_age_days * MILLIS_PER_DAY =>
            {
                self.ineligible_reasons.push("stale_evidence".to_string());
            }
            None => self
                .ineligible_reasons
                .push("latest_run_at_ms_missing".to_string()),
            _ => {}
        }
        self.routing_eligible = self.preview_eligible
            && self.runs >= policy.min_routing_runs_per_candidate
            && self.distinct_cases >= policy.min_routing_distinct_cases
            && !(policy.require_task_type_match && self.task_type.is_none())
            && !(policy.require_tag_overlap && self.tag.is_none())
            && !(policy.require_version_compatible && self.harness_version.is_none())
            && self
                .inconclusive_rate
                .map_or(true, |rate| rate <= policy.max_inconclusive_rate)
            && self
                .human_rolled_back_rate
                .map_or(true, |rate| rate <= policy.max_rolled_back_rate)
            && self.latest_run_at_ms.is_some_and(|latest| {
                generated_at_ms.saturating_sub(latest) <= policy.max_age_days * MILLIS_PER_DAY
            });
    }
}

pub trait CaseStore {
    fn put_case(&mut self, case: EvalCaseManifest) -> Result<()>;
}

pub trait EvalStore: CaseStore {
    fn ingest_run(&mut self, run: EvalRunIngest) -> Result<EvalIngestReceipt>;
    fn report(&self, query: EvalReportQuery) -> Result<EvalReport>;
}

#[derive(Debug, Clone)]
pub struct StoredEvalRun {
    pub run_id: String,
    pub run: EvalRunIngest,
}

#[derive(Debug, Default)]
pub struct InMemoryEvalStore {
    cases: BTreeMap<(String, String), EvalCaseManifest>,
    source_index: BTreeMap<(String, String), String>,
    runs: Vec<StoredEvalRun>,
}

impl InMemoryEvalStore {
    pub fn runs(&self) -> &[StoredEvalRun] {
        &self.runs
    }
}

impl CaseStore for InMemoryEvalStore {
    fn put_case(&mut self, case: EvalCaseManifest) -> Result<()> {
        case.validate()?;
        self.cases
            .insert((case.case_id.clone(), case.case_revision.clone()), case);
        Ok(())
    }
}

impl EvalStore for InMemoryEvalStore {
    fn ingest_run(&mut self, run: EvalRunIngest) -> Result<EvalIngestReceipt> {
        run.validate()?;
        if !self
            .cases
            .contains_key(&(run.case_id.clone(), run.case_revision.clone()))
        {
            return Err(EvalStoreError(format!(
                "unknown eval case `{}` revision `{}`",
                run.case_id, run.case_revision
            )));
        }
        if let Some(source_run_id) = run
            .source_run_id
            .as_ref()
            .filter(|id| !id.trim().is_empty())
        {
            let key = (run.harness.clone(), source_run_id.clone());
            if let Some(existing) = self.source_index.get(&key) {
                return Ok(EvalIngestReceipt {
                    run_id: existing.clone(),
                    inserted: false,
                });
            }
            let run_id = run.stable_run_id();
            self.source_index.insert(key, run_id.clone());
            self.runs.push(StoredEvalRun {
                run_id: run_id.clone(),
                run,
            });
            return Ok(EvalIngestReceipt {
                run_id,
                inserted: true,
            });
        }

        let run_id = run.stable_run_id();
        self.runs.push(StoredEvalRun {
            run_id: run_id.clone(),
            run,
        });
        Ok(EvalIngestReceipt {
            run_id,
            inserted: true,
        })
    }

    fn report(&self, query: EvalReportQuery) -> Result<EvalReport> {
        Ok(EvalReport {
            rows: build_report_rows(&self.cases, self.runs.iter(), &query),
        })
    }
}

pub fn build_report_rows<'a>(
    cases: &BTreeMap<(String, String), EvalCaseManifest>,
    runs: impl Iterator<Item = &'a StoredEvalRun>,
    query: &EvalReportQuery,
) -> Vec<EvalReportRow> {
    let mut groups: BTreeMap<ReportGroupKey, Vec<&EvalRunIngest>> = BTreeMap::new();
    for stored in runs {
        let Some(case) = cases.get(&(stored.run.case_id.clone(), stored.run.case_revision.clone()))
        else {
            continue;
        };
        if query
            .task_type
            .is_some_and(|task_type| case.task_type != task_type)
        {
            continue;
        }
        if query
            .tag
            .as_ref()
            .is_some_and(|tag| !case.has_tag(tag.as_str()))
        {
            continue;
        }
        if query
            .harness
            .as_ref()
            .is_some_and(|harness| stored.run.harness != *harness)
        {
            continue;
        }
        if query
            .harness_version
            .as_ref()
            .is_some_and(|version| stored.run.harness_version.as_deref() != Some(version.as_str()))
        {
            continue;
        }
        if query
            .strategy_id
            .as_ref()
            .is_some_and(|strategy| stored.run.strategy_id != *strategy)
        {
            continue;
        }
        if query.exclude_cache_hits && stored.run.cache_status == Some(CacheStatus::Hit) {
            continue;
        }
        let event_time = stored.run.started_at_ms.or(stored.run.finished_at_ms);
        if query.since_ms.is_some_and(|since| event_time < Some(since)) {
            continue;
        }
        if query.until_ms.is_some_and(|until| event_time > Some(until)) {
            continue;
        }
        groups
            .entry(ReportGroupKey::from_run(&stored.run, query))
            .or_default()
            .push(&stored.run);
    }

    groups
        .into_iter()
        .map(|(key, runs)| report_row(key, runs, query.min_runs))
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ReportGroupKey {
    harness: String,
    harness_version: Option<String>,
    strategy_id: Option<String>,
}

impl ReportGroupKey {
    fn from_run(run: &EvalRunIngest, query: &EvalReportQuery) -> Self {
        ReportGroupKey {
            harness: run.harness.clone(),
            harness_version: query
                .group_by_harness_version
                .then(|| run.harness_version.clone())
                .flatten(),
            strategy_id: query.group_by_strategy.then(|| run.strategy_id.clone()),
        }
    }
}

fn report_row(key: ReportGroupKey, runs: Vec<&EvalRunIngest>, min_runs: u64) -> EvalReportRow {
    let mut row = EvalReportRow {
        harness: key.harness,
        harness_version: key.harness_version,
        strategy_id: key.strategy_id,
        runs: runs.len() as u64,
        ..EvalReportRow::default()
    };
    let mut latencies = Vec::new();
    let mut costs = Vec::new();
    let mut retries = 0_u64;
    let mut retry_known = 0_u64;
    let mut cache_hits = 0_u64;
    let mut cache_known = 0_u64;
    let mut distinct_cases = BTreeSet::new();
    let mut first_run_at_ms: Option<u64> = None;
    let mut latest_run_at_ms: Option<u64> = None;

    for run in runs {
        distinct_cases.insert((run.case_id.clone(), run.case_revision.clone()));
        if let Some(event_time) = run.started_at_ms.or(run.finished_at_ms) {
            first_run_at_ms =
                Some(first_run_at_ms.map_or(event_time, |first| first.min(event_time)));
            latest_run_at_ms =
                Some(latest_run_at_ms.map_or(event_time, |latest| latest.max(event_time)));
        }
        match run.outcome.verdict {
            Verdict::Pass => row.pass_count += 1,
            Verdict::Fail => row.fail_count += 1,
            Verdict::Partial => row.partial_count += 1,
            Verdict::Inconclusive => row.inconclusive_count += 1,
            Verdict::NotEvaluated => row.not_evaluated_count += 1,
        }
        if let Some(latency) = run.latency_ms() {
            latencies.push(latency);
        }
        if let Some(cost) = run.cost_micros() {
            costs.push(cost);
        }
        if let Some(retry_count) = run.retry_count {
            retry_known += 1;
            if retry_count > 0 {
                retries += 1;
            }
        }
        if let Some(cache_status) = run.cache_status {
            cache_known += 1;
            if cache_status == CacheStatus::Hit {
                cache_hits += 1;
            }
        }
        for human_outcome in &run.human_outcomes {
            match human_outcome.kind {
                HumanOutcomeKind::Accepted => row.human_accepted_count += 1,
                HumanOutcomeKind::Edited => row.human_edited_count += 1,
                HumanOutcomeKind::Retried => row.human_retried_count += 1,
                HumanOutcomeKind::Abandoned => row.human_abandoned_count += 1,
                HumanOutcomeKind::RolledBack => row.human_rolled_back_count += 1,
            }
        }
    }

    row.success_rate = ratio(row.pass_count, row.runs);
    row.inconclusive_rate = ratio(row.inconclusive_count, row.runs);
    row.median_latency_ms = median_u64(&mut latencies);
    row.median_cost_micros = median_u64(&mut costs);
    row.retry_rate = ratio(retries, retry_known);
    row.cache_hit_rate = ratio(cache_hits, cache_known);
    row.distinct_cases = distinct_cases.len() as u64;
    row.human_acceptance_rate = ratio(row.human_accepted_count, row.runs);
    row.human_edited_rate = ratio(row.human_edited_count, row.runs);
    row.human_retried_rate = ratio(row.human_retried_count, row.runs);
    row.human_abandoned_rate = ratio(row.human_abandoned_count, row.runs);
    row.human_rolled_back_rate = ratio(row.human_rolled_back_count, row.runs);
    row.first_run_at_ms = first_run_at_ms;
    row.latest_run_at_ms = latest_run_at_ms;
    row.insufficient_sample = row.runs < min_runs;
    row
}

fn evidence_snapshot_id(query: &EvalReportQuery, rows: &[EvalEvidenceRow]) -> String {
    let mut hasher = Sha256::new();
    if let Ok(query_json) = serde_json::to_vec(query) {
        hasher.update(query_json);
    }
    hasher.update(b"\0");
    if let Ok(rows_json) = serde_json::to_vec(rows) {
        hasher.update(rows_json);
    }
    format!("eval_evidence_{:x}", hasher.finalize())
}

fn require_non_empty(field: &str, value: &str, problems: &mut Vec<String>) {
    if value.trim().is_empty() {
        problems.push(format!("{field} must not be empty"));
    }
}

fn finish_validation(problems: Vec<String>) -> Result<()> {
    if problems.is_empty() {
        Ok(())
    } else {
        Err(EvalStoreError(problems.join("; ")))
    }
}

fn first_string(value: &serde_json::Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(json_string))
        .filter(|value| !value.trim().is_empty())
}

fn json_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn string_at(value: &serde_json::Value, path: &[&str]) -> Option<String> {
    let mut current = value;
    for key in path {
        current = current.get(*key)?;
    }
    json_string(current).filter(|value| !value.trim().is_empty())
}

fn first_number(value: &serde_json::Value, keys: &[&str]) -> Option<f64> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(json_number))
        .filter(|value| value.is_finite())
}

fn json_number(value: &serde_json::Value) -> Option<f64> {
    match value {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.parse::<f64>().ok(),
        _ => None,
    }
}

fn first_u64(value: &serde_json::Value, keys: &[&str]) -> Option<u64> {
    first_number(value, keys).and_then(|value| {
        if value.is_finite() && value >= 0.0 {
            Some(value.round() as u64)
        } else {
            None
        }
    })
}

fn push_metric_from_first_number(
    metrics: &mut Vec<EvalMetric>,
    name: &str,
    unit: &str,
    value: &serde_json::Value,
    keys: &[&str],
) {
    if let Some(metric_value) = first_number(value, keys) {
        metrics.push(EvalMetric {
            name: name.to_string(),
            value: metric_value,
            unit: unit.to_string(),
            source: "harness".to_string(),
        });
    }
}

fn parse_run_status_from_input(kind: HarnessKind, value: &serde_json::Value) -> Option<RunStatus> {
    if kind == HarnessKind::Aider {
        if let Some(exit_status) = first_number(value, &["exit_status", "exit_code"]) {
            return Some(if exit_status == 0.0 {
                RunStatus::Succeeded
            } else {
                RunStatus::Failed
            });
        }
    }
    first_string(value, &["status", "run_status"]).and_then(|status| match status.as_str() {
        "success" | "succeeded" | "completed" | "complete" => Some(RunStatus::Succeeded),
        "failed" | "failure" | "error" => Some(RunStatus::Failed),
        "cancelled" | "canceled" => Some(RunStatus::Cancelled),
        "timed_out" | "timeout" => Some(RunStatus::TimedOut),
        "inconclusive" => Some(RunStatus::Inconclusive),
        _ => None,
    })
}

fn parse_verdict_from_input(value: &serde_json::Value) -> Option<Verdict> {
    first_string(value, &["verdict"])
        .or_else(|| string_at(value, &["outcome", "verdict"]))
        .and_then(|verdict| match verdict.as_str() {
            "pass" | "success" | "succeeded" => Some(Verdict::Pass),
            "fail" | "failure" | "failed" => Some(Verdict::Fail),
            "partial" => Some(Verdict::Partial),
            "inconclusive" => Some(Verdict::Inconclusive),
            "not_evaluated" | "not-evaluated" | "unknown" => Some(Verdict::NotEvaluated),
            _ => None,
        })
}

fn parse_cache_status(value: &str) -> Option<CacheStatus> {
    match value {
        "hit" => Some(CacheStatus::Hit),
        "miss" => Some(CacheStatus::Miss),
        "bypass" | "bypassed" => Some(CacheStatus::Bypass),
        _ => None,
    }
}

fn parse_artifacts(value: &serde_json::Value) -> Result<Vec<EvalArtifactRef>> {
    let mut artifacts = Vec::new();
    if let Some(items) = value.get("artifacts").and_then(|value| value.as_array()) {
        for (index, item) in items.iter().enumerate() {
            let artifact: EvalArtifactRef =
                serde_json::from_value(item.clone()).map_err(|err| {
                    EvalStoreError(format!(
                        "artifacts[{index}] is not a valid artifact ref: {err}"
                    ))
                })?;
            let mut problems = Vec::new();
            artifact.validate(index, &mut problems);
            finish_validation(problems)?;
            artifacts.push(artifact);
        }
    }
    if let Some(sha256) = first_string(value, &["patch_sha256"]) {
        artifacts.push(EvalArtifactRef {
            kind: ArtifactKind::Patch,
            reference: format!("patch:{sha256}"),
            sha256: Some(sha256),
            privacy_level: PrivacyClass::Standard,
            metadata: serde_json::json!({}),
        });
    }
    Ok(artifacts)
}

fn parse_mechanical_checks(value: &serde_json::Value) -> Result<Vec<CheckResult>> {
    let Some(items) = value
        .get("mechanical_checks")
        .and_then(|value| value.as_array())
    else {
        return Ok(Vec::new());
    };
    let summaries = items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            serde_json::from_value::<MechanicalCheckSummary>(item.clone()).map_err(|err| {
                EvalStoreError(format!(
                    "mechanical_checks[{index}] is not a valid check summary: {err}"
                ))
            })
        })
        .collect::<Result<Vec<_>>>()?;
    normalize_mechanical_checks(&summaries)
}

fn metric_as_u64(metrics: &[EvalMetric], name: &str) -> Option<u64> {
    metrics
        .iter()
        .find(|metric| metric.name == name && metric.value.is_finite() && metric.value >= 0.0)
        .map(|metric| metric.value.round() as u64)
}

fn ratio(numerator: u64, denominator: u64) -> Option<f64> {
    if denominator == 0 {
        None
    } else {
        Some(numerator as f64 / denominator as f64)
    }
}

fn median_u64(values: &mut [u64]) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    let mid = values.len() / 2;
    if values.len() % 2 == 1 {
        Some(values[mid])
    } else {
        Some((values[mid - 1] + values[mid]) / 2)
    }
}

fn looks_like_absolute_path(value: &str) -> bool {
    value.starts_with('/') || value.starts_with("~/")
}

fn collect_forbidden_metadata_keys(
    value: &serde_json::Value,
    path: &str,
    problems: &mut Vec<String>,
) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map {
                if is_forbidden_raw_key(key) {
                    problems.push(format!("{path}.{key} is not allowed in eval metadata"));
                }
                collect_forbidden_metadata_keys(child, &format!("{path}.{key}"), problems);
            }
        }
        serde_json::Value::Array(items) => {
            for (i, child) in items.iter().enumerate() {
                collect_forbidden_metadata_keys(child, &format!("{path}[{i}]"), problems);
            }
        }
        _ => {}
    }
}

fn is_forbidden_raw_key(key: &str) -> bool {
    let normalized = key.to_ascii_lowercase();
    FORBIDDEN_METADATA_KEYS.contains(&normalized.as_str())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CacheLookupSummary {
    pub status: CacheStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer: Option<String>,
}

impl From<&CacheLookupReceipt> for CacheLookupSummary {
    fn from(receipt: &CacheLookupReceipt) -> Self {
        CacheLookupSummary {
            status: receipt.status,
            layer: Some(format!("{:?}", receipt.layer)),
        }
    }
}
