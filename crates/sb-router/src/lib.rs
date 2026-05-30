//! The router: resolve a request's `model`/route into an ordered candidate
//! list (primary + fallbacks), hard-filtering on capabilities and policy,
//! and emitting an explainable `RouteDecision`. Deterministic in v1.

use sb_core::{AiRequest, ExecutionTarget, HealthState, RouteDecision, RouteRequire, TargetRef};

pub struct RoutePlan {
    pub candidates: Vec<ExecutionTarget>,
    pub decision: RouteDecision,
}

pub fn plan_route(
    req: &AiRequest,
    route_name: &str,
    require: &RouteRequire,
    candidates: &[ExecutionTarget],
) -> RoutePlan {
    let mut decision = RouteDecision::new(req.id.clone(), "ordered_fallback");
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

        survivors.push(candidate.clone());
    }

    if let Some(selected) = survivors.first() {
        decision.selected = Some(TargetRef::new(selected.id.clone()));
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
        );
        assert_eq!(plan.decision.selected.unwrap().target_id, "openai/o");
        assert!(plan
            .decision
            .rejected
            .iter()
            .any(|rejected| rejected.target_id == "gemini/g"));
    }
}
