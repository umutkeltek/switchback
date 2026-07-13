use sb_core::{
    AiRequest, CacheKey, CacheLayer, CacheLookupReceipt, CachePolicy, CacheStatus, ContentPart,
    EvaluationEvent, EvaluationEventKind, ExecutionJob, ExecutionReceipt, ExecutionSource,
    ExecutionTaskType, HarnessDescriptor, InputSize, LatencyPreference, RequestedCapabilities,
    EXECUTION_POLICY_VERSION,
};

use crate::Engine;

pub(crate) fn lookup_exact_cache(
    engine: &Engine,
    req: &AiRequest,
    policy: &CachePolicy,
    body_redacted: bool,
) -> CacheLookupReceipt {
    if body_redacted {
        return body_redacted_cache_bypass(policy);
    }
    let now_unix = sb_trace::now_unix();
    let Ok(mut cache) = engine.exact_cache.lock() else {
        return CacheLookupReceipt {
            layer: CacheLayer::ExactRequest,
            status: CacheStatus::Bypass,
            key: None,
            reason: Some("cache_lock_poisoned".to_string()),
            policy_version: policy.version.clone(),
            ttl_seconds: policy.ttl_seconds,
        };
    };
    let receipt = CacheLookupReceipt::for_request(req, policy, &cache, now_unix);
    if receipt.status == CacheStatus::Miss {
        cache.remember_at(CacheKey::exact_request(req), now_unix);
    }
    receipt
}

pub(crate) fn preview_cache_receipt(req: &AiRequest, policy: &CachePolicy) -> CacheLookupReceipt {
    CacheLookupReceipt::for_request(
        req,
        policy,
        &sb_core::ExactRequestCache::new(),
        sb_trace::now_unix(),
    )
}

fn body_redacted_cache_bypass(policy: &CachePolicy) -> CacheLookupReceipt {
    CacheLookupReceipt {
        layer: policy.layer,
        status: CacheStatus::Bypass,
        key: None,
        reason: Some("scoped_body_redacted".to_string()),
        policy_version: policy.version.clone(),
        ttl_seconds: policy.ttl_seconds,
    }
}

fn execution_job_for_receipt(req: &AiRequest, body_redacted: bool) -> ExecutionJob {
    if !body_redacted {
        return ExecutionJob::from_request(req);
    }
    let input_chars = req
        .system
        .as_ref()
        .map(String::len)
        .unwrap_or(0)
        .saturating_add(
            req.messages
                .iter()
                .flat_map(|message| &message.content)
                .map(|part| match part {
                    ContentPart::Text { text } => text.len(),
                    _ => 0,
                })
                .sum(),
        );
    ExecutionJob {
        job_id: req.id.clone(),
        task_type: ExecutionTaskType::infer(req),
        source: ExecutionSource::infer(req),
        privacy_level: req.privacy_class,
        latency_preference: LatencyPreference::infer(req),
        cost_ceiling_micros: req
            .metadata
            .get("cost_ceiling_micros")
            .and_then(|value| value.parse::<u64>().ok()),
        context_fingerprint: "redacted:quality_eval".to_string(),
        requested_capabilities: RequestedCapabilities::from_request(req),
        input_size: InputSize {
            message_count: req.messages.len(),
            tool_count: req.tools.len(),
            approx_input_chars: input_chars,
        },
        tenant: req.tenant.clone(),
        project: req.project.clone(),
        workspace_id: req.metadata.get("workspace_id").cloned(),
    }
}

pub(crate) fn harness_candidates_for_task(
    config: &sb_core::Config,
    task_type: ExecutionTaskType,
) -> Vec<HarnessDescriptor> {
    config
        .harnesses
        .iter()
        .filter(|harness| {
            harness.supported_task_types.is_empty()
                || harness.supported_task_types.contains(&task_type)
        })
        .cloned()
        .collect()
}

pub(crate) fn harness_candidates_for_plan(
    config: &sb_core::Config,
    plan: &sb_router::RoutePlan,
) -> Vec<HarnessDescriptor> {
    let task_type = plan
        .decision
        .receipt
        .as_ref()
        .map(|receipt| receipt.job.task_type)
        .unwrap_or(ExecutionTaskType::Unknown);
    harness_candidates_for_task(config, task_type)
}

pub(crate) fn attach_execution_receipt(
    plan: &mut sb_router::RoutePlan,
    req: &AiRequest,
    cache: CacheLookupReceipt,
    body_redacted: bool,
) {
    let selected_route = plan
        .decision
        .selected
        .as_ref()
        .map(|target| target.target_id.clone());
    let estimated_latency_ms = selected_route.as_ref().and_then(|selected| {
        plan.candidates
            .iter()
            .find(|target| &target.id == selected)
            .and_then(|target| {
                if req.stream {
                    target.ttft_ewma_ms.or(target.latency_ewma_ms)
                } else {
                    target.latency_ewma_ms
                }
            })
    });
    plan.decision.receipt = Some(ExecutionReceipt {
        policy_version: EXECUTION_POLICY_VERSION.to_string(),
        job: execution_job_for_receipt(req, body_redacted),
        candidates: plan
            .candidates
            .iter()
            .map(|target| target.id.clone())
            .collect(),
        selected_route,
        fallback_path: plan
            .decision
            .fallbacks
            .iter()
            .map(|target| target.target_id.clone())
            .collect(),
        reasons: plan.decision.reason.clone(),
        estimated_cost_micros: None,
        estimated_latency_ms,
        cache,
    });
}

pub(crate) fn route_selected_event(decision: &sb_core::RouteDecision) -> EvaluationEvent {
    let mut event = EvaluationEvent::new(EvaluationEventKind::RouteSelected);
    event.target_id = decision
        .selected
        .as_ref()
        .map(|target| target.target_id.clone());
    event
        .metadata
        .insert("strategy".to_string(), decision.strategy.clone());
    event.metadata.insert(
        "fallback_count".to_string(),
        decision.fallbacks.len().to_string(),
    );
    event.metadata.insert(
        "rejected_count".to_string(),
        decision.rejected.len().to_string(),
    );
    event
}

#[cfg(test)]
mod tests {
    use sb_core::{CacheStatus, Message, PrivacyClass};

    use super::*;

    #[test]
    fn body_redacted_receipt_never_derives_a_cache_key_or_fingerprint() {
        let mut request = AiRequest::new("auto/judge", vec![Message::user("sampled-body-canary")]);
        request.privacy_class = PrivacyClass::Confidential;
        let policy = CachePolicy {
            allow_confidential: true,
            ..CachePolicy::default()
        };

        let cache = body_redacted_cache_bypass(&policy);
        assert_eq!(cache.status, CacheStatus::Bypass);
        assert!(cache.key.is_none());
        assert_eq!(cache.reason.as_deref(), Some("scoped_body_redacted"));
        assert_eq!(
            execution_job_for_receipt(&request, true).context_fingerprint,
            "redacted:quality_eval"
        );
        assert_ne!(
            execution_job_for_receipt(&request, false).context_fingerprint,
            "redacted:quality_eval"
        );
    }
}
