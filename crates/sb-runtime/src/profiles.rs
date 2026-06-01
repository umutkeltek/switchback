use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use sb_core::{
    AiRequest, ComboStrategy, ExecutionProfile, ExecutionTarget, RouteRequire, ScoringPolicy,
    TenantConfig,
};

use crate::{ExecError, Snapshot};

pub(crate) fn routing_policy(
    snap: &Snapshot,
    profile: Option<ExecutionProfile>,
) -> sb_core::RoutingPolicy {
    let mut policy = sb_core::RoutingPolicy {
        profile,
        scoring: None,
        cost_aware: snap.runtime.cost_aware,
        max_price_per_mtok: snap.config.server.cost_max_per_mtok,
        latency_aware: snap.runtime.latency_aware,
        allow_free: snap.config.server.cost_allow_free,
        allow_promo: snap.config.server.cost_allow_promo,
        allow_aggregator: snap.config.server.cost_allow_aggregator,
        enforce_lane_policy: false,
        unknown_cost: snap.config.server.cost_unknown,
        unknown_context: snap.config.server.context_unknown,
    };

    match profile {
        Some(ExecutionProfile::Auto) => {
            policy.scoring = Some(ScoringPolicy::balanced());
        }
        Some(ExecutionProfile::Cheap) => {
            policy.scoring = Some(ScoringPolicy::cheap());
            policy.cost_aware = true;
            policy.latency_aware = false;
        }
        Some(ExecutionProfile::Fast) => {
            policy.scoring = Some(ScoringPolicy::fast());
            policy.cost_aware = false;
            policy.latency_aware = true;
        }
        Some(ExecutionProfile::Coding) => {
            policy.scoring = Some(ScoringPolicy::coding());
        }
        Some(ExecutionProfile::Private) => {
            policy.allow_free = false;
            policy.allow_promo = false;
            policy.allow_aggregator = false;
            policy.enforce_lane_policy = true;
        }
        Some(ExecutionProfile::LargeContext) => {
            policy.scoring = Some(ScoringPolicy::large_context());
        }
        None => {}
    }

    policy
}

#[derive(Debug, Clone)]
pub(crate) struct CandidateResolution {
    pub(crate) route_name: String,
    pub(crate) require: RouteRequire,
    pub(crate) candidates: Vec<sb_core::ExecutionTarget>,
    pub(crate) unknown: Vec<String>,
    pub(crate) profile: Option<ExecutionProfile>,
    pub(crate) combo: Option<ResolvedCombo>,
}

#[derive(Debug, Clone)]
pub(crate) struct ResolvedCombo {
    pub(crate) name: String,
    pub(crate) strategy: ComboStrategy,
}

pub(crate) fn apply_combo_order(
    combo_rr: &Mutex<HashMap<String, usize>>,
    resolved: &mut CandidateResolution,
    advance_cursor: bool,
) {
    let Some(combo) = &resolved.combo else {
        return;
    };
    match combo.strategy {
        ComboStrategy::Fallback => {}
        ComboStrategy::RoundRobin => {
            let len = resolved.candidates.len();
            if len <= 1 {
                return;
            }
            let mut cursors = combo_rr.lock().expect("combo rr mutex");
            let cursor = cursors.entry(combo.name.clone()).or_default();
            let offset = *cursor % len;
            if advance_cursor {
                *cursor = cursor.wrapping_add(1);
            }
            resolved.candidates.rotate_left(offset);
        }
    }
}

/// Resolve a model to ordered candidate targets — the routing front-half shared
/// by `execute` and `preview_route`. Precedence: execution profile route →
/// exact route → combo profile → explicit `provider/model` → wildcard route →
/// default pass-through provider → 404. Each candidate is stamped with its
/// non-secret account-pool health so the router can demote locked pools.
pub(crate) fn resolve_candidates(
    snap: &Snapshot,
    model: &str,
) -> Result<CandidateResolution, ExecError> {
    let profile = ExecutionProfile::from_model(model);
    let (route_name, require, mut candidates, unknown, combo): (
        String,
        RouteRequire,
        Vec<sb_core::ExecutionTarget>,
        Vec<String>,
        Option<ResolvedCombo>,
    ) = if let Some(profile) = profile {
        let route = snap
            .config
            .exact_route_for(model)
            .or_else(|| snap.config.wildcard_route())
            .ok_or_else(|| {
                ExecError::new(
                    404,
                    "invalid_request_error",
                    format!(
                        "execution profile `{}` needs a matching route or catch-all `*` route",
                        profile.id()
                    ),
                    None,
                )
            })?;
        let mut candidates = Vec::new();
        let mut unknown = Vec::new();
        for target_id in &route.targets {
            match snap.registry.target_for(target_id) {
                Some(target) => candidates.push(target),
                None => unknown.push(target_id.clone()),
            }
        }
        let route_name = if route.match_.model.as_deref() == Some(model) {
            route.name.clone()
        } else {
            format!("{} via {}", profile.id(), route.name)
        };
        (route_name, route.require.clone(), candidates, unknown, None)
    } else if let Some(route) = snap.config.exact_route_for(model) {
        let mut candidates = Vec::new();
        let mut unknown = Vec::new();
        for target_id in &route.targets {
            match snap.registry.target_for(target_id) {
                Some(target) => candidates.push(target),
                None => unknown.push(target_id.clone()),
            }
        }
        (
            route.name.clone(),
            route.require.clone(),
            candidates,
            unknown,
            None,
        )
    } else if let Some(combo_cfg) = snap.config.combo_for(model) {
        let mut candidates = Vec::new();
        let mut unknown = Vec::new();
        for target_id in &combo_cfg.models {
            match snap.registry.target_for(target_id) {
                Some(target) => candidates.push(target),
                None => unknown.push(target_id.clone()),
            }
        }
        (
            format!("combo/{model}"),
            combo_cfg.require.clone(),
            candidates,
            unknown,
            Some(ResolvedCombo {
                name: model.to_string(),
                strategy: combo_cfg.strategy,
            }),
        )
    } else if let Some(target) = snap.registry.target_for(model) {
        (
            "direct".to_string(),
            RouteRequire::default(),
            vec![target],
            Vec::new(),
            None,
        )
    } else if let Some(route) = snap.config.wildcard_route() {
        let mut candidates = Vec::new();
        let mut unknown = Vec::new();
        for target_id in &route.targets {
            match snap.registry.target_for(target_id) {
                Some(target) => candidates.push(target),
                None => unknown.push(target_id.clone()),
            }
        }
        (
            route.name.clone(),
            route.require.clone(),
            candidates,
            unknown,
            None,
        )
    } else if let Some(provider) = snap.config.server.default_provider.as_deref() {
        match snap.registry.target_for_provider_model(provider, model) {
            Some(mut target) => {
                // Unknown-model pass-through: forwarded verbatim, so its
                // capabilities + price are NOT catalog-verified (Oracle #5).
                target.unverified = true;
                (
                    format!("default:{provider}"),
                    RouteRequire::default(),
                    vec![target],
                    Vec::new(),
                    None,
                )
            }
            None => {
                return Err(ExecError::new(
                    404,
                    "invalid_request_error",
                    format!("default_provider `{provider}` is not a configured provider"),
                    None,
                ));
            }
        }
    } else {
        return Err(ExecError::new(
            404,
            "invalid_request_error",
            format!(
                "no route or target for model `{model}` — add a route, use `provider/model`, or set server.default_provider"
            ),
            None,
        ));
    };

    for candidate in candidates.iter_mut() {
        let ph = snap
            .resolver
            .pool_health(&candidate.provider_id, &candidate.model);
        candidate.healthy_accounts = Some(if ph.circuit_open { 0 } else { ph.healthy });
    }
    Ok(CandidateResolution {
        route_name,
        require,
        candidates,
        unknown,
        profile,
        combo,
    })
}

pub(crate) fn plan_resolved_route(
    combo_rr: &Mutex<HashMap<String, usize>>,
    snap: &Snapshot,
    req: &AiRequest,
    mut resolved: CandidateResolution,
    advance_combo_cursor: bool,
) -> Result<(String, sb_router::RoutePlan), ExecError> {
    apply_combo_order(combo_rr, &mut resolved, advance_combo_cursor);
    let route_name = resolved.route_name.clone();
    let tenant = req
        .tenant
        .as_deref()
        .and_then(|tenant_id| snap.config.tenant(tenant_id));
    if let Some(tenant) = tenant {
        if !tenant_route_allowed(tenant, &route_name) {
            return Err(ExecError::new(
                403,
                "tenant_policy_denied",
                format!(
                    "tenant `{}` is not allowed to use route `{route_name}`",
                    tenant.id
                ),
                None,
            ));
        }
    }
    let mut tenant_rejections = Vec::new();
    if let Some(tenant) = tenant {
        resolved.candidates.retain(|candidate| {
            if !tenant_provider_allowed(tenant, &candidate.provider_id) {
                tenant_rejections.push((
                    candidate.id.clone(),
                    format!(
                        "tenant policy: provider `{}` is not allowed",
                        candidate.provider_id
                    ),
                ));
                return false;
            }
            if !tenant_candidate_has_account(snap, tenant, candidate) {
                tenant_rejections.push((
                    candidate.id.clone(),
                    "tenant policy: no allowed account for provider".to_string(),
                ));
                return false;
            }
            true
        });
    }
    let policy = routing_policy(snap, resolved.profile);
    let mut plan = sb_router::plan_route(
        req,
        &resolved.route_name,
        &resolved.require,
        &resolved.candidates,
        &policy,
    );
    if let Some(combo) = &resolved.combo {
        plan.decision.strategy = match combo.strategy {
            ComboStrategy::Fallback => "combo_fallback",
            ComboStrategy::RoundRobin => "combo_round_robin",
        }
        .to_string();
        plan.decision.add_reason(format!("combo={}", combo.name));
        plan.decision
            .add_reason(format!("combo_strategy={}", combo.strategy.as_str()));
    }
    if let Some(tenant) = tenant {
        plan.decision.add_reason(format!("tenant={}", tenant.id));
    }
    for (target_id, reason) in tenant_rejections {
        plan.decision.reject(target_id, reason);
    }
    Ok((route_name, plan))
}

fn tenant_route_allowed(tenant: &TenantConfig, route_name: &str) -> bool {
    if tenant.allowed_routes.is_empty() {
        return true;
    }
    tenant.allowed_routes.iter().any(|allowed| {
        allowed == route_name
            || route_name == format!("combo/{allowed}")
            || route_name.ends_with(&format!(" via {allowed}"))
    })
}

fn tenant_provider_allowed(tenant: &TenantConfig, provider_id: &str) -> bool {
    tenant.allowed_providers.is_empty()
        || tenant
            .allowed_providers
            .iter()
            .any(|allowed| allowed == provider_id)
}

fn tenant_candidate_has_account(
    snap: &Snapshot,
    tenant: &TenantConfig,
    candidate: &ExecutionTarget,
) -> bool {
    if tenant.allowed_accounts.is_empty() {
        return true;
    }
    let allowed = tenant_allowed_accounts(tenant, &candidate.provider_id);
    if allowed.is_empty() {
        return false;
    }
    let configured = snap.resolver.account_ids(&candidate.provider_id);
    configured
        .iter()
        .any(|account_id| allowed.contains(account_id))
}

pub(crate) fn tenant_allowed_accounts(tenant: &TenantConfig, provider_id: &str) -> HashSet<String> {
    tenant
        .allowed_accounts
        .iter()
        .filter_map(|account_ref| {
            let (provider, account) = account_ref.split_once('/')?;
            (provider == provider_id).then(|| account.to_string())
        })
        .collect()
}
