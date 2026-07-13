//! Bounded live-traffic response-quality evaluation.
//!
//! Raw sampled material is accepted only into zeroizing, byte-capped buffers.
//! The durable store receives metadata-only reservations/finalizations; the
//! judge itself runs through the ordinary engine behind an explicit candidate
//! allowlist. Serving-site wiring intentionally lands in the next commit.

mod capture;
mod rubric;
mod worker;

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use sb_core::{AiRequest, ExecutionTaskType, PrivacyClass, QualityEvalConfig};
use tokio::sync::{mpsc, Semaphore};

pub(crate) use capture::{tee_stream, QualityCapture};
pub(crate) use rubric::{evaluator_id, RUBRIC_VERSION};

pub(crate) const QUALITY_EVAL_ORIGIN_KEY: &str = "internal_origin";
pub(crate) const QUALITY_EVAL_ORIGIN_VALUE: &str = "quality_eval";
pub(crate) const QUALITY_CLASS: &str = "any";
const ROLLING_WINDOW_MS: i64 = 24 * 60 * 60 * 1000;

pub(crate) struct QualityEval {
    sender: mpsc::Sender<QualityJob>,
    receiver: Mutex<Option<mpsc::Receiver<QualityJob>>>,
    permits: Arc<Semaphore>,
    store: OnceLock<Arc<dyn sb_store::StateStore>>,
    backoff: Mutex<Backoff>,
    stats: QualityStats,
}

#[derive(Default)]
struct QualityStats {
    queue_depth: AtomicUsize,
    attempted: AtomicU64,
    scored: AtomicU64,
    ungradable: AtomicU64,
    failed: AtomicU64,
    dropped: AtomicU64,
    budget_skipped: AtomicU64,
}

pub(super) struct QualityJob {
    served_request_id: String,
    served_target_id: String,
    class: String,
    sample_revision: u64,
    evaluator_id: String,
    input: capture::CaptureBuffer,
    output: capture::CaptureBuffer,
}

#[derive(Default)]
struct Backoff {
    consecutive_failures: u32,
    paused_until: Option<Instant>,
    probe_in_flight: bool,
}

impl Backoff {
    fn open(&self, now: Instant) -> bool {
        self.paused_until.is_some_and(|until| now < until)
    }

    fn try_enter(&mut self, now: Instant) -> bool {
        let Some(until) = self.paused_until else {
            return true;
        };
        if now < until || self.probe_in_flight {
            return false;
        }
        self.probe_in_flight = true;
        true
    }

    fn success(&mut self) {
        self.consecutive_failures = 0;
        self.paused_until = None;
        self.probe_in_flight = false;
    }

    fn failure(&mut self, now: Instant, cfg: &QualityEvalConfig) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        if self.probe_in_flight || self.consecutive_failures >= cfg.failure_backoff_after {
            self.paused_until = now.checked_add(Duration::from_secs(cfg.failure_backoff_secs));
            self.probe_in_flight = false;
        }
    }

    fn pause(&mut self, now: Instant, cfg: &QualityEvalConfig) {
        self.consecutive_failures = cfg.failure_backoff_after;
        self.paused_until = now.checked_add(Duration::from_secs(cfg.failure_backoff_secs));
        self.probe_in_flight = false;
    }
}

impl QualityEval {
    pub(crate) fn new(config: &QualityEvalConfig) -> Self {
        let (sender, receiver) = mpsc::channel(config.queue_capacity.max(1));
        Self {
            sender,
            receiver: Mutex::new(Some(receiver)),
            permits: Arc::new(Semaphore::new(config.capture_slots.max(1))),
            store: OnceLock::new(),
            backoff: Mutex::new(Backoff::default()),
            stats: QualityStats::default(),
        }
    }

    pub(crate) fn attach_store(&self, store: Arc<dyn sb_store::StateStore>) {
        let _ = self.store.set(store);
    }

    pub(crate) fn begin(
        self: &Arc<Self>,
        req: &AiRequest,
        snap: &crate::Snapshot,
    ) -> Option<QualityCapture> {
        let cfg = &snap.config.server.quality_eval;
        if !cfg.enabled {
            return None;
        }
        if !eligible_request(req)
            || self
                .backoff
                .lock()
                .map(|backoff| backoff.open(Instant::now()))
                .unwrap_or(true)
        {
            return None;
        }
        let store = self.store.get()?;
        let since = sb_store::now_millis().saturating_sub(ROLLING_WINDOW_MS);
        let budget = match store.quality_judgment_budget(since) {
            Ok(budget) => budget,
            Err(_) => {
                if let Ok(mut backoff) = self.backoff.lock() {
                    backoff.pause(Instant::now(), cfg);
                }
                self.stats.failed.fetch_add(1, Ordering::Relaxed);
                return None;
            }
        };
        if budget.attempted >= u64::from(cfg.max_judgments_per_24h)
            || budget.cost_micros >= cfg.max_cost_micros_per_24h
        {
            self.stats.budget_skipped.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let permit = self.permits.clone().try_acquire_owned().ok()?;
        let input = capture::render_request(req, cfg.max_input_bytes)?;
        if input.chars() < cfg.min_input_chars {
            return None;
        }
        Some(QualityCapture::new(
            Arc::clone(self),
            permit,
            req.id.clone(),
            snap.revision,
            rubric::evaluator_id(&cfg.body_allowed_targets),
            input,
            cfg,
        ))
    }

    fn try_enqueue(&self, job: QualityJob) -> bool {
        match self.sender.try_send(job) {
            Ok(()) => {
                self.stats.queue_depth.fetch_add(1, Ordering::Relaxed);
                true
            }
            Err(_) => {
                self.stats.dropped.fetch_add(1, Ordering::Relaxed);
                false
            }
        }
    }

    pub(crate) fn take_receiver(&self) -> Option<mpsc::Receiver<QualityJob>> {
        self.receiver.lock().ok()?.take()
    }

    pub(crate) async fn run_worker(
        self: Arc<Self>,
        engine: Arc<crate::Engine>,
        mut receiver: mpsc::Receiver<QualityJob>,
    ) {
        while let Some(job) = receiver.recv().await {
            self.stats.queue_depth.fetch_sub(1, Ordering::Relaxed);
            worker::process_job(&self, &engine, job).await;
        }
    }
}

impl crate::Engine {
    /// Start the sole serial judge worker. Disabled configurations do not take
    /// the receiver and spawn no task, preserving the feature-off runtime.
    pub fn spawn_quality_eval_worker(self: Arc<Self>) -> Option<tokio::task::JoinHandle<()>> {
        if !self.snapshot().config.server.quality_eval.enabled {
            return None;
        }
        let eval = Arc::clone(&self.quality_eval);
        let receiver = eval.take_receiver()?;
        Some(tokio::spawn(eval.run_worker(self, receiver)))
    }

    /// Startup WAL recovery: abandon crash-orphaned reservations, then rebuild
    /// the current evaluator's live quality ring from fresh scored rows. Store
    /// failures pause evaluation but never fail serving startup.
    pub(crate) fn recover_quality_eval_from_store(&self) {
        let snap = self.snapshot();
        let cfg = &snap.config.server.quality_eval;
        if !cfg.enabled {
            return;
        }
        let Some(store) = self.store() else {
            return;
        };
        let now = Instant::now();
        let now_ms = sb_store::now_millis();
        let evaluator_id = evaluator_id(&cfg.body_allowed_targets);
        let ttl_ms = (snap.config.server.scorecard.window.ttl_secs as i64).saturating_mul(1000);
        let abandoned = store.abandon_started_quality_judgments(now_ms);
        let replay = store.replay_quality_judgments(&evaluator_id, now_ms.saturating_sub(ttl_ms));
        match (abandoned, replay) {
            (Ok(_), Ok(rows)) => self.scorecard.replay_quality(
                &rows,
                &evaluator_id,
                &snap.config.server.scorecard,
                cfg,
                now,
                now_ms,
            ),
            (abandoned, replay) => {
                tracing::warn!(
                    abandon_ok = abandoned.is_ok(),
                    replay_ok = replay.is_ok(),
                    "quality evaluation startup recovery paused after state-store failure"
                );
                if let Ok(mut backoff) = self.quality_eval.backoff.lock() {
                    backoff.pause(now, cfg);
                }
            }
        }
    }

    /// Enabled-only operator projection for `/v1/usage`. All fields are
    /// metadata aggregates; sampled material has no representation here.
    pub fn quality_eval_projection(&self) -> Option<serde_json::Value> {
        let snap = self.snapshot();
        let cfg = &snap.config.server.quality_eval;
        if !cfg.enabled {
            return None;
        }
        let now = Instant::now();
        let now_ms = sb_store::now_millis();
        let since_ms = now_ms.saturating_sub(ROLLING_WINDOW_MS);
        let evaluator_id = evaluator_id(&cfg.body_allowed_targets);
        let budget = self
            .store()
            .and_then(|store| store.quality_judgment_budget(since_ms).ok())
            .unwrap_or_default();
        let rows = self
            .store()
            .and_then(|store| store.recent_quality_judgments(2_000).ok())
            .unwrap_or_default();
        let mut scored = 0u64;
        let mut ungradable = 0u64;
        let mut failed = 0u64;
        for row in rows.iter().filter(|row| row.created_at_ms >= since_ms) {
            match row.status.as_str() {
                "scored" => scored += 1,
                "ungradable" => ungradable += 1,
                "invalid" | "failed" | "timeout" | "abandoned" => failed += 1,
                _ => {}
            }
        }
        let per_target = self
            .scorecard
            .project_all_quality(
                QUALITY_CLASS,
                &evaluator_id,
                &snap.config.server.scorecard,
                cfg,
                now,
            )
            .into_iter()
            .map(|(target_id, signal)| {
                (
                    target_id,
                    serde_json::json!({
                        "ewma": signal.ewma,
                        "samples": signal.samples,
                        "age_secs": signal.age_secs,
                    }),
                )
            })
            .collect::<serde_json::Map<_, _>>();
        let paused_until = self
            .quality_eval
            .backoff
            .lock()
            .ok()
            .and_then(|backoff| backoff.paused_until)
            .and_then(|until| until.checked_duration_since(now))
            .map(|remaining| now_ms.saturating_add(remaining.as_millis() as i64));
        Some(serde_json::json!({
            "mode": if cfg.routing_weight > 0.0 { "score" } else { "observe" },
            "evaluator_id": evaluator_id,
            "queue_depth": self.quality_eval.stats.queue_depth.load(Ordering::Relaxed),
            "paused_until": paused_until,
            "rolling_24h": {
                "attempted": budget.attempted,
                "scored": scored,
                "ungradable": ungradable,
                "failed": failed,
                "dropped": self.quality_eval.stats.dropped.load(Ordering::Relaxed),
                "budget_skipped": self.quality_eval.stats.budget_skipped.load(Ordering::Relaxed),
                "cost_micros": budget.cost_micros,
            },
            "per_target": per_target,
        }))
    }
}

fn eligible_request(req: &AiRequest) -> bool {
    !matches!(
        ExecutionTaskType::infer(req),
        ExecutionTaskType::Judge | ExecutionTaskType::Embeddings
    ) && req
        .metadata
        .get(QUALITY_EVAL_ORIGIN_KEY)
        .map(String::as_str)
        != Some(QUALITY_EVAL_ORIGIN_VALUE)
        && req.privacy_class != PrivacyClass::Confidential
        && req.tools.is_empty()
        && req.server_tools.is_empty()
        && capture::request_is_text_only(req)
}

#[cfg(test)]
mod tests {
    use sb_core::{ContentPart, Message, ToolSpec};

    use super::*;

    fn config() -> QualityEvalConfig {
        QualityEvalConfig {
            enabled: true,
            failure_backoff_after: 3,
            failure_backoff_secs: 60,
            ..QualityEvalConfig::default()
        }
    }

    #[test]
    fn backoff_opens_after_three_failures_and_allows_one_probe() {
        let cfg = config();
        let start = Instant::now();
        let mut backoff = Backoff::default();
        for _ in 0..2 {
            assert!(backoff.try_enter(start));
            backoff.failure(start, &cfg);
            assert!(!backoff.open(start));
        }
        assert!(backoff.try_enter(start));
        backoff.failure(start, &cfg);
        assert!(backoff.open(start));
        assert!(!backoff.try_enter(start));

        let after = start + Duration::from_secs(61);
        assert!(backoff.try_enter(after));
        assert!(!backoff.try_enter(after));
        backoff.success();
        assert!(backoff.try_enter(after));
    }

    #[test]
    fn eligibility_excludes_both_recursion_markers_confidential_and_non_text() {
        let plain = AiRequest::new("mock/echo", vec![Message::user("enough text")]);
        assert!(eligible_request(&plain));

        let mut task_judge = plain.clone();
        task_judge
            .metadata
            .insert("task_type".into(), "judge".into());
        assert!(!eligible_request(&task_judge));

        let inferred_judge = AiRequest::new("auto/judge", vec![Message::user("judge")]);
        assert!(!eligible_request(&inferred_judge));

        let mut origin = plain.clone();
        origin.metadata.insert(
            QUALITY_EVAL_ORIGIN_KEY.into(),
            QUALITY_EVAL_ORIGIN_VALUE.into(),
        );
        assert!(!eligible_request(&origin));

        let mut confidential = plain.clone();
        confidential.privacy_class = PrivacyClass::Confidential;
        assert!(!eligible_request(&confidential));

        let mut image = plain.clone();
        image.messages[0]
            .content
            .push(ContentPart::image_base64("image/png", "YWJj"));
        assert!(!eligible_request(&image));

        let mut tools = plain;
        tools.tools.push(ToolSpec {
            name: "lookup".into(),
            description: None,
            parameters: serde_json::json!({"type": "object"}),
        });
        assert!(!eligible_request(&tools));
    }

    #[test]
    fn full_queue_drops_newest_without_blocking() {
        let cfg = QualityEvalConfig {
            queue_capacity: 1,
            ..config()
        };
        let eval = QualityEval::new(&cfg);
        let job = |id: &str| QualityJob {
            served_request_id: id.into(),
            served_target_id: "mock/echo".into(),
            class: QUALITY_CLASS.into(),
            sample_revision: 1,
            evaluator_id: rubric::evaluator_id(&cfg.body_allowed_targets),
            input: capture::CaptureBuffer::from_bytes(b"input".to_vec(), 16).unwrap(),
            output: capture::CaptureBuffer::from_bytes(b"output".to_vec(), 16).unwrap(),
        };
        assert!(eval.try_enqueue(job("one")));
        assert!(!eval.try_enqueue(job("two")));
        assert_eq!(eval.stats.queue_depth.load(Ordering::Relaxed), 1);
        assert_eq!(eval.stats.dropped.load(Ordering::Relaxed), 1);
    }
}
