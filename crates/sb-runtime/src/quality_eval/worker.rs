use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use sb_core::{ContentPart, CostProfile, FinishReason};
use sb_store::{
    QualityJudgmentFinalization, QualityJudgmentReservation, QualityJudgmentReserveOutcome,
};
use zeroize::{Zeroize, Zeroizing};

use super::rubric::{self, ParsedJudgment};
use super::{QualityEval, QualityJob, ROLLING_WINDOW_MS, RUBRIC_VERSION};

fn split_target(target_id: &str) -> Option<(&str, &str)> {
    target_id
        .split_once('/')
        .filter(|(provider, model)| !provider.trim().is_empty() && !model.trim().is_empty())
}

fn estimate_cost_micros(
    prompt_bytes: usize,
    max_output_tokens: u32,
    prices: &[CostProfile],
) -> Option<u64> {
    if prices.is_empty()
        || prices.iter().any(|price| {
            !price.input_per_mtok.is_finite()
                || !price.output_per_mtok.is_finite()
                || price.input_per_mtok <= 0.0
                || price.output_per_mtok <= 0.0
        })
    {
        return None;
    }
    // A tokenizer can approach one token per UTF-8 byte. Reserving one token
    // per prompt byte plus fixed chat overhead is intentionally pessimistic,
    // but it is a true upper bound for the durable hard-cost gate.
    let input_tokens = prompt_bytes.saturating_add(16) as f64;
    let output_tokens = f64::from(max_output_tokens);
    prices
        .iter()
        .map(|price| {
            (input_tokens * price.input_per_mtok + output_tokens * price.output_per_mtok).ceil()
                as u64
        })
        .max()
}

fn allowed_prices(snap: &crate::Snapshot, targets: &[String]) -> Option<Vec<CostProfile>> {
    targets
        .iter()
        .map(|target| {
            let (provider, model) = split_target(target)?;
            snap.registry.cost_profile(provider, model)
        })
        .collect()
}

enum Terminal {
    Scored { score_norm: f64, reason: String },
    Ungradable { reason: String },
    Invalid,
    Failed,
    Timeout,
}

fn job_matches_config(job: &QualityJob, cfg: &sb_core::QualityEvalConfig) -> bool {
    cfg.enabled
        && job.evaluator_id == rubric::evaluator_id(&cfg.body_allowed_targets)
        && job.input.len() <= cfg.max_input_bytes
        && job.output.len() <= cfg.max_output_bytes
        && job.input.chars() >= cfg.min_input_chars
        && job.output.chars() >= cfg.min_output_chars
        && !cfg.body_allowed_targets.is_empty()
}

pub(super) async fn process_job(
    eval: &Arc<QualityEval>,
    engine: &Arc<crate::Engine>,
    job: QualityJob,
) {
    let snap = engine.snapshot();
    let cfg = &snap.config.server.quality_eval;
    let evaluator_id = rubric::evaluator_id(&cfg.body_allowed_targets);
    if !job_matches_config(&job, cfg) {
        eval.stats.dropped.fetch_add(1, Ordering::Relaxed);
        return;
    }
    if !eval
        .backoff
        .lock()
        .map(|mut backoff| backoff.try_enter(Instant::now()))
        .unwrap_or(false)
    {
        eval.stats.dropped.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let Some(input) = job.input.as_str() else {
        return;
    };
    let Some(output) = job.output.as_str() else {
        return;
    };
    let judge_request = rubric::build_judge_request(cfg, input, output);
    let judge_request_id = judge_request.id.clone();
    let Some(prices) = allowed_prices(&snap, &cfg.body_allowed_targets) else {
        eval.stats.budget_skipped.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let Some(reserved_cost_micros) = estimate_cost_micros(
        rubric::prompt_bytes(&judge_request),
        cfg.judge_max_output_tokens,
        &prices,
    ) else {
        eval.stats.budget_skipped.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let judgment_id = sb_core::new_id("qj");
    let created_at_ms = sb_store::now_millis();
    let reservation = QualityJudgmentReservation {
        judgment_id: judgment_id.clone(),
        judge_request_id,
        served_request_id: job.served_request_id,
        served_target_id: job.served_target_id.clone(),
        class: job.class.clone(),
        sample_revision: job.sample_revision,
        judge_revision: snap.revision,
        evaluator_id: evaluator_id.clone(),
        rubric_version: RUBRIC_VERSION.to_string(),
        judge_target_id: None,
        input_chars: job.input.chars().min(u32::MAX as usize) as u32,
        output_chars: job.output.chars().min(u32::MAX as usize) as u32,
        reserved_cost_micros,
        created_at_ms,
    };
    let Some(store) = eval.store.get() else {
        return;
    };
    match store.reserve_quality_judgment(
        &reservation,
        u64::from(cfg.max_judgments_per_24h),
        cfg.max_cost_micros_per_24h,
        created_at_ms.saturating_sub(ROLLING_WINDOW_MS),
    ) {
        Ok(QualityJudgmentReserveOutcome::Reserved) => {
            eval.stats.attempted.fetch_add(1, Ordering::Relaxed);
        }
        Ok(QualityJudgmentReserveOutcome::Duplicate) => return,
        Ok(QualityJudgmentReserveOutcome::BudgetExceeded(_)) => {
            eval.stats.budget_skipped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        Err(_) => {
            if let Ok(mut backoff) = eval.backoff.lock() {
                backoff.pause(Instant::now(), cfg);
            }
            eval.stats.failed.fetch_add(1, Ordering::Relaxed);
            return;
        }
    }

    let allowed = cfg
        .body_allowed_targets
        .iter()
        .cloned()
        .collect::<HashSet<_>>();
    let timeout = Duration::from_millis(cfg.judge_timeout_ms);
    let execution = tokio::time::timeout(
        timeout,
        engine.execute_scoped(Arc::clone(&snap), judge_request, Instant::now(), allowed),
    )
    .await;
    let (terminal, judge_target_id, actual_cost_micros) = match execution {
        Err(_) => (Terminal::Timeout, None, None),
        Ok(scoped) => {
            let witness_target = scoped
                .success
                .as_ref()
                .map(|success| success.target_id.clone());
            let witness_cost = scoped.success.as_ref().map(|success| success.cost_micros);
            match scoped.outcome {
                crate::ExecOutcome::Collected { mut response, .. }
                    if response.finish_reason == FinishReason::Stop =>
                {
                    let text = Zeroizing::new(response.message.text());
                    let parsed = rubric::parse(&text);
                    for part in &mut response.message.content {
                        if let ContentPart::Text { text } = part {
                            text.zeroize();
                        }
                    }
                    let terminal = match parsed {
                        ParsedJudgment::Scored {
                            score_norm,
                            reason_code,
                        } => Terminal::Scored {
                            score_norm,
                            reason: reason_code.as_str().to_string(),
                        },
                        ParsedJudgment::Ungradable { reason_code } => Terminal::Ungradable {
                            reason: reason_code.as_str().to_string(),
                        },
                        ParsedJudgment::Invalid => Terminal::Invalid,
                    };
                    (terminal, witness_target, witness_cost)
                }
                crate::ExecOutcome::Collected { .. }
                | crate::ExecOutcome::Stream { .. }
                | crate::ExecOutcome::Error(_) => (Terminal::Failed, witness_target, witness_cost),
            }
        }
    };

    let (status, score_norm, reason_code, valid) = match &terminal {
        Terminal::Scored { score_norm, reason } => {
            ("scored", Some(*score_norm), Some(reason.clone()), true)
        }
        Terminal::Ungradable { reason } => ("ungradable", None, Some(reason.clone()), true),
        Terminal::Invalid => ("invalid", None, None, false),
        Terminal::Failed => ("failed", None, None, false),
        Terminal::Timeout => ("timeout", None, None, false),
    };
    let finalized = store.finalize_quality_judgment(&QualityJudgmentFinalization {
        judgment_id: judgment_id.clone(),
        judge_target_id,
        status: status.to_string(),
        score_norm,
        reason_code,
        actual_cost_micros,
        completed_at_ms: sb_store::now_millis(),
    });
    if !matches!(finalized, Ok(true)) {
        if let Ok(mut backoff) = eval.backoff.lock() {
            backoff.pause(Instant::now(), cfg);
        }
        eval.stats.failed.fetch_add(1, Ordering::Relaxed);
        return;
    }

    if valid {
        if let Ok(mut backoff) = eval.backoff.lock() {
            backoff.success();
        }
    } else if let Ok(mut backoff) = eval.backoff.lock() {
        backoff.failure(Instant::now(), cfg);
    }
    match terminal {
        Terminal::Scored { score_norm, .. } => {
            eval.stats.scored.fetch_add(1, Ordering::Relaxed);
            engine.scorecard.record_quality(
                &job.served_target_id,
                &job.class,
                &evaluator_id,
                &snap.config.server.scorecard,
                cfg,
                crate::scorecard::QualitySample {
                    judgment_id,
                    ts: Instant::now(),
                    created_at_ms,
                    score_norm,
                },
            );
        }
        Terminal::Ungradable { .. } => {
            eval.stats.ungradable.fetch_add(1, Ordering::Relaxed);
        }
        Terminal::Invalid | Terminal::Failed | Terminal::Timeout => {
            eval.stats.failed.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reservation_estimate_is_conservative_and_unknown_price_fails_closed() {
        let prices = [CostProfile {
            input_per_mtok: 2.0,
            output_per_mtok: 4.0,
        }];
        assert_eq!(estimate_cost_micros(400, 100, &prices), Some(1_232));
        assert_eq!(estimate_cost_micros(400, 100, &[]), None);
        assert_eq!(
            estimate_cost_micros(
                400,
                100,
                &[CostProfile {
                    input_per_mtok: 0.0,
                    output_per_mtok: 4.0,
                }]
            ),
            None
        );
    }

    #[test]
    fn queued_job_is_discarded_after_evaluator_or_bound_changes() {
        let mut cfg = sb_core::QualityEvalConfig {
            enabled: true,
            body_allowed_targets: vec!["judge/a".to_string()],
            min_input_chars: 1,
            min_output_chars: 1,
            max_input_bytes: 16,
            max_output_bytes: 16,
            ..sb_core::QualityEvalConfig::default()
        };
        let mut job = QualityJob {
            served_request_id: "req".to_string(),
            served_target_id: "served/echo".to_string(),
            class: "any".to_string(),
            sample_revision: 1,
            evaluator_id: rubric::evaluator_id(&cfg.body_allowed_targets),
            input: super::super::capture::CaptureBuffer::from_bytes(
                b"input".to_vec(),
                cfg.max_input_bytes,
            )
            .unwrap(),
            output: super::super::capture::CaptureBuffer::from_bytes(
                b"output".to_vec(),
                cfg.max_output_bytes,
            )
            .unwrap(),
        };
        assert!(job_matches_config(&job, &cfg));

        cfg.body_allowed_targets = vec!["judge/b".to_string()];
        assert!(!job_matches_config(&job, &cfg));
        cfg.body_allowed_targets = vec!["judge/a".to_string()];
        job.evaluator_id = "quality-v0:retired-calibration".to_string();
        assert!(!job_matches_config(&job, &cfg));
        job.evaluator_id = rubric::evaluator_id(&cfg.body_allowed_targets);
        cfg.max_input_bytes = 4;
        assert!(!job_matches_config(&job, &cfg));
        cfg.max_input_bytes = 16;
        cfg.enabled = false;
        assert!(!job_matches_config(&job, &cfg));
    }
}
