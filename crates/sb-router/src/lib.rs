//! The router: resolve a request's `model`/route into an ordered candidate
//! list (primary + fallbacks), hard-filtering on capabilities and policy,
//! and emitting an explainable `RouteDecision`. Deterministic in v1.

use std::cmp::Ordering;

use sb_core::{
    AiRequest, ExecutionTarget, HealthState, RouteDecision, RouteRequire, RoutingPolicy, TargetRef,
};

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

pub fn plan_route(
    req: &AiRequest,
    route_name: &str,
    require: &RouteRequire,
    candidates: &[ExecutionTarget],
    policy: &RoutingPolicy,
) -> RoutePlan {
    let strategy = if policy.cost_aware {
        "cost_aware"
    } else if policy.latency_aware {
        "latency_aware"
    } else {
        "ordered_fallback"
    };
    let mut decision = RouteDecision::new(req.id.clone(), strategy);
    let streaming_required = require.streaming == Some(true) || req.stream;
    let tools_required = require.tool_calling == Some(true) || req.requires_tools();
    let json_schema_required = require.json_schema == Some(true)
        || matches!(
            req.response_format,
            Some(sb_core::ResponseFormat::JsonSchema { .. })
        );

    decision.add_reason(format!("route={route_name}"));
    decision.add_reason(format!("stream_required={streaming_required}"));
    decision.add_reason(format!("tools_required={tools_required}"));
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
            }
        }

        if candidate.health == HealthState::Down {
            decision.reject(candidate.id.clone(), "target health is down");
            continue;
        }

        // Cost-routing gates (cost-aware only): exclude disallowed lanes
        // (free/promo/aggregator) and reject priced candidates over the ceiling.
        if policy.cost_aware {
            if let Some(blocked) = blocked_lane(candidate, policy) {
                decision.reject(
                    candidate.id.clone(),
                    format!("policy: `{blocked}` lane not allowed"),
                );
                continue;
            }
            if let (Some(max), Some(cost)) = (policy.max_price_per_mtok, &candidate.cost) {
                let blended = cost.blended_per_mtok();
                if blended > max {
                    decision.reject(
                        candidate.id.clone(),
                        format!("blended price {blended:.2}/Mtok > max {max:.2}/Mtok"),
                    );
                    continue;
                }
            }
        }

        survivors.push(candidate.clone());
    }

    // Cost-aware: re-order survivors cheapest-first by blended price. Stable, so
    // declared order breaks ties; unpriced candidates keep their relative order
    // and sort after all priced ones (unknown cost is treated as "not cheaper").
    if policy.cost_aware {
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
    use sb_core::{CapabilityProfile, ExecutionTargetKind, Message};

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
}
