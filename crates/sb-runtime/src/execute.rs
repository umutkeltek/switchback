use std::collections::HashSet;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use sb_adapter::{AdapterError, PreparedRequest};
use sb_core::{AiRequest, ErrorClass, EvaluationEvent, EvaluationEventKind, FinishReason};
use sb_credentials::ResolveOutcome;
use tracing::Instrument as _;

use super::collect::{collect_response, precommit_stream};
use super::execution_meta::{
    attach_execution_receipt, lookup_exact_cache, native_execution_observation,
    route_selected_event,
};
use super::finish_attempt::{AttemptFinishCtx, AttemptToken, FinishOutcome};
use super::hedge::run_hedge;
use super::helpers::{
    high_lossiness_schema_warning, resolve_egress, retry_backoff, retryable, session_affinity_key,
};
use super::profiles::{
    apply_request_client_profile, client_profile_allowed_accounts, combine_allowed_accounts,
    plan_resolved_route, resolve_candidates, tenant_allowed_accounts,
};
use super::stream::{attempt_speed, meter_stream, ttft_ms, watch_first_token, StreamFinish};
use super::{DenialTrace, Engine, ExecError, ExecOutcome, LaneWake, Snapshot};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScopedSuccess {
    pub(crate) target_id: String,
    pub(crate) cost_micros: u64,
}

pub(crate) struct ScopedExecution {
    pub(crate) outcome: ExecOutcome,
    pub(crate) success: Option<ScopedSuccess>,
}

struct ExecutionScope {
    allowed_targets: HashSet<String>,
    success: Mutex<Option<ScopedSuccess>>,
}

impl ExecutionScope {
    fn record_success(&self, target_id: &str, cost_micros: u64) {
        if let Ok(mut success) = self.success.lock() {
            *success = Some(ScopedSuccess {
                target_id: target_id.to_string(),
                cost_micros,
            });
        }
    }
}

impl Engine {
    /// Pin a snapshot and run the request to a committed outcome. Returns the
    /// pinned revision alongside the outcome so the HTTP edge can stamp
    /// `x-switchback-revision`. This is the runtime's public entry point.
    pub async fn execute(&self, req: AiRequest, started: Instant) -> (u64, ExecOutcome) {
        let snap = self.snapshot();
        let revision = snap.revision;
        let outcome = self.execute_inner(&snap, req, started, None).await;
        (revision, outcome)
    }

    /// Execute against one already-pinned snapshot while fail-closed filtering
    /// every route candidate not explicitly allowed. This is the body-privacy
    /// seam used only by the internal quality evaluator: filtering happens
    /// before credential resolution and adapter construction.
    pub(crate) async fn execute_scoped(
        &self,
        snap: std::sync::Arc<Snapshot>,
        req: AiRequest,
        started: Instant,
        allowed_targets: HashSet<String>,
    ) -> ScopedExecution {
        let scope = ExecutionScope {
            allowed_targets,
            success: Mutex::new(None),
        };
        let outcome = self.execute_inner(&snap, req, started, Some(&scope)).await;
        let success = scope.success.into_inner().unwrap_or_default();
        ScopedExecution { outcome, success }
    }

    /// The shared execution core — route resolution + two-level (target ×
    /// account) fallback. Format-agnostic: every ingress format funnels through
    /// here, then renders the committed result in its own wire format. (One
    /// loop, not two — the 9router duplication trap avoided.)
    async fn execute_inner(
        &self,
        snap: &Snapshot,
        mut req: AiRequest,
        started: Instant,
        scope: Option<&ExecutionScope>,
    ) -> ExecOutcome {
        // The caller pinned ONE compiled snapshot for this request's whole
        // lifetime — a config publish mid-request never tears it across revisions.
        let rt = &snap.runtime;
        // Model settings (`switchback` body object, `x-switchback-profile`):
        // taken out of the request first so none of it can reach an
        // upstream, whatever happens next.
        let request_settings = match sb_core::take_request_settings(&mut req) {
            Ok(settings) => settings,
            Err(e) => {
                let message = e.to_string();
                self.record_denial_trace(DenialTrace {
                    request_id: &req.id,
                    revision: snap.revision,
                    tenant: req.tenant.as_deref(),
                    project: req.project.as_deref(),
                    inbound_model: &req.model,
                    status: 400,
                    error_type: "invalid_settings",
                    message: &message,
                    started,
                    streamed: req.stream,
                });
                return ExecOutcome::Error(ExecError::new(400, "invalid_settings", message, None));
            }
        };

        // Global spend cap: once attributed spend reaches `budget.max_usd`, reject
        // new requests (402) rather than keep billing. Per-provider caps are checked
        // per target in the loop below.
        if let Some(max) = rt.budget_max_usd {
            let spent = match self.global_spend_usd() {
                Ok(spent) => spent,
                Err(e) => {
                    let message = format!("usage store unavailable for budget check: {e}");
                    self.record_denial_trace(DenialTrace {
                        request_id: &req.id,
                        revision: snap.revision,
                        tenant: req.tenant.as_deref(),
                        project: req.project.as_deref(),
                        inbound_model: &req.model,
                        status: 503,
                        error_type: "usage_store_unavailable",
                        message: &message,
                        started,
                        streamed: req.stream,
                    });
                    return ExecOutcome::Error(ExecError::new(
                        503,
                        "usage_store_unavailable",
                        message,
                        None,
                    ));
                }
            };
            if spent >= max {
                let message = format!("budget exceeded: spent ${spent:.4} of ${max:.4} cap");
                self.record_denial_trace(DenialTrace {
                    request_id: &req.id,
                    revision: snap.revision,
                    tenant: req.tenant.as_deref(),
                    project: req.project.as_deref(),
                    inbound_model: &req.model,
                    status: 402,
                    error_type: "budget_exceeded",
                    message: &message,
                    started,
                    streamed: req.stream,
                });
                return ExecOutcome::Error(ExecError::new(402, "budget_exceeded", message, None));
            }
        }

        // Per-tenant hard spend cap (Oracle #4): reject before dispatch once the
        // tenant's attributed spend reaches its configured budget. Reconciliation
        // happens after — `record_usage` accrues the actual cost to the tenant.
        if let Some(tenant) = req.tenant.as_deref() {
            if let Some(budget) = snap.config.tenant(tenant).and_then(|t| t.budget_usd) {
                let spent = match self.tenant_spend_usd(tenant) {
                    Ok(spent) => spent,
                    Err(e) => {
                        let message = format!("usage store unavailable for budget check: {e}");
                        self.record_denial_trace(DenialTrace {
                            request_id: &req.id,
                            revision: snap.revision,
                            tenant: req.tenant.as_deref(),
                            project: req.project.as_deref(),
                            inbound_model: &req.model,
                            status: 503,
                            error_type: "usage_store_unavailable",
                            message: &message,
                            started,
                            streamed: req.stream,
                        });
                        return ExecOutcome::Error(ExecError::new(
                            503,
                            "usage_store_unavailable",
                            message,
                            None,
                        ));
                    }
                };
                if spent >= budget {
                    let message = format!(
                        "tenant `{tenant}` budget exceeded: spent ${spent:.4} of ${budget:.4} cap"
                    );
                    self.record_denial_trace(DenialTrace {
                        request_id: &req.id,
                        revision: snap.revision,
                        tenant: req.tenant.as_deref(),
                        project: req.project.as_deref(),
                        inbound_model: &req.model,
                        status: 402,
                        error_type: "tenant_budget_exceeded",
                        message: &message,
                        started,
                        streamed: req.stream,
                    });
                    return ExecOutcome::Error(ExecError::new(
                        402,
                        "tenant_budget_exceeded",
                        message,
                        None,
                    ));
                }
            }
        }

        // RTK-style tool-result compression (opt-in): shrink bulky tool outputs in
        // the prompt before dispatch. Fail-safe (never-grow/never-empty), so the
        // worst case is a no-op. Metadata-only log, never the content.
        if snap.config.server.compress_tool_results {
            let stats = sb_compress::compress_request(&mut req);
            if stats.saved() > 0 {
                tracing::info!(
                    request_id = %req.id,
                    rtk_bytes_before = stats.bytes_before,
                    rtk_bytes_after = stats.bytes_after,
                    rtk_saved = stats.saved(),
                    rtk_filters = ?stats.filters_applied,
                    "rtk compression"
                );
            }
        }

        // Plugin pre-route hook (Oracle #6): inspect / modify / reject the
        // request before routing. A plugin rejection short-circuits here.
        if let sb_plugin::PluginOutcome::Reject { status, message } =
            snap.plugins.pre_route(&mut req)
        {
            self.record_denial_trace(DenialTrace {
                request_id: &req.id,
                revision: snap.revision,
                tenant: req.tenant.as_deref(),
                project: req.project.as_deref(),
                inbound_model: &req.model,
                status,
                error_type: "plugin_rejected",
                message: &message,
                started,
                streamed: req.stream,
            });
            return ExecOutcome::Error(ExecError::new(status, "plugin_rejected", message, None));
        }
        let client_profile = match apply_request_client_profile(snap, &mut req) {
            Ok(profile) => profile,
            Err(e) => {
                self.record_denial_trace(DenialTrace {
                    request_id: &req.id,
                    revision: snap.revision,
                    tenant: req.tenant.as_deref(),
                    project: req.project.as_deref(),
                    inbound_model: &req.model,
                    status: e.status,
                    error_type: &e.error_type,
                    message: &e.message,
                    started,
                    streamed: req.stream,
                });
                return ExecOutcome::Error(e);
            }
        };
        let cache_policy = snap.config.server.execution_cache.policy();
        let body_redacted = scope.is_some();
        let cache_receipt = if request_settings.as_ref().is_some_and(|s| s.no_cache) {
            crate::execution_meta::no_cache_bypass(&cache_policy)
        } else if request_settings.is_some() {
            crate::execution_meta::settings_cache_bypass(&cache_policy)
        } else {
            lookup_exact_cache(self, &req, &cache_policy, body_redacted)
        };

        // Resolve the request's model to candidate targets (route → provider/model
        // → default provider → 404), pool-health-stamped. Shared with route-preview.
        let mut resolved =
            match resolve_candidates(snap, &req.model, &self.scorecard, self.lanes.as_deref()) {
                Ok(resolved) => resolved,
                Err(e) => {
                    self.record_denial_trace(DenialTrace {
                        request_id: &req.id,
                        revision: snap.revision,
                        tenant: req.tenant.as_deref(),
                        project: req.project.as_deref(),
                        inbound_model: &req.model,
                        status: e.status,
                        error_type: &e.error_type,
                        message: &e.message,
                        started,
                        streamed: req.stream,
                    });
                    return ExecOutcome::Error(e);
                }
            };
        let mut scope_rejected = Vec::new();
        if let Some(scope) = scope {
            resolved.candidates.retain(|candidate| {
                let allowed = scope.allowed_targets.contains(&candidate.id);
                if !allowed {
                    scope_rejected.push(candidate.id.clone());
                }
                allowed
            });
        }
        // Speed tricks (`server.optimize`, `routes[].optimize`): what the
        // ordering pass needs, computed once so every plan of this request
        // agrees. Nothing configured = nothing computed.
        let tune_rejected = match self.apply_tune_locks(snap, &req, &mut resolved) {
            Ok(rejected) => rejected,
            Err(e) => {
                self.record_denial_trace(DenialTrace {
                    request_id: &req.id,
                    revision: snap.revision,
                    tenant: req.tenant.as_deref(),
                    project: req.project.as_deref(),
                    inbound_model: &req.model,
                    status: e.status,
                    error_type: &e.error_type,
                    message: &e.message,
                    started,
                    streamed: req.stream,
                });
                return ExecOutcome::Error(e);
            }
        };
        let conversation = crate::optimize::conversation_of(&req);
        let prompt_key = sb_core::conversation_key(&req, None);
        let sticky = self.sticky_for(snap, conversation.as_deref());
        resolved.optimize = self.optimize_hints(snap, &req, &resolved, sticky.as_ref());
        let unknown = resolved.unknown.clone();
        // Lanes with an engine control: the request counts on its first
        // choice's engine model from here (before any wake or lane wait), so
        // an unload or `apply: now` waits for it; a closed gate holds it
        // here (bounded).
        let first_admission = match self
            .admit_first_choice(snap, &req, client_profile.as_ref(), &mut resolved)
            .await
        {
            Ok(admission) => admission,
            Err(e) => {
                self.record_denial_trace(DenialTrace {
                    request_id: &req.id,
                    revision: snap.revision,
                    tenant: req.tenant.as_deref(),
                    project: req.project.as_deref(),
                    inbound_model: &req.model,
                    status: e.status,
                    error_type: &e.error_type,
                    message: &e.message,
                    started,
                    streamed: req.stream,
                });
                return ExecOutcome::Error(e);
            }
        };
        // Opt-in (`wake_on_request`): wake the first choice's lane if routing
        // is about to skip it as down, bounded, before the first byte.
        let lane_wake = self
            .wake_lane_before_dispatch(snap, &req, client_profile.as_ref(), &mut resolved)
            .await;
        // Opt-in (`max_in_flight`, `exclusive_group`): the first choice's
        // lane gives it a place (or it spills to the next target) and takes
        // its GPU group, bounded, before the first byte.
        let mut lane_gpu = self
            .take_lane_before_dispatch(snap, &req, client_profile.as_ref(), &mut resolved)
            .await;
        if let Some(provider) = lane_gpu.as_ref().and_then(|note| note.waited_on()) {
            self.optimize.count_queue_wait(provider);
        }

        let planned = plan_resolved_route(
            &self.combo_rr,
            snap,
            &req,
            client_profile.as_ref(),
            resolved,
            true,
        )
        .and_then(|(route_name, mut plan)| {
            if let Some(note) = &lane_wake {
                note.apply(&mut plan)?;
            }
            if let Some(note) = &lane_gpu {
                note.apply(&mut plan)?;
            }
            Ok((route_name, plan))
        });
        let (route_name, mut plan) = match planned {
            Ok(plan) => plan,
            Err(e) => {
                self.record_denial_trace(DenialTrace {
                    request_id: &req.id,
                    revision: snap.revision,
                    tenant: req.tenant.as_deref(),
                    project: req.project.as_deref(),
                    inbound_model: &req.model,
                    status: e.status,
                    error_type: &e.error_type,
                    message: &e.message,
                    started,
                    streamed: req.stream,
                });
                return ExecOutcome::Error(e);
            }
        };
        for target_id in scope_rejected {
            plan.decision
                .reject(target_id, "quality_eval scope: target not body-allowlisted");
        }
        for (target_id, reason) in tune_rejected {
            plan.decision.reject(target_id, reason);
        }
        // The router admitted a text-only target for an image-bearing request
        // rather than failing it. Honour that decision by actually removing the
        // image parts here — the adapters downstream would otherwise forward
        // them to a provider that answers with its own opaque 400, which is the
        // failure this whole path exists to prevent.
        if plan.decision.degraded("vision_in") {
            let dropped = req.strip_image_parts(
                "[image omitted: this model cannot accept image input, \
                 so the attached image was not sent]",
            );
            tracing::info!(
                request_id = %req.id,
                route = %route_name,
                dropped_images = dropped,
                "capability_fallback: served image request as text on a text-only route"
            );
        }
        // Per-target model settings: the request each attempt sends, with
        // that target's defaults/profile/locks applied. Nothing configured
        // and nothing asked = an empty plan and the request goes unchanged.
        let settings_plan = match crate::model_settings::plan_settings(
            &self.model_settings,
            snap,
            &route_name,
            &req,
            request_settings.as_ref(),
            &plan.candidates,
        ) {
            Ok(settings_plan) => settings_plan,
            Err(e) => {
                self.record_denial_trace(DenialTrace {
                    request_id: &req.id,
                    revision: snap.revision,
                    tenant: req.tenant.as_deref(),
                    project: req.project.as_deref(),
                    inbound_model: &req.model,
                    status: e.status,
                    error_type: &e.error_type,
                    message: &e.message,
                    started,
                    streamed: req.stream,
                });
                return ExecOutcome::Error(e);
            }
        };
        // Per-request load settings: one variant of the first target, made
        // (or found) by the engine control; other targets cannot honour
        // them, so they leave the plan.
        let mut settings_plan = settings_plan;
        settings_plan.set_variants(self.model_variants.get().cloned());
        // Kept only while the plan still has that target (a spill or a lane
        // wait may have moved the request elsewhere).
        if let Some((target_id, admission)) = first_admission {
            if plan
                .candidates
                .iter()
                .any(|candidate| candidate.id == target_id)
            {
                settings_plan.add_admission(&target_id, admission);
            }
        }
        if let Some(load) = request_settings
            .as_ref()
            .map(|settings| &settings.load)
            .filter(|load| !load.is_empty())
        {
            let role = req
                .metadata
                .get(sb_core::SETTINGS_ROLE_METADATA_KEY)
                .map(String::as_str)
                .unwrap_or("client");
            let variant = match crate::model_settings::check_request_load(
                &self.model_settings,
                &req,
                role,
                load,
                &plan.candidates,
            ) {
                Ok((target, policy)) => match self.model_variants.get() {
                    Some(variants) => variants
                        .ensure_variant(&target, load, &policy, role)
                        .await
                        .map(|lease| (target, lease)),
                    None => Err(ExecError::new(
                        400,
                        "no_engine_control",
                        format!("`{}` has no engine control for load settings", target.id),
                        None,
                    )),
                },
                Err(e) => Err(e),
            };
            match variant {
                Ok((target, lease)) => {
                    let name = lease.name;
                    for dropped in plan.candidates.iter().skip(1) {
                        plan.decision.reject(
                            dropped.id.clone(),
                            "request load settings: served by the first target's variant only",
                        );
                    }
                    plan.candidates.truncate(1);
                    plan.decision.fallbacks.clear();
                    plan.decision
                        .add_reason(format!("settings variant {name} of {}", target.id));
                    settings_plan.set_model(&target, &req, name, lease.note);
                    // The variant's admission replaces the base model's: the
                    // request reaches only the variant.
                    settings_plan.add_admission(&target.id, lease.admission);
                }
                Err(e) => {
                    self.record_denial_trace(DenialTrace {
                        request_id: &req.id,
                        revision: snap.revision,
                        tenant: req.tenant.as_deref(),
                        project: req.project.as_deref(),
                        inbound_model: &req.model,
                        status: e.status,
                        error_type: &e.error_type,
                        message: &e.message,
                        started,
                        streamed: req.stream,
                    });
                    return ExecOutcome::Error(e);
                }
            }
        }
        self.pin_sticky_slot(snap, &req, sticky.as_ref(), &mut plan, &mut settings_plan);
        if let Some(note) = plan
            .candidates
            .first()
            .and_then(|first| settings_plan.summary_for(&first.id))
            .and_then(crate::optimize::guard_note)
        {
            plan.decision.add_optimize(note);
        }
        if !settings_plan.is_empty() {
            if let Some(summary) = plan
                .candidates
                .first()
                .and_then(|first| settings_plan.summary_for(&first.id))
            {
                plan.decision.settings = Some(summary.to_string());
            }
            for candidate in &plan.candidates {
                if let Some(summary) = settings_plan.summary_for(&candidate.id) {
                    plan.decision
                        .add_reason(format!("settings target={} {summary}", candidate.id));
                }
            }
        }
        attach_execution_receipt(&mut plan, &req, cache_receipt.clone(), body_redacted);
        // Plugin post-route hook (Oracle #6): observe the explainable decision.
        snap.plugins.post_route(&req, &plan.decision);
        let summary = plan.decision.summary();
        let mut quality_capture = self.quality_eval.begin(&req, snap);
        let mut last_err: Option<AdapterError> = None;
        let mut unavailable_retry_after: Option<Duration> = None;

        // One trace per request: the route decision + every attempt + outcome + cost
        // + the egress path each attempt took. Metadata only (sb-trace upholds the
        // no-secrets invariant).
        let session_id = session_affinity_key(&req).map(str::to_string);
        let mut trace = sb_trace::RequestTrace::start(
            req.id.clone(),
            snap.revision,
            req.model.clone(),
            route_name.clone(),
            plan.decision.clone(),
        )
        .with_principal(req.tenant.clone(), req.project.clone())
        .with_session_id(session_id.clone())
        .with_client_metadata(
            req.metadata.get("client_profile").cloned(),
            req.metadata.get("client_protocol").cloned(),
        )
        .with_native_execution(native_execution_observation(&req));

        trace.event(EvaluationEvent::new(EvaluationEventKind::RunStarted));
        trace.event(EvaluationEvent::cache_lookup(cache_receipt));
        trace.event(route_selected_event(&plan.decision));

        // Parent span for this request; each attempt opens a child span around the
        // upstream call. A `tracing-opentelemetry` layer exports this tree as one
        // distributed trace with no changes here — the OTel-ready seam.
        let request_span = tracing::info_span!(
            "switchback.request",
            request_id = %req.id,
            inbound_model = %req.model,
            route = %route_name,
            streamed = req.stream,
            gen_ai.request.model = %req.model,
            langfuse.trace.name = "switchback.request",
            langfuse.user.id = req.tenant.as_deref().unwrap_or(""),
            langfuse.session.id = session_id.as_deref().unwrap_or(""),
            langfuse.trace.metadata.project = req.project.as_deref().unwrap_or(""),
            langfuse.trace.metadata.route = %route_name,
            langfuse.trace.metadata.inbound_model = %req.model,
        );

        // Hedging fast-path (non-streaming only): race the top candidates, take the
        // first success, cancel the losers. On total hedge failure, fall through to
        // the normal sequential fallback loop below.
        // A target that still needs a wake (`prefer_awake`) is never raced: a
        // hedge would dispatch into a machine that is not up.
        let hedgeable = !plan
            .candidates
            .iter()
            .take(snap.config.server.hedge.max_parallel.max(1) as usize)
            .any(|candidate| candidate.wake.is_some());
        if scope.is_none()
            && rt.hedge_enabled
            && !req.stream
            && plan.candidates.len() >= 2
            && hedgeable
        {
            // The place the first choice's lane gave this request goes to
            // that lane's racer; an unused one comes back for the loop.
            let held = lane_gpu.as_mut().map(|note| note.lend_to_hedge());
            let hedged = run_hedge(
                snap,
                &req,
                &plan.candidates,
                &settings_plan,
                &self.scorecard,
                &snap.config.server.scorecard,
                &|provider_id: &str| match held.as_ref().and_then(|held| held.take_for(provider_id))
                {
                    Some(busy) => Ok(Some(busy)),
                    None => self.lane_busy(snap, provider_id),
                },
            )
            .await;
            if let (Some(note), Some(held)) = (lane_gpu.as_mut(), held) {
                note.give_back(held);
            }
            if let Some(win) = hedged {
                tracing::info!(
                    request_id = %req.id, model = %req.model, target = %win.target_id,
                    account = %win.account_id, status = 200u16,
                    latency_ms = started.elapsed().as_millis() as u64, route = %summary
                );
                if let Err(e) = self.record_usage(
                    &snap.registry,
                    &req.id,
                    &win.provider_id,
                    &win.model,
                    &win.account_id,
                    req.tenant.as_deref(),
                    req.project.as_deref(),
                    win.response.usage.clone(),
                    started,
                    false,
                    win.speed.clone(),
                ) {
                    let message = format!("usage persistence failed: {e}");
                    self.record_trace(trace.finish(
                        500,
                        started.elapsed().as_millis() as u64,
                        false,
                    ));
                    return ExecOutcome::Error(ExecError::new(
                        500,
                        "usage_persistence_failed",
                        message,
                        None,
                    ));
                }
                trace.attempt(sb_trace::Attempt::success(
                    &win.target_id,
                    &win.provider_id,
                    &win.model,
                    &win.account_id,
                    &win.egress,
                    win.latency_ms,
                ));
                for canceled in &win.canceled {
                    trace.attempt(sb_trace::Attempt::failed(
                        &canceled.target_id,
                        &canceled.provider_id,
                        &canceled.model,
                        "unknown",
                        "unknown",
                        started.elapsed().as_millis() as u64,
                        "hedge_cancelled",
                        false,
                    ));
                }
                let cost =
                    snap.registry
                        .cost_micros(&win.provider_id, &win.model, &win.response.usage);
                if let Some(scope) = scope {
                    scope.record_success(&win.target_id, cost);
                }
                trace.set_usage(win.response.usage.clone(), cost);
                trace.set_ttft_ms(
                    win.first_token_at
                        .map(|at| at.saturating_duration_since(started).as_millis() as u64),
                );
                self.traces
                    .record(trace.finish(200, started.elapsed().as_millis() as u64, false));
                if let Some(capture) = quality_capture.take() {
                    capture.complete_response(
                        win.target_id.clone(),
                        super::quality_eval::QUALITY_CLASS.to_string(),
                        &win.response,
                    );
                }
                return ExecOutcome::Collected {
                    response: Box::new(win.response),
                    summary,
                };
            }
        }

        // Lanes a `prefer_awake` fallback already woke for this request.
        let mut woken_for_fallback: HashSet<String> = HashSet::new();
        let candidate_count = plan.candidates.len();
        'targets: for (index, target) in plan.candidates.iter().enumerate() {
            // A target skipped before dispatch lets go of the admission
            // taken for it earlier (it is never reached).
            let Some(adapter) = snap.registry.adapter(&target.provider_id) else {
                settings_plan.release(&target.id);
                continue 'targets;
            };
            // Circuit breaker: if this provider is OPEN (it's been failing), don't
            // even attempt it — fall straight over to the next target.
            if !snap.resolver.circuit_allows(&target.provider_id) {
                settings_plan.release(&target.id);
                tracing::info!(
                    request_id = %req.id, target = %target.id, provider = %target.provider_id,
                    "circuit open — skipping provider"
                );
                continue 'targets;
            }
            // Per-provider spend cap: route around a provider that has hit its cap.
            if let Some(cap) = snap
                .config
                .server
                .budget
                .per_provider_usd
                .get(&target.provider_id)
            {
                let spent = match self.provider_spend_usd(&target.provider_id) {
                    Ok(spent) => spent,
                    Err(e) => {
                        let message = format!("usage store unavailable for budget check: {e}");
                        self.record_denial_trace(DenialTrace {
                            request_id: &req.id,
                            revision: snap.revision,
                            tenant: req.tenant.as_deref(),
                            project: req.project.as_deref(),
                            inbound_model: &req.model,
                            status: 503,
                            error_type: "usage_store_unavailable",
                            message: &message,
                            started,
                            streamed: req.stream,
                        });
                        return ExecOutcome::Error(ExecError::new(
                            503,
                            "usage_store_unavailable",
                            message,
                            None,
                        ));
                    }
                };
                if spent >= *cap {
                    tracing::info!(
                        request_id = %req.id, provider = %target.provider_id,
                        spent_usd = spent, cap_usd = *cap, "provider over budget — skipping"
                    );
                    settings_plan.release(&target.id);
                    continue 'targets;
                }
            }

            // Opt-in lanes (`wake_on_request`): the lane counts this request
            // as in-flight work from here until the attempt fails over, the
            // collected response is complete, or the stream ends (the guard
            // then moves into the stream finalizer). Drop releases it on every
            // path, unwinding included. `None` for any other target.
            // A lane in an exclusive GPU group it does not hold right now is
            // skipped here (only the first choice waits for it, above).
            // `prefer_awake`: the request fell back to a target whose machine
            // still needs a wake. Wake it now, bounded, before the first
            // byte; a lane that does not come up is skipped, not counted as a
            // failure.
            if target.wake.is_some() && !woken_for_fallback.contains(&target.provider_id) {
                match self.wake_for_fallback(&target.provider_id).await {
                    LaneWake::Ready { detail } => {
                        trace.warning(format!(
                            "prefer_awake: fell back to {}; local lane `{}` {detail}",
                            target.id, target.provider_id
                        ));
                        woken_for_fallback.insert(target.provider_id.clone());
                    }
                    LaneWake::NotReady {
                        detail,
                        retry_after,
                    } => {
                        trace.warning(format!(
                            "target {} skipped: local lane `{}` down ({detail}); not counted as a failure",
                            target.id, target.provider_id
                        ));
                        unavailable_retry_after = Some(
                            unavailable_retry_after
                                .map_or(retry_after, |current| current.min(retry_after)),
                        );
                        settings_plan.release(&target.id);
                        continue 'targets;
                    }
                }
            }
            // The engine model's admission (lanes with an engine control),
            // before the lane's place: a request never holds a place while
            // it waits at a closed gate or for a pending apply. A closed gate
            // is waited for only by the last target; any other is skipped.
            let may_wait = index + 1 == candidate_count;
            let model_admission = match settings_plan.admission_for(snap, target, may_wait).await {
                Ok(admission) => admission,
                Err(e) => {
                    trace.warning(format!("target {} skipped: {}", target.id, e.message));
                    let retry_after = e.retry_after.unwrap_or(crate::LANE_GROUP_RETRY_AFTER);
                    unavailable_retry_after = Some(
                        unavailable_retry_after
                            .map_or(retry_after, |current| current.min(retry_after)),
                    );
                    continue 'targets;
                }
            };
            let held = lane_gpu
                .as_mut()
                .and_then(|note| note.take_busy(&target.provider_id));
            let mut lane_busy = match held {
                Some(busy) => Some(busy),
                None => match self.lane_busy(snap, &target.provider_id) {
                    Ok(busy) => busy,
                    Err(reason) => {
                        trace.warning(format!(
                            "target {} skipped: local lane `{}` {reason}",
                            target.id, target.provider_id
                        ));
                        unavailable_retry_after = Some(
                            unavailable_retry_after
                                .map_or(crate::LANE_GROUP_RETRY_AFTER, |current| {
                                    current.min(crate::LANE_GROUP_RETRY_AFTER)
                                }),
                        );
                        continue 'targets;
                    }
                },
            };
            // Dispatch on the engine model (never during a preset re-read);
            // the count is held as long as the lane's busy guard (to the
            // stream's end): unload, `apply: now` and a preset re-read wait
            // for it.
            if let Some(mut model_busy) = model_admission {
                if let Err(e) = settings_plan.dispatch(snap, target, &mut model_busy).await {
                    trace.warning(format!("target {} skipped: {}", target.id, e.message));
                    let retry_after = e.retry_after.unwrap_or(crate::LANE_GROUP_RETRY_AFTER);
                    unavailable_retry_after = Some(
                        unavailable_retry_after
                            .map_or(retry_after, |current| current.min(retry_after)),
                    );
                    continue 'targets;
                }
                lane_busy = Some(crate::model_settings::hold_with_lane(
                    lane_busy.take(),
                    model_busy,
                ));
            }
            let mut tried_accounts: HashSet<String> = HashSet::new();
            let allowed_accounts = req
                .tenant
                .as_deref()
                .and_then(|tenant_id| snap.config.tenant(tenant_id))
                .filter(|tenant| !tenant.allowed_accounts.is_empty())
                .map(|tenant| tenant_allowed_accounts(tenant, &target.provider_id));
            let profile_allowed_accounts =
                client_profile_allowed_accounts(client_profile.as_ref(), &target.provider_id);
            let allowed_accounts =
                combine_allowed_accounts(allowed_accounts, profile_allowed_accounts);

            loop {
                match snap.resolver.resolve_with_session_allowed(
                    &target.provider_id,
                    &target.model,
                    &tried_accounts,
                    session_affinity_key(&req),
                    allowed_accounts.as_ref(),
                ) {
                    ResolveOutcome::Selected { account_id, lease } => {
                        let attempt_started = Instant::now();
                        // Outbound path for this account: account override → provider
                        // default → server default. `egress_eff` is what the pool will
                        // actually use (falls back to "direct" if it's disabled), so
                        // the trace records the truth. A `select_egress` plugin
                        // (Oracle #6) may pin a named path, overriding the config.
                        let egress_id =
                            snap.plugins.select_egress(&req, &target.id).or_else(|| {
                                resolve_egress(&snap.config, &target.provider_id, &account_id)
                            });
                        let egress_eff = snap.registry.effective_egress(egress_id.as_deref());
                        // Upgrade an OAuth account's lease to a freshly-refreshed
                        // token (no-op for api-key accounts). A refresh failure is
                        // an auth failure on this account → fall over like any other.
                        let lease = match snap
                            .resolver
                            .fresh_lease(&target.provider_id, &account_id, lease)
                            .await
                        {
                            Ok(lease) => lease,
                            Err(e) => {
                                let error = AdapterError::new(
                                    ErrorClass::Authentication,
                                    format!("oauth refresh failed: {e}"),
                                );
                                snap.resolver.report_failure(
                                    &target.provider_id,
                                    &account_id,
                                    &target.model,
                                    error.class,
                                );
                                trace.attempt(sb_trace::Attempt::failed(
                                    &target.id,
                                    &target.provider_id,
                                    &target.model,
                                    &account_id,
                                    egress_eff.as_str(),
                                    attempt_started.elapsed().as_millis() as u64,
                                    error.class.as_str(),
                                    true,
                                ));
                                tried_accounts.insert(account_id);
                                last_err = Some(error);
                                continue;
                            }
                        };
                        let request_warnings = adapter.request_warnings(&req, target);
                        for warning in &request_warnings {
                            tracing::warn!(
                                request_id = %req.id,
                                target = %target.id,
                                warning = %warning,
                                "request translation warning"
                            );
                            trace.warning(format!("{}: {warning}", target.id));
                        }
                        if snap.config.server.strict_schema_downlevel {
                            if let Some(warning) = high_lossiness_schema_warning(&request_warnings)
                            {
                                let message = format!(
                                    "high-lossiness schema downlevel rejected for target `{}`: {warning}",
                                    target.id
                                );
                                let attempt_ms = attempt_started.elapsed().as_millis() as u64;
                                trace.attempt(sb_trace::Attempt::failed(
                                    &target.id,
                                    &target.provider_id,
                                    &target.model,
                                    &account_id,
                                    egress_eff.as_str(),
                                    attempt_ms,
                                    ErrorClass::UnsupportedCapability.as_str(),
                                    false,
                                ));
                                snap.plugins.post_attempt(&sb_plugin::AttemptInfo {
                                    request_id: &req.id,
                                    target_id: &target.id,
                                    provider_id: &target.provider_id,
                                    account_id: &account_id,
                                    egress: egress_eff.as_str(),
                                    ok: false,
                                    error_class: Some(ErrorClass::UnsupportedCapability.as_str()),
                                    latency_ms: attempt_ms,
                                });
                                self.record_trace(trace.finish(
                                    ErrorClass::UnsupportedCapability.http_status(),
                                    started.elapsed().as_millis() as u64,
                                    false,
                                ));
                                return ExecOutcome::Error(ExecError::new(
                                    ErrorClass::UnsupportedCapability.http_status(),
                                    "schema_downlevel_rejected",
                                    message,
                                    Some(summary),
                                ));
                            }
                        }
                        let attempt_span = tracing::info_span!(
                            parent: &request_span,
                            "switchback.attempt",
                            target = %target.id,
                            provider = %target.provider_id,
                            account = %account_id,
                            egress = %egress_eff,
                        );
                        // outcome-routing-v1 §1: one token per DISPATCHED
                        // attempt, consumed exactly once by whichever
                        // terminal branch fires — the type has no
                        // Clone/Copy, so reusing it for a second
                        // `finish_attempt` call is a compile error, not a
                        // runtime bug. `scorecard_cfg` is captured here (from
                        // the snapshot already pinned for this whole
                        // request) so a config hot-reload mid-stream can't
                        // reclassify this attempt.
                        let mut attempt_token = AttemptToken::new();
                        let scorecard_cfg = snap.config.server.scorecard.clone();
                        // Same-target retry on transient errors (timeout/network/5xx)
                        // before we fall over to another account. The lease + egress
                        // are reused; each retry waits an exponential backoff.
                        let (attempt_req, attempt_target) = settings_plan.prepare(&req, target);
                        let prepared =
                            PreparedRequest::new(attempt_req, attempt_target, Some(lease.clone()))
                                .with_egress(egress_id.clone());
                        let mut exec = adapter
                            .execute(prepared)
                            .instrument(attempt_span.clone())
                            .await;
                        let mut retry_n = 0u32;
                        while let Err(err) = &exec {
                            let retry = &snap.config.server.retry;
                            if retry_n >= rt.retry_max || !retryable(err.class) {
                                break;
                            }
                            // outcome-routing-v1 F5: each individual adapter
                            // dispatch is its own attempt for breaker/plugin/
                            // scorecard purposes — finalize THIS failed
                            // dispatch's token (as a real TargetFailure)
                            // before redispatching, so timeout->timeout->
                            // success records 2 TargetFailure + 1 Success
                            // instead of collapsing every retry into
                            // whatever the eventual terminal outcome is.
                            let retry_error_class = err.class;
                            let retry_attempt_ms = attempt_started.elapsed().as_millis() as u64;
                            Engine::finish_attempt(
                                attempt_token,
                                &snap.resolver,
                                &snap.plugins,
                                &self.scorecard,
                                &scorecard_cfg,
                                AttemptFinishCtx {
                                    request_id: &req.id,
                                    target_id: &target.id,
                                    provider_id: &target.provider_id,
                                    account_id: &account_id,
                                    egress: egress_eff.as_str(),
                                    latency_ms: retry_attempt_ms,
                                },
                                FinishOutcome::Failed {
                                    error_class: retry_error_class,
                                },
                            );
                            retry_n += 1;
                            let delay = retry_backoff(retry, retry_n);
                            tracing::info!(
                                request_id = %req.id, target = %target.id, account = %account_id,
                                retry = retry_n, class = retry_error_class.as_str(),
                                delay_ms = delay.as_millis() as u64, "retrying transient failure"
                            );
                            tokio::time::sleep(delay).await;
                            let (attempt_req, attempt_target) = settings_plan.prepare(&req, target);
                            let prepared = PreparedRequest::new(
                                attempt_req,
                                attempt_target,
                                Some(lease.clone()),
                            )
                            .with_egress(egress_id.clone());
                            attempt_token = AttemptToken::new();
                            exec = adapter
                                .execute(prepared)
                                .instrument(attempt_span.clone())
                                .await;
                        }
                        match exec {
                            Ok(stream) => {
                                if req.stream {
                                    let stream = match precommit_stream(stream).await {
                                        Ok(stream) => stream,
                                        Err(error) => {
                                            snap.resolver.report_failure(
                                                &target.provider_id,
                                                &account_id,
                                                &target.model,
                                                error.class,
                                            );
                                            let fell_over = error.should_fallback();
                                            let attempt_ms =
                                                attempt_started.elapsed().as_millis() as u64;
                                            trace.attempt(sb_trace::Attempt::failed(
                                                &target.id,
                                                &target.provider_id,
                                                &target.model,
                                                &account_id,
                                                egress_eff.as_str(),
                                                attempt_ms,
                                                error.class.as_str(),
                                                fell_over,
                                            ));
                                            Engine::finish_attempt(
                                                attempt_token,
                                                &snap.resolver,
                                                &snap.plugins,
                                                &self.scorecard,
                                                &scorecard_cfg,
                                                AttemptFinishCtx {
                                                    request_id: &req.id,
                                                    target_id: &target.id,
                                                    provider_id: &target.provider_id,
                                                    account_id: &account_id,
                                                    egress: egress_eff.as_str(),
                                                    latency_ms: attempt_ms,
                                                },
                                                FinishOutcome::Failed {
                                                    error_class: error.class,
                                                },
                                            );
                                            if fell_over {
                                                tried_accounts.insert(account_id);
                                                last_err = Some(error);
                                                continue;
                                            }
                                            self.record_trace(trace.finish(
                                                error.class.http_status(),
                                                started.elapsed().as_millis() as u64,
                                                false,
                                            ));
                                            return ExecOutcome::Error(ExecError::upstream(
                                                &error, &summary,
                                            ));
                                        }
                                    };
                                    tracing::info!(
                                        request_id = %req.id, model = %req.model, target = %target.id,
                                        account = %account_id, status = 200u16,
                                        latency_ms = started.elapsed().as_millis() as u64, route = %summary
                                    );
                                    let (stream, quality_handle) = match quality_capture.take() {
                                        Some(capture) => {
                                            let (stream, handle) = super::quality_eval::tee_stream(
                                                stream,
                                                capture,
                                                target.id.clone(),
                                                super::quality_eval::QUALITY_CLASS.to_string(),
                                            );
                                            (stream, Some(handle))
                                        }
                                        None => (stream, None),
                                    };
                                    // Meter the stream: record usage/cost AND finalize
                                    // the trace when it completes (the terminal
                                    // UsageDelta is known only after the client drains
                                    // the stream). One callback does both.
                                    let ledger = self.ledger.clone();
                                    let usage_required = self.store_required;
                                    let traces = self.traces.clone();
                                    let trace_store = self.store();
                                    let registry = snap.registry.clone();
                                    let resolver = snap.resolver.clone();
                                    let plugins = snap.plugins.clone();
                                    let scorecard = self.scorecard.clone();
                                    let (rid, tid, pid, mdl, acct, egress, tnt, prj) = (
                                        req.id.clone(),
                                        target.id.clone(),
                                        target.provider_id.clone(),
                                        target.model.clone(),
                                        account_id.clone(),
                                        egress_eff.clone(),
                                        req.tenant.clone(),
                                        req.project.clone(),
                                    );
                                    // TTFT: record time-to-first-event against this
                                    // attempt's start, so interactive routing learns
                                    // each host's first-byte responsiveness.
                                    let registry_ttft = registry.clone();
                                    let (pid_ttft, mdl_ttft) =
                                        (target.provider_id.clone(), target.model.clone());
                                    let (stream, first_token) = watch_first_token(stream);
                                    let (stream, content_seen) =
                                        crate::optimize::watch_content(stream);
                                    let observer = self.observer(
                                        snap,
                                        target,
                                        &settings_plan,
                                        conversation.clone(),
                                        req.tenant.as_deref(),
                                        prompt_key.clone(),
                                    );
                                    let lane_busy = lane_busy.take();
                                    let metered = meter_stream(
                                        stream,
                                        attempt_started,
                                        move |ttft_ms| {
                                            registry_ttft.record_ttft(&pid_ttft, &mdl_ttft, ttft_ms)
                                        },
                                        move |usage, finish_reason, finish| {
                                            // Released when this finalizer
                                            // returns: last byte, upstream
                                            // error or client disconnect.
                                            let _lane_busy = lane_busy;
                                            trace.set_ttft_ms(ttft_ms(&first_token, started));
                                            let latency = started.elapsed().as_millis() as u64;
                                            let attempt_ms =
                                                attempt_started.elapsed().as_millis() as u64;
                                            match finish {
                                                StreamFinish::Clean => {
                                                    let mut usage = usage;
                                                    usage.fill_cached_from_timings();
                                                    let speed = attempt_speed(
                                                        &usage,
                                                        &first_token,
                                                        attempt_started,
                                                        Instant::now(),
                                                    );
                                                    observer.served(
                                                        &usage,
                                                        speed.prompt_tps,
                                                        crate::optimize::empty_after_thinking(
                                                            finish_reason,
                                                            content_seen.text(),
                                                            content_seen.reasoning(),
                                                            &usage,
                                                        ),
                                                    );
                                                    resolver.report_success(&pid, &acct);
                                                    trace.attempt(sb_trace::Attempt::success(
                                                        &tid,
                                                        &pid,
                                                        &mdl,
                                                        &acct,
                                                        egress.as_str(),
                                                        attempt_ms,
                                                    ));
                                                    registry.record_latency(
                                                        &pid,
                                                        &mdl,
                                                        attempt_ms as f64,
                                                    );
                                                    if let Some(tps) = speed.decode_tps {
                                                        registry.record_decode_tps(&pid, &mdl, tps);
                                                    }
                                                    let cost =
                                                        registry.cost_micros(&pid, &mdl, &usage);
                                                    let cache_savings = registry
                                                        .cache_savings_micros(&pid, &mdl, &usage);
                                                    Engine::finish_attempt(
                                                        attempt_token,
                                                        &resolver,
                                                        &plugins,
                                                        &scorecard,
                                                        &scorecard_cfg,
                                                        AttemptFinishCtx {
                                                            request_id: &rid,
                                                            target_id: &tid,
                                                            provider_id: &pid,
                                                            account_id: &acct,
                                                            egress: egress.as_str(),
                                                            latency_ms: attempt_ms,
                                                        },
                                                        FinishOutcome::Ok {
                                                            finish_reason: finish_reason
                                                                .unwrap_or(FinishReason::Stop),
                                                            cost_micros: Some(cost),
                                                        },
                                                    );
                                                    let usage_record =
                                                        sb_ledger::UsageRecord::priced(
                                                            rid,
                                                            pid,
                                                            mdl,
                                                            Some(acct),
                                                            usage.clone(),
                                                            latency,
                                                            true,
                                                            cost,
                                                        )
                                                        .with_tenant(tnt)
                                                        .with_project(prj)
                                                        .with_cache_savings(Some(cache_savings))
                                                        .with_speed(speed);
                                                    if usage_required {
                                                        if let Err(e) = ledger
                                                            .record_checked_post_commit(
                                                                usage_record,
                                                            )
                                                        {
                                                            tracing::error!(
                                                                error = %e,
                                                                "required usage store write failed after stream commit"
                                                            );
                                                        }
                                                    } else {
                                                        ledger.record(usage_record);
                                                    }
                                                    trace.set_usage(usage, cost);
                                                    super::trace_persist::record_trace_to(
                                                        &traces,
                                                        trace_store.as_ref(),
                                                        trace.finish(200, latency, true),
                                                    );
                                                    if let Some(handle) = quality_handle {
                                                        handle.complete(finish_reason);
                                                    }
                                                }
                                                StreamFinish::UpstreamError(class) => {
                                                    resolver
                                                        .report_failure(&pid, &acct, &mdl, class);
                                                    trace.attempt(sb_trace::Attempt::failed(
                                                        &tid,
                                                        &pid,
                                                        &mdl,
                                                        &acct,
                                                        egress.as_str(),
                                                        attempt_ms,
                                                        class.as_str(),
                                                        false,
                                                    ));
                                                    Engine::finish_attempt(
                                                        attempt_token,
                                                        &resolver,
                                                        &plugins,
                                                        &scorecard,
                                                        &scorecard_cfg,
                                                        AttemptFinishCtx {
                                                            request_id: &rid,
                                                            target_id: &tid,
                                                            provider_id: &pid,
                                                            account_id: &acct,
                                                            egress: egress.as_str(),
                                                            latency_ms: attempt_ms,
                                                        },
                                                        FinishOutcome::Failed {
                                                            error_class: class,
                                                        },
                                                    );
                                                    super::trace_persist::record_trace_to(
                                                        &traces,
                                                        trace_store.as_ref(),
                                                        trace.finish(
                                                            class.http_status(),
                                                            latency,
                                                            true,
                                                        ),
                                                    );
                                                }
                                                StreamFinish::Aborted => {
                                                    tracing::info!(
                                                        request_id = %rid, latency_ms = latency,
                                                        "client aborted stream"
                                                    );
                                                    trace.attempt(sb_trace::Attempt::failed(
                                                        &tid,
                                                        &pid,
                                                        &mdl,
                                                        &acct,
                                                        egress.as_str(),
                                                        attempt_ms,
                                                        "client_aborted",
                                                        false,
                                                    ));
                                                    Engine::finish_attempt(
                                                        attempt_token,
                                                        &resolver,
                                                        &plugins,
                                                        &scorecard,
                                                        &scorecard_cfg,
                                                        AttemptFinishCtx {
                                                            request_id: &rid,
                                                            target_id: &tid,
                                                            provider_id: &pid,
                                                            account_id: &acct,
                                                            egress: egress.as_str(),
                                                            latency_ms: attempt_ms,
                                                        },
                                                        FinishOutcome::Cancelled,
                                                    );
                                                    super::trace_persist::record_trace_to(
                                                        &traces,
                                                        trace_store.as_ref(),
                                                        trace.finish(499, latency, true),
                                                    );
                                                }
                                            }
                                        },
                                    );
                                    return ExecOutcome::Stream {
                                        stream: metered,
                                        summary,
                                    };
                                }

                                let (stream, first_token) = watch_first_token(stream);
                                match collect_response(
                                    stream,
                                    req.id.clone(),
                                    req.model.clone(),
                                    snap.config.server.max_response_bytes,
                                )
                                .await
                                {
                                    Ok(mut response) => {
                                        // Before the cost, as streams do: a
                                        // cache the engine reports only in
                                        // its timings is priced as cached.
                                        response.usage.fill_cached_from_timings();
                                        trace.set_ttft_ms(ttft_ms(&first_token, started));
                                        snap.resolver
                                            .report_success(&target.provider_id, &account_id);
                                        tracing::info!(
                                            request_id = %req.id, model = %req.model, target = %target.id,
                                            account = %account_id, status = 200u16,
                                            latency_ms = started.elapsed().as_millis() as u64, route = %summary
                                        );
                                        let attempt_ms =
                                            attempt_started.elapsed().as_millis() as u64;
                                        let cost = snap.registry.cost_micros(
                                            &target.provider_id,
                                            &target.model,
                                            &response.usage,
                                        );
                                        // outcome-routing-v1 F8: finalize the
                                        // attempt (upstream truth: this WAS a
                                        // success) BEFORE the usage-
                                        // persistence step below — a required
                                        // usage-store failure still fails the
                                        // client-facing response closed
                                        // (unchanged), but the scorecard/
                                        // breaker/plugins must learn about
                                        // this real upstream success
                                        // regardless of whether persistence
                                        // succeeds.
                                        Engine::finish_attempt(
                                            attempt_token,
                                            &snap.resolver,
                                            &snap.plugins,
                                            &self.scorecard,
                                            &scorecard_cfg,
                                            AttemptFinishCtx {
                                                request_id: &req.id,
                                                target_id: &target.id,
                                                provider_id: &target.provider_id,
                                                account_id: &account_id,
                                                egress: egress_eff.as_str(),
                                                latency_ms: attempt_ms,
                                            },
                                            FinishOutcome::Ok {
                                                finish_reason: response.finish_reason,
                                                cost_micros: Some(cost),
                                            },
                                        );
                                        let speed = attempt_speed(
                                            &response.usage,
                                            &first_token,
                                            attempt_started,
                                            Instant::now(),
                                        );
                                        self.observer(
                                            snap,
                                            target,
                                            &settings_plan,
                                            conversation.clone(),
                                            req.tenant.as_deref(),
                                            prompt_key.clone(),
                                        )
                                        .served(
                                            &response.usage,
                                            speed.prompt_tps,
                                            crate::optimize::response_empty_after_thinking(
                                                &response,
                                            ),
                                        );
                                        if let Err(e) = self.record_usage(
                                            &snap.registry,
                                            &req.id,
                                            &target.provider_id,
                                            &target.model,
                                            &account_id,
                                            req.tenant.as_deref(),
                                            req.project.as_deref(),
                                            response.usage.clone(),
                                            started,
                                            false,
                                            speed,
                                        ) {
                                            let message = format!("usage persistence failed: {e}");
                                            self.record_trace(trace.finish(
                                                500,
                                                started.elapsed().as_millis() as u64,
                                                false,
                                            ));
                                            return ExecOutcome::Error(ExecError::new(
                                                500,
                                                "usage_persistence_failed",
                                                message,
                                                None,
                                            ));
                                        }
                                        trace.attempt(sb_trace::Attempt::success(
                                            &target.id,
                                            &target.provider_id,
                                            &target.model,
                                            &account_id,
                                            egress_eff.as_str(),
                                            attempt_ms,
                                        ));
                                        snap.registry.record_latency(
                                            &target.provider_id,
                                            &target.model,
                                            attempt_ms as f64,
                                        );
                                        trace.set_usage(response.usage.clone(), cost);
                                        if let Some(scope) = scope {
                                            scope.record_success(&target.id, cost);
                                        }
                                        self.record_trace(trace.finish(
                                            200,
                                            started.elapsed().as_millis() as u64,
                                            false,
                                        ));
                                        if let Some(capture) = quality_capture.take() {
                                            capture.complete_response(
                                                target.id.clone(),
                                                super::quality_eval::QUALITY_CLASS.to_string(),
                                                &response,
                                            );
                                        }
                                        return ExecOutcome::Collected {
                                            response: Box::new(response),
                                            summary,
                                        };
                                    }
                                    Err(error) => {
                                        snap.resolver.report_failure(
                                            &target.provider_id,
                                            &account_id,
                                            &target.model,
                                            error.class,
                                        );
                                        let fell_over = error.should_fallback();
                                        let attempt_ms =
                                            attempt_started.elapsed().as_millis() as u64;
                                        trace.attempt(sb_trace::Attempt::failed(
                                            &target.id,
                                            &target.provider_id,
                                            &target.model,
                                            &account_id,
                                            egress_eff.as_str(),
                                            attempt_ms,
                                            error.class.as_str(),
                                            fell_over,
                                        ));
                                        // NOTE: the pre-refactor code path here called
                                        // `circuit_record` but — unlike every other
                                        // terminal branch — never called `post_attempt`.
                                        // Consolidating into one seam means this branch
                                        // now gets the same plugin observation + scorecard
                                        // record as every other failure path; leaving a
                                        // non-stream collect failure unscored would be a
                                        // gap in the very feature this commit wires up.
                                        Engine::finish_attempt(
                                            attempt_token,
                                            &snap.resolver,
                                            &snap.plugins,
                                            &self.scorecard,
                                            &scorecard_cfg,
                                            AttemptFinishCtx {
                                                request_id: &req.id,
                                                target_id: &target.id,
                                                provider_id: &target.provider_id,
                                                account_id: &account_id,
                                                egress: egress_eff.as_str(),
                                                latency_ms: attempt_ms,
                                            },
                                            FinishOutcome::Failed {
                                                error_class: error.class,
                                            },
                                        );
                                        if fell_over {
                                            tried_accounts.insert(account_id);
                                            last_err = Some(error);
                                            continue;
                                        }
                                        self.record_trace(trace.finish(
                                            error.class.http_status(),
                                            started.elapsed().as_millis() as u64,
                                            false,
                                        ));
                                        return ExecOutcome::Error(ExecError::upstream(
                                            &error, &summary,
                                        ));
                                    }
                                }
                            }
                            Err(error) => {
                                snap.resolver.report_failure(
                                    &target.provider_id,
                                    &account_id,
                                    &target.model,
                                    error.class,
                                );
                                let fell_over = error.should_fallback();
                                let attempt_ms = attempt_started.elapsed().as_millis() as u64;
                                trace.attempt(sb_trace::Attempt::failed(
                                    &target.id,
                                    &target.provider_id,
                                    &target.model,
                                    &account_id,
                                    egress_eff.as_str(),
                                    attempt_ms,
                                    error.class.as_str(),
                                    fell_over,
                                ));
                                Engine::finish_attempt(
                                    attempt_token,
                                    &snap.resolver,
                                    &snap.plugins,
                                    &self.scorecard,
                                    &scorecard_cfg,
                                    AttemptFinishCtx {
                                        request_id: &req.id,
                                        target_id: &target.id,
                                        provider_id: &target.provider_id,
                                        account_id: &account_id,
                                        egress: egress_eff.as_str(),
                                        latency_ms: attempt_ms,
                                    },
                                    FinishOutcome::Failed {
                                        error_class: error.class,
                                    },
                                );
                                if fell_over {
                                    tried_accounts.insert(account_id);
                                    last_err = Some(error);
                                    continue;
                                }
                                self.record_trace(trace.finish(
                                    error.class.http_status(),
                                    started.elapsed().as_millis() as u64,
                                    false,
                                ));
                                return ExecOutcome::Error(ExecError::upstream(&error, &summary));
                            }
                        }
                    }
                    ResolveOutcome::AllUnavailable { retry_after } => {
                        if let Some(retry_after) = retry_after {
                            unavailable_retry_after = Some(
                                unavailable_retry_after
                                    .map_or(retry_after, |current| current.min(retry_after)),
                            );
                        }
                        continue 'targets;
                    }
                    ResolveOutcome::NoAccounts => continue 'targets,
                }
            }
        }

        if let Some(error) = last_err {
            self.record_trace(trace.finish(
                error.class.http_status(),
                started.elapsed().as_millis() as u64,
                false,
            ));
            return ExecOutcome::Error(ExecError::upstream(&error, &summary));
        }

        if let Some(retry_after) = unavailable_retry_after {
            self.record_trace(trace.finish(503, started.elapsed().as_millis() as u64, false));
            return ExecOutcome::Error(
                ExecError::new(
                    503,
                    "provider_unavailable",
                    format!(
                        "all accounts for eligible targets are temporarily unavailable; retry after {}ms",
                        retry_after.as_millis()
                    ),
                    Some(summary),
                )
                .with_retry_after(retry_after),
            );
        }

        let rejected = plan
            .decision
            .rejected
            .iter()
            .map(|rejected| format!("{}:{}", rejected.target_id, rejected.reason))
            .collect::<Vec<_>>()
            .join(",");
        self.record_trace(trace.finish(400, started.elapsed().as_millis() as u64, false));
        ExecOutcome::Error(ExecError::new(
            400,
            "invalid_request_error",
            format!(
                "no eligible target: rejected={} unknown=[{}]",
                rejected,
                unknown.join(",")
            ),
            Some(summary),
        ))
    }
}
