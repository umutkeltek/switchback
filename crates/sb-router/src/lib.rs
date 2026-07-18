//! The router: resolve a request's `model`/route into an ordered candidate
//! list (primary + fallbacks), hard-filtering on capabilities and policy,
//! and emitting an explainable `RouteDecision`. Deterministic in v1.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use sb_core::{
    AiRequest, DemoteTrigger, ExecutionProfile, ExecutionTarget, HealthState, OutcomeSignal,
    OutcomeTier, RouteDecision, RouteRequire, RouteScore, RoutingPolicy, ScoringPolicy, TargetRef,
    UnknownContextPolicy, UnknownCostPolicy,
};
use sb_core::{ContentPart, ImageSource};

pub struct RoutePlan {
    pub candidates: Vec<ExecutionTarget>,
    pub decision: RouteDecision,
}

/// If a candidate sits on a lane the policy disallows, return that lane's name
/// (so the rejection reason can name it). `None` = no disallowed lane.
fn blocked_lane(target: &ExecutionTarget, policy: &RoutingPolicy) -> Option<&'static str> {
    let has = |tag: &str| target.policy_tags.iter().any(|t| t == tag);
    if !policy.allow_free && has("free") {
        return Some("free");
    }
    if !policy.allow_promo && has("promo") {
        return Some("promo");
    }
    if !policy.allow_aggregator && has("aggregator") {
        return Some("aggregator");
    }
    None
}

fn strategy_name(policy: &RoutingPolicy) -> &'static str {
    match policy.profile {
        Some(ExecutionProfile::Auto) => "auto",
        Some(ExecutionProfile::Cheap) => "auto/cheap",
        Some(ExecutionProfile::Fast) => "auto/fast",
        Some(ExecutionProfile::Coding) => "auto/coding",
        Some(ExecutionProfile::Private) => "auto/private",
        Some(ExecutionProfile::LargeContext) => "auto/large-context",
        Some(ExecutionProfile::Judge) => "auto/judge",
        Some(ExecutionProfile::Extract) => "auto/extract",
        None if policy.scoring.is_some() => "score",
        None if policy.cost_aware => "cost_aware",
        None if policy.latency_aware => "latency_aware",
        None => "ordered_fallback",
    }
}

fn is_coding_target(target: &ExecutionTarget) -> bool {
    let has_tag = target
        .task_tags
        .iter()
        .chain(&target.policy_tags)
        .any(|tag| {
            matches!(
                tag.to_ascii_lowercase().as_str(),
                "coding" | "code" | "coder"
            )
        });
    if has_tag {
        return true;
    }
    let id = target.id.to_ascii_lowercase();
    id.contains("coding") || id.contains("coder") || id.contains("code")
}

fn rank_factor(index: usize, len: usize) -> f64 {
    if len <= 1 {
        return 1.0;
    }
    1.0 - (index as f64 / (len - 1) as f64)
}

fn account_availability_factor(target: &ExecutionTarget) -> f64 {
    match target.healthy_accounts {
        Some(0) => 0.0,
        Some(n) => (n as f64 / 4.0).min(1.0),
        None => 0.5,
    }
}

fn health_factor(health: HealthState) -> f64 {
    match health {
        HealthState::Healthy => 1.0,
        HealthState::Degraded => 0.5,
        HealthState::Down => 0.0,
    }
}

// --- outcome-routing-v1 §6/§7: tiered demotion + outcome evidence in reasons ---

/// Outer sort key for the demotion pass: 2 = no healthy accounts in the pool
/// (Oracle #3, unchanged), 1 = the outcome scorecard currently reports this
/// target `Demoted` (live-traffic evidence), 0 = everyone else. A stable sort
/// by this key preserves whatever order the strategy (+ tie-break) already
/// established within a rank — demotion only reorders, it never rejects.
fn demote_rank(target: &ExecutionTarget) -> u8 {
    if target.healthy_accounts == Some(0) {
        2
    } else if matches!(target.outcome, Some(outcome) if outcome.tier == OutcomeTier::Demoted) {
        1
    } else {
        0
    }
}

fn format_pct(rate: f32) -> String {
    format!("{:.1}%", rate * 100.0)
}

/// Humanize a millisecond latency: sub-second stays `940ms`, at-or-above one
/// second becomes `3.1s` (outcome-routing-v1 §7 formatting helpers).
fn format_latency_humanized(ms: u32) -> String {
    if ms >= 1000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{ms}ms")
    }
}

fn format_cps_dollars(micros: u64) -> String {
    format!("${:.4}", micros as f64 / 1_000_000.0)
}

/// `outcome/select target=… ok=…% p50=… p95=… n=… [cps=$…]` for the target
/// the router actually selected, only when it carries scorecard evidence.
fn format_outcome_select(target_id: &str, outcome: &OutcomeSignal) -> String {
    let mut line = format!(
        "outcome/select target={target_id} ok={} p50={} p95={} n={}",
        format_pct(outcome.success_rate),
        format_latency_humanized(outcome.p50_latency_ms),
        format_latency_humanized(outcome.p95_latency_ms),
        outcome.samples,
    );
    if outcome.cost_per_success_micros > 0 {
        line.push_str(&format!(
            " cps={}",
            format_cps_dollars(outcome.cost_per_success_micros)
        ));
    }
    line
}

/// §7 demote reason code, read straight off `OutcomeSignal.demote_trigger`
/// (F12) — populated by `Scorecard::project()` from the entry's own
/// hysteresis transition, not guessed by the router from aggregate stats.
/// Falls back to `"success"` in the (should-never-happen) case of a
/// `Demoted` signal with no recorded trigger.
fn demote_reason_code(outcome: &OutcomeSignal) -> &'static str {
    match outcome.demote_trigger {
        Some(DemoteTrigger::Truncation) => "truncation",
        Some(DemoteTrigger::Streak) => "streak",
        Some(DemoteTrigger::Success) | None => "success",
    }
}

/// `outcome/demote target=… reason=<code> ok=…% trunc=…% n=… tier=demoted
/// [fails=N] [err=…]` for a rank-1 (scorecard-demoted) target. `fails=N`
/// (the entry's own `consecutive_failures`, F12) is appended only when the
/// trigger is the gate-free fast-demote streak, matching §7's example.
fn format_outcome_demote(target_id: &str, outcome: &OutcomeSignal) -> String {
    let mut line = format!(
        "outcome/demote target={target_id} reason={} ok={} trunc={} n={} tier=demoted",
        demote_reason_code(outcome),
        format_pct(outcome.success_rate),
        format_pct(outcome.truncation_rate),
        outcome.samples,
    );
    if outcome.demote_trigger == Some(DemoteTrigger::Streak) {
        line.push_str(&format!(" fails={}", outcome.consecutive_failures));
    }
    if let Some(err) = outcome.dominant_error {
        line.push_str(&format!(" err={}", err.as_str()));
    }
    line
}

/// Non-score-strategy tie-break (outcome-routing-v1 §6, Codex's qualified-peer
/// rule): within each maximal run of adjacent survivors the strategy's own
/// primary order considers equal (`same_group`), reorder by higher outcome
/// health — but ONLY when every member of the run has qualified evidence
/// (`samples >= min_samples`). Any unqualified peer leaves that run's
/// declared/strategy order completely untouched, protecting operator-declared
/// order from partial evidence. Emits one `outcome/tiebreak` reason line per
/// run actually reordered.
fn apply_outcome_tiebreak(
    survivors: &mut [ExecutionTarget],
    min_samples: u32,
    same_group: impl Fn(&ExecutionTarget, &ExecutionTarget) -> bool,
    decision: &mut RouteDecision,
) {
    let mut i = 0;
    while i < survivors.len() {
        let mut j = i + 1;
        while j < survivors.len() && same_group(&survivors[i], &survivors[j]) {
            j += 1;
        }
        if j - i > 1 {
            let qualified = survivors[i..j].iter().all(|t| {
                t.outcome
                    .map(|outcome| outcome.samples >= min_samples)
                    .unwrap_or(false)
            });
            if qualified {
                let before: Vec<String> = survivors[i..j].iter().map(|t| t.id.clone()).collect();
                survivors[i..j].sort_by(|a, b| {
                    let a_health = a.outcome.map(|o| o.health_factor).unwrap_or(0.0);
                    let b_health = b.outcome.map(|o| o.health_factor).unwrap_or(0.0);
                    b_health.partial_cmp(&a_health).unwrap_or(Ordering::Equal)
                });
                let after: Vec<String> = survivors[i..j].iter().map(|t| t.id.clone()).collect();
                if before != after {
                    let winner_health =
                        survivors[i].outcome.map(|o| o.health_factor).unwrap_or(0.0);
                    let winner_id = survivors[i].id.clone();
                    let over = survivors[i + 1..j]
                        .iter()
                        .map(|t| t.id.as_str())
                        .collect::<Vec<_>>()
                        .join(",");
                    decision.add_reason(format!(
                        "outcome/tiebreak target={winner_id} health={winner_health:.3} over={over}"
                    ));
                }
            }
        }
        i = j;
    }
}

fn inverse_range_factor(value: Option<f64>, min: Option<f64>, max: Option<f64>) -> f64 {
    let Some(value) = value else {
        return 0.0;
    };
    let (Some(min), Some(max)) = (min, max) else {
        return 1.0;
    };
    if (max - min).abs() < f64::EPSILON {
        return 1.0;
    }
    ((max - value) / (max - min)).clamp(0.0, 1.0)
}

fn range_bounds(values: impl Iterator<Item = Option<f64>>) -> (Option<f64>, Option<f64>) {
    let mut min: Option<f64> = None;
    let mut max: Option<f64> = None;
    for value in values.flatten() {
        min = Some(min.map_or(value, |current| current.min(value)));
        max = Some(max.map_or(value, |current| current.max(value)));
    }
    (min, max)
}

/// One request's shared response-quality comparison. It is computed once over
/// all hard-filtered survivors, before outcome-tier partitioning, so both the
/// ordering pass and the final score receipt use the same peer baseline.
struct QualityContext {
    baseline: f64,
    qualified_factors: BTreeMap<String, f64>,
}

impl QualityContext {
    fn new(candidates: &[ExecutionTarget], policy: &RoutingPolicy) -> Option<Self> {
        let config = &policy.quality_eval;
        if policy.scoring.is_none() || !config.enabled || config.routing_weight <= 0.0 {
            return None;
        }

        let qualified: Vec<_> = candidates
            .iter()
            .filter_map(|target| {
                target
                    .quality
                    .as_ref()
                    .filter(|quality| {
                        quality.age_secs <= policy.scorecard.window.ttl_secs
                            && quality.samples >= config.routing_min_samples
                    })
                    .map(|quality| (target, quality))
            })
            .collect();
        if qualified.len() < 2 {
            return None;
        }

        let baseline = qualified
            .iter()
            .map(|(_, quality)| quality.ewma)
            .sum::<f64>()
            / qualified.len() as f64;
        let qualified_factors = qualified
            .into_iter()
            .map(|(target, quality)| {
                let confidence = (quality.samples as f64
                    / config.routing_full_confidence_samples as f64)
                    .min(1.0);
                let factor = baseline + confidence * (quality.ewma - baseline);
                (target.id.clone(), factor)
            })
            .collect();

        Some(Self {
            baseline,
            qualified_factors,
        })
    }

    fn factor(&self, target_id: &str) -> f64 {
        self.qualified_factors
            .get(target_id)
            .copied()
            .unwrap_or(self.baseline)
    }
}

fn format_quality_reason(
    target: &ExecutionTarget,
    policy: &RoutingPolicy,
    context: Option<&QualityContext>,
) -> Option<String> {
    if !policy.quality_eval.enabled {
        return None;
    }
    let quality = target.quality.as_ref()?;
    if quality.age_secs > policy.scorecard.window.ttl_secs {
        return None;
    }

    if let Some(context) = context {
        Some(format!(
            "outcome/quality target={} q={:.3} n={} age={}s rubric=quality-v1 mode=score factor={:.3} weight={:.3}",
            target.id,
            quality.ewma,
            quality.samples,
            quality.age_secs,
            context.factor(&target.id),
            policy.quality_eval.routing_weight,
        ))
    } else {
        Some(format!(
            "outcome/quality target={} q={:.3} n={} age={}s rubric=quality-v1 mode=observe",
            target.id, quality.ewma, quality.samples, quality.age_secs,
        ))
    }
}

fn provider_file_ref_scope_mismatch(
    req: &AiRequest,
    candidate: &ExecutionTarget,
) -> Option<String> {
    req.messages
        .iter()
        .flat_map(|message| &message.content)
        .find_map(|part| match part {
            ContentPart::Image {
                source:
                    ImageSource::ProviderFileRef {
                        provider: Some(provider),
                        ..
                    },
                ..
            } if provider != &candidate.provider_id => Some(provider.clone()),
            _ => None,
        })
}

fn route_scores(
    candidates: &[ExecutionTarget],
    policy: &RoutingPolicy,
    streaming_required: bool,
    quality_context: Option<&QualityContext>,
) -> Vec<RouteScore> {
    let cost_bounds = range_bounds(
        candidates
            .iter()
            .map(|target| target.cost.map(|cost| cost.blended_per_mtok())),
    );
    let latency_signal = |target: &ExecutionTarget| {
        if streaming_required {
            target.ttft_ewma_ms.or(target.latency_ewma_ms)
        } else {
            target.latency_ewma_ms
        }
    };
    let latency_bounds = range_bounds(candidates.iter().map(latency_signal));
    let max_context = candidates
        .iter()
        .filter_map(|target| target.capabilities.max_context_tokens)
        .max();
    // outcome-routing-v1 F15: when the scorecard is disabled, or no
    // candidate in this set carries ANY evidence, the `outcome_health`
    // factor must not be added at all (not even a neutral 1.0) — otherwise
    // a candidate that never had scorecard evidence gets a serialized score
    // breakdown that differs from pre-outcome-routing behavior.
    let any_outcome_evidence =
        policy.scorecard.enabled && candidates.iter().any(|target| target.outcome.is_some());

    candidates
        .iter()
        .enumerate()
        .map(|(index, target)| {
            let rank = rank_factor(index, candidates.len());
            let mut factors = BTreeMap::new();
            factors.insert("selection_rank".to_string(), rank);
            factors.insert("health".to_string(), health_factor(target.health));
            factors.insert(
                "account_availability".to_string(),
                account_availability_factor(target),
            );
            factors.insert(
                "cost".to_string(),
                target
                    .cost
                    .map(|cost| {
                        inverse_range_factor(
                            Some(cost.blended_per_mtok()),
                            cost_bounds.0,
                            cost_bounds.1,
                        )
                    })
                    .unwrap_or(0.0),
            );
            factors.insert(
                "latency".to_string(),
                inverse_range_factor(target.latency_ewma_ms, latency_bounds.0, latency_bounds.1),
            );
            factors.insert(
                "ttft".to_string(),
                inverse_range_factor(
                    target.ttft_ewma_ms.or(target.latency_ewma_ms),
                    latency_bounds.0,
                    latency_bounds.1,
                ),
            );
            factors.insert("task_fit".to_string(), f64::from(is_coding_target(target)));
            if let Some(max_context) = max_context {
                let factor = target
                    .capabilities
                    .max_context_tokens
                    .map(|context| context as f64 / max_context as f64)
                    .unwrap_or(0.0);
                factors.insert("context_fit".to_string(), factor);
            } else {
                factors.insert("context_fit".to_string(), 0.0);
            }
            // outcome-routing-v1 §6/F15: outcome_health weighted factor.
            // Missing scorecard evidence on THIS candidate (but present on a
            // peer) is neutral (1.0), not a penalty — fail-open. When NO
            // candidate has evidence at all (or the scorecard is disabled),
            // the key is omitted entirely (see `any_outcome_evidence` above).
            if any_outcome_evidence {
                factors.insert(
                    "outcome_health".to_string(),
                    target
                        .outcome
                        .map(|outcome| outcome.health_factor as f64)
                        .unwrap_or(1.0),
                );
            }
            if let Some(context) = quality_context {
                factors.insert("response_quality".to_string(), context.factor(&target.id));
            }
            let score = policy
                .scoring
                .map(|scoring| {
                    weighted_score(
                        &factors,
                        scoring,
                        policy.scorecard.score_weight,
                        policy.quality_eval.routing_weight,
                    )
                })
                .unwrap_or(rank);

            RouteScore {
                target_id: target.id.clone(),
                score,
                factors,
            }
        })
        .collect()
}

/// `outcome_health`'s weight rides `ServerConfig.scorecard.score_weight`
/// (outcome-routing-v1 §5/§6) rather than a fixed `ScoringPolicy` field, since
/// it is a single cross-cutting knob rather than a per-profile tuning value.
fn weighted_score(
    factors: &BTreeMap<String, f64>,
    scoring: ScoringPolicy,
    outcome_weight: f64,
    quality_weight: f64,
) -> f64 {
    let mut weighted = 0.0;
    let mut total = 0.0;
    for (factor, value) in factors {
        let weight = match factor.as_str() {
            "outcome_health" => outcome_weight,
            "response_quality" => quality_weight,
            _ => scoring.weight_for(factor),
        };
        if weight <= 0.0 {
            continue;
        }
        weighted += weight * value;
        total += weight;
    }
    if total <= f64::EPSILON {
        0.0
    } else {
        (weighted / total).clamp(0.0, 1.0)
    }
}

/// Strategy-specific ordering + qualified tie-break, scoped to ONE demote-
/// rank partition (outcome-routing-v1 F10): the caller partitions
/// `survivors` by [`demote_rank`] FIRST (stable, preserving declared order
/// within each rank), then calls this once per contiguous rank slice — never
/// on the whole (unpartitioned) list. This is the fix for the bug where an
/// unqualified demoted/no-account peer sitting between two qualified healthy
/// peers in the DECLARED order could poison the qualified tie-break's
/// "every member of this run is qualified" check, suppressing a legitimate
/// reorder among peers that share the SAME rank. `score_by_target` is the
/// shared, globally-normalized score map for the `score` strategy (computed
/// once over the whole survivor set before partitioning, so cost/latency
/// bound normalization doesn't shift between ranks); `None` for every other
/// strategy.
fn order_partition(
    part: &mut [ExecutionTarget],
    policy: &RoutingPolicy,
    streaming_required: bool,
    score_by_target: Option<&BTreeMap<String, f64>>,
    decision: &mut RouteDecision,
) {
    if let Some(score_by_target) = score_by_target {
        part.sort_by(|a, b| {
            score_by_target
                .get(&b.id)
                .copied()
                .unwrap_or(0.0)
                .partial_cmp(&score_by_target.get(&a.id).copied().unwrap_or(0.0))
                .unwrap_or(Ordering::Equal)
        });
    } else if policy.profile == Some(ExecutionProfile::Coding) {
        part.sort_by_key(|target| u8::from(!is_coding_target(target)));
        apply_outcome_tiebreak(
            part,
            policy.scorecard.demotion.min_samples,
            |a, b| is_coding_target(a) == is_coding_target(b),
            decision,
        );
    } else if policy.profile == Some(ExecutionProfile::LargeContext) {
        part.sort_by(|a, b| {
            b.capabilities
                .max_context_tokens
                .unwrap_or(0)
                .cmp(&a.capabilities.max_context_tokens.unwrap_or(0))
        });
        apply_outcome_tiebreak(
            part,
            policy.scorecard.demotion.min_samples,
            |a, b| {
                a.capabilities.max_context_tokens.unwrap_or(0)
                    == b.capabilities.max_context_tokens.unwrap_or(0)
            },
            decision,
        );
    } else if policy.cost_aware {
        let cost_cmp = |a: &ExecutionTarget, b: &ExecutionTarget| match (
            a.cost.map(|c| c.blended_per_mtok()),
            b.cost.map(|c| c.blended_per_mtok()),
        ) {
            (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(Ordering::Equal),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        };
        part.sort_by(|a, b| cost_cmp(a, b));
        apply_outcome_tiebreak(
            part,
            policy.scorecard.demotion.min_samples,
            |a, b| cost_cmp(a, b) == Ordering::Equal,
            decision,
        );
    } else if policy.latency_aware {
        // Fastest-first. Interactive (streaming) requests rank on TTFT (first-byte
        // responsiveness), falling back to total latency when a host has never been
        // streamed; non-streaming requests rank on total latency. An unmeasured
        // target sorts FIRST so a cold host gets sampled, then its EWMA places it.
        let interactive = streaming_required;
        let signal = |t: &ExecutionTarget| -> Option<f64> {
            if interactive {
                t.ttft_ewma_ms.or(t.latency_ewma_ms)
            } else {
                t.latency_ewma_ms
            }
        };
        let latency_cmp = |a: &ExecutionTarget, b: &ExecutionTarget| match (signal(a), signal(b)) {
            (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(Ordering::Equal),
            (None, Some(_)) => Ordering::Less,
            (Some(_), None) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        };
        part.sort_by(|a, b| latency_cmp(a, b));
        apply_outcome_tiebreak(
            part,
            policy.scorecard.demotion.min_samples,
            |a, b| latency_cmp(a, b) == Ordering::Equal,
            decision,
        );
    } else {
        // Plain declared order (ordered_fallback): no strategy differentiates
        // survivors, so the whole partition is one tie-break group.
        apply_outcome_tiebreak(
            part,
            policy.scorecard.demotion.min_samples,
            |_, _| true,
            decision,
        );
    }
}

/// The strategy's own "selected/cheapest/fastest/widest" summary reason
/// line, computed from the FINAL winner across ALL ranks (outcome-routing-v1
/// F11) — never a pre-partition snapshot that a later, lower-priority rank
/// could still displace. `selected` is `survivors.first()` AFTER the
/// demote-rank partition and all per-rank ordering has already happened.
fn strategy_summary_reason(
    policy: &RoutingPolicy,
    selected: Option<&ExecutionTarget>,
    streaming_required: bool,
    score_by_target: Option<&BTreeMap<String, f64>>,
) -> Option<String> {
    let selected = selected?;
    if let Some(score_by_target) = score_by_target {
        let score = score_by_target.get(&selected.id).copied().unwrap_or(0.0);
        Some(format!("score: selected={} score={score:.3}", selected.id))
    } else if policy.profile == Some(ExecutionProfile::Coding) {
        let fit = if is_coding_target(selected) {
            "coding"
        } else {
            "unclassified"
        };
        Some(format!("auto/coding: selected={} fit={fit}", selected.id))
    } else if policy.profile == Some(ExecutionProfile::LargeContext) {
        let context = selected
            .capabilities
            .max_context_tokens
            .map(|tokens| tokens.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        Some(format!(
            "auto/large-context: widest={} context={context}",
            selected.id
        ))
    } else if policy.cost_aware {
        let price = selected
            .cost
            .map(|c| format!("{:.2}/Mtok", c.blended_per_mtok()))
            .unwrap_or_else(|| "unpriced".to_string());
        Some(format!(
            "cost_aware: cheapest={} blended={price}",
            selected.id
        ))
    } else if policy.latency_aware {
        let interactive = streaming_required;
        let signal = if interactive {
            selected.ttft_ewma_ms.or(selected.latency_ewma_ms)
        } else {
            selected.latency_ewma_ms
        };
        let metric = if interactive { "ttft" } else { "latency" };
        let val = signal
            .map(|ms| format!("{ms:.0}ms"))
            .unwrap_or_else(|| "unmeasured".to_string());
        Some(format!(
            "latency_aware: fastest={} {metric}={val}",
            selected.id
        ))
    } else {
        None
    }
}

pub fn plan_route(
    req: &AiRequest,
    route_name: &str,
    require: &RouteRequire,
    candidates: &[ExecutionTarget],
    policy: &RoutingPolicy,
) -> RoutePlan {
    let strategy = strategy_name(policy);
    let mut decision = RouteDecision::new(req.id.clone(), strategy);
    if let Some(profile) = policy.profile {
        decision.profile = Some(profile.id().to_string());
        decision.add_reason(format!("profile={}", profile.id()));
    }
    let streaming_required = require.streaming == Some(true) || req.stream;
    let tools_required = require.tool_calling == Some(true) || req.requires_tools();
    let server_tools_required = require.server_tools == Some(true) || req.requires_server_tools();
    let mut server_tool_protocols = req.required_server_tool_protocols();
    server_tool_protocols.extend(require.server_tool_protocols.iter().copied());
    let vision_required = require.vision_in == Some(true)
        || !require.vision_sources.is_empty()
        || req.requires_vision();
    let audio_required = require.audio_in == Some(true);
    let file_required = require.file_in == Some(true);
    let image_out_required = require.image_out == Some(true);
    let reasoning_required = require.reasoning_summary == Some(true);
    let mut image_sources: BTreeSet<_> = req.required_image_sources();
    image_sources.extend(require.vision_sources.iter().copied());
    let json_schema_required = require.json_schema == Some(true)
        || matches!(
            req.response_format,
            Some(sb_core::ResponseFormat::JsonSchema { .. })
        );

    decision.add_reason(format!("route={route_name}"));
    decision.add_reason(format!("stream_required={streaming_required}"));
    decision.add_reason(format!("tools_required={tools_required}"));
    decision.add_reason(format!("server_tools_required={server_tools_required}"));
    if server_tools_required && !server_tool_protocols.is_empty() {
        decision.add_reason(format!(
            "server_tool_protocols={}",
            server_tool_protocols
                .iter()
                .map(|protocol| protocol.as_str())
                .collect::<Vec<_>>()
                .join(",")
        ));
    }
    decision.add_reason(format!("vision_required={vision_required}"));
    if vision_required {
        decision.add_reason(format!("image_count={}", req.image_count()));
        if !image_sources.is_empty() {
            decision.add_reason(format!(
                "image_sources={}",
                image_sources
                    .iter()
                    .map(|source| source.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            ));
        }
    }
    decision.add_reason(format!("json_schema_required={json_schema_required}"));

    let mut survivors = Vec::new();

    for candidate in candidates {
        if streaming_required && !candidate.capabilities.streaming {
            decision.reject(
                candidate.id.clone(),
                "streaming required but target does not support it",
            );
            continue;
        }

        if tools_required && !candidate.capabilities.tool_calling {
            decision.reject(
                candidate.id.clone(),
                "tool calling required but target does not support it",
            );
            continue;
        }
        if server_tools_required && !candidate.capabilities.server_tools {
            decision.reject(
                candidate.id.clone(),
                "server tools required but target does not support them",
            );
            continue;
        }
        if server_tools_required {
            if let Some(unsupported) = server_tool_protocols.iter().copied().find(|protocol| {
                !candidate
                    .capabilities
                    .supports_server_tool_protocol(*protocol)
            }) {
                decision.reject(
                    candidate.id.clone(),
                    format!(
                        "server tool protocol {} required but target does not support it",
                        unsupported.as_str()
                    ),
                );
                continue;
            }
        }

        if vision_required && !candidate.capabilities.vision_in {
            decision.reject(
                candidate.id.clone(),
                "vision input required but target does not support it",
            );
            continue;
        }
        if audio_required && !candidate.capabilities.audio_in {
            decision.reject(
                candidate.id.clone(),
                "audio input required but target does not support it",
            );
            continue;
        }
        if file_required && !candidate.capabilities.file_in {
            decision.reject(
                candidate.id.clone(),
                "file input required but target does not support it",
            );
            continue;
        }
        if image_out_required && !candidate.capabilities.image_out {
            decision.reject(
                candidate.id.clone(),
                "image output required but target does not support it",
            );
            continue;
        }
        if reasoning_required && !candidate.capabilities.reasoning_summary {
            decision.reject(
                candidate.id.clone(),
                "reasoning summary required but target does not support it",
            );
            continue;
        }
        if vision_required {
            if let Some(unsupported) = image_sources
                .iter()
                .copied()
                .find(|source| !candidate.capabilities.supports_image_source(*source))
            {
                decision.reject(
                    candidate.id.clone(),
                    format!(
                        "image source {} required but target does not support it",
                        unsupported.as_str()
                    ),
                );
                continue;
            }
            if let Some(owner) = provider_file_ref_scope_mismatch(req, candidate) {
                decision.reject(
                    candidate.id.clone(),
                    format!(
                        "provider file image ref belongs to `{owner}` but target provider is `{}`",
                        candidate.provider_id
                    ),
                );
                continue;
            }
        }

        if json_schema_required && !candidate.capabilities.json_schema {
            decision.reject(
                candidate.id.clone(),
                "structured output (json_schema) required but target does not support it",
            );
            continue;
        }

        if let Some(required) = require.min_context_tokens {
            if let Some(max_context) = candidate.capabilities.max_context_tokens {
                if max_context < required {
                    decision.reject(
                        candidate.id.clone(),
                        format!("context window {max_context} < required {required}"),
                    );
                    continue;
                }
            } else if policy.unknown_context == UnknownContextPolicy::Reject {
                decision.reject(
                    candidate.id.clone(),
                    format!("context window unknown for required {required}"),
                );
                continue;
            }
        }

        if candidate.health == HealthState::Down {
            decision.reject(candidate.id.clone(), "target health is down");
            continue;
        }

        // Hard policy gates: max price and disallowed lanes are eligibility
        // rules. `cost_aware` only affects ordering after this point.
        if let Some(blocked) = blocked_lane(candidate, policy) {
            decision.reject(
                candidate.id.clone(),
                format!("policy: `{blocked}` lane not allowed"),
            );
            continue;
        }
        if let Some(max) = policy.max_price_per_mtok {
            if let Some(cost) = &candidate.cost {
                let blended = cost.blended_per_mtok();
                if blended > max {
                    decision.reject(
                        candidate.id.clone(),
                        format!("blended price {blended:.2}/Mtok > max {max:.2}/Mtok"),
                    );
                    continue;
                }
            } else if policy.unknown_cost == UnknownCostPolicy::Reject {
                decision.reject(
                    candidate.id.clone(),
                    format!("price unknown for max {max:.2}/Mtok policy"),
                );
                continue;
            }
        } else if policy.unknown_cost == UnknownCostPolicy::Reject && candidate.cost.is_none() {
            decision.reject(
                candidate.id.clone(),
                "price unknown and policy rejects unknown cost",
            );
            continue;
        }

        survivors.push(candidate.clone());
    }

    // outcome-routing-v1 F10: partition by demote_rank FIRST. A stable sort
    // preserves the declared/candidate order within each rank; strategy
    // ordering + the qualified tie-break then run WITHIN each rank slice
    // below, so an unqualified demoted/no-account peer in one rank can never
    // suppress a valid reorder among peers that belong to a DIFFERENT rank
    // (the pre-fix bug: tie-break qualification ran on the whole
    // pre-partition list, so one unqualified demoted peer sitting between
    // two qualified healthy peers poisoned their tie-break).
    //
    // `score_by_target` (the `score` strategy only) is computed ONCE here,
    // over the whole survivor set, so cost/latency bound normalization is
    // identical regardless of which rank a candidate ends up in — only the
    // SORT is scoped per-rank in `order_partition`.
    let quality_context = QualityContext::new(&survivors, policy);
    let score_by_target: Option<BTreeMap<String, f64>> = policy.scoring.map(|_| {
        route_scores(
            &survivors,
            policy,
            streaming_required,
            quality_context.as_ref(),
        )
        .into_iter()
        .map(|score| (score.target_id, score.score))
        .collect()
    });

    // Tiered demotion (Oracle #3 + outcome-routing-v1 §6): a target whose pool
    // has NO currently-usable account (`healthy_accounts == Some(0)` — all
    // locked, or circuit open) demotes to rank 2, unchanged from before; a
    // target the outcome scorecard currently reports `Demoted` (live-traffic
    // evidence, no account-pool problem) demotes to rank 1; everyone else is
    // rank 0. Demotion (not rejection) keeps them as a last resort, so e.g. a
    // lock that expires by attempt time still works and we never fail a
    // request that the credential layer — or a recovering target — could
    // have served.
    let no_account_demoted = survivors
        .iter()
        .filter(|c| c.healthy_accounts == Some(0))
        .count();
    let scorecard_demoted: Vec<(String, OutcomeSignal)> = survivors
        .iter()
        .filter(|c| c.healthy_accounts != Some(0))
        .filter_map(|c| match c.outcome {
            Some(outcome) if outcome.tier == OutcomeTier::Demoted => Some((c.id.clone(), outcome)),
            _ => None,
        })
        .collect();
    if no_account_demoted > 0 || !scorecard_demoted.is_empty() {
        survivors.sort_by_key(demote_rank);
    }

    // Strategy ordering + qualified tie-break, scoped to each rank slice.
    let end0 = survivors.partition_point(|t| demote_rank(t) == 0);
    let end1 = end0 + survivors[end0..].partition_point(|t| demote_rank(t) == 1);
    let (part0, rest) = survivors.split_at_mut(end0);
    let (part1, part2) = rest.split_at_mut(end1 - end0);
    for part in [part0, part1, part2] {
        order_partition(
            part,
            policy,
            streaming_required,
            score_by_target.as_ref(),
            &mut decision,
        );
    }

    if no_account_demoted > 0 {
        decision.add_reason(format!(
            "demoted {no_account_demoted} target(s) with no healthy accounts"
        ));
    }
    for (target_id, outcome) in &scorecard_demoted {
        decision.add_reason(format_outcome_demote(target_id, outcome));
    }

    // outcome-routing-v1 F11: the strategy summary line describes the ACTUAL
    // final winner — computed from `survivors.first()` AFTER the demote-rank
    // partition and all per-rank ordering above, never a pre-partition
    // snapshot a lower-priority rank could still displace.
    if let Some(reason) = strategy_summary_reason(
        policy,
        survivors.first(),
        streaming_required,
        score_by_target.as_ref(),
    ) {
        decision.add_reason(reason);
    }

    decision.scores = route_scores(
        &survivors,
        policy,
        streaming_required,
        quality_context.as_ref(),
    );

    if let Some(selected) = survivors.first() {
        decision.selected = Some(TargetRef::new(selected.id.clone()));
        if let Some(outcome) = selected.outcome {
            decision.add_reason(format_outcome_select(&selected.id, &outcome));
        }
        if let Some(reason) = format_quality_reason(selected, policy, quality_context.as_ref()) {
            decision.add_reason(reason);
        }
        // Unknown-model pass-through: flag the decision so the client/operator
        // doesn't treat it as a catalog-known model (Oracle #5).
        if selected.unverified {
            decision.unverified = true;
            decision
                .add_reason("unverified passthrough: capabilities + price not catalog-verified");
        }
        for fallback in survivors.iter().skip(1) {
            decision.fallbacks.push(TargetRef::new(fallback.id.clone()));
        }
    }

    RoutePlan {
        candidates: survivors,
        decision,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sb_core::{
        CapabilityProfile, ContentPart, ExecutionTargetKind, ImageSourceKind, Message,
        QualitySignal, Role, ScoringPolicy,
    };

    #[test]
    fn rejects_non_streaming_targets_when_stream_is_required() {
        let mut request = AiRequest::new("mock/echo", vec![Message::user("hi")]);
        request.stream = true;

        let mut first = ExecutionTarget::new("mock", "no-stream", ExecutionTargetKind::ModelApi);
        first.capabilities = CapabilityProfile {
            streaming: false,
            ..CapabilityProfile::default()
        };

        let mut second = ExecutionTarget::new("mock", "stream", ExecutionTargetKind::ModelApi);
        second.capabilities = CapabilityProfile::default();

        let plan = plan_route(
            &request,
            "default",
            &RouteRequire::default(),
            &[first, second],
            &RoutingPolicy::default(),
        );

        assert_eq!(plan.decision.selected.unwrap().target_id, "mock/stream");
        assert!(plan
            .decision
            .rejected
            .iter()
            .any(|rejected| rejected.target_id == "mock/no-stream"));
    }

    #[test]
    fn rejects_targets_without_json_schema_when_structured_output_required() {
        use sb_core::ResponseFormat;
        let mut request = AiRequest::new("x", vec![Message::user("hi")]);
        request.response_format = Some(ResponseFormat::JsonSchema {
            name: "out".into(),
            schema: Default::default(),
            strict: true,
        });

        // Gemini-like: no native structured output.
        let mut gemini = ExecutionTarget::new("gemini", "g", ExecutionTargetKind::ModelApi);
        gemini.capabilities = CapabilityProfile {
            json_schema: false,
            ..CapabilityProfile::default()
        };
        // OpenAI-like: supports it.
        let mut openai = ExecutionTarget::new("openai", "o", ExecutionTargetKind::ModelApi);
        openai.capabilities = CapabilityProfile {
            json_schema: true,
            ..CapabilityProfile::default()
        };

        let plan = plan_route(
            &request,
            "default",
            &RouteRequire::default(),
            &[gemini, openai],
            &RoutingPolicy::default(),
        );
        assert_eq!(plan.decision.selected.unwrap().target_id, "openai/o");
        assert!(plan
            .decision
            .rejected
            .iter()
            .any(|rejected| rejected.target_id == "gemini/g"));
    }

    #[test]
    fn rejects_targets_without_vision_when_image_input_is_present() {
        let request = AiRequest::new(
            "x",
            vec![Message {
                role: Role::User,
                content: vec![
                    ContentPart::text("inspect this"),
                    ContentPart::image_base64("image/png", "abc"),
                ],
                cache_hint: None,
            }],
        );

        let text_only = ExecutionTarget::new("mock", "text", ExecutionTargetKind::ModelApi);
        let mut vision = ExecutionTarget::new("mock", "vision", ExecutionTargetKind::ModelApi);
        vision.capabilities = CapabilityProfile {
            vision_in: true,
            ..CapabilityProfile::default()
        };

        let plan = plan_route(
            &request,
            "default",
            &RouteRequire::default(),
            &[text_only, vision],
            &RoutingPolicy::default(),
        );

        assert_eq!(plan.decision.selected.unwrap().target_id, "mock/vision");
        assert!(plan
            .decision
            .reason
            .iter()
            .any(|reason| reason == "vision_required=true"));
        assert!(plan.decision.rejected.iter().any(|rejected| {
            rejected.target_id == "mock/text" && rejected.reason.contains("vision input required")
        }));
    }

    #[test]
    fn explicit_vision_requirement_selects_vision_target_for_text_request() {
        let request = AiRequest::new("x", vec![Message::user("text only")]);
        let text_only = ExecutionTarget::new("mock", "text", ExecutionTargetKind::ModelApi);
        let mut vision = ExecutionTarget::new("mock", "vision", ExecutionTargetKind::ModelApi);
        vision.capabilities = CapabilityProfile {
            vision_in: true,
            ..CapabilityProfile::default()
        };
        let require = RouteRequire {
            vision_in: Some(true),
            ..RouteRequire::default()
        };

        let plan = plan_route(
            &request,
            "default",
            &require,
            &[text_only, vision],
            &RoutingPolicy::default(),
        );

        assert_eq!(plan.decision.selected.unwrap().target_id, "mock/vision");
        assert!(plan
            .decision
            .reason
            .iter()
            .any(|reason| reason == "image_count=0"));
    }

    #[test]
    fn vision_source_requirement_implies_vision_requirement() {
        let request = AiRequest::new("x", vec![Message::user("text only")]);
        let text_only = ExecutionTarget::new("mock", "text", ExecutionTargetKind::ModelApi);
        let mut vision = ExecutionTarget::new("mock", "vision", ExecutionTargetKind::ModelApi);
        vision.capabilities = CapabilityProfile {
            vision_in: true,
            vision_sources: vec![ImageSourceKind::RemoteUrl],
            ..CapabilityProfile::default()
        };
        let require = RouteRequire {
            vision_sources: vec![ImageSourceKind::RemoteUrl],
            ..RouteRequire::default()
        };

        let plan = plan_route(
            &request,
            "default",
            &require,
            &[text_only, vision],
            &RoutingPolicy::default(),
        );

        assert_eq!(plan.decision.selected.unwrap().target_id, "mock/vision");
    }

    #[test]
    fn rejects_targets_missing_required_image_source_kind() {
        let request = AiRequest::new(
            "x",
            vec![Message {
                role: Role::User,
                content: vec![ContentPart::image_url("https://example.test/img.png", None)],
                cache_hint: None,
            }],
        );

        let mut inline_only =
            ExecutionTarget::new("mock", "inline-only", ExecutionTargetKind::ModelApi);
        inline_only.capabilities = CapabilityProfile {
            vision_in: true,
            vision_sources: vec![ImageSourceKind::InlineBase64],
            ..CapabilityProfile::default()
        };
        let mut url_target = ExecutionTarget::new("mock", "url", ExecutionTargetKind::ModelApi);
        url_target.capabilities = CapabilityProfile {
            vision_in: true,
            vision_sources: vec![ImageSourceKind::RemoteUrl],
            ..CapabilityProfile::default()
        };

        let plan = plan_route(
            &request,
            "default",
            &RouteRequire::default(),
            &[inline_only, url_target],
            &RoutingPolicy::default(),
        );

        assert_eq!(plan.decision.selected.unwrap().target_id, "mock/url");
        assert!(plan
            .decision
            .reason
            .iter()
            .any(|reason| reason == "image_sources=remote_url"));
        assert!(plan.decision.rejected.iter().any(|rejected| {
            rejected.target_id == "mock/inline-only"
                && rejected.reason.contains("image source remote_url required")
        }));
    }

    #[test]
    fn rejects_targets_without_required_server_tool_protocol() {
        let mut request = AiRequest::new("x", vec![Message::user("search")]);
        request.server_tools.push(sb_core::ServerToolSpec::new(
            sb_core::ServerToolProtocol::OpenAiResponses,
            "web_search",
            sb_core::Json::Null,
        ));

        let text_only = ExecutionTarget::new("mock", "text", ExecutionTargetKind::ModelApi);
        let mut responses =
            ExecutionTarget::new("mock", "responses", ExecutionTargetKind::ModelApi);
        responses.capabilities = CapabilityProfile {
            server_tools: true,
            server_tool_protocols: vec![sb_core::ServerToolProtocol::OpenAiResponses],
            ..CapabilityProfile::default()
        };

        let plan = plan_route(
            &request,
            "default",
            &RouteRequire::default(),
            &[text_only, responses],
            &RoutingPolicy::default(),
        );

        assert_eq!(plan.decision.selected.unwrap().target_id, "mock/responses");
        assert!(plan
            .decision
            .reason
            .iter()
            .any(|reason| reason == "server_tools_required=true"));
        assert!(plan.decision.rejected.iter().any(|rejected| {
            rejected.target_id == "mock/text"
                && rejected
                    .reason
                    .contains("server tools required but target does not support them")
        }));
    }

    #[test]
    fn rejects_provider_file_ref_for_wrong_target_provider() {
        let request = AiRequest::new(
            "x",
            vec![Message {
                role: Role::User,
                content: vec![ContentPart::image_file_ref(
                    Some("openai"),
                    "file_123",
                    None,
                )],
                cache_hint: None,
            }],
        );

        let mut anthropic =
            ExecutionTarget::new("anthropic", "claude", ExecutionTargetKind::ModelApi);
        anthropic.capabilities = CapabilityProfile {
            vision_in: true,
            vision_sources: vec![ImageSourceKind::ProviderFileRef],
            ..CapabilityProfile::default()
        };
        let mut openai = ExecutionTarget::new("openai", "gpt", ExecutionTargetKind::ModelApi);
        openai.capabilities = CapabilityProfile {
            vision_in: true,
            vision_sources: vec![ImageSourceKind::ProviderFileRef],
            ..CapabilityProfile::default()
        };

        let plan = plan_route(
            &request,
            "default",
            &RouteRequire::default(),
            &[anthropic, openai],
            &RoutingPolicy::default(),
        );

        assert_eq!(plan.decision.selected.unwrap().target_id, "openai/gpt");
        assert!(plan.decision.rejected.iter().any(|rejected| {
            rejected.target_id == "anthropic/claude"
                && rejected.reason.contains("belongs to `openai`")
        }));
    }

    fn priced(provider: &str, model: &str, input: f64, output: f64) -> ExecutionTarget {
        let mut t = ExecutionTarget::new(provider, model, ExecutionTargetKind::ModelApi);
        t.cost = Some(sb_core::CostProfile {
            input_per_mtok: input,
            output_per_mtok: output,
        });
        t
    }

    #[test]
    fn cost_aware_orders_cheapest_first() {
        let request = AiRequest::new("m", vec![Message::user("hi")]);
        // Declared order is expensive→cheap; cost-aware must flip it.
        let pricey = priced("anthropic", "opus", 5.0, 25.0); // blended 30
        let mid = priced("openai", "gpt", 2.5, 15.0); // blended 17.5
        let cheap = priced("deepseek", "v4", 0.14, 0.28); // blended 0.42
        let policy = RoutingPolicy {
            cost_aware: true,
            ..Default::default()
        };
        let plan = plan_route(
            &request,
            "default",
            &RouteRequire::default(),
            &[pricey, mid, cheap],
            &policy,
        );
        assert_eq!(plan.decision.selected.unwrap().target_id, "deepseek/v4");
        let order: Vec<_> = plan.candidates.iter().map(|c| c.id.clone()).collect();
        assert_eq!(order, vec!["deepseek/v4", "openai/gpt", "anthropic/opus"]);
        assert_eq!(plan.decision.strategy, "cost_aware");
    }

    #[test]
    fn cost_aware_records_score_factors_per_candidate() {
        let request = AiRequest::new("m", vec![Message::user("hi")]);
        let pricey = priced("anthropic", "opus", 5.0, 25.0);
        let cheap = priced("deepseek", "v4", 0.14, 0.28);
        let policy = RoutingPolicy {
            cost_aware: true,
            ..Default::default()
        };
        let plan = plan_route(
            &request,
            "default",
            &RouteRequire::default(),
            &[pricey, cheap],
            &policy,
        );

        assert_eq!(plan.decision.scores.len(), 2);
        assert_eq!(plan.decision.scores[0].target_id, "deepseek/v4");
        assert!(plan.decision.scores[0].score > plan.decision.scores[1].score);
        assert_eq!(
            plan.decision.scores[0].factors.get("cost"),
            Some(&1.0),
            "cheapest candidate should have the strongest cost factor"
        );
        assert!(plan.decision.scores[0]
            .factors
            .contains_key("account_availability"));
    }

    #[test]
    fn weighted_scoring_orders_by_total_score_without_cost_mode() {
        let request = AiRequest::new("m", vec![Message::user("hi")]);
        let pricey = priced("anthropic", "opus", 5.0, 25.0);
        let cheap = priced("deepseek", "v4", 0.14, 0.28);
        let policy = RoutingPolicy {
            scoring: Some(ScoringPolicy {
                cost: 1.0,
                ..ScoringPolicy::balanced()
            }),
            ..Default::default()
        };
        let plan = plan_route(
            &request,
            "default",
            &RouteRequire::default(),
            &[pricey, cheap],
            &policy,
        );

        assert_eq!(plan.decision.strategy, "score");
        assert_eq!(plan.decision.selected.unwrap().target_id, "deepseek/v4");
        assert_eq!(plan.decision.scores[0].target_id, "deepseek/v4");
        assert!(plan.decision.scores[0].score > plan.decision.scores[1].score);
        assert!(plan
            .decision
            .reason
            .iter()
            .any(|reason| reason.contains("score: selected=deepseek/v4")));
    }

    #[test]
    fn cost_aware_off_preserves_declared_order() {
        let request = AiRequest::new("m", vec![Message::user("hi")]);
        let pricey = priced("anthropic", "opus", 5.0, 25.0);
        let cheap = priced("deepseek", "v4", 0.14, 0.28);
        let plan = plan_route(
            &request,
            "default",
            &RouteRequire::default(),
            &[pricey, cheap],
            &RoutingPolicy::default(), // cost_aware = false
        );
        assert_eq!(plan.decision.selected.unwrap().target_id, "anthropic/opus");
        assert_eq!(plan.decision.strategy, "ordered_fallback");
    }

    #[test]
    fn max_price_rejects_over_budget_candidates() {
        let request = AiRequest::new("m", vec![Message::user("hi")]);
        let pricey = priced("anthropic", "opus", 5.0, 25.0); // blended 30
        let cheap = priced("deepseek", "v4", 0.14, 0.28); // blended 0.42
        let policy = RoutingPolicy {
            cost_aware: true,
            max_price_per_mtok: Some(10.0),
            ..Default::default()
        };
        let plan = plan_route(
            &request,
            "default",
            &RouteRequire::default(),
            &[pricey, cheap],
            &policy,
        );
        assert_eq!(plan.decision.selected.unwrap().target_id, "deepseek/v4");
        assert!(plan
            .decision
            .rejected
            .iter()
            .any(|r| r.target_id == "anthropic/opus" && r.reason.contains("max")));
    }

    #[test]
    fn max_price_applies_when_cost_aware_false() {
        let request = AiRequest::new("m", vec![Message::user("hi")]);
        let pricey = priced("anthropic", "opus", 5.0, 25.0);
        let cheap = priced("deepseek", "v4", 0.14, 0.28);
        let policy = RoutingPolicy {
            max_price_per_mtok: Some(10.0),
            ..Default::default()
        };
        let plan = plan_route(
            &request,
            "default",
            &RouteRequire::default(),
            &[pricey, cheap],
            &policy,
        );

        assert_eq!(plan.decision.strategy, "ordered_fallback");
        assert_eq!(plan.decision.selected.unwrap().target_id, "deepseek/v4");
        assert!(plan
            .decision
            .rejected
            .iter()
            .any(|r| r.target_id == "anthropic/opus" && r.reason.contains("max")));
    }

    #[test]
    fn unknown_cost_rejected_when_policy_rejects() {
        let request = AiRequest::new("m", vec![Message::user("hi")]);
        let unpriced = ExecutionTarget::new("local", "ollama", ExecutionTargetKind::ModelApi);
        let priced = priced("deepseek", "v4", 0.14, 0.28);
        let policy = RoutingPolicy {
            unknown_cost: UnknownCostPolicy::Reject,
            ..Default::default()
        };

        let plan = plan_route(
            &request,
            "default",
            &RouteRequire::default(),
            &[unpriced, priced],
            &policy,
        );

        assert_eq!(plan.decision.selected.unwrap().target_id, "deepseek/v4");
        assert!(plan
            .decision
            .rejected
            .iter()
            .any(|r| r.target_id == "local/ollama" && r.reason.contains("price unknown")));
    }

    fn tagged(
        provider: &str,
        model: &str,
        input: f64,
        output: f64,
        tags: &[&str],
    ) -> ExecutionTarget {
        let mut t = priced(provider, model, input, output);
        t.policy_tags = tags.iter().map(|s| s.to_string()).collect();
        t
    }

    #[test]
    fn cost_aware_gates_a_disallowed_lane() {
        let request = AiRequest::new("m", vec![Message::user("hi")]);
        // The aggregator host is cheaper, but allow_aggregator=false excludes it.
        let agg = tagged("together", "m", 0.1, 0.2, &["aggregator"]);
        let direct = tagged("deepseek", "m", 0.5, 0.5, &[]);
        let policy = RoutingPolicy {
            cost_aware: true,
            allow_aggregator: false,
            ..Default::default()
        };
        let plan = plan_route(
            &request,
            "default",
            &RouteRequire::default(),
            &[agg, direct],
            &policy,
        );
        assert_eq!(
            plan.decision.selected.unwrap().target_id,
            "deepseek/m",
            "aggregator excluded despite being cheaper"
        );
        assert!(plan
            .decision
            .rejected
            .iter()
            .any(|r| r.target_id == "together/m" && r.reason.contains("aggregator")));
    }

    #[test]
    fn lane_gate_applies_when_cost_aware_false() {
        let request = AiRequest::new("m", vec![Message::user("hi")]);
        let agg = tagged("together", "m", 0.1, 0.2, &["aggregator"]);
        let direct = tagged("deepseek", "m", 0.5, 0.5, &[]);
        let policy = RoutingPolicy {
            allow_aggregator: false,
            ..Default::default()
        };

        let plan = plan_route(
            &request,
            "default",
            &RouteRequire::default(),
            &[agg, direct],
            &policy,
        );

        assert_eq!(plan.decision.strategy, "ordered_fallback");
        assert_eq!(plan.decision.selected.unwrap().target_id, "deepseek/m");
        assert!(plan
            .decision
            .rejected
            .iter()
            .any(|r| r.target_id == "together/m" && r.reason.contains("aggregator")));
    }

    #[test]
    fn cost_aware_allows_all_lanes_by_default() {
        let request = AiRequest::new("m", vec![Message::user("hi")]);
        let agg = tagged("together", "m", 0.1, 0.2, &["aggregator", "free"]);
        let direct = tagged("deepseek", "m", 0.5, 0.5, &[]);
        let policy = RoutingPolicy {
            cost_aware: true,
            ..Default::default()
        };
        let plan = plan_route(
            &request,
            "default",
            &RouteRequire::default(),
            &[agg, direct],
            &policy,
        );
        assert_eq!(
            plan.decision.selected.unwrap().target_id,
            "together/m",
            "default allows every lane; cheapest wins"
        );
    }

    fn with_latency(provider: &str, model: &str, ewma_ms: Option<f64>) -> ExecutionTarget {
        let mut t = ExecutionTarget::new(provider, model, ExecutionTargetKind::ModelApi);
        t.latency_ewma_ms = ewma_ms;
        t
    }

    #[test]
    fn latency_aware_orders_fastest_first() {
        let request = AiRequest::new("m", vec![Message::user("hi")]);
        let slow = with_latency("a", "m", Some(800.0));
        let fast = with_latency("b", "m", Some(120.0));
        let mid = with_latency("c", "m", Some(300.0));
        let policy = RoutingPolicy {
            latency_aware: true,
            ..Default::default()
        };
        let plan = plan_route(
            &request,
            "default",
            &RouteRequire::default(),
            &[slow, fast, mid],
            &policy,
        );
        let order: Vec<_> = plan.candidates.iter().map(|c| c.id.clone()).collect();
        assert_eq!(order, vec!["b/m", "c/m", "a/m"]);
        assert_eq!(plan.decision.strategy, "latency_aware");
    }

    #[test]
    fn latency_aware_explores_unmeasured_first() {
        let request = AiRequest::new("m", vec![Message::user("hi")]);
        let measured = with_latency("a", "m", Some(50.0));
        let cold = with_latency("b", "m", None); // never measured → explore first
        let policy = RoutingPolicy {
            latency_aware: true,
            ..Default::default()
        };
        let plan = plan_route(
            &request,
            "default",
            &RouteRequire::default(),
            &[measured, cold],
            &policy,
        );
        assert_eq!(
            plan.decision.selected.unwrap().target_id,
            "b/m",
            "an unmeasured target is sampled before measured ones"
        );
    }

    #[test]
    fn cost_aware_wins_when_both_toggles_are_on() {
        let request = AiRequest::new("m", vec![Message::user("hi")]);
        let mut cheap_slow = priced("a", "m", 0.1, 0.2);
        cheap_slow.latency_ewma_ms = Some(900.0);
        let mut pricey_fast = priced("b", "m", 5.0, 25.0);
        pricey_fast.latency_ewma_ms = Some(50.0);
        let policy = RoutingPolicy {
            cost_aware: true,
            latency_aware: true,
            ..Default::default()
        };
        let plan = plan_route(
            &request,
            "default",
            &RouteRequire::default(),
            &[pricey_fast, cheap_slow],
            &policy,
        );
        assert_eq!(plan.decision.selected.unwrap().target_id, "a/m");
        assert_eq!(plan.decision.strategy, "cost_aware");
    }

    #[test]
    fn unpriced_candidates_sort_after_priced() {
        let request = AiRequest::new("m", vec![Message::user("hi")]);
        let unpriced = ExecutionTarget::new("local", "ollama", ExecutionTargetKind::ModelApi);
        let cheap = priced("deepseek", "v4", 0.14, 0.28);
        let policy = RoutingPolicy {
            cost_aware: true,
            ..Default::default()
        };
        let plan = plan_route(
            &request,
            "default",
            &RouteRequire::default(),
            &[unpriced, cheap],
            &policy,
        );
        // Priced cheap wins; the unknown-cost local target is the fallback.
        let order: Vec<_> = plan.candidates.iter().map(|c| c.id.clone()).collect();
        assert_eq!(order, vec!["deepseek/v4", "local/ollama"]);
    }

    #[test]
    fn unknown_context_rejected_when_policy_rejects() {
        let request = AiRequest::new("m", vec![Message::user("hi")]);
        let unknown = ExecutionTarget::new("unknown", "m", ExecutionTargetKind::ModelApi);
        let mut large = ExecutionTarget::new("large", "m", ExecutionTargetKind::ModelApi);
        large.capabilities.max_context_tokens = Some(128_000);
        let policy = RoutingPolicy {
            unknown_context: UnknownContextPolicy::Reject,
            ..Default::default()
        };
        let require = RouteRequire {
            min_context_tokens: Some(32_000),
            ..Default::default()
        };

        let plan = plan_route(&request, "default", &require, &[unknown, large], &policy);

        assert_eq!(plan.decision.selected.unwrap().target_id, "large/m");
        assert!(plan
            .decision
            .rejected
            .iter()
            .any(|r| r.target_id == "unknown/m" && r.reason.contains("context window unknown")));
    }

    fn with_pool(provider: &str, model: &str, healthy: usize) -> ExecutionTarget {
        let mut t = ExecutionTarget::new(provider, model, ExecutionTargetKind::ModelApi);
        t.healthy_accounts = Some(healthy);
        t
    }

    fn with_outcome(
        provider: &str,
        model: &str,
        tier: OutcomeTier,
        samples: u32,
        health_factor: f32,
    ) -> ExecutionTarget {
        let mut t = ExecutionTarget::new(provider, model, ExecutionTargetKind::ModelApi);
        t.outcome = Some(OutcomeSignal {
            samples,
            success_rate: health_factor,
            p50_latency_ms: 100,
            p95_latency_ms: 940,
            cost_per_success_micros: 0,
            truncation_rate: 0.0,
            dominant_error: None,
            tier,
            health_factor,
            demote_trigger: (tier == OutcomeTier::Demoted).then_some(DemoteTrigger::Streak),
            consecutive_failures: 0,
        });
        t
    }

    fn with_quality(
        provider: &str,
        model: &str,
        ewma: f64,
        samples: u32,
        age_secs: u64,
    ) -> ExecutionTarget {
        let mut target = ExecutionTarget::new(provider, model, ExecutionTargetKind::ModelApi);
        target.quality = Some(QualitySignal {
            ewma,
            samples,
            age_secs,
            evaluator_id: "quality-v1:test".to_string(),
        });
        target
    }

    fn quality_score_policy(weight: f64) -> RoutingPolicy {
        let mut policy = RoutingPolicy {
            scoring: Some(ScoringPolicy {
                selection_rank: 0.0,
                health: 0.0,
                account_availability: 0.0,
                cost: 0.0,
                latency: 0.0,
                ttft: 0.0,
                task_fit: 0.0,
                context_fit: 0.0,
            }),
            ..Default::default()
        };
        policy.quality_eval.enabled = true;
        policy.quality_eval.routing_min_samples = 5;
        policy.quality_eval.routing_full_confidence_samples = 10;
        policy.quality_eval.routing_weight = weight;
        policy
    }

    fn quality_factor<'a>(plan: &'a RoutePlan, target_id: &str) -> Option<&'a f64> {
        plan.decision
            .scores
            .iter()
            .find(|score| score.target_id == target_id)
            .and_then(|score| score.factors.get("response_quality"))
    }

    #[test]
    fn disabled_or_absent_quality_is_byte_identical_to_previous_decision() {
        let request = AiRequest::new("x", vec![Message::user("hi")]);
        let baseline_targets = [
            ExecutionTarget::new("p1", "m", ExecutionTargetKind::ModelApi),
            ExecutionTarget::new("p2", "m", ExecutionTargetKind::ModelApi),
        ];
        let baseline = plan_route(
            &request,
            "default",
            &RouteRequire::default(),
            &baseline_targets,
            &quality_score_policy(0.05),
        );

        let mut disabled_policy = quality_score_policy(0.05);
        disabled_policy.quality_eval.enabled = false;
        let stamped = [
            with_quality("p1", "m", 0.1, 10, 1),
            with_quality("p2", "m", 0.9, 10, 1),
        ];
        let disabled = plan_route(
            &request,
            "default",
            &RouteRequire::default(),
            &stamped,
            &disabled_policy,
        );

        assert_eq!(disabled.decision.reason, baseline.decision.reason);
        assert_eq!(disabled.decision.summary(), baseline.decision.summary());
        for (actual, expected) in disabled
            .decision
            .scores
            .iter()
            .zip(&baseline.decision.scores)
        {
            assert_eq!(actual.target_id, expected.target_id);
            assert_eq!(actual.score, expected.score);
            assert_eq!(actual.factors, expected.factors);
        }
    }

    #[test]
    fn weight_zero_observes_without_a_factor_or_reorder() {
        let request = AiRequest::new("x", vec![Message::user("hi")]);
        let targets = [
            with_quality("p1", "m", 0.1, 10, 7),
            with_quality("p2", "m", 0.9, 10, 8),
        ];
        let plan = plan_route(
            &request,
            "default",
            &RouteRequire::default(),
            &targets,
            &quality_score_policy(0.0),
        );

        assert_eq!(plan.decision.selected.as_ref().unwrap().target_id, "p1/m");
        assert!(plan
            .decision
            .scores
            .iter()
            .all(|score| !score.factors.contains_key("response_quality")));
        assert!(plan.decision.reason.iter().any(|reason| {
            reason
                == "outcome/quality target=p1/m q=0.100 n=10 age=7s rubric=quality-v1 mode=observe"
        }));
    }

    #[test]
    fn one_qualified_target_cannot_steer() {
        let request = AiRequest::new("x", vec![Message::user("hi")]);
        let targets = [
            ExecutionTarget::new("p1", "m", ExecutionTargetKind::ModelApi),
            with_quality("p2", "m", 1.0, 10, 1),
        ];
        let plan = plan_route(
            &request,
            "default",
            &RouteRequire::default(),
            &targets,
            &quality_score_policy(0.05),
        );

        assert_eq!(plan.decision.selected.as_ref().unwrap().target_id, "p1/m");
        assert!(quality_factor(&plan, "p2/m").is_none());
    }

    #[test]
    fn quality_centering_and_confidence_match_the_formula() {
        let request = AiRequest::new("x", vec![Message::user("hi")]);
        let targets = [
            with_quality("p1", "m", 0.2, 5, 2),
            with_quality("p2", "m", 0.8, 10, 3),
        ];
        let plan = plan_route(
            &request,
            "default",
            &RouteRequire::default(),
            &targets,
            &quality_score_policy(0.05),
        );

        assert_eq!(plan.decision.selected.as_ref().unwrap().target_id, "p2/m");
        assert!((quality_factor(&plan, "p1/m").unwrap() - 0.35).abs() < 1e-12);
        assert!((quality_factor(&plan, "p2/m").unwrap() - 0.8).abs() < 1e-12);
        assert!(plan.decision.reason.iter().any(|reason| {
            reason
                == "outcome/quality target=p2/m q=0.800 n=10 age=3s rubric=quality-v1 mode=score factor=0.800 weight=0.050"
        }));
    }

    #[test]
    fn unknown_peer_receives_the_qualified_baseline() {
        let request = AiRequest::new("x", vec![Message::user("hi")]);
        let targets = [
            with_quality("low", "m", 0.2, 10, 1),
            ExecutionTarget::new("unknown", "m", ExecutionTargetKind::ModelApi),
            with_quality("high", "m", 0.8, 10, 1),
        ];
        let plan = plan_route(
            &request,
            "default",
            &RouteRequire::default(),
            &targets,
            &quality_score_policy(0.05),
        );

        assert!((quality_factor(&plan, "unknown/m").unwrap() - 0.5).abs() < 1e-12);
        assert_eq!(
            plan.candidates
                .iter()
                .map(|target| target.id.as_str())
                .collect::<Vec<_>>(),
            vec!["high/m", "unknown/m", "low/m"]
        );
    }

    #[test]
    fn equal_quality_scores_are_a_stable_no_op() {
        let request = AiRequest::new("x", vec![Message::user("hi")]);
        let targets = [
            with_quality("p1", "m", 0.7, 10, 1),
            with_quality("p2", "m", 0.7, 10, 1),
        ];
        let plan = plan_route(
            &request,
            "default",
            &RouteRequire::default(),
            &targets,
            &quality_score_policy(0.05),
        );

        assert_eq!(plan.decision.selected.as_ref().unwrap().target_id, "p1/m");
        assert_eq!(quality_factor(&plan, "p1/m"), Some(&0.7));
        assert_eq!(quality_factor(&plan, "p2/m"), Some(&0.7));
    }

    #[test]
    fn stale_quality_is_absent_from_scoring_and_reasons() {
        let request = AiRequest::new("x", vec![Message::user("hi")]);
        let mut policy = quality_score_policy(0.05);
        policy.scorecard.window.ttl_secs = 60;
        let targets = [
            with_quality("p1", "m", 0.1, 10, 61),
            with_quality("p2", "m", 0.9, 10, 61),
        ];
        let plan = plan_route(
            &request,
            "default",
            &RouteRequire::default(),
            &targets,
            &policy,
        );

        assert_eq!(plan.decision.selected.unwrap().target_id, "p1/m");
        assert!(plan
            .decision
            .scores
            .iter()
            .all(|score| !score.factors.contains_key("response_quality")));
        assert!(!plan
            .decision
            .reason
            .iter()
            .any(|reason| reason.starts_with("outcome/quality")));
    }

    #[test]
    fn quality_reorders_healthy_peers_inside_their_tier() {
        let request = AiRequest::new("x", vec![Message::user("hi")]);
        let low = with_quality("low", "m", 0.1, 10, 1);
        let high = with_quality("high", "m", 0.9, 10, 1);
        let plan = plan_route(
            &request,
            "default",
            &RouteRequire::default(),
            &[low, high],
            &quality_score_policy(0.05),
        );

        assert_eq!(plan.decision.selected.unwrap().target_id, "high/m");
    }

    #[test]
    fn high_quality_demoted_target_cannot_cross_the_healthy_tier() {
        let request = AiRequest::new("x", vec![Message::user("hi")]);
        let mut demoted = with_quality("demoted", "m", 1.0, 10, 1);
        demoted.outcome = with_outcome("demoted", "m", OutcomeTier::Demoted, 20, 0.1).outcome;
        let mut healthy = with_quality("healthy", "m", 0.0, 10, 1);
        healthy.outcome = with_outcome("healthy", "m", OutcomeTier::Healthy, 20, 0.9).outcome;
        let plan = plan_route(
            &request,
            "default",
            &RouteRequire::default(),
            &[demoted, healthy],
            &quality_score_policy(0.05),
        );

        assert_eq!(plan.decision.selected.unwrap().target_id, "healthy/m");
    }

    #[test]
    fn low_quality_healthy_target_cannot_fall_below_the_demoted_tier() {
        let request = AiRequest::new("x", vec![Message::user("hi")]);
        let mut healthy = with_quality("healthy", "m", 0.0, 10, 1);
        healthy.outcome = with_outcome("healthy", "m", OutcomeTier::Healthy, 20, 0.1).outcome;
        let mut demoted = with_quality("demoted", "m", 1.0, 10, 1);
        demoted.outcome = with_outcome("demoted", "m", OutcomeTier::Demoted, 20, 0.9).outcome;
        let plan = plan_route(
            &request,
            "default",
            &RouteRequire::default(),
            &[demoted, healthy],
            &quality_score_policy(0.05),
        );

        assert_eq!(plan.decision.selected.unwrap().target_id, "healthy/m");
    }

    #[test]
    fn demotes_targets_with_no_healthy_accounts() {
        let request = AiRequest::new("x", vec![Message::user("hi")]);
        // Declared order puts the degraded (0 healthy accounts) target first.
        let degraded = with_pool("p1", "m", 0);
        let healthy = with_pool("p2", "m", 2);
        let plan = plan_route(
            &request,
            "default",
            &RouteRequire::default(),
            &[degraded, healthy],
            &RoutingPolicy::default(), // ordered_fallback
        );
        assert_eq!(
            plan.decision.selected.unwrap().target_id,
            "p2/m",
            "the healthy-pool target is selected over the declared-first degraded one"
        );
        let order: Vec<_> = plan.candidates.iter().map(|c| c.id.clone()).collect();
        assert_eq!(
            order,
            vec!["p2/m", "p1/m"],
            "degraded target demoted to last"
        );
        assert!(plan
            .decision
            .reason
            .iter()
            .any(|r| r.contains("no healthy accounts")));
    }

    #[test]
    fn all_degraded_still_selects_a_last_resort() {
        let request = AiRequest::new("x", vec![Message::user("hi")]);
        // Every target has a locked pool — demotion must NOT empty the set, so the
        // credential layer still gets a chance (a lock may expire by attempt time).
        let plan = plan_route(
            &request,
            "default",
            &RouteRequire::default(),
            &[with_pool("p1", "m", 0), with_pool("p2", "m", 0)],
            &RoutingPolicy::default(),
        );
        assert!(plan.decision.selected.is_some());
        assert_eq!(plan.candidates.len(), 2, "demotion reorders, never rejects");
    }

    #[test]
    fn tiered_demotion_orders_healthy_then_scorecard_demoted_then_no_accounts() {
        let request = AiRequest::new("x", vec![Message::user("hi")]);
        // Declared order is worst-first to prove the tiered sort actually reorders:
        // no-healthy-accounts (rank 2) sinks below scorecard-demoted (rank 1),
        // which sinks below a plain healthy target (rank 0).
        let no_accounts = with_pool("p1", "m", 0);
        let scorecard_demoted = with_outcome("p2", "m", OutcomeTier::Demoted, 20, 0.3);
        let healthy = with_pool("p3", "m", 2);
        let plan = plan_route(
            &request,
            "default",
            &RouteRequire::default(),
            &[no_accounts, scorecard_demoted, healthy],
            &RoutingPolicy::default(),
        );
        let order: Vec<_> = plan.candidates.iter().map(|c| c.id.clone()).collect();
        assert_eq!(
            order,
            vec!["p3/m", "p2/m", "p1/m"],
            "healthy, then scorecard-demoted, then no-healthy-accounts"
        );
        assert_eq!(plan.decision.selected.unwrap().target_id, "p3/m");
        assert!(plan
            .decision
            .reason
            .iter()
            .any(|r| r.starts_with("outcome/demote target=p2/m")));
    }

    #[test]
    fn a_lone_scorecard_demoted_target_is_still_selected_as_last_resort() {
        // Mirrors an exact-model/"pinned" route that resolves to exactly one
        // candidate: a pin needs no special-case exemption code in the
        // demotion pass (mirroring today's healthy_accounts pass) because
        // there is nothing else in the survivor list to reorder against.
        let request = AiRequest::new("x", vec![Message::user("hi")]);
        let only = with_outcome("p1", "m", OutcomeTier::Demoted, 20, 0.2);
        let plan = plan_route(
            &request,
            "default:p1",
            &RouteRequire::default(),
            &[only],
            &RoutingPolicy::default(),
        );
        assert_eq!(plan.decision.selected.unwrap().target_id, "p1/m");
        assert_eq!(plan.candidates.len(), 1);
    }

    #[test]
    fn all_scorecard_demoted_still_selects_a_last_resort() {
        let request = AiRequest::new("x", vec![Message::user("hi")]);
        let plan = plan_route(
            &request,
            "default",
            &RouteRequire::default(),
            &[
                with_outcome("p1", "m", OutcomeTier::Demoted, 20, 0.1),
                with_outcome("p2", "m", OutcomeTier::Demoted, 20, 0.2),
            ],
            &RoutingPolicy::default(),
        );
        assert!(plan.decision.selected.is_some());
        assert_eq!(plan.candidates.len(), 2, "demotion reorders, never rejects");
    }

    #[test]
    fn outcome_tiebreak_preserves_declared_order_when_a_peer_is_unqualified() {
        let request = AiRequest::new("x", vec![Message::user("hi")]);
        // Declared order is worse-health-first; `b` has thin evidence (samples
        // below the default min_samples=8 gate), so the whole group must keep
        // its declared order rather than reordering on partial evidence.
        let a = with_outcome("p1", "m", OutcomeTier::Healthy, 20, 0.3);
        let b = with_outcome("p2", "m", OutcomeTier::Healthy, 2, 0.9);
        let plan = plan_route(
            &request,
            "default",
            &RouteRequire::default(),
            &[a, b],
            &RoutingPolicy::default(),
        );
        let order: Vec<_> = plan.candidates.iter().map(|c| c.id.clone()).collect();
        assert_eq!(
            order,
            vec!["p1/m", "p2/m"],
            "unqualified peer keeps the declared order untouched"
        );
        assert!(!plan
            .decision
            .reason
            .iter()
            .any(|r| r.starts_with("outcome/tiebreak")));
    }

    #[test]
    fn outcome_tiebreak_reorders_by_health_when_all_qualified() {
        let request = AiRequest::new("x", vec![Message::user("hi")]);
        // Declared order is worse-health-first; both are qualified (samples
        // >= min_samples=8), so the tie-break reorders by higher health.
        let a = with_outcome("p1", "m", OutcomeTier::Healthy, 20, 0.3);
        let b = with_outcome("p2", "m", OutcomeTier::Healthy, 20, 0.9);
        let plan = plan_route(
            &request,
            "default",
            &RouteRequire::default(),
            &[a, b],
            &RoutingPolicy::default(),
        );
        let order: Vec<_> = plan.candidates.iter().map(|c| c.id.clone()).collect();
        assert_eq!(
            order,
            vec!["p2/m", "p1/m"],
            "higher outcome health_factor wins the qualified tie-break"
        );
        assert!(plan
            .decision
            .reason
            .iter()
            .any(|r| r.starts_with("outcome/tiebreak")));
    }

    #[test]
    fn score_strategy_outcome_health_factor_moves_an_otherwise_tied_target() {
        let request = AiRequest::new("m", vec![Message::user("hi")]);
        let a = with_outcome("p1", "m", OutcomeTier::Healthy, 20, 0.2);
        let b = with_outcome("p2", "m", OutcomeTier::Healthy, 20, 0.9);
        // Zero every existing weight so `outcome_health` (weighted from
        // `ServerConfig.scorecard.score_weight`, not a `ScoringPolicy` field)
        // is the only factor that can move the ranking.
        let zero_weights = ScoringPolicy {
            selection_rank: 0.0,
            health: 0.0,
            account_availability: 0.0,
            cost: 0.0,
            latency: 0.0,
            ttft: 0.0,
            task_fit: 0.0,
            context_fit: 0.0,
        };
        let policy = RoutingPolicy {
            scoring: Some(zero_weights),
            ..Default::default()
        };
        let plan = plan_route(
            &request,
            "default",
            &RouteRequire::default(),
            &[a, b],
            &policy,
        );
        assert_eq!(plan.decision.strategy, "score");
        assert_eq!(
            plan.decision.selected.unwrap().target_id,
            "p2/m",
            "higher outcome_health wins when it is the only nonzero-weighted factor"
        );
    }

    #[test]
    fn outcome_evidence_emits_a_select_or_demote_reason_line() {
        let request = AiRequest::new("x", vec![Message::user("hi")]);
        let healthy = with_outcome("p1", "m", OutcomeTier::Healthy, 20, 0.95);
        let plan = plan_route(
            &request,
            "default",
            &RouteRequire::default(),
            &[healthy],
            &RoutingPolicy::default(),
        );
        assert!(plan
            .decision
            .reason
            .iter()
            .any(|r| r.starts_with("outcome/select") || r.starts_with("outcome/demote")));
    }

    #[test]
    fn outcome_absent_everywhere_is_byte_identical_to_pre_outcome_routing() {
        // No target carries scorecard evidence anywhere in this plan — the
        // decision must come out exactly as it did before outcome-routing-v1
        // (same reason lines, same order, same selection, AND the same
        // serialized score/factor breakdown -- F15: `outcome_health` must
        // not be added at all when no candidate has evidence).
        let request = AiRequest::new("x", vec![Message::user("hi")]);
        let a = ExecutionTarget::new("p1", "m", ExecutionTargetKind::ModelApi);
        let b = ExecutionTarget::new("p2", "m", ExecutionTargetKind::ModelApi);
        let plan = plan_route(
            &request,
            "default",
            &RouteRequire::default(),
            &[a, b],
            &RoutingPolicy::default(),
        );

        assert_eq!(
            plan.decision.reason,
            vec![
                "route=default".to_string(),
                "stream_required=false".to_string(),
                "tools_required=false".to_string(),
                "server_tools_required=false".to_string(),
                "vision_required=false".to_string(),
                "json_schema_required=false".to_string(),
            ]
        );
        let order: Vec<_> = plan.candidates.iter().map(|c| c.id.clone()).collect();
        assert_eq!(order, vec!["p1/m", "p2/m"]);
        assert_eq!(plan.decision.selected.unwrap().target_id, "p1/m");

        // Full score/factor comparison (F15), not just "no outcome_health
        // key": both candidates are plain/unstamped, so every factor is the
        // same fixed baseline regardless of the outcome-routing feature.
        fn factors(rank: f64) -> BTreeMap<String, f64> {
            [
                ("selection_rank".to_string(), rank),
                ("health".to_string(), 1.0),
                ("account_availability".to_string(), 0.5),
                ("cost".to_string(), 0.0),
                ("latency".to_string(), 0.0),
                ("ttft".to_string(), 0.0),
                ("task_fit".to_string(), 0.0),
                ("context_fit".to_string(), 0.0),
            ]
            .into_iter()
            .collect()
        }
        let expected = [
            RouteScore {
                target_id: "p1/m".to_string(),
                score: 1.0,
                factors: factors(1.0),
            },
            RouteScore {
                target_id: "p2/m".to_string(),
                score: 0.0,
                factors: factors(0.0),
            },
        ];
        assert_eq!(
            plan.decision.scores.len(),
            expected.len(),
            "score count unchanged"
        );
        for (actual, want) in plan.decision.scores.iter().zip(expected.iter()) {
            assert_eq!(actual.target_id, want.target_id);
            assert!((actual.score - want.score).abs() < 1e-9);
            assert_eq!(
                actual.factors, want.factors,
                "no outcome_health key, and every other factor byte-identical to pre-feature"
            );
        }
    }

    #[test]
    fn demote_rank_partition_runs_before_qualified_tiebreak_so_an_unqualified_demoted_peer_cannot_suppress_it(
    ) {
        // F10: declared order interleaves an unqualified DEMOTED peer (C)
        // between two qualified HEALTHY peers (A, B) so a pre-partition
        // single-pass tie-break (the old bug) would see [A, C, B] as one
        // "same_group" run, fail the qualification check because of C, and
        // never reorder A/B at all. Partitioning by demote_rank FIRST keeps
        // C entirely out of A/B's tie-break group.
        let request = AiRequest::new("x", vec![Message::user("hi")]);
        let a = with_outcome("a", "m", OutcomeTier::Healthy, 20, 0.3);
        let b = with_outcome("b", "m", OutcomeTier::Healthy, 20, 0.9); // better health
        let c = with_outcome("c", "m", OutcomeTier::Demoted, 2, 0.5); // unqualified (thin samples)
        let plan = plan_route(
            &request,
            "default",
            &RouteRequire::default(),
            &[a, c, b],
            &RoutingPolicy::default(),
        );
        let order: Vec<_> = plan.candidates.iter().map(|c| c.id.clone()).collect();
        assert_eq!(
            order,
            vec!["b/m", "a/m", "c/m"],
            "A/B still tie-break by health within rank 0; C (rank 1) never interferes"
        );
        assert_eq!(plan.decision.selected.unwrap().target_id, "b/m");
    }

    #[test]
    fn outcome_select_reason_names_the_target_actually_displaced_into_by_rank() {
        // F11: the score strategy's raw weighted winner (X) has NO healthy
        // accounts (rank 2), while Y is un-demoted (rank 0) with a lower raw
        // score. The demote-rank partition must still put Y first overall,
        // and every reason line describing "the selected target" (the score
        // summary line AND the final `outcome/select` line) must name Y, not
        // the pre-partition score winner X.
        let request = AiRequest::new("m", vec![Message::user("hi")]);
        let mut x = with_outcome("x", "m", OutcomeTier::Healthy, 20, 0.9);
        x.healthy_accounts = Some(0); // no healthy accounts -> rank 2
        let y = with_outcome("y", "m", OutcomeTier::Healthy, 20, 0.2);
        let zero_weights = ScoringPolicy {
            selection_rank: 0.0,
            health: 0.0,
            account_availability: 0.0,
            cost: 0.0,
            latency: 0.0,
            ttft: 0.0,
            task_fit: 0.0,
            context_fit: 0.0,
        };
        let policy = RoutingPolicy {
            scoring: Some(zero_weights),
            ..Default::default()
        };
        let plan = plan_route(
            &request,
            "default",
            &RouteRequire::default(),
            &[x, y],
            &policy,
        );

        assert_eq!(
            plan.decision.selected.as_ref().unwrap().target_id,
            "y/m",
            "the no-healthy-accounts target is demoted below the plain one"
        );
        let score_line = plan
            .decision
            .reason
            .iter()
            .find(|r| r.starts_with("score: selected="))
            .expect("score summary line present");
        assert!(
            score_line.contains("selected=y/m"),
            "score summary line must name the ACTUAL final winner, not the pre-partition raw-score winner: {score_line}"
        );
        let select_line = plan
            .decision
            .reason
            .iter()
            .find(|r| r.starts_with("outcome/select"))
            .expect("outcome/select line present");
        assert!(select_line.contains("target=y/m"));
    }

    #[test]
    fn demote_reason_line_reads_the_trigger_and_fails_count_directly_from_the_signal() {
        // F12: reason=<trigger> (and fails=N for a streak trigger) come
        // straight off OutcomeSignal.demote_trigger/consecutive_failures,
        // not a guess from aggregate stats.
        let request = AiRequest::new("x", vec![Message::user("hi")]);
        let mut streaky = ExecutionTarget::new("p1", "m", ExecutionTargetKind::ModelApi);
        streaky.outcome = Some(OutcomeSignal {
            samples: 3,
            success_rate: 0.0,
            p50_latency_ms: 0,
            p95_latency_ms: 0,
            cost_per_success_micros: 0,
            truncation_rate: 0.0,
            dominant_error: None,
            tier: OutcomeTier::Demoted,
            health_factor: 0.0,
            demote_trigger: Some(DemoteTrigger::Streak),
            consecutive_failures: 3,
        });
        let healthy = ExecutionTarget::new("p2", "m", ExecutionTargetKind::ModelApi);
        let plan = plan_route(
            &request,
            "default",
            &RouteRequire::default(),
            &[streaky, healthy],
            &RoutingPolicy::default(),
        );
        let line = plan
            .decision
            .reason
            .iter()
            .find(|r| r.starts_with("outcome/demote target=p1/m"))
            .expect("demote reason line present");
        assert!(line.contains("reason=streak"), "line={line}");
        assert!(line.contains("fails=3"), "line={line}");

        let mut truncated = ExecutionTarget::new("p3", "m", ExecutionTargetKind::ModelApi);
        truncated.outcome = Some(OutcomeSignal {
            samples: 16,
            success_rate: 0.88,
            p50_latency_ms: 0,
            p95_latency_ms: 0,
            cost_per_success_micros: 0,
            truncation_rate: 0.312,
            dominant_error: None,
            tier: OutcomeTier::Demoted,
            health_factor: 0.5,
            demote_trigger: Some(DemoteTrigger::Truncation),
            consecutive_failures: 0,
        });
        let plan2 = plan_route(
            &request,
            "default",
            &RouteRequire::default(),
            &[
                truncated,
                ExecutionTarget::new("p4", "m", ExecutionTargetKind::ModelApi),
            ],
            &RoutingPolicy::default(),
        );
        let line2 = plan2
            .decision
            .reason
            .iter()
            .find(|r| r.starts_with("outcome/demote target=p3/m"))
            .expect("demote reason line present");
        assert!(line2.contains("reason=truncation"), "line={line2}");
        assert!(!line2.contains("fails="), "line={line2}");
    }

    #[test]
    fn interactive_requests_rank_on_ttft_not_total_latency() {
        // p1: snappy first byte (50ms) but slow overall (2s). p2: slow first byte
        // (400ms) but quick overall (600ms).
        let mut p1 = ExecutionTarget::new("p1", "m", ExecutionTargetKind::ModelApi);
        p1.ttft_ewma_ms = Some(50.0);
        p1.latency_ewma_ms = Some(2000.0);
        let mut p2 = ExecutionTarget::new("p2", "m", ExecutionTargetKind::ModelApi);
        p2.ttft_ewma_ms = Some(400.0);
        p2.latency_ewma_ms = Some(600.0);
        let policy = RoutingPolicy {
            latency_aware: true,
            ..Default::default()
        };

        // Streaming → rank on TTFT → p1 (50ms first byte) wins.
        let mut streaming = AiRequest::new("m", vec![Message::user("hi")]);
        streaming.stream = true;
        let plan = plan_route(
            &streaming,
            "default",
            &RouteRequire::default(),
            &[p2.clone(), p1.clone()],
            &policy,
        );
        assert_eq!(plan.decision.selected.unwrap().target_id, "p1/m");
        assert!(plan.decision.reason.iter().any(|r| r.contains("ttft=50ms")));

        // Non-streaming → rank on total latency → p2 (600ms) wins.
        let nonstream = AiRequest::new("m", vec![Message::user("hi")]);
        let plan = plan_route(
            &nonstream,
            "default",
            &RouteRequire::default(),
            &[p2, p1],
            &policy,
        );
        assert_eq!(plan.decision.selected.unwrap().target_id, "p2/m");
        assert!(plan
            .decision
            .reason
            .iter()
            .any(|r| r.contains("latency=600ms")));
    }

    #[test]
    fn unverified_passthrough_is_flagged_in_the_decision() {
        let request = AiRequest::new("ghost/model", vec![Message::user("hi")]);
        let mut target =
            ExecutionTarget::new("openrouter", "ghost/model", ExecutionTargetKind::ModelApi);
        target.unverified = true;
        let plan = plan_route(
            &request,
            "default:openrouter",
            &RouteRequire::default(),
            &[target],
            &RoutingPolicy::default(),
        );
        assert!(plan.decision.unverified);
        assert!(plan
            .decision
            .reason
            .iter()
            .any(|r| r.contains("unverified passthrough")));
    }

    #[test]
    fn a_catalog_known_target_is_not_unverified() {
        let request = AiRequest::new("m", vec![Message::user("hi")]);
        let plan = plan_route(
            &request,
            "default",
            &RouteRequire::default(),
            &[ExecutionTarget::new(
                "p",
                "m",
                ExecutionTargetKind::ModelApi,
            )],
            &RoutingPolicy::default(),
        );
        assert!(!plan.decision.unverified);
    }

    #[test]
    fn unknown_pool_health_is_not_demoted() {
        let request = AiRequest::new("x", vec![Message::user("hi")]);
        // `None` = not stamped (unknown) — must keep declared order, not be demoted.
        let a = ExecutionTarget::new("p1", "m", ExecutionTargetKind::ModelApi);
        let b = ExecutionTarget::new("p2", "m", ExecutionTargetKind::ModelApi);
        let plan = plan_route(
            &request,
            "default",
            &RouteRequire::default(),
            &[a, b],
            &RoutingPolicy::default(),
        );
        assert_eq!(plan.decision.selected.unwrap().target_id, "p1/m");
        assert!(!plan
            .decision
            .reason
            .iter()
            .any(|r| r.contains("no healthy accounts")));
    }

    #[test]
    fn auto_coding_prefers_coding_tagged_targets() {
        let request = AiRequest::new("auto/coding", vec![Message::user("hi")]);
        let general = ExecutionTarget::new("openai", "general", ExecutionTargetKind::ModelApi);
        let mut coder = ExecutionTarget::new("anthropic", "sonnet", ExecutionTargetKind::ModelApi);
        coder.task_tags.push("coding".to_string());
        let policy = RoutingPolicy {
            profile: Some(ExecutionProfile::Coding),
            ..Default::default()
        };
        let plan = plan_route(
            &request,
            "auto/coding via default",
            &RouteRequire::default(),
            &[general, coder],
            &policy,
        );
        assert_eq!(
            plan.decision.selected.unwrap().target_id,
            "anthropic/sonnet"
        );
        assert_eq!(plan.decision.strategy, "auto/coding");
        assert_eq!(plan.decision.profile.as_deref(), Some("auto/coding"));
    }

    #[test]
    fn auto_private_enforces_lane_policy_without_cost_routing() {
        let request = AiRequest::new("auto/private", vec![Message::user("hi")]);
        let aggregator = tagged("openrouter", "m", 0.1, 0.2, &["aggregator"]);
        let direct = tagged("openai", "m", 5.0, 20.0, &[]);
        let policy = RoutingPolicy {
            profile: Some(ExecutionProfile::Private),
            allow_aggregator: false,
            enforce_lane_policy: true,
            ..Default::default()
        };
        let plan = plan_route(
            &request,
            "auto/private via default",
            &RouteRequire::default(),
            &[aggregator, direct],
            &policy,
        );
        assert_eq!(plan.decision.selected.unwrap().target_id, "openai/m");
        assert!(plan
            .decision
            .rejected
            .iter()
            .any(|r| r.target_id == "openrouter/m" && r.reason.contains("aggregator")));
    }

    #[test]
    fn auto_large_context_orders_widest_context_first() {
        let request = AiRequest::new("auto/large-context", vec![Message::user("hi")]);
        let mut small = ExecutionTarget::new("p1", "small", ExecutionTargetKind::ModelApi);
        small.capabilities.max_context_tokens = Some(32_000);
        let mut large = ExecutionTarget::new("p2", "large", ExecutionTargetKind::ModelApi);
        large.capabilities.max_context_tokens = Some(200_000);
        let policy = RoutingPolicy {
            profile: Some(ExecutionProfile::LargeContext),
            ..Default::default()
        };
        let plan = plan_route(
            &request,
            "auto/large-context via default",
            &RouteRequire::default(),
            &[small, large],
            &policy,
        );
        assert_eq!(plan.decision.selected.unwrap().target_id, "p2/large");
        assert!(plan
            .decision
            .reason
            .iter()
            .any(|r| r.contains("context=200000")));
    }

    #[test]
    fn auto_judge_emits_profile_and_strategy() {
        let request = AiRequest::new("auto/judge", vec![Message::user("hi")]);
        let deepseek = priced("deepseek", "v4-pro", 1.74, 3.48);
        let free = tagged("openrouter", "free", 0.0, 0.0, &["free", "aggregator"]);
        let policy = RoutingPolicy {
            profile: Some(ExecutionProfile::Judge),
            scoring: Some(ScoringPolicy::judge()),
            ..Default::default()
        };

        let plan = plan_route(
            &request,
            "auto/judge via default",
            &RouteRequire::default(),
            &[deepseek, free],
            &policy,
        );

        assert_eq!(plan.decision.strategy, "auto/judge");
        assert_eq!(plan.decision.profile.as_deref(), Some("auto/judge"));
    }

    #[test]
    fn auto_extract_emits_profile_and_strategy() {
        let request = AiRequest::new("auto/extract", vec![Message::user("hi")]);
        let free = tagged("openrouter", "free", 0.0, 0.0, &["free", "aggregator"]);
        let paid = priced("deepseek", "v4-flash", 0.14, 0.28);
        let policy = RoutingPolicy {
            profile: Some(ExecutionProfile::Extract),
            scoring: Some(ScoringPolicy::extract()),
            ..Default::default()
        };

        let plan = plan_route(
            &request,
            "auto/extract via default",
            &RouteRequire::default(),
            &[free, paid],
            &policy,
        );

        assert_eq!(plan.decision.strategy, "auto/extract");
        assert_eq!(plan.decision.profile.as_deref(), Some("auto/extract"));
    }
}
