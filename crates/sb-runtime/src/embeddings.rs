use std::collections::HashSet;
use std::time::Instant;

use sb_adapter::AdapterError;
use sb_core::{AiRequest, ErrorClass, FinishReason};
use sb_credentials::ResolveOutcome;

use super::finish_attempt::{AttemptFinishCtx, AttemptToken, FinishOutcome};
use super::helpers::{resolve_egress, session_affinity_key};
use super::outcome::embeddings_usage;
use super::profiles::{plan_resolved_route, resolve_candidates, tenant_allowed_accounts};
use super::{DenialTrace, EmbeddingsOutcome, Engine, ExecError, Snapshot};

impl Engine {
    /// Execute an OpenAI-compatible embeddings request through the same runtime
    /// controls as chat/messages: route decision, account fallback, budgets,
    /// egress selection, trace, ledger, and snapshot pinning. The HTTP edge
    /// remains responsible only for auth/admission and wire rendering.
    pub async fn execute_embeddings(
        &self,
        body: serde_json::Value,
        tenant: Option<String>,
        project: Option<String>,
        session_id: Option<String>,
        started: Instant,
    ) -> (u64, EmbeddingsOutcome) {
        let snap = self.snapshot();
        let revision = snap.revision;
        let outcome = self
            .execute_embeddings_inner(&snap, body, tenant, project, session_id, started)
            .await;
        (revision, outcome)
    }

    async fn execute_embeddings_inner(
        &self,
        snap: &Snapshot,
        body: serde_json::Value,
        tenant: Option<String>,
        project: Option<String>,
        session_id: Option<String>,
        started: Instant,
    ) -> EmbeddingsOutcome {
        let model = match body.get("model").and_then(|m| m.as_str()) {
            Some(model) if !model.is_empty() => model.to_string(),
            _ => {
                let request_id = sb_core::new_id("req");
                let message = "missing or invalid \"model\"";
                self.record_denial_trace(DenialTrace {
                    request_id: &request_id,
                    revision: snap.revision,
                    tenant: None,
                    project: None,
                    inbound_model: "<missing>",
                    status: 400,
                    error_type: "invalid_request_error",
                    message,
                    started,
                    streamed: false,
                });
                return EmbeddingsOutcome::Error {
                    request_id,
                    error: ExecError::new(400, "invalid_request_error", message, None),
                };
            }
        };

        let mut req = AiRequest::new(model, Vec::new());
        req.tenant = tenant;
        req.project = project;
        if let Some(session_id) = session_id {
            req.metadata.insert("session_id".to_string(), session_id);
        }

        if let Some(max) = snap.runtime.budget_max_usd {
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
                        streamed: false,
                    });
                    return EmbeddingsOutcome::Error {
                        request_id: req.id,
                        error: ExecError::new(503, "usage_store_unavailable", message, None),
                    };
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
                    streamed: false,
                });
                return EmbeddingsOutcome::Error {
                    request_id: req.id,
                    error: ExecError::new(402, "budget_exceeded", message, None),
                };
            }
        }
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
                            streamed: false,
                        });
                        return EmbeddingsOutcome::Error {
                            request_id: req.id,
                            error: ExecError::new(503, "usage_store_unavailable", message, None),
                        };
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
                        streamed: false,
                    });
                    return EmbeddingsOutcome::Error {
                        request_id: req.id,
                        error: ExecError::new(402, "tenant_budget_exceeded", message, None),
                    };
                }
            }
        }

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
                streamed: false,
            });
            return EmbeddingsOutcome::Error {
                request_id: req.id,
                error: ExecError::new(status, "plugin_rejected", message, None),
            };
        }

        let resolved = match resolve_candidates(snap, &req.model, &self.scorecard) {
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
                    streamed: false,
                });
                return EmbeddingsOutcome::Error {
                    request_id: req.id,
                    error: e,
                };
            }
        };
        let unknown = resolved.unknown.clone();
        let (route_name, plan) =
            match plan_resolved_route(&self.combo_rr, snap, &req, None, resolved, true) {
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
                        streamed: false,
                    });
                    return EmbeddingsOutcome::Error {
                        request_id: req.id,
                        error: e,
                    };
                }
            };
        snap.plugins.post_route(&req, &plan.decision);
        let summary = format!("{} embeddings", plan.decision.summary());
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

        'targets: for target in plan.candidates.iter() {
            let Some(adapter) = snap.registry.adapter(&target.provider_id) else {
                continue 'targets;
            };
            if !snap.resolver.circuit_allows(&target.provider_id) {
                continue 'targets;
            }
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
                            streamed: false,
                        });
                        return EmbeddingsOutcome::Error {
                            request_id: req.id,
                            error: ExecError::new(503, "usage_store_unavailable", message, None),
                        };
                    }
                };
                if spent >= *cap {
                    continue 'targets;
                }
            }

            let mut tried_accounts = HashSet::new();
            let allowed_accounts = req
                .tenant
                .as_deref()
                .and_then(|tenant_id| snap.config.tenant(tenant_id))
                .filter(|tenant| !tenant.allowed_accounts.is_empty())
                .map(|tenant| tenant_allowed_accounts(tenant, &target.provider_id));
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
                        let egress_id =
                            snap.plugins.select_egress(&req, &target.id).or_else(|| {
                                resolve_egress(&snap.config, &target.provider_id, &account_id)
                            });
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

                        let mut call_body = body.clone();
                        if let Some(obj) = call_body.as_object_mut() {
                            obj.insert(
                                "model".to_string(),
                                serde_json::Value::String(target.model.clone()),
                            );
                        }

                        // outcome-routing-v1 F7: embeddings attempts funnel
                        // through the SAME finish_attempt seam as chat/
                        // messages attempts -- embeddings routing already
                        // consumes scorecard projections (`resolve_candidates`
                        // above), so it must also feed the scorecard back.
                        let attempt_token = AttemptToken::new();
                        let scorecard_cfg = snap.config.server.scorecard.clone();

                        match adapter
                            .embeddings(call_body, target.clone(), Some(lease), egress_id.clone())
                            .await
                        {
                            Ok(value) => {
                                snap.resolver
                                    .report_success(&target.provider_id, &account_id);
                                let usage = embeddings_usage(&value);
                                let attempt_ms = attempt_started.elapsed().as_millis() as u64;
                                let cost = snap.registry.cost_micros(
                                    &target.provider_id,
                                    &target.model,
                                    &usage,
                                );
                                // F8-style ordering: finalize the attempt
                                // (upstream truth) BEFORE usage persistence,
                                // so a required usage-store failure below
                                // still leaves this real success recorded.
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
                                        cost_micros: Some(cost),
                                    },
                                );
                                if let Err(e) = self.record_usage(
                                    &snap.registry,
                                    &req.id,
                                    &target.provider_id,
                                    &target.model,
                                    &account_id,
                                    req.tenant.as_deref(),
                                    req.project.as_deref(),
                                    usage.clone(),
                                    started,
                                    false,
                                ) {
                                    let message = format!("usage persistence failed: {e}");
                                    self.record_trace(trace.finish(
                                        500,
                                        started.elapsed().as_millis() as u64,
                                        false,
                                    ));
                                    return EmbeddingsOutcome::Error {
                                        request_id: req.id,
                                        error: ExecError::new(
                                            500,
                                            "usage_persistence_failed",
                                            message,
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
                                trace.set_usage(usage, cost);
                                self.record_trace(trace.finish(
                                    200,
                                    started.elapsed().as_millis() as u64,
                                    false,
                                ));
                                return EmbeddingsOutcome::Json {
                                    value,
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
                                return EmbeddingsOutcome::Error {
                                    request_id: req.id,
                                    error: ExecError::upstream(&error, &summary),
                                };
                            }
                        }
                    }
                    ResolveOutcome::AllUnavailable { .. } => continue 'targets,
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
            return EmbeddingsOutcome::Error {
                request_id: req.id,
                error: ExecError::upstream(&error, &summary),
            };
        }

        let rejected = plan
            .decision
            .rejected
            .iter()
            .map(|rejected| format!("{}:{}", rejected.target_id, rejected.reason))
            .collect::<Vec<_>>()
            .join(",");
        self.traces
            .record(trace.finish(400, started.elapsed().as_millis() as u64, false));
        EmbeddingsOutcome::Error {
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
