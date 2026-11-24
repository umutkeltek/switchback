//! `/v1/audio/*` through the same runtime as chat and embeddings: route
//! resolution with a hard `audio_in` (speech to text) or `audio_out` (text
//! to speech) capability filter, local-lane wake, limits and GPU groups,
//! target and account fallback, budgets, plugins, traces and one usage
//! record per served request. The body is passed through: the gateway only
//! sets `model` to the target's own model id. Audio is never cached.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use sb_adapter::{AdapterError, AudioBody, AudioCall, AudioRequest};
use sb_core::{AiRequest, ErrorClass, FinishReason, PricingUnit, UnitPrice, WorkloadKind};
use sb_credentials::ResolveOutcome;

use super::finish_attempt::{AttemptFinishCtx, AttemptToken, FinishOutcome};
use super::helpers::{resolve_egress, session_affinity_key};
use super::outcome::audio_seconds;
use super::profiles::{plan_resolved_route, resolve_candidates, tenant_allowed_accounts};
use super::stream::StreamFinish;
use super::{AudioOutcome, DenialTrace, Engine, ExecError, LaneBusy, Snapshot};

/// A collected transcript larger than this (with no
/// `server.max_response_bytes`) is refused rather than held in memory.
const DEFAULT_TRANSCRIPT_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// One audio request as the HTTP edge parsed it.
#[derive(Debug, Clone)]
pub struct AudioInput {
    pub call: AudioCall,
    /// The model the client asked for (a route, alias or `provider/model`).
    pub model: String,
    /// The body to forward; its `model` is replaced per target.
    pub body: AudioBody,
    /// The client asked for a streamed transcript (`stream=true`).
    pub stream: bool,
    /// Characters of input text (speech only), for the usage record.
    pub input_chars: Option<u64>,
}

impl AudioInput {
    fn kind(&self) -> WorkloadKind {
        match self.call {
            AudioCall::Transcription => WorkloadKind::AudioTranscription,
            AudioCall::Translation => WorkloadKind::AudioTranslation,
            AudioCall::Speech => WorkloadKind::AudioSpeech,
        }
    }

    fn kind_name(&self) -> &'static str {
        match self.call {
            AudioCall::Transcription => "audio_transcription",
            AudioCall::Translation => "audio_translation",
            AudioCall::Speech => "audio_speech",
        }
    }

    /// The body for one target: the same fields, `model` set to the
    /// target's own model id. File bytes are shared, not copied.
    fn body_for(&self, model: &str) -> AudioBody {
        match &self.body {
            AudioBody::Json(value) => {
                let mut value = value.clone();
                if let Some(object) = value.as_object_mut() {
                    object.insert(
                        "model".to_string(),
                        serde_json::Value::String(model.to_string()),
                    );
                }
                AudioBody::Json(value)
            }
            AudioBody::Form(parts) => AudioBody::Form(
                parts
                    .iter()
                    .map(|part| {
                        let mut part = part.clone();
                        if part.name == "model" && part.filename.is_none() {
                            part.data = bytes::Bytes::copy_from_slice(model.as_bytes());
                        }
                        part
                    })
                    .collect(),
            ),
        }
    }

    /// Whether the gateway reads the whole answer before it sends it: a
    /// transcript, small, whose `duration` goes into the usage record.
    /// Speech and streamed transcripts pass through as they arrive.
    fn collects(&self) -> bool {
        self.call != AudioCall::Speech && !self.stream
    }
}

impl Engine {
    /// Execute one `/v1/audio/*` request. The HTTP edge stays responsible
    /// for auth, admission, the upload limit and wire parsing.
    pub async fn execute_audio(
        &self,
        input: AudioInput,
        tenant: Option<String>,
        project: Option<String>,
        session_id: Option<String>,
        started: Instant,
    ) -> (u64, AudioOutcome) {
        let snap = self.snapshot();
        let revision = snap.revision;
        let outcome = self
            .execute_audio_inner(&snap, input, tenant, project, session_id, started)
            .await;
        (revision, outcome)
    }

    fn audio_denied(
        &self,
        snap: &Snapshot,
        req: &AiRequest,
        error: ExecError,
        started: Instant,
    ) -> AudioOutcome {
        self.record_denial_trace(DenialTrace {
            request_id: &req.id,
            revision: snap.revision,
            tenant: req.tenant.as_deref(),
            project: req.project.as_deref(),
            inbound_model: &req.model,
            status: error.status,
            error_type: &error.error_type,
            message: &error.message,
            started,
            streamed: false,
        });
        AudioOutcome::Error {
            request_id: req.id.clone(),
            error,
        }
    }

    async fn execute_audio_inner(
        &self,
        snap: &Snapshot,
        input: AudioInput,
        tenant: Option<String>,
        project: Option<String>,
        session_id: Option<String>,
        started: Instant,
    ) -> AudioOutcome {
        let mut req = AiRequest::new(input.model.clone(), Vec::new());
        req.tenant = tenant;
        req.project = project;
        if let Some(session_id) = session_id {
            req.metadata.insert("session_id".to_string(), session_id);
        }
        if input.model.is_empty() {
            let error = ExecError::new(400, "invalid_request_error", "missing \"model\"", None);
            return self.audio_denied(snap, &req, error, started);
        }

        if let Some(max) = snap.runtime.budget_max_usd {
            match self.global_spend_usd() {
                Err(e) => {
                    let error = ExecError::new(
                        503,
                        "usage_store_unavailable",
                        format!("usage store unavailable for budget check: {e}"),
                        None,
                    );
                    return self.audio_denied(snap, &req, error, started);
                }
                Ok(spent) if spent >= max => {
                    let error = ExecError::new(
                        402,
                        "budget_exceeded",
                        format!("budget exceeded: spent ${spent:.4} of ${max:.4} cap"),
                        None,
                    );
                    return self.audio_denied(snap, &req, error, started);
                }
                Ok(_) => {}
            }
        }
        if let Some(tenant) = req.tenant.clone() {
            if let Some(budget) = snap.config.tenant(&tenant).and_then(|t| t.budget_usd) {
                match self.tenant_spend_usd(&tenant) {
                    Err(e) => {
                        let error = ExecError::new(
                            503,
                            "usage_store_unavailable",
                            format!("usage store unavailable for budget check: {e}"),
                            None,
                        );
                        return self.audio_denied(snap, &req, error, started);
                    }
                    Ok(spent) if spent >= budget => {
                        let error = ExecError::new(
                            402,
                            "tenant_budget_exceeded",
                            format!(
                                "tenant `{tenant}` budget exceeded: spent ${spent:.4} of ${budget:.4} cap"
                            ),
                            None,
                        );
                        return self.audio_denied(snap, &req, error, started);
                    }
                    Ok(_) => {}
                }
            }
        }

        if let sb_plugin::PluginOutcome::Reject { status, message } =
            snap.plugins.pre_route(&mut req)
        {
            let error = ExecError::new(status, "plugin_rejected", message, None);
            return self.audio_denied(snap, &req, error, started);
        }

        let mut resolved = match resolve_candidates(
            snap,
            &req.model,
            &self.scorecard,
            self.lanes.as_deref(),
            &self.route_roles,
        ) {
            Ok(resolved) => resolved,
            Err(error) => return self.audio_denied(snap, &req, error, started),
        };
        // The hard capability filter: a target whose provider does not
        // declare it is rejected by the router with the reason, like vision.
        // Set before the lane wake and lane limit plans, so they never pick
        // a target that cannot take audio.
        match input.call {
            AudioCall::Speech => resolved.require.audio_out = Some(true),
            AudioCall::Transcription | AudioCall::Translation => {
                resolved.require.audio_in = Some(true)
            }
        }
        // Tuning locks, as for chat (see `execute`).
        let tune_rejected = match self.apply_tune_locks(snap, &req, &mut resolved) {
            Ok(rejected) => rejected,
            Err(error) => return self.audio_denied(snap, &req, error, started),
        };
        let unknown = resolved.unknown.clone();
        // Lanes with an engine control: the request counts on its first
        // choice's engine model from here, as for chat (see `execute`).
        let first_admission = match self
            .admit_first_choice(snap, &req, None, &mut resolved)
            .await
        {
            Ok(admission) => admission,
            Err(error) => return self.audio_denied(snap, &req, error, started),
        };
        let lane_wake = self
            .wake_lane_before_dispatch(snap, &req, None, &mut resolved)
            .await;
        let mut lane_gpu = self
            .take_lane_before_dispatch(snap, &req, None, &mut resolved)
            .await;
        let planned = plan_resolved_route(&self.combo_rr, snap, &req, None, resolved, true)
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
            Err(error) => return self.audio_denied(snap, &req, error, started),
        };
        for (target_id, reason) in tune_rejected {
            plan.decision.reject(target_id, reason);
        }
        // Per-model admissions (engine control): the first choice's, kept
        // while the plan still has its target.
        let mut admissions = crate::model_settings::admissions_only(
            &self.model_settings,
            self.model_variants.get().cloned(),
        );
        if let Some((target_id, admission)) = first_admission {
            if plan
                .candidates
                .iter()
                .any(|candidate| candidate.id == target_id)
            {
                admissions.add_admission(&target_id, admission);
            }
        }
        snap.plugins.post_route(&req, &plan.decision);
        let summary = format!("{} {}", plan.decision.summary(), input.kind_name());
        let mut trace = sb_trace::RequestTrace::start(
            req.id.clone(),
            snap.revision,
            req.model.clone(),
            route_name,
            plan.decision.clone(),
        )
        .with_principal(req.tenant.clone(), req.project.clone())
        .with_session_id(session_affinity_key(&req).map(str::to_string));
        let mut last_err: Option<AdapterError> = None;
        let mut unavailable_retry_after: Option<Duration> = None;

        'targets: for (index, target) in plan.candidates.iter().enumerate() {
            let Some(adapter) = snap.registry.adapter(&target.provider_id) else {
                admissions.release(&target.id);
                continue 'targets;
            };
            if !snap.resolver.circuit_allows(&target.provider_id) {
                admissions.release(&target.id);
                continue 'targets;
            }
            if let Some(cap) = snap
                .config
                .server
                .budget
                .per_provider_usd
                .get(&target.provider_id)
            {
                match self.provider_spend_usd(&target.provider_id) {
                    Err(e) => {
                        let error = ExecError::new(
                            503,
                            "usage_store_unavailable",
                            format!("usage store unavailable for budget check: {e}"),
                            None,
                        );
                        return self.audio_denied(snap, &req, error, started);
                    }
                    Ok(spent) if spent >= *cap => {
                        admissions.release(&target.id);
                        continue 'targets;
                    }
                    Ok(_) => {}
                }
            }

            // The engine model's admission, before the lane's place (a
            // closed gate is waited for only by the last target).
            let may_wait = index + 1 == plan.candidates.len();
            let model_admission = match admissions.admission_for(snap, target, may_wait).await {
                Ok(admission) => admission,
                Err(e) => {
                    tracing::info!(
                        request_id = %req.id, target = %target.id,
                        "audio target skipped: {}", e.message
                    );
                    trace.warning(format!("target {} skipped: {}", target.id, e.message));
                    let retry_after = e.retry_after.unwrap_or(crate::LANE_GROUP_RETRY_AFTER);
                    unavailable_retry_after = Some(
                        unavailable_retry_after
                            .map_or(retry_after, |current| current.min(retry_after)),
                    );
                    continue 'targets;
                }
            };
            // Lanes that count work (wake on request, a limit, a GPU group,
            // a shared host) hold this request as in flight until its
            // answer has been sent: the guard travels with the body.
            let held = lane_gpu
                .as_mut()
                .and_then(|note| note.take_busy(&target.provider_id));
            let mut lane_busy = match held {
                Some(busy) => Some(busy),
                None => match self.lane_busy(snap, &target.provider_id) {
                    Ok(busy) => busy,
                    Err(reason) => {
                        tracing::info!(
                            request_id = %req.id, target = %target.id,
                            "audio target skipped: local lane {reason}"
                        );
                        continue 'targets;
                    }
                },
            };
            // Dispatch on the engine model; counted with the lane's guard,
            // which travels with the body: an unload or preset re-read waits
            // for the answer's end.
            if let Some(mut model_busy) = model_admission {
                if let Err(e) = admissions.dispatch(snap, target, &mut model_busy).await {
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
            let mut tried_accounts = HashSet::new();
            let allowed_accounts = req
                .tenant
                .as_deref()
                .and_then(|tenant_id| snap.config.tenant(tenant_id))
                .filter(|tenant| !tenant.allowed_accounts.is_empty())
                .map(|tenant| tenant_allowed_accounts(tenant, &target.provider_id));
            loop {
                let (account_id, lease) = match snap.resolver.resolve_with_session_allowed(
                    &target.provider_id,
                    &target.model,
                    &tried_accounts,
                    session_affinity_key(&req),
                    allowed_accounts.as_ref(),
                ) {
                    ResolveOutcome::Selected { account_id, lease } => (account_id, lease),
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
                };
                let attempt_started = Instant::now();
                let egress_id = snap
                    .plugins
                    .select_egress(&req, &target.id)
                    .or_else(|| resolve_egress(&snap.config, &target.provider_id, &account_id));
                let egress_eff = snap.registry.effective_egress(egress_id.as_deref());
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

                let attempt_token = AttemptToken::new();
                let scorecard_cfg = snap.config.server.scorecard.clone();
                let request = AudioRequest {
                    call: input.call,
                    body: input.body_for(&target.model),
                };
                let attempt = match adapter
                    .audio(request, target.clone(), Some(lease), egress_id.clone())
                    .await
                {
                    Ok(response) => {
                        let streams_through = !input.collects()
                            || response
                                .content_type
                                .as_deref()
                                .is_some_and(|ct| ct.starts_with("text/event-stream"));
                        if streams_through {
                            Ok((response, None))
                        } else {
                            let limit = snap
                                .config
                                .server
                                .max_response_bytes
                                .unwrap_or(DEFAULT_TRANSCRIPT_MAX_BYTES);
                            match collect_body(response.body, limit).await {
                                Ok(bytes) => Ok((
                                    sb_adapter::AudioResponse {
                                        status: response.status,
                                        content_type: response.content_type,
                                        body: futures::stream::empty().boxed(),
                                    },
                                    Some(bytes),
                                )),
                                Err(error) => Err(error),
                            }
                        }
                    }
                    Err(error) => Err(error),
                };
                let attempt_ms = attempt_started.elapsed().as_millis() as u64;
                match attempt {
                    Ok((response, collected)) => {
                        let (unit, units) = match (input.call, &collected) {
                            (AudioCall::Speech, _) => (
                                Some(PricingUnit::PerCharacter),
                                input.input_chars.map(|n| n as f64),
                            ),
                            (_, Some(bytes)) => {
                                let seconds = serde_json::from_slice::<serde_json::Value>(bytes)
                                    .ok()
                                    .as_ref()
                                    .and_then(audio_seconds);
                                (seconds.map(|_| PricingUnit::PerSecond), seconds)
                            }
                            (_, None) => (None, None),
                        };
                        let cost = audio_cost(
                            snap.registry.unit_price(&target.provider_id, &target.model),
                            unit,
                            units,
                        );
                        let usage = AudioUsage {
                            request_id: req.id.clone(),
                            provider_id: target.provider_id.clone(),
                            model: target.model.clone(),
                            account_id: account_id.clone(),
                            kind: input.kind(),
                            unit,
                            units,
                            cost,
                            tenant: req.tenant.clone(),
                            project: req.project.clone(),
                            requested_model: (input.model != target.id
                                && input.model != target.model)
                                .then(|| input.model.clone()),
                        };
                        let Some(collected) = collected else {
                            // A streamed answer (speech, a transcript as
                            // events) is only a success once its last byte
                            // has gone out: the outcome, usage and trace
                            // are written by a guard on the body, as for a
                            // streamed chat answer.
                            let finish = StreamedAudioFinish {
                                usage,
                                trace,
                                attempt_token,
                                scorecard_cfg,
                                target_id: target.id.clone(),
                                egress: egress_eff.clone(),
                                started,
                                attempt_started,
                                lane_busy: lane_busy.take(),
                                ledger: self.ledger.clone(),
                                usage_required: self.store_required,
                                traces: self.traces.clone(),
                                trace_store: self.store(),
                                registry: snap.registry.clone(),
                                resolver: snap.resolver.clone(),
                                plugins: snap.plugins.clone(),
                                scorecard: self.scorecard.clone(),
                            };
                            let body = guard_body(response.body, move |end| finish.run(end));
                            return AudioOutcome::Body {
                                status: response.status,
                                content_type: response.content_type,
                                body,
                                summary,
                                request_id: req.id,
                            };
                        };
                        snap.resolver
                            .report_success(&target.provider_id, &account_id);
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
                                finish_reason: FinishReason::Stop,
                                cost_micros: cost,
                            },
                        );
                        let latency = started.elapsed().as_millis() as u64;
                        let record = usage.record(latency, false);
                        let recorded = if self.store_required {
                            self.ledger.record_checked(record)
                        } else {
                            self.ledger.record(record);
                            Ok(())
                        };
                        if let Err(e) = recorded {
                            self.record_trace(trace.finish(500, latency, false));
                            return AudioOutcome::Error {
                                request_id: req.id,
                                error: ExecError::new(
                                    500,
                                    "usage_persistence_failed",
                                    format!("usage persistence failed: {e}"),
                                    None,
                                ),
                            };
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
                        self.record_trace(trace.finish(200, latency, false));
                        drop(lane_busy.take());
                        return AudioOutcome::Body {
                            status: response.status,
                            content_type: response.content_type,
                            body: futures::stream::once(async move { Ok(collected) }).boxed(),
                            summary,
                            request_id: req.id,
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
                        return AudioOutcome::Error {
                            request_id: req.id,
                            error: ExecError::upstream(&error, &summary),
                        };
                    }
                }
            }
        }

        if let Some(error) = last_err {
            self.record_trace(trace.finish(
                error.class.http_status(),
                started.elapsed().as_millis() as u64,
                false,
            ));
            return AudioOutcome::Error {
                request_id: req.id,
                error: ExecError::upstream(&error, &summary),
            };
        }
        if let Some(retry_after) = unavailable_retry_after {
            self.record_trace(trace.finish(503, started.elapsed().as_millis() as u64, false));
            return AudioOutcome::Error {
                request_id: req.id,
                error: ExecError::new(
                    503,
                    "provider_unavailable",
                    format!(
                        "all accounts for eligible targets are temporarily unavailable; retry after {}ms",
                        retry_after.as_millis()
                    ),
                    Some(summary),
                )
                .with_retry_after(retry_after),
            };
        }
        let rejected = plan
            .decision
            .rejected
            .iter()
            .map(|rejected| format!("{}:{}", rejected.target_id, rejected.reason))
            .collect::<Vec<_>>()
            .join(",");
        self.record_trace(trace.finish(400, started.elapsed().as_millis() as u64, false));
        AudioOutcome::Error {
            request_id: req.id,
            error: ExecError::new(
                400,
                "invalid_request_error",
                format!(
                    "no eligible target: rejected={} unknown=[{}]",
                    rejected,
                    unknown.join(",")
                ),
                Some(summary),
            ),
        }
    }
}

/// The price of one audio answer, when the registry prices this model in
/// the unit the answer was measured in (seconds of audio for a transcript,
/// characters for speech). A price in another unit is not applied: the cost
/// stays unknown rather than wrong.
fn audio_cost(
    price: Option<UnitPrice>,
    unit: Option<PricingUnit>,
    units: Option<f64>,
) -> Option<u64> {
    let price = price.filter(|price| Some(price.pricing_unit) == unit)?;
    sb_ledger::compute_unit_cost_micros(Some(price), units)
}

/// What the usage record of one served audio request says, known when the
/// engine's answer starts; only the latency waits for its end.
struct AudioUsage {
    request_id: String,
    provider_id: String,
    model: String,
    account_id: String,
    kind: WorkloadKind,
    unit: Option<PricingUnit>,
    units: Option<f64>,
    cost: Option<u64>,
    tenant: Option<String>,
    project: Option<String>,
    requested_model: Option<String>,
}

impl AudioUsage {
    fn record(self, latency_ms: u64, streamed: bool) -> sb_ledger::UsageRecord {
        let record = sb_ledger::UsageRecord::workload(
            self.request_id,
            self.provider_id,
            self.model,
            Some(self.account_id),
            self.kind,
            self.unit,
            self.units,
            self.cost,
            latency_ms,
        )
        .with_tenant(self.tenant)
        .with_project(self.project)
        .with_requested_model(self.requested_model);
        sb_ledger::UsageRecord { streamed, ..record }
    }
}

/// Everything the end of a streamed audio answer writes: the account and
/// scorecard outcome, the usage record and the trace. Owned, because it runs
/// after `execute_audio` has returned.
struct StreamedAudioFinish {
    usage: AudioUsage,
    trace: sb_trace::RequestTrace,
    attempt_token: AttemptToken,
    scorecard_cfg: crate::scorecard::ScorecardConfig,
    target_id: String,
    egress: String,
    started: Instant,
    attempt_started: Instant,
    /// The lane place, given back when this runs.
    lane_busy: Option<LaneBusy>,
    ledger: Arc<sb_ledger::UsageLedger>,
    usage_required: bool,
    traces: Arc<sb_trace::TraceLog>,
    trace_store: Option<Arc<dyn sb_store::StateStore>>,
    registry: Arc<sb_adapters::AdapterRegistry>,
    resolver: Arc<sb_credentials::CredentialResolver>,
    plugins: sb_plugin::PluginHost,
    scorecard: Arc<crate::scorecard::Scorecard>,
}

impl StreamedAudioFinish {
    /// Mirrors the streamed chat finalizer: a clean end is a success with
    /// usage; an engine that breaks off is a failure with the error's
    /// status; a client that goes away is `client_aborted` (499). Neither
    /// failure writes usage.
    fn run(self, end: StreamFinish) {
        let StreamedAudioFinish {
            usage,
            mut trace,
            attempt_token,
            scorecard_cfg,
            target_id,
            egress,
            started,
            attempt_started,
            lane_busy,
            ledger,
            usage_required,
            traces,
            trace_store,
            registry,
            resolver,
            plugins,
            scorecard,
        } = self;
        let _lane_busy = lane_busy;
        let latency = started.elapsed().as_millis() as u64;
        let attempt_ms = attempt_started.elapsed().as_millis() as u64;
        let (request_id, provider_id, model, account_id) = (
            usage.request_id.clone(),
            usage.provider_id.clone(),
            usage.model.clone(),
            usage.account_id.clone(),
        );
        let ctx = AttemptFinishCtx {
            request_id: &request_id,
            target_id: &target_id,
            provider_id: &provider_id,
            account_id: &account_id,
            egress: egress.as_str(),
            latency_ms: attempt_ms,
        };
        let (attempt, outcome, status) = match end {
            StreamFinish::Clean => {
                resolver.report_success(&provider_id, &account_id);
                registry.record_latency(&provider_id, &model, attempt_ms as f64);
                let cost = usage.cost;
                let record = usage.record(latency, true);
                if usage_required {
                    if let Err(e) = ledger.record_checked_post_commit(record) {
                        tracing::error!(
                            error = %e,
                            "required usage store write failed after audio stream commit"
                        );
                    }
                } else {
                    ledger.record(record);
                }
                (
                    sb_trace::Attempt::success(
                        &target_id,
                        &provider_id,
                        &model,
                        &account_id,
                        egress.as_str(),
                        attempt_ms,
                    ),
                    FinishOutcome::Ok {
                        finish_reason: FinishReason::Stop,
                        cost_micros: cost,
                    },
                    200,
                )
            }
            StreamFinish::UpstreamError(class) => {
                resolver.report_failure(&provider_id, &account_id, &model, class);
                (
                    sb_trace::Attempt::failed(
                        &target_id,
                        &provider_id,
                        &model,
                        &account_id,
                        egress.as_str(),
                        attempt_ms,
                        class.as_str(),
                        false,
                    ),
                    FinishOutcome::Failed { error_class: class },
                    class.http_status(),
                )
            }
            StreamFinish::Aborted => {
                tracing::info!(
                    request_id = %request_id, latency_ms = latency,
                    "client aborted audio stream"
                );
                (
                    sb_trace::Attempt::failed(
                        &target_id,
                        &provider_id,
                        &model,
                        &account_id,
                        egress.as_str(),
                        attempt_ms,
                        "client_aborted",
                        false,
                    ),
                    FinishOutcome::Cancelled,
                    499,
                )
            }
        };
        trace.attempt(attempt);
        Engine::finish_attempt(
            attempt_token,
            &resolver,
            &plugins,
            &scorecard,
            &scorecard_cfg,
            ctx,
            outcome,
        );
        super::trace_persist::record_trace_to(
            &traces,
            trace_store.as_ref(),
            trace.finish(status, latency, true),
        );
    }
}

/// Fires `on_finish` exactly once: at the clean end of the body, at the
/// first upstream error, or (still armed when dropped) when the client
/// went away first.
struct BodyGuard<F: FnOnce(StreamFinish)> {
    on_finish: Option<F>,
}

impl<F: FnOnce(StreamFinish)> BodyGuard<F> {
    fn fire(&mut self, end: StreamFinish) {
        if let Some(finish) = self.on_finish.take() {
            finish(end);
        }
    }
}

impl<F: FnOnce(StreamFinish)> Drop for BodyGuard<F> {
    fn drop(&mut self) {
        self.fire(StreamFinish::Aborted);
    }
}

type AudioBodyStream = futures::stream::BoxStream<'static, Result<bytes::Bytes, AdapterError>>;

/// Wrap a streamed audio body so its end, an engine break-off or a client
/// hang-up is recorded once.
fn guard_body<F>(body: AudioBodyStream, on_finish: F) -> AudioBodyStream
where
    F: FnOnce(StreamFinish) + Send + 'static,
{
    let guard = BodyGuard {
        on_finish: Some(on_finish),
    };
    futures::stream::unfold((body, guard), |(mut body, mut guard)| async move {
        match body.next().await {
            Some(Ok(chunk)) => Some((Ok(chunk), (body, guard))),
            Some(Err(error)) => {
                guard.fire(StreamFinish::UpstreamError(error.class));
                Some((Err(error), (body, guard)))
            }
            None => {
                guard.fire(StreamFinish::Clean);
                None
            }
        }
    })
    .boxed()
}

/// Read a whole answer, refusing one larger than `limit` bytes. A read
/// error is an attempt failure: nothing has reached the client yet.
async fn collect_body(
    mut body: futures::stream::BoxStream<'static, Result<bytes::Bytes, AdapterError>>,
    limit: u64,
) -> Result<bytes::Bytes, AdapterError> {
    let mut collected = bytes::BytesMut::new();
    while let Some(chunk) = body.next().await {
        let chunk = chunk?;
        if (collected.len() + chunk.len()) as u64 > limit {
            return Err(AdapterError::new(
                ErrorClass::InvalidRequest,
                format!("audio answer is larger than {limit} bytes"),
            ));
        }
        collected.extend_from_slice(&chunk);
    }
    Ok(collected.freeze())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sb_adapter::FormPart;

    fn part(name: &str, filename: Option<&str>, data: &'static [u8]) -> FormPart {
        FormPart {
            name: name.to_string(),
            filename: filename.map(str::to_string),
            content_type: None,
            data: bytes::Bytes::from_static(data),
        }
    }

    #[test]
    fn body_for_sets_only_the_model_field_and_shares_the_file() {
        let file = part("file", Some("a.wav"), b"RIFF");
        let input = AudioInput {
            call: AudioCall::Transcription,
            model: "transcribe".to_string(),
            body: AudioBody::Form(vec![
                part("model", None, b"transcribe"),
                file.clone(),
                part("num_speakers", None, b"2"),
            ]),
            stream: false,
            input_chars: None,
        };
        let AudioBody::Form(parts) = input.body_for("whisper-large") else {
            panic!("form expected");
        };
        assert_eq!(parts[0].data.as_ref(), b"whisper-large");
        assert_eq!(
            parts[1].data.as_ptr(),
            file.data.as_ptr(),
            "file not copied"
        );
        assert_eq!(parts[2].name, "num_speakers");
        assert_eq!(parts[2].data.as_ref(), b"2");
    }

    #[test]
    fn only_a_plain_transcript_is_collected() {
        let mut input = AudioInput {
            call: AudioCall::Transcription,
            model: "m".to_string(),
            body: AudioBody::Json(serde_json::json!({})),
            stream: false,
            input_chars: None,
        };
        assert!(input.collects());
        input.stream = true;
        assert!(!input.collects());
        input.stream = false;
        input.call = AudioCall::Speech;
        assert!(!input.collects());
    }
}
