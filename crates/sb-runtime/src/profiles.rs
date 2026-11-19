use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use sb_core::{
    AiRequest, ClientProfileConfig, ComboStrategy, ExecutionProfile, ExecutionTarget,
    PreferAwakeConfig, RouteRequire, ScoringPolicy, TenantConfig,
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
        vision_degrade: snap.config.server.vision_degrade,
        unknown_cost: snap.config.server.cost_unknown,
        unknown_context: snap.config.server.context_unknown,
        scorecard: snap.config.server.scorecard.clone(),
        quality_eval: snap.config.server.quality_eval.clone(),
        pin_order: false,
        wake: None,
        rank_on_decode_speed: false,
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
        Some(ExecutionProfile::Judge) => {
            policy.scoring = Some(ScoringPolicy::judge());
        }
        Some(ExecutionProfile::Extract) => {
            policy.scoring = Some(ScoringPolicy::extract());
            policy.cost_aware = true;
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
    /// The resolved route pins its declared order (`RouteConfig.pin_order`).
    pub(crate) pin_order: bool,
    /// `(target_id, reason)` for candidates whose managed local lane is known
    /// to be down. [`plan_resolved_route`] skips them (recorded on the
    /// decision's `rejected`) unless no other candidate can serve.
    pub(crate) lane_down: Vec<(String, String)>,
    /// Wake-aware ordering in force for this route (`prefer_awake` with
    /// `enabled: true`), else `None`. When set, candidates on a lane that
    /// wakes on request carry [`ExecutionTarget::wake`].
    pub(crate) prefer_awake: Option<PreferAwakeConfig>,
    /// The per-request speed tricks' view of the candidates (stickiness,
    /// loaded models, prompt speeds), set by the runtime after resolving
    /// when the route has any trick on; every plan of the request applies
    /// it the same way.
    pub(crate) optimize: Option<crate::optimize::Hints>,
}

/// What a model resolved to before candidates are stamped: route name,
/// requirements, candidates, unknown target ids, combo, `pin_order`, and the
/// route's own `prefer_awake`.
type ResolvedRoute = (
    String,
    RouteRequire,
    Vec<sb_core::ExecutionTarget>,
    Vec<String>,
    Option<ResolvedCombo>,
    bool,
    Option<sb_core::PreferAwakeOverride>,
);

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

pub(crate) fn apply_request_client_profile(
    snap: &Snapshot,
    req: &mut AiRequest,
) -> Result<Option<ClientProfileConfig>, ExecError> {
    let requested = req.metadata.get("client_profile").cloned();
    let source = req
        .metadata
        .get("client_profile_source")
        .map(String::as_str);
    let configured = requested.as_deref().and_then(|id| {
        snap.config
            .client_profiles
            .iter()
            .find(|profile| profile.id == id)
    });
    let inferred = if configured.is_none() && source != Some("header") {
        infer_client_profile_for_model(snap, req)?
    } else {
        None
    };
    let Some(profile) = configured.or(inferred) else {
        if source == Some("header") {
            let id = requested.unwrap_or_default();
            return Err(ExecError::new(
                422,
                "invalid_request_error",
                format!("client profile `{id}` is not configured"),
                None,
            ));
        }
        return Ok(None);
    };

    if !profile.enabled {
        return Err(ExecError::new(
            403,
            "client_profile_disabled",
            format!("client profile `{}` is disabled", profile.id),
            None,
        ));
    }
    if let Some(protocol) = req.metadata.get("client_protocol") {
        if protocol != profile.kind.protocol() {
            return Err(ExecError::new(
                422,
                "invalid_request_error",
                format!(
                    "client profile `{}` expects protocol `{}` but request used `{protocol}`",
                    profile.id,
                    profile.kind.protocol()
                ),
                None,
            ));
        }
    }
    if !profile.models.is_empty() && !profile.models.iter().any(|model| model == &req.model) {
        return Err(ExecError::new(
            403,
            "client_profile_model_denied",
            format!(
                "client profile `{}` does not allow model `{}`",
                profile.id, req.model
            ),
            None,
        ));
    }

    req.metadata
        .insert("client_profile".to_string(), profile.id.clone());
    req.metadata.insert(
        "client_profile_mode".to_string(),
        profile.mode.as_str().to_string(),
    );
    Ok(Some(profile.clone()))
}

fn infer_client_profile_for_model<'a>(
    snap: &'a Snapshot,
    req: &AiRequest,
) -> Result<Option<&'a ClientProfileConfig>, ExecError> {
    let matches = snap
        .config
        .client_profiles
        .iter()
        .filter(|profile| profile.models.iter().any(|model| model == &req.model))
        .collect::<Vec<_>>();
    if matches.len() > 1 {
        let ids = matches
            .iter()
            .map(|profile| profile.id.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(ExecError::new(
            409,
            "ambiguous_client_profile",
            format!(
                "model `{}` matches multiple client profiles: {ids}",
                req.model
            ),
            None,
        ));
    }
    Ok(matches.into_iter().next())
}

pub(crate) fn client_profile_allowed_accounts(
    profile: Option<&ClientProfileConfig>,
    provider_id: &str,
) -> Option<HashSet<String>> {
    let profile = profile?;
    if profile.accounts.is_empty() {
        return None;
    }
    Some(profile_accounts_for_provider(profile, provider_id))
}

pub(crate) fn combine_allowed_accounts(
    left: Option<HashSet<String>>,
    right: Option<HashSet<String>>,
) -> Option<HashSet<String>> {
    match (left, right) {
        (None, None) => None,
        (Some(accounts), None) | (None, Some(accounts)) => Some(accounts),
        (Some(left), Some(right)) => Some(left.intersection(&right).cloned().collect()),
    }
}

/// Resolve a model to ordered candidate targets — the routing front-half shared
/// by `execute` and `preview_route`. Precedence: execution profile route →
/// exact route → combo profile → explicit `provider/model` → wildcard route →
/// default pass-through provider → 404. Each candidate is stamped with its
/// non-secret account-pool health so the router can demote locked pools, and
/// (outcome-routing-v1 §1 read seam) its rolling outcome-scorecard evidence —
/// a pure in-memory read, `None` when disabled/no evidence yet, so routing
/// behaves exactly as before unless a caller (commit 5) consults it.
pub(crate) fn resolve_candidates(
    snap: &Snapshot,
    model: &str,
    scorecard: &crate::scorecard::Scorecard,
    lanes: Option<&dyn crate::LaneStatus>,
) -> Result<CandidateResolution, ExecError> {
    let profile = ExecutionProfile::from_model(model);
    let (route_name, require, mut candidates, unknown, combo, pin_order, route_prefer_awake): ResolvedRoute = if let Some(profile) = profile {
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
        (
            route_name,
            route.require.clone(),
            candidates,
            unknown,
            None,
            route.pin_order,
            route.prefer_awake,
        )
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
            route.pin_order,
            route.prefer_awake,
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
            false,
            None,
        )
    } else if let Some(target) = snap.registry.target_for(model) {
        (
            "direct".to_string(),
            RouteRequire::default(),
            vec![target],
            Vec::new(),
            None,
            false,
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
            route.pin_order,
            route.prefer_awake,
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
                    false,
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

    let now = std::time::Instant::now();
    let quality_evaluator_id = snap.config.server.quality_eval.enabled.then(|| {
        crate::quality_eval::evaluator_id(&snap.config.server.quality_eval.body_allowed_targets)
    });
    for candidate in candidates.iter_mut() {
        let ph = snap
            .resolver
            .pool_health(&candidate.provider_id, &candidate.model);
        candidate.healthy_accounts = Some(if ph.circuit_open { 0 } else { ph.healthy });
        candidate.outcome =
            scorecard.project(&candidate.id, "any", &snap.config.server.scorecard, now);
        if let Some(evaluator_id) = &quality_evaluator_id {
            candidate.quality = scorecard.project_quality(
                &candidate.id,
                crate::quality_eval::QUALITY_CLASS,
                evaluator_id,
                &snap.config.server.scorecard,
                &snap.config.server.quality_eval,
                now,
            );
        }
    }
    // Wake-aware ordering (`prefer_awake`): the route's own fields over the
    // server default. Only when on is a target's wake need stamped, so a
    // route without it plans exactly as before.
    let server_default = snap.config.server.prefer_awake;
    let effective = route_prefer_awake.map_or(server_default, |route| route.over(&server_default));
    let prefer_awake = effective.enabled.then_some(effective);
    if let (Some(_), Some(lanes)) = (prefer_awake, lanes) {
        for candidate in candidates.iter_mut() {
            if crate::lanes::wakes_on_request(snap, &candidate.provider_id) {
                candidate.wake = lanes.wake_need(&candidate.provider_id);
            }
        }
    }
    let lane_down = known_down_lanes(snap, &candidates, lanes);
    Ok(CandidateResolution {
        route_name,
        require,
        candidates,
        unknown,
        profile,
        combo,
        pin_order,
        lane_down,
        prefer_awake,
        optimize: None,
    })
}

/// Candidates whose managed local lane is known to be down. Only a lane the
/// request's pinned snapshot still lists in `local_executors` with
/// `route_health: true` counts: the lane view is built once at startup, so
/// without this a reload that removes a lane, or turns the setting off, would
/// keep skipping its provider.
fn known_down_lanes(
    snap: &Snapshot,
    candidates: &[ExecutionTarget],
    lanes: Option<&dyn crate::LaneStatus>,
) -> Vec<(String, String)> {
    let Some(lanes) = lanes else {
        return Vec::new();
    };
    candidates
        .iter()
        .filter(|candidate| {
            snap.config
                .local_executors
                .iter()
                .any(|lane| lane.name == candidate.provider_id && lane.route_health)
        })
        .filter_map(|candidate| {
            let detail = lanes.known_down(&candidate.provider_id)?;
            Some((
                candidate.id.clone(),
                format!(
                    "local lane `{}` is down ({detail}); skipped before dispatch, not counted as a failure",
                    candidate.provider_id
                ),
            ))
        })
        .collect()
}

/// Plan the resolved candidates, skipping targets on a managed local lane
/// known to be down: they are never dispatched, so a lane stopped on purpose
/// collects no failures (no scorecard sample, no account lock). Fail-open:
/// when skipping them leaves nothing that can serve the request — after
/// tenant, client-profile and router hard filters — they are kept, because a
/// stale probe must never turn into a hard outage; the attempt then decides.
///
/// With `prefer_awake` on, a target whose lane is down but wakes on request
/// (it carries [`ExecutionTarget::wake`]) is not skipped: the router orders it
/// behind the targets that are up, and it is woken only if the request
/// reaches it. When `x-switchback-wake: never` left nothing that can serve,
/// the answer is a typed 503 (`local_lane_asleep`) instead of a 400.
pub(crate) fn plan_resolved_route(
    combo_rr: &Mutex<HashMap<String, usize>>,
    snap: &Snapshot,
    req: &AiRequest,
    client_profile: Option<&ClientProfileConfig>,
    resolved: CandidateResolution,
    advance_combo_cursor: bool,
) -> Result<(String, sb_router::RoutePlan), ExecError> {
    let (route_name, plan) = plan_lanes_skipped(
        combo_rr,
        snap,
        req,
        client_profile,
        resolved,
        advance_combo_cursor,
    )?;
    if plan.decision.selected.is_none() {
        let asleep: Vec<&str> = plan
            .decision
            .rejected
            .iter()
            .filter(|rejected| {
                rejected
                    .reason
                    .starts_with("prefer_awake: x-switchback-wake=never")
            })
            .map(|rejected| rejected.target_id.as_str())
            .collect();
        if !asleep.is_empty() {
            return Err(ExecError::new(
                503,
                "local_lane_asleep",
                format!(
                    "x-switchback-wake=never and every target that can serve needs a wake ({}); send x-switchback-wake: always or auto to wake one",
                    asleep.join(",")
                ),
                Some(plan.decision.summary()),
            ));
        }
    }
    Ok((route_name, plan))
}

fn plan_lanes_skipped(
    combo_rr: &Mutex<HashMap<String, usize>>,
    snap: &Snapshot,
    req: &AiRequest,
    client_profile: Option<&ClientProfileConfig>,
    mut resolved: CandidateResolution,
    advance_combo_cursor: bool,
) -> Result<(String, sb_router::RoutePlan), ExecError> {
    let mut lane_down = std::mem::take(&mut resolved.lane_down);
    // A caller-side filter (e.g. a quality-eval scope) may already have
    // removed some of them; those carry their own rejection.
    lane_down.retain(|(target_id, _)| resolved.candidates.iter().any(|c| &c.id == target_id));
    // `prefer_awake`: a down lane that wakes on request stays a candidate.
    lane_down.retain(|(target_id, _)| {
        !resolved
            .candidates
            .iter()
            .any(|c| &c.id == target_id && c.wake.is_some())
    });
    if lane_down.is_empty() {
        return plan_candidates(
            combo_rr,
            snap,
            req,
            client_profile,
            resolved,
            advance_combo_cursor,
        );
    }
    let all = resolved.clone();
    resolved.candidates.retain(|candidate| {
        !lane_down
            .iter()
            .any(|(target_id, _)| *target_id == candidate.id)
    });
    let (route_name, mut plan) = plan_candidates(
        combo_rr,
        snap,
        req,
        client_profile,
        resolved,
        advance_combo_cursor,
    )?;
    if plan.decision.selected.is_some() {
        for (target_id, reason) in lane_down {
            plan.decision.reject(target_id, reason);
        }
        return Ok((route_name, plan));
    }
    let (route_name, mut plan) = plan_candidates(combo_rr, snap, req, client_profile, all, false)?;
    let kept: Vec<&str> = lane_down
        .iter()
        .map(|(target_id, _)| target_id.as_str())
        .filter(|target_id| plan.candidates.iter().any(|c| c.id == *target_id))
        .collect();
    if !kept.is_empty() {
        plan.decision.add_reason(format!(
            "local lane known down but kept: no other candidate can serve ({})",
            kept.join(",")
        ));
    }
    Ok((route_name, plan))
}

pub(crate) fn plan_candidates(
    combo_rr: &Mutex<HashMap<String, usize>>,
    snap: &Snapshot,
    req: &AiRequest,
    client_profile: Option<&ClientProfileConfig>,
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
    let mut profile_rejections = Vec::new();
    if let Some(profile) = client_profile {
        if !profile.accounts.is_empty() {
            resolved.candidates.retain(|candidate| {
                if client_profile_candidate_has_account(snap, profile, candidate) {
                    return true;
                }
                profile_rejections.push((
                    candidate.id.clone(),
                    format!(
                        "client profile `{}`: no pinned account for provider `{}`",
                        profile.id, candidate.provider_id
                    ),
                ));
                false
            });
        }
    }
    let mut policy = routing_policy(snap, resolved.profile);
    policy.pin_order = resolved.pin_order;
    let wake_note = resolved.prefer_awake.map(|cfg| {
        let decided = crate::prefer_awake::decide(&cfg, req);
        policy.wake = Some(decided.policy);
        policy.rank_on_decode_speed = decided.long_output;
        decided.note
    });
    let mut plan = sb_router::plan_route(
        req,
        &resolved.route_name,
        &resolved.require,
        &resolved.candidates,
        &policy,
    );
    if let Some(hints) = &resolved.optimize {
        crate::optimize::apply_hints(&mut plan, hints, resolved.pin_order);
    }
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
    if let Some(profile) = client_profile {
        plan.decision
            .add_reason(format!("client_profile={}", profile.id));
        plan.decision
            .add_reason(format!("client_profile_mode={}", profile.mode.as_str()));
    }
    if let Some(Some(note)) = wake_note {
        plan.decision.add_reason(note);
    }
    for (target_id, reason) in tenant_rejections {
        plan.decision.reject(target_id, reason);
    }
    for (target_id, reason) in profile_rejections {
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

fn client_profile_candidate_has_account(
    snap: &Snapshot,
    profile: &ClientProfileConfig,
    candidate: &ExecutionTarget,
) -> bool {
    if profile.accounts.is_empty() {
        return true;
    }
    let allowed = profile_accounts_for_provider(profile, &candidate.provider_id);
    if allowed.is_empty() {
        return false;
    }
    let configured = snap.resolver.account_ids(&candidate.provider_id);
    configured
        .iter()
        .any(|account_id| allowed.contains(account_id))
}

fn profile_accounts_for_provider(
    profile: &ClientProfileConfig,
    provider_id: &str,
) -> HashSet<String> {
    profile
        .accounts
        .iter()
        .filter_map(|account_ref| {
            let (provider, account) = account_ref.split_once('/')?;
            (provider == provider_id).then(|| account.to_string())
        })
        .collect()
}
