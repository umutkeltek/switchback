//! `sb-trace` — end-to-end request tracing.
//!
//! Each request that enters the execution runtime produces exactly ONE
//! [`TraceRecord`] tying together the explainable
//! [`RouteDecision`](sb_core::RouteDecision), every
//! `(target, account, egress)` attempt with its outcome/latency/error-class,
//! the final status, and the attributed usage + cost. It complements the usage
//! ledger ("see every cost") and the route header ("see every decision") with a
//! single "see every request, end to end" surface.
//!
//! INVARIANT (mirrors AGENTS.md #3 — no secrets in logs): a trace is
//! **metadata only**. No credentials, no prompt/response bodies, no message
//! content. `account_id` and `egress` are identifiers, not secrets. The types
//! here are `Serialize`-only and carry nothing that could leak a key.

use std::collections::VecDeque;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use sb_core::{EvaluationEvent, RouteDecision, Usage};
use serde::Serialize;
use sha2::{Digest, Sha256};

pub const NATIVE_EXECUTION_LANE_ID_META: &str = "switchback_lane_id";
pub const NATIVE_EXECUTION_LANE_REVISION_META: &str = "switchback_lane_revision";
pub const NATIVE_EXECUTION_REQUESTED_EFFORT_META: &str = "requested_native_effort";
pub const NATIVE_EXECUTION_OBSERVED_EFFORT_META: &str = "observed_native_effort";
pub const NATIVE_EXECUTION_OBSERVED_PATH_META: &str = "observed_native_effort_path";

/// Seconds since the Unix epoch. Kept here so sb-trace stays free of a time crate.
pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The result of a single execution attempt against one account.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum AttemptOutcome {
    /// The upstream returned successfully.
    Success,
    /// The attempt failed; `class` is the error class, `fell_over` is whether
    /// the request then tried another account/target.
    Failed { class: String, fell_over: bool },
}

/// One execution attempt: which target/account/egress was used and how it went.
#[derive(Debug, Clone, Serialize)]
pub struct Attempt {
    pub target_id: String,
    pub provider_id: String,
    pub model: String,
    pub account_id: String,
    /// Outbound network path used (`"direct"` until the egress layer is wired).
    pub egress: String,
    pub latency_ms: u64,
    #[serde(flatten)]
    pub outcome: AttemptOutcome,
}

/// Request-scoped observation of the native harness effort that reached the
/// Switchback edge. The lane declaration comes from internal launcher headers;
/// the observed effort comes from the provider-facing request body. Keeping
/// both makes lowering or drift visible instead of treating configuration as
/// execution proof.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct NativeExecutionObservation {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lane_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lane_revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_effort_path: Option<String>,
}

impl NativeExecutionObservation {
    pub fn is_empty(&self) -> bool {
        self.lane_id.is_none()
            && self.lane_revision.is_none()
            && self.requested_effort.is_none()
            && self.observed_effort.is_none()
            && self.observed_effort_path.is_none()
    }
}

impl Attempt {
    pub fn success(
        target_id: impl Into<String>,
        provider_id: impl Into<String>,
        model: impl Into<String>,
        account_id: impl Into<String>,
        egress: impl Into<String>,
        latency_ms: u64,
    ) -> Self {
        Attempt {
            target_id: target_id.into(),
            provider_id: provider_id.into(),
            model: model.into(),
            account_id: account_id.into(),
            egress: egress.into(),
            latency_ms,
            outcome: AttemptOutcome::Success,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn failed(
        target_id: impl Into<String>,
        provider_id: impl Into<String>,
        model: impl Into<String>,
        account_id: impl Into<String>,
        egress: impl Into<String>,
        latency_ms: u64,
        class: impl Into<String>,
        fell_over: bool,
    ) -> Self {
        Attempt {
            target_id: target_id.into(),
            provider_id: provider_id.into(),
            model: model.into(),
            account_id: account_id.into(),
            egress: egress.into(),
            latency_ms,
            outcome: AttemptOutcome::Failed {
                class: class.into(),
                fell_over,
            },
        }
    }
}

/// One request's full lifecycle. Metadata only — see the crate-level invariant.
#[derive(Debug, Clone, Serialize)]
pub struct TraceRecord {
    pub request_id: String,
    /// The runtime/config snapshot revision pinned for this request.
    pub revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Native client profile that entered the gateway (for example `codex` or
    /// `claude-code`). Metadata only; set by the HTTP edge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_profile: Option<String>,
    /// Inbound wire protocol used by that client (`openai_responses`,
    /// `anthropic_messages`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_protocol: Option<String>,
    /// Requested-versus-observed native effort bound to the immutable lane
    /// revision. This is execution evidence, not a routing or quality claim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_execution: Option<NativeExecutionObservation>,
    pub timestamp_unix: u64,
    /// The model the client asked for (pre-routing).
    pub inbound_model: String,
    /// The route name that matched (`default`, `coding`, `direct`, `default:<p>`).
    pub route: String,
    /// The full explainable routing decision (selected, fallbacks, rejected).
    pub decision: RouteDecision,
    pub attempts: Vec<Attempt>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<EvaluationEvent>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    pub final_status: u16,
    pub total_latency_ms: u64,
    pub streamed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    pub cost_micros: u64,
}

/// Accumulates a [`TraceRecord`] across a request's execution. The server holds
/// one of these per in-flight request, records each attempt, then finishes it.
#[derive(Debug, Clone)]
pub struct RequestTrace {
    request_id: String,
    revision: u64,
    tenant: Option<String>,
    project: Option<String>,
    session_id: Option<String>,
    client_profile: Option<String>,
    client_protocol: Option<String>,
    native_execution: Option<NativeExecutionObservation>,
    inbound_model: String,
    route: String,
    decision: RouteDecision,
    attempts: Vec<Attempt>,
    events: Vec<EvaluationEvent>,
    warnings: Vec<String>,
    usage: Option<Usage>,
    cost_micros: u64,
}

impl RequestTrace {
    pub fn start(
        request_id: impl Into<String>,
        revision: u64,
        inbound_model: impl Into<String>,
        route: impl Into<String>,
        decision: RouteDecision,
    ) -> Self {
        RequestTrace {
            request_id: request_id.into(),
            revision,
            tenant: None,
            project: None,
            session_id: None,
            client_profile: None,
            client_protocol: None,
            native_execution: None,
            inbound_model: inbound_model.into(),
            route: route.into(),
            decision,
            attempts: Vec::new(),
            events: Vec::new(),
            warnings: Vec::new(),
            usage: None,
            cost_micros: 0,
        }
    }

    pub fn with_principal(mut self, tenant: Option<String>, project: Option<String>) -> Self {
        self.tenant = tenant;
        self.project = project;
        self
    }

    pub fn with_session_id(mut self, session_id: Option<String>) -> Self {
        self.session_id = session_id.filter(|id| !id.is_empty());
        self
    }

    pub fn with_client_metadata(
        mut self,
        client_profile: Option<String>,
        client_protocol: Option<String>,
    ) -> Self {
        self.client_profile = client_profile.filter(|id| !id.is_empty());
        self.client_protocol = client_protocol.filter(|id| !id.is_empty());
        self
    }

    pub fn with_native_execution(
        mut self,
        observation: Option<NativeExecutionObservation>,
    ) -> Self {
        self.native_execution = observation.filter(|value| !value.is_empty());
        self
    }

    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    /// Record one execution attempt (success or failure).
    pub fn attempt(&mut self, attempt: Attempt) {
        self.attempts.push(attempt);
    }

    /// Record one metadata-only execution evaluation event.
    pub fn event(&mut self, event: EvaluationEvent) {
        self.events.push(event);
    }

    /// Attach a metadata-only request warning. Duplicate warnings are ignored so
    /// retries/fallbacks do not spam the trace.
    pub fn warning(&mut self, warning: impl Into<String>) {
        let warning = warning.into();
        if !self.warnings.iter().any(|existing| existing == &warning) {
            self.warnings.push(warning);
        }
    }

    /// Attach the attributed usage + cost (server computes cost from the catalog).
    pub fn set_usage(&mut self, usage: Usage, cost_micros: u64) {
        self.usage = Some(usage);
        self.cost_micros = cost_micros;
    }

    /// Finalize into an immutable record.
    pub fn finish(
        mut self,
        final_status: u16,
        total_latency_ms: u64,
        streamed: bool,
    ) -> TraceRecord {
        let mut final_event = EvaluationEvent::new(sb_core::EvaluationEventKind::FinalStatus);
        final_event.status = Some(final_status.to_string());
        final_event.latency_ms = Some(total_latency_ms);
        self.events.push(final_event);

        TraceRecord {
            request_id: self.request_id,
            revision: self.revision,
            tenant: self.tenant,
            project: self.project,
            session_id: self.session_id,
            client_profile: self.client_profile,
            client_protocol: self.client_protocol,
            native_execution: self.native_execution,
            timestamp_unix: now_unix(),
            inbound_model: self.inbound_model,
            route: self.route,
            decision: self.decision,
            attempts: self.attempts,
            events: self.events,
            warnings: self.warnings,
            final_status,
            total_latency_ms,
            streamed,
            usage: self.usage,
            cost_micros: self.cost_micros,
        }
    }
}

/// Bounded in-memory ring of recent traces, with an optional JSONL audit sink.
/// Unlike the usage ledger (unbounded — it is accounting), traces are
/// high-volume and observational, so the in-memory view is a fixed-size ring.
pub struct TraceLog {
    ring: Mutex<VecDeque<TraceRecord>>,
    cap: usize,
    sink: Option<PathBuf>,
    /// Fraction of requests to record (0.0–1.0). Decided per request by a stable
    /// hash of the request id, so a request is either fully traced or dropped.
    sample_rate: f64,
}

impl TraceLog {
    pub fn in_memory(cap: usize) -> Self {
        Self::new(cap, None, 1.0)
    }

    /// Also append each record as a JSONL line to `path` (an audit trail).
    pub fn with_sink(cap: usize, path: impl Into<PathBuf>) -> Self {
        Self::new(cap, Some(path.into()), 1.0)
    }

    /// Full control: ring capacity, optional JSONL sink, and sample rate.
    pub fn new(cap: usize, sink: Option<PathBuf>, sample_rate: f64) -> Self {
        Self {
            ring: Mutex::new(VecDeque::with_capacity(cap.min(1024))),
            cap: cap.max(1),
            sink,
            sample_rate: sample_rate.clamp(0.0, 1.0),
        }
    }

    /// Whether to record this request's trace, by a stable hash of its id (so
    /// the decision is deterministic and the same across a request's finalize).
    fn sampled(&self, request_id: &str) -> bool {
        if self.sample_rate >= 1.0 {
            return true;
        }
        if self.sample_rate <= 0.0 {
            return false;
        }
        let digest = Sha256::digest(request_id.as_bytes());
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&digest[..8]);
        // Top 53 bits → a uniform fraction in [0, 1).
        let frac = (u64::from_be_bytes(bytes) >> 11) as f64 / (1u64 << 53) as f64;
        frac < self.sample_rate
    }

    /// Append a record (if sampled in). Best-effort JSONL write (an IO error
    /// never breaks a request); the in-memory ring append evicts oldest at cap.
    pub fn record(&self, record: TraceRecord) {
        if !self.sampled(&record.request_id) {
            return;
        }
        if let Some(path) = &self.sink {
            if let Ok(line) = serde_json::to_string(&record) {
                if let Ok(mut file) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                {
                    let _ = writeln!(file, "{line}");
                }
            }
        }
        if let Ok(mut ring) = self.ring.lock() {
            if ring.len() == self.cap {
                ring.pop_front();
            }
            ring.push_back(record);
        }
    }

    /// Most recent `limit` traces, newest first.
    pub fn recent(&self, limit: usize) -> Vec<TraceRecord> {
        self.ring
            .lock()
            .map(|ring| ring.iter().rev().take(limit).cloned().collect())
            .unwrap_or_default()
    }

    /// Look up a single trace by request id (newest match).
    pub fn get(&self, request_id: &str) -> Option<TraceRecord> {
        self.ring
            .lock()
            .ok()?
            .iter()
            .rev()
            .find(|r| r.request_id == request_id)
            .cloned()
    }

    pub fn len(&self) -> usize {
        self.ring.lock().map(|r| r.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for TraceLog {
    fn default() -> Self {
        Self::in_memory(256)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sb_core::RouteDecision;

    fn decision() -> RouteDecision {
        RouteDecision::new("req-1", "fill_first")
    }

    #[test]
    fn record_then_recent_is_newest_first() {
        let log = TraceLog::in_memory(8);
        for i in 0..3 {
            let t = RequestTrace::start(format!("req-{i}"), 1, "m", "default", decision());
            log.record(t.finish(200, 10, false));
        }
        let recent = log.recent(10);
        assert_eq!(recent.len(), 3);
        assert_eq!(recent[0].request_id, "req-2", "newest first");
    }

    #[test]
    fn ring_evicts_oldest_at_cap() {
        let log = TraceLog::in_memory(2);
        for i in 0..5 {
            let t = RequestTrace::start(format!("req-{i}"), 1, "m", "default", decision());
            log.record(t.finish(200, 1, false));
        }
        assert_eq!(log.len(), 2, "ring is bounded");
        let ids: Vec<_> = log.recent(10).into_iter().map(|r| r.request_id).collect();
        assert_eq!(ids, vec!["req-4", "req-3"], "only the last two survive");
    }

    #[test]
    fn attempts_and_fallover_are_recorded() {
        let mut t = RequestTrace::start("req-x", 1, "coding", "coding", decision());
        t.attempt(Attempt::failed(
            "anthropic/c",
            "anthropic",
            "c",
            "acct-1",
            "direct",
            5,
            "rate_limited",
            true,
        ));
        t.attempt(Attempt::success(
            "openrouter/gpt",
            "openrouter",
            "gpt",
            "acct-2",
            "direct",
            8,
        ));
        let rec = t.finish(200, 20, false);
        assert_eq!(rec.attempts.len(), 2, "fallover records both attempts");
        assert!(matches!(
            rec.attempts[0].outcome,
            AttemptOutcome::Failed {
                fell_over: true,
                ..
            }
        ));
        assert!(matches!(rec.attempts[1].outcome, AttemptOutcome::Success));
    }

    #[test]
    fn evaluation_events_are_recorded() {
        let mut t = RequestTrace::start("req-events", 1, "m", "default", decision());
        t.event(EvaluationEvent::new(
            sb_core::EvaluationEventKind::RunStarted,
        ));
        let rec = t.finish(200, 1, false);
        assert_eq!(rec.events.len(), 2);
        assert_eq!(rec.events[0].kind, sb_core::EvaluationEventKind::RunStarted);
        assert_eq!(
            rec.events[1].kind,
            sb_core::EvaluationEventKind::FinalStatus
        );
    }

    #[test]
    fn record_carries_pinned_revision() {
        let t = RequestTrace::start("req-rev", 42, "m", "default", decision());
        let rec = t.finish(200, 1, false);
        assert_eq!(rec.revision, 42);
    }

    #[test]
    fn record_carries_requested_and_observed_native_effort() {
        let observation = NativeExecutionObservation {
            lane_id: Some("gpt56-sol-ultra".to_string()),
            lane_revision: Some(format!("sha256:{}", "a".repeat(64))),
            requested_effort: Some("ultra".to_string()),
            observed_effort: Some("ultra".to_string()),
            observed_effort_path: Some("/reasoning/effort".to_string()),
        };
        let record = RequestTrace::start("req-effort", 7, "gpt-5.6-sol", "coding", decision())
            .with_native_execution(Some(observation.clone()))
            .finish(200, 5, false);
        assert_eq!(record.native_execution, Some(observation));
    }

    #[test]
    fn record_serializes_without_secret_fields() {
        // The record type only carries metadata; assert the JSON shape has no
        // obvious credential-bearing keys and includes the attempt identifiers.
        let mut t = RequestTrace::start("req-redaction-check", 1, "m", "default", decision());
        t.attempt(Attempt::success("p/m", "p", "m", "acct", "direct", 3));
        let json = serde_json::to_string(&t.finish(200, 4, false)).unwrap();
        for banned in [
            "token",
            "secret",
            "api_key",
            "authorization",
            "bearer",
            "password",
        ] {
            assert!(
                !json.to_lowercase().contains(banned),
                "trace leaked `{banned}`"
            );
        }
        assert!(json.contains("acct") && json.contains("\"egress\":\"direct\""));
    }

    #[test]
    fn get_by_request_id() {
        let log = TraceLog::in_memory(8);
        let t = RequestTrace::start("find-me", 1, "m", "default", decision());
        log.record(t.finish(200, 1, false));
        assert!(log.get("find-me").is_some());
        assert!(log.get("nope").is_none());
    }

    #[test]
    fn sample_rate_zero_records_nothing_one_records_all() {
        let none = TraceLog::new(64, None, 0.0);
        let all = TraceLog::new(64, None, 1.0);
        for i in 0..20 {
            let id = format!("req-{i}");
            none.record(
                RequestTrace::start(id.clone(), 1, "m", "default", decision())
                    .finish(200, 1, false),
            );
            all.record(
                RequestTrace::start(id, 1, "m", "default", decision()).finish(200, 1, false),
            );
        }
        assert_eq!(none.len(), 0, "sample 0.0 drops every trace");
        assert_eq!(all.len(), 20, "sample 1.0 keeps every trace");
    }

    #[test]
    fn sampling_is_stable_per_request_id() {
        // Half-rate: the same id always lands the same side of the cut.
        let log = TraceLog::new(1024, None, 0.5);
        let decide = |id: &str| log.sampled(id);
        for i in 0..50 {
            let id = format!("req-{i}");
            assert_eq!(decide(&id), decide(&id), "decision is deterministic");
        }
        // And it actually samples a subset (not all, not none) over many ids.
        let kept = (0..1000).filter(|i| log.sampled(&format!("r{i}"))).count();
        assert!((300..700).contains(&kept), "≈half sampled, got {kept}/1000");
    }
}
