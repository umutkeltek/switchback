//! The router: resolve a request's `model`/route into an ordered candidate
//! list (primary + fallbacks), hard-filtering on capabilities and policy,
//! and emitting an explainable `RouteDecision`. Deterministic in v1.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use sb_core::{
    AiRequest, ExecutionProfile, ExecutionTarget, HealthState, RouteDecision, RouteRequire,
    RouteScore, RoutingPolicy, ScoringPolicy, TargetRef, UnknownContextPolicy, UnknownCostPolicy,
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
            let score = policy
                .scoring
                .map(|scoring| weighted_score(&factors, scoring))
                .unwrap_or(rank);

            RouteScore {
                target_id: target.id.clone(),
                score,
                factors,
            }
        })
        .collect()
}

fn weighted_score(factors: &BTreeMap<String, f64>, scoring: ScoringPolicy) -> f64 {
    let mut weighted = 0.0;
    let mut total = 0.0;
    for (factor, value) in factors {
        let weight = scoring.weight_for(factor);
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

    if policy.scoring.is_some() {
        let scores = route_scores(&survivors, policy, streaming_required);
        let score_by_target = scores
            .iter()
            .map(|score| (score.target_id.as_str(), score.score))
            .collect::<BTreeMap<_, _>>();
        survivors.sort_by(|a, b| {
            score_by_target
                .get(b.id.as_str())
                .copied()
                .unwrap_or(0.0)
                .partial_cmp(&score_by_target.get(a.id.as_str()).copied().unwrap_or(0.0))
                .unwrap_or(Ordering::Equal)
        });
        if let Some(selected) = survivors.first() {
            let score = score_by_target
                .get(selected.id.as_str())
                .copied()
                .unwrap_or(0.0);
            decision.add_reason(format!("score: selected={} score={score:.3}", selected.id));
        }
    // Cost-aware: re-order survivors cheapest-first by blended price. Stable, so
    // declared order breaks ties; unpriced candidates keep their relative order
    // and sort after all priced ones (unknown cost is treated as "not cheaper").
    } else if policy.profile == Some(ExecutionProfile::Coding) {
        survivors.sort_by_key(|target| u8::from(!is_coding_target(target)));
        if let Some(selected) = survivors.first() {
            let fit = if is_coding_target(selected) {
                "coding"
            } else {
                "unclassified"
            };
            decision.add_reason(format!("auto/coding: selected={} fit={fit}", selected.id));
        }
    } else if policy.profile == Some(ExecutionProfile::LargeContext) {
        survivors.sort_by(|a, b| {
            b.capabilities
                .max_context_tokens
                .unwrap_or(0)
                .cmp(&a.capabilities.max_context_tokens.unwrap_or(0))
        });
        if let Some(selected) = survivors.first() {
            let context = selected
                .capabilities
                .max_context_tokens
                .map(|tokens| tokens.to_string())
                .unwrap_or_else(|| "unknown".to_string());
            decision.add_reason(format!(
                "auto/large-context: widest={} context={context}",
                selected.id
            ));
        }
    } else if policy.cost_aware {
        survivors.sort_by(|a, b| {
            match (
                a.cost.map(|c| c.blended_per_mtok()),
                b.cost.map(|c| c.blended_per_mtok()),
            ) {
                (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(Ordering::Equal),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => Ordering::Equal,
            }
        });
        if let Some(selected) = survivors.first() {
            let price = selected
                .cost
                .map(|c| format!("{:.2}/Mtok", c.blended_per_mtok()))
                .unwrap_or_else(|| "unpriced".to_string());
            decision.add_reason(format!(
                "cost_aware: cheapest={} blended={price}",
                selected.id
            ));
        }
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
        survivors.sort_by(|a, b| match (signal(a), signal(b)) {
            (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(Ordering::Equal),
            (None, Some(_)) => Ordering::Less,
            (Some(_), None) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        });
        if let Some(selected) = survivors.first() {
            let metric = if interactive { "ttft" } else { "latency" };
            let val = signal(selected)
                .map(|ms| format!("{ms:.0}ms"))
                .unwrap_or_else(|| "unmeasured".to_string());
            decision.add_reason(format!(
                "latency_aware: fastest={} {metric}={val}",
                selected.id
            ));
        }
    }

    // Account-pool health (Oracle #3): a target whose pool has NO currently-usable
    // account (`healthy_accounts == Some(0)` — all locked, or circuit open) is
    // demoted below targets that can actually execute, so routing stops ranking
    // them as equally executable. Stable, so the strategy order above is preserved
    // within each group. Demotion (not rejection) keeps them as a last resort, so
    // a lock that expires by attempt time still works and we never fail a request
    // that the credential layer could have served.
    let degraded = survivors
        .iter()
        .filter(|c| c.healthy_accounts == Some(0))
        .count();
    if degraded > 0 {
        survivors.sort_by_key(|c| u8::from(c.healthy_accounts == Some(0)));
        decision.add_reason(format!(
            "demoted {degraded} target(s) with no healthy accounts"
        ));
    }

    decision.scores = route_scores(&survivors, policy, streaming_required);

    if let Some(selected) = survivors.first() {
        decision.selected = Some(TargetRef::new(selected.id.clone()));
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
        CapabilityProfile, ContentPart, ExecutionTargetKind, ImageSourceKind, Message, Role,
        ScoringPolicy,
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
