use sb_core::{
    AiRequest, CacheKey, CacheLayer, CacheLookupReceipt, CachePolicy, CacheStatus, EvaluationEvent,
    EvaluationEventKind, ExecutionJob, ExecutionReceipt, EXECUTION_POLICY_VERSION,
};

use crate::Engine;

pub(crate) fn lookup_exact_cache(engine: &Engine, req: &AiRequest) -> CacheLookupReceipt {
    let policy = CachePolicy::exact_request();
    let Ok(mut cache) = engine.exact_cache.lock() else {
        return CacheLookupReceipt {
            layer: CacheLayer::ExactRequest,
            status: CacheStatus::Bypass,
            key: None,
            reason: Some("cache_lock_poisoned".to_string()),
            policy_version: policy.version,
        };
    };
    let receipt = CacheLookupReceipt::for_request(req, &policy, &cache);
    if receipt.status == CacheStatus::Miss {
        cache.remember(CacheKey::exact_request(req));
    }
    receipt
}

pub(crate) fn preview_cache_receipt(req: &AiRequest) -> CacheLookupReceipt {
    CacheLookupReceipt::for_request(
        req,
        &CachePolicy::exact_request(),
        &sb_core::ExactRequestCache::new(),
    )
}

pub(crate) fn attach_execution_receipt(
    plan: &mut sb_router::RoutePlan,
    req: &AiRequest,
    cache: CacheLookupReceipt,
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
        job: ExecutionJob::from_request(req),
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
