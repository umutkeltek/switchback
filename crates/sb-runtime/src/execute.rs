use std::collections::HashSet;
use std::time::Instant;

use sb_adapter::{AdapterError, PreparedRequest};
use sb_core::{AiRequest, ErrorClass, EvaluationEvent, EvaluationEventKind};
use sb_credentials::ResolveOutcome;
use tracing::Instrument as _;

use super::collect::{collect_response, precommit_stream};
use super::execution_meta::{attach_execution_receipt, lookup_exact_cache, route_selected_event};
use super::hedge::run_hedge;
use super::helpers::{
    high_lossiness_schema_warning, resolve_egress, retry_backoff, retryable, session_affinity_key,
};
use super::profiles::{
    apply_request_client_profile, client_profile_allowed_accounts, combine_allowed_accounts,
    plan_resolved_route, resolve_candidates, tenant_allowed_accounts,
};
use super::stream::{meter_stream, StreamFinish};
use super::{DenialTrace, Engine, ExecError, ExecOutcome, Snapshot};

impl Engine {
    /// Pin a snapshot and run the request to a committed outcome. Returns the
    /// pinned revision alongside the outcome so the HTTP edge can stamp
    /// `x-switchback-revision`. This is the runtime's public entry point.
    pub async fn execute(&self, req: AiRequest, started: Instant) -> (u64, ExecOutcome) {
        let snap = self.snapshot();
        let revision = snap.revision;
        let outcome = self.execute_inner(&snap, req, started).await;
        (revision, outcome)
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
    ) -> ExecOutcome {
        // The caller pinned ONE compiled snapshot for this request's whole
        // lifetime — a config publish mid-request never tears it across revisions.
        let rt = &snap.runtime;

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
        let cache_receipt = lookup_exact_cache(self, &req, &cache_policy);

        // Resolve the request's model to candidate targets (route → provider/model
        // → default provider → 404), pool-health-stamped. Shared with route-preview.
        let resolved = match resolve_candidates(snap, &req.model) {
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
        let unknown = resolved.unknown.clone();

        let (route_name, mut plan) = match plan_resolved_route(
            &self.combo_rr,
            snap,
            &req,
            client_profile.as_ref(),
            resolved,
            true,
        ) {
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
        attach_execution_receipt(&mut plan, &req, cache_receipt.clone());
        // Plugin post-route hook (Oracle #6): observe the explainable decision.
        snap.plugins.post_route(&req, &plan.decision);
        let summary = plan.decision.summary();
        let mut last_err: Option<AdapterError> = None;

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
        );

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
        if rt.hedge_enabled && !req.stream && plan.candidates.len() >= 2 {
            if let Some(win) = run_hedge(snap, &req, &plan.candidates).await {
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
                trace.set_usage(win.response.usage.clone(), cost);
                self.traces
                    .record(trace.finish(200, started.elapsed().as_millis() as u64, false));
                return ExecOutcome::Collected {
                    response: Box::new(win.response),
                    summary,
                };
            }
        }

        'targets: for target in plan.candidates.iter() {
            let Some(adapter) = snap.registry.adapter(&target.provider_id) else {
                continue 'targets;
            };
            // Circuit breaker: if this provider is OPEN (it's been failing), don't
            // even attempt it — fall straight over to the next target.
            if !snap.resolver.circuit_allows(&target.provider_id) {
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
                    continue 'targets;
                }
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
                        // Same-target retry on transient errors (timeout/network/5xx)
                        // before we fall over to another account. The lease + egress
                        // are reused; each retry waits an exponential backoff.
                        let prepared =
                            PreparedRequest::new(req.clone(), target.clone(), Some(lease.clone()))
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
                            retry_n += 1;
                            let delay = retry_backoff(retry, retry_n);
                            tracing::info!(
                                request_id = %req.id, target = %target.id, account = %account_id,
                                retry = retry_n, class = err.class.as_str(),
                                delay_ms = delay.as_millis() as u64, "retrying transient failure"
                            );
                            tokio::time::sleep(delay).await;
                            let prepared = PreparedRequest::new(
                                req.clone(),
                                target.clone(),
                                Some(lease.clone()),
                            )
                            .with_egress(egress_id.clone());
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
                                            snap.resolver
                                                .circuit_record(&target.provider_id, false);
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
                                            snap.plugins.post_attempt(&sb_plugin::AttemptInfo {
                                                request_id: &req.id,
                                                target_id: &target.id,
                                                provider_id: &target.provider_id,
                                                account_id: &account_id,
                                                egress: egress_eff.as_str(),
                                                ok: false,
                                                error_class: Some(error.class.as_str()),
                                                latency_ms: attempt_ms,
                                            });
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
                                    let metered = meter_stream(
                                        stream,
                                        attempt_started,
                                        move |ttft_ms| {
                                            registry_ttft.record_ttft(&pid_ttft, &mdl_ttft, ttft_ms)
                                        },
                                        move |usage, finish| {
                                            let latency = started.elapsed().as_millis() as u64;
                                            let attempt_ms =
                                                attempt_started.elapsed().as_millis() as u64;
                                            match finish {
                                                StreamFinish::Clean => {
                                                    resolver.report_success(&pid, &acct);
                                                    resolver.circuit_record(&pid, true);
                                                    trace.attempt(sb_trace::Attempt::success(
                                                        &tid,
                                                        &pid,
                                                        &mdl,
                                                        &acct,
                                                        egress.as_str(),
                                                        attempt_ms,
                                                    ));
                                                    plugins.post_attempt(&sb_plugin::AttemptInfo {
                                                        request_id: &rid,
                                                        target_id: &tid,
                                                        provider_id: &pid,
                                                        account_id: &acct,
                                                        egress: egress.as_str(),
                                                        ok: true,
                                                        error_class: None,
                                                        latency_ms: attempt_ms,
                                                    });
                                                    registry.record_latency(
                                                        &pid,
                                                        &mdl,
                                                        attempt_ms as f64,
                                                    );
                                                    let cost =
                                                        registry.cost_micros(&pid, &mdl, &usage);
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
                                                        .with_project(prj);
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
                                                }
                                                StreamFinish::UpstreamError(class) => {
                                                    resolver
                                                        .report_failure(&pid, &acct, &mdl, class);
                                                    resolver.circuit_record(&pid, false);
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
                                                    plugins.post_attempt(&sb_plugin::AttemptInfo {
                                                        request_id: &rid,
                                                        target_id: &tid,
                                                        provider_id: &pid,
                                                        account_id: &acct,
                                                        egress: egress.as_str(),
                                                        ok: false,
                                                        error_class: Some(class.as_str()),
                                                        latency_ms: attempt_ms,
                                                    });
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
                                                    plugins.post_attempt(&sb_plugin::AttemptInfo {
                                                        request_id: &rid,
                                                        target_id: &tid,
                                                        provider_id: &pid,
                                                        account_id: &acct,
                                                        egress: egress.as_str(),
                                                        ok: false,
                                                        error_class: Some("client_aborted"),
                                                        latency_ms: attempt_ms,
                                                    });
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

                                match collect_response(
                                    stream,
                                    req.id.clone(),
                                    req.model.clone(),
                                    snap.config.server.max_response_bytes,
                                )
                                .await
                                {
                                    Ok(response) => {
                                        snap.resolver
                                            .report_success(&target.provider_id, &account_id);
                                        snap.resolver.circuit_record(&target.provider_id, true);
                                        tracing::info!(
                                            request_id = %req.id, model = %req.model, target = %target.id,
                                            account = %account_id, status = 200u16,
                                            latency_ms = started.elapsed().as_millis() as u64, route = %summary
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
                                        let attempt_ms =
                                            attempt_started.elapsed().as_millis() as u64;
                                        trace.attempt(sb_trace::Attempt::success(
                                            &target.id,
                                            &target.provider_id,
                                            &target.model,
                                            &account_id,
                                            egress_eff.as_str(),
                                            attempt_ms,
                                        ));
                                        snap.plugins.post_attempt(&sb_plugin::AttemptInfo {
                                            request_id: &req.id,
                                            target_id: &target.id,
                                            provider_id: &target.provider_id,
                                            account_id: &account_id,
                                            egress: egress_eff.as_str(),
                                            ok: true,
                                            error_class: None,
                                            latency_ms: attempt_ms,
                                        });
                                        snap.registry.record_latency(
                                            &target.provider_id,
                                            &target.model,
                                            attempt_ms as f64,
                                        );
                                        let cost = snap.registry.cost_micros(
                                            &target.provider_id,
                                            &target.model,
                                            &response.usage,
                                        );
                                        trace.set_usage(response.usage.clone(), cost);
                                        self.record_trace(trace.finish(
                                            200,
                                            started.elapsed().as_millis() as u64,
                                            false,
                                        ));
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
                                        snap.resolver.circuit_record(&target.provider_id, false);
                                        let fell_over = error.should_fallback();
                                        trace.attempt(sb_trace::Attempt::failed(
                                            &target.id,
                                            &target.provider_id,
                                            &target.model,
                                            &account_id,
                                            egress_eff.as_str(),
                                            attempt_started.elapsed().as_millis() as u64,
                                            error.class.as_str(),
                                            fell_over,
                                        ));
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
                                snap.resolver.circuit_record(&target.provider_id, false);
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
                                snap.plugins.post_attempt(&sb_plugin::AttemptInfo {
                                    request_id: &req.id,
                                    target_id: &target.id,
                                    provider_id: &target.provider_id,
                                    account_id: &account_id,
                                    egress: egress_eff.as_str(),
                                    ok: false,
                                    error_class: Some(error.class.as_str()),
                                    latency_ms: attempt_ms,
                                });
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
            return ExecOutcome::Error(ExecError::upstream(&error, &summary));
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
