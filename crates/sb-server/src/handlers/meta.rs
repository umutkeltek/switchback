use std::collections::{BTreeMap, BTreeSet, HashSet};

use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use sb_core::{
    AiRequest, AuthConfig, ClientProfileConfig, ClientProfileKind, Config, ExecutionProfile,
    ProviderConfig,
};
use serde::Deserialize;

use crate::http_response::openai_error;
use crate::tenancy::Principal;
use crate::AppState;

/// The embedded single-page dashboard (no build step, no external assets).
pub(crate) async fn dashboard() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        include_str!("../dashboard.html"),
    )
}

pub(crate) async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": true }))
}

/// Usage/cost summary from the append-only ledger — requests + attributed cost
/// (micro-USD and USD) by model and provider. The "see every cost" surface that
/// complements the explainable "see every decision" route headers.
pub(crate) async fn usage(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
) -> Json<serde_json::Value> {
    let summary = state.ledger.summary();
    let durability = state.ledger.durability_health();
    if let Some(tenant) = scoped_tenant(&principal) {
        let (requests, total_cost_micros) =
            summary.by_tenant.get(tenant).copied().unwrap_or_default();
        let mut by_tenant = serde_json::Map::new();
        by_tenant.insert(
            tenant.to_string(),
            serde_json::json!([requests, total_cost_micros]),
        );
        return Json(serde_json::json!({
            "requests": requests,
            "total_cost_micros": total_cost_micros,
            "total_cost_usd": total_cost_micros as f64 / 1_000_000.0,
            "by_model": {},
            "by_provider": {},
            "by_tenant": by_tenant,
            "scope": { "tenant": tenant },
            "durability": durability,
        }));
    }
    Json(serde_json::json!({
        "requests": summary.requests,
        "total_cost_micros": summary.total_cost_micros,
        "total_cost_usd": summary.total_cost_micros as f64 / 1_000_000.0,
        "by_model": summary.by_model,
        "by_provider": summary.by_provider,
        "by_tenant": summary.by_tenant,
        "durability": durability,
    }))
}

/// Reconcile the served usage summary against durable usage events and known
/// memory fallback records.
pub(crate) async fn usage_reconcile(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
) -> Json<serde_json::Value> {
    Json(
        serde_json::to_value(state.ledger.reconcile(scoped_tenant(&principal)))
            .unwrap_or_else(|_| serde_json::json!({ "status": "inconsistent" })),
    )
}

/// `GET /v1/client-profiles` — machine-readable readiness for clients that want
/// to point at Switchback while keeping their native protocol shape. Credentials
/// still live in Switchback provider/accounts; this is only protocol/setup
/// metadata and non-secret account health.
pub(crate) async fn client_profiles(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
) -> Json<serde_json::Value> {
    let snap = state.snapshot();
    let scoped = crate::controlplane::scoped_config_for_principal(&snap.config, &principal);
    let visible_models = model_ids_for_config(&scoped);
    let visible_model_set = visible_models
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    let account_health = account_health_by_ref(&scoped, &snap.resolver);
    let profiles = effective_client_profiles(&scoped)
        .into_iter()
        .map(|profile| client_profile_status(&scoped, &visible_model_set, &account_health, profile))
        .collect::<Vec<_>>();
    Json(serde_json::json!({
        "object": "list",
        "metadata_only": true,
        "base_path": "/v1",
        "profiles": profiles,
    }))
}

#[derive(Default, Deserialize)]
pub(crate) struct TracesQuery {
    limit: Option<usize>,
    tenant: Option<String>,
    session_id: Option<String>,
    model: Option<String>,
    status: Option<u16>,
    since_ms: Option<i64>,
}

#[derive(Deserialize)]
pub(crate) struct SessionsQuery {
    limit: Option<usize>,
    trace_limit: Option<usize>,
}

/// Recent request traces, newest first — the "see every request, end to end"
/// surface (route decision + every account/egress attempt + cost). Metadata
/// only; never secrets or message content.
pub(crate) async fn traces(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Query(q): Query<TracesQuery>,
) -> Json<serde_json::Value> {
    let limit = q.limit.unwrap_or(50).min(1000);
    if let Some(store) = state.engine.store() {
        match store.query_traces(&store_trace_query(&principal, &q, limit)) {
            Ok(events) => {
                let traces = events
                    .iter()
                    .filter_map(trace_event_json)
                    .collect::<Vec<_>>();
                return Json(serde_json::json!({
                    "count": traces.len(),
                    "traces": traces,
                    "source": { "kind": "state_store", "metadata_only": true },
                }));
            }
            Err(e) => {
                tracing::warn!(error = %e, "state store trace query failed; falling back to ring");
            }
        }
    }
    let recent = filtered_memory_traces(&state, &principal, &q, limit);
    Json(serde_json::json!({
        "count": recent.len(),
        "traces": recent,
        "source": { "kind": "recent_trace_ring", "metadata_only": true },
    }))
}

/// One trace by request id.
pub(crate) async fn trace_by_id(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> Response {
    if let Some(store) = state.engine.store() {
        match store.get_trace(&id) {
            Ok(Some(event)) if trace_event_visible_to(&principal, &event) => {
                if let Some(value) = trace_event_json(&event) {
                    return (StatusCode::OK, Json(value)).into_response();
                }
            }
            Ok(Some(_)) | Ok(None) => {}
            Err(e) => {
                tracing::warn!(error = %e, request_id = %id, "state store trace lookup failed");
            }
        }
    }
    match state.traces.get(&id) {
        Some(rec) if trace_visible_to(&principal, &rec) => {
            (StatusCode::OK, Json(rec)).into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(openai_error(&format!("no trace `{id}`"), "not_found")),
        )
            .into_response(),
        Some(_) => (
            StatusCode::NOT_FOUND,
            Json(openai_error(&format!("no trace `{id}`"), "not_found")),
        )
            .into_response(),
    }
}

/// Metadata-only session rollups from the recent trace ring. This is the
/// Langfuse-adjacent "group the workflow" surface without storing prompt or
/// response content.
pub(crate) async fn sessions(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Query(q): Query<SessionsQuery>,
) -> Json<serde_json::Value> {
    let trace_limit = q.trace_limit.unwrap_or(1000).min(5000);
    let mut source = serde_json::json!({
        "kind": "recent_trace_ring",
        "trace_limit": trace_limit,
        "metadata_only": true,
    });

    let mut unsessioned_count = 0usize;
    let mut sessions: BTreeMap<String, SessionRollupBuilder> = BTreeMap::new();
    if let Some(store) = state.engine.store() {
        match store.query_traces(&store_trace_query(
            &principal,
            &TracesQuery::default(),
            trace_limit,
        )) {
            Ok(events) => {
                source = serde_json::json!({
                    "kind": "state_store",
                    "trace_limit": trace_limit,
                    "metadata_only": true,
                });
                for event in events {
                    add_trace_event_to_sessions(event, &mut sessions, &mut unsessioned_count);
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "state store session query failed; falling back to ring");
                add_memory_traces_to_sessions(
                    filtered_memory_traces(
                        &state,
                        &principal,
                        &TracesQuery::default(),
                        trace_limit,
                    ),
                    &mut sessions,
                    &mut unsessioned_count,
                );
            }
        }
    } else {
        add_memory_traces_to_sessions(
            filtered_memory_traces(&state, &principal, &TracesQuery::default(), trace_limit),
            &mut sessions,
            &mut unsessioned_count,
        );
    }

    let limit = q.limit.unwrap_or(50).min(1000);
    let mut items = sessions
        .into_values()
        .map(SessionRollup::from)
        .collect::<Vec<_>>();
    items.sort_by(|a, b| {
        b.last_timestamp_unix
            .cmp(&a.last_timestamp_unix)
            .then_with(|| a.session_id.cmp(&b.session_id))
    });
    items.truncate(limit);

    Json(serde_json::json!({
        "count": items.len(),
        "sessions": items,
        "unsessioned_count": unsessioned_count,
        "source": source,
    }))
}

pub(crate) async fn session_by_id(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(session_id): Path<String>,
    Query(q): Query<SessionsQuery>,
) -> Response {
    let limit = q.trace_limit.unwrap_or(100).min(1000);
    let query = TracesQuery {
        limit: Some(limit),
        session_id: Some(session_id.clone()),
        ..TracesQuery::default()
    };
    let (traces, events) = trace_values_for_query(&state, &principal, &query, limit);
    let mut rollup = SessionRollupBuilder::new(session_id.clone());
    for event in events {
        rollup.add_event(event);
    }
    if rollup.request_count == 0 {
        for trace in filtered_memory_trace_records(&state, &principal, &query, limit) {
            rollup.add_record(trace);
        }
    }
    if rollup.request_count == 0 {
        return (
            StatusCode::NOT_FOUND,
            Json(openai_error(
                &format!("no session `{session_id}`"),
                "not_found",
            )),
        )
            .into_response();
    }
    Json(serde_json::json!({
        "session": SessionRollup::from(rollup),
        "traces": traces,
        "count": traces.len(),
        "source": trace_source(&state),
    }))
    .into_response()
}

pub(crate) async fn session_traces(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(session_id): Path<String>,
    Query(q): Query<TracesQuery>,
) -> Json<serde_json::Value> {
    let limit = q.limit.unwrap_or(100).min(1000);
    let query = TracesQuery {
        limit: Some(limit),
        session_id: Some(session_id),
        tenant: q.tenant,
        model: q.model,
        status: q.status,
        since_ms: q.since_ms,
    };
    let (traces, _events) = trace_values_for_query(&state, &principal, &query, limit);
    Json(serde_json::json!({
        "count": traces.len(),
        "traces": traces,
        "source": trace_source(&state),
    }))
}

/// Re-run routing for an existing trace's metadata against the current snapshot,
/// without executing upstream. Because Switchback does not store prompt bodies,
/// this intentionally replays only model, stream, tenant/project, and session
/// context.
pub(crate) async fn trace_route_preview(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> Response {
    let Some(ctx) = trace_preview_context(&state, &principal, &id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(openai_error(&format!("no trace `{id}`"), "not_found")),
        )
            .into_response();
    };

    let mut req = AiRequest::new(ctx.inbound_model.clone(), Vec::new());
    req.id = sb_core::new_id("preview");
    req.stream = ctx.streamed;
    req.tenant = ctx.tenant.clone();
    req.project = ctx.project.clone();
    if let Some(session_id) = &ctx.session_id {
        req.metadata
            .insert("session_id".to_string(), session_id.clone());
    }

    match state.engine.preview_route(&req) {
        Ok((revision, plan)) => {
            let current_decision =
                serde_json::to_value(&plan.decision).unwrap_or_else(|_| serde_json::json!({}));
            Json(serde_json::json!({
            "source_request_id": ctx.request_id,
            "revision": revision,
            "original_revision": ctx.original_revision,
            "principal": {
                "tenant": req.tenant,
                "project": req.project,
                "session_id": ctx.session_id,
            },
            "decision": plan.decision,
            "candidates": plan.candidates.iter().map(|c| &c.id).collect::<Vec<_>>(),
            "original": {
                "revision": ctx.original_revision,
                "decision": ctx.original_decision,
            },
            "current": {
                "revision": revision,
                "decision": current_decision,
            },
            "diff": route_decision_diff(&ctx.original_decision, &current_decision, ctx.original_revision, revision),
            "assumptions": [
                "metadata_only_trace: request body is not stored",
                "preview uses inbound_model, stream flag, tenant, project, and session_id only"
            ],
        }))
            .into_response()
        }
        Err(e) => (
            StatusCode::from_u16(e.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            Json(serde_json::json!({"error": {"message": e.message, "type": e.error_type}})),
        )
            .into_response(),
    }
}

struct TracePreviewContext {
    request_id: String,
    original_revision: u64,
    tenant: Option<String>,
    project: Option<String>,
    session_id: Option<String>,
    inbound_model: String,
    streamed: bool,
    original_decision: serde_json::Value,
}

fn trace_preview_context(
    state: &AppState,
    principal: &Principal,
    request_id: &str,
) -> Option<TracePreviewContext> {
    if let Some(store) = state.engine.store() {
        match store.get_trace(request_id) {
            Ok(Some(event)) if trace_event_visible_to(principal, &event) => {
                let trace_json = trace_event_json(&event)?;
                return Some(TracePreviewContext {
                    request_id: event.request_id,
                    original_revision: event.revision,
                    tenant: event.tenant,
                    project: event.project,
                    session_id: event.session_id,
                    inbound_model: event.inbound_model,
                    streamed: event.streamed,
                    original_decision: trace_json
                        .get("decision")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!({})),
                });
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(error = %e, request_id, "state store trace lookup failed");
            }
        }
    }
    let trace = state
        .traces
        .get(request_id)
        .filter(|trace| trace_visible_to(principal, trace))?;
    Some(TracePreviewContext {
        request_id: trace.request_id,
        original_revision: trace.revision,
        tenant: trace.tenant,
        project: trace.project,
        session_id: trace.session_id,
        inbound_model: trace.inbound_model,
        streamed: trace.streamed,
        original_decision: serde_json::to_value(trace.decision).ok()?,
    })
}

fn route_decision_diff(
    original: &serde_json::Value,
    current: &serde_json::Value,
    original_revision: u64,
    current_revision: u64,
) -> serde_json::Value {
    let original_rejected = rejection_map(original);
    let current_rejected = rejection_map(current);
    let added_rejections = current_rejected
        .iter()
        .filter(|(target, _)| !original_rejected.contains_key(*target))
        .map(|(target, reason)| serde_json::json!({"target_id": target, "reason": reason}))
        .collect::<Vec<_>>();
    let removed_rejections = original_rejected
        .iter()
        .filter(|(target, _)| !current_rejected.contains_key(*target))
        .map(|(target, reason)| serde_json::json!({"target_id": target, "reason": reason}))
        .collect::<Vec<_>>();
    let changed_rejections = current_rejected
        .iter()
        .filter_map(|(target, reason)| {
            let original_reason = original_rejected.get(target)?;
            (original_reason != reason).then(|| {
                serde_json::json!({
                    "target_id": target,
                    "from": original_reason,
                    "to": reason,
                })
            })
        })
        .collect::<Vec<_>>();
    let original_selected = selected_target(original);
    let current_selected = selected_target(current);
    let original_strategy = original.get("strategy").and_then(serde_json::Value::as_str);
    let current_strategy = current.get("strategy").and_then(serde_json::Value::as_str);
    serde_json::json!({
        "revision_changed": original_revision != current_revision,
        "selected_changed": original_selected != current_selected,
        "strategy_changed": original_strategy != current_strategy,
        "fallbacks_changed": fallback_targets(original) != fallback_targets(current),
        "original_selected": original_selected,
        "current_selected": current_selected,
        "original_strategy": original_strategy,
        "current_strategy": current_strategy,
        "added_rejections": added_rejections,
        "removed_rejections": removed_rejections,
        "changed_rejections": changed_rejections,
    })
}

fn selected_target(decision: &serde_json::Value) -> Option<String> {
    decision
        .pointer("/selected/target_id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

fn fallback_targets(decision: &serde_json::Value) -> Vec<String> {
    decision
        .get("fallbacks")
        .and_then(serde_json::Value::as_array)
        .map(|fallbacks| {
            fallbacks
                .iter()
                .filter_map(|fallback| fallback.get("target_id")?.as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn rejection_map(decision: &serde_json::Value) -> BTreeMap<String, String> {
    decision
        .get("rejected")
        .and_then(serde_json::Value::as_array)
        .map(|rejections| {
            rejections
                .iter()
                .filter_map(|rejection| {
                    Some((
                        rejection.get("target_id")?.as_str()?.to_string(),
                        rejection
                            .get("reason")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Debug, serde::Serialize)]
struct SessionRollup {
    session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    tenant: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    project: Option<String>,
    request_count: usize,
    error_count: usize,
    streamed_count: usize,
    first_timestamp_unix: u64,
    last_timestamp_unix: u64,
    last_request_id: String,
    last_status: u16,
    total_latency_ms: u64,
    avg_latency_ms: u64,
    cost_micros: u64,
    models: Vec<String>,
    providers: Vec<String>,
}

struct SessionRollupBuilder {
    session_id: String,
    tenant: Option<String>,
    project: Option<String>,
    request_count: usize,
    error_count: usize,
    streamed_count: usize,
    first_timestamp_unix: u64,
    last_timestamp_unix: u64,
    last_request_id: String,
    last_status: u16,
    total_latency_ms: u64,
    cost_micros: u64,
    models: BTreeSet<String>,
    providers: BTreeSet<String>,
}

impl SessionRollupBuilder {
    fn new(session_id: String) -> Self {
        Self {
            session_id,
            tenant: None,
            project: None,
            request_count: 0,
            error_count: 0,
            streamed_count: 0,
            first_timestamp_unix: u64::MAX,
            last_timestamp_unix: 0,
            last_request_id: String::new(),
            last_status: 0,
            total_latency_ms: 0,
            cost_micros: 0,
            models: BTreeSet::new(),
            providers: BTreeSet::new(),
        }
    }

    fn add_record(&mut self, trace: sb_trace::TraceRecord) {
        self.request_count += 1;
        if trace.final_status >= 400 {
            self.error_count += 1;
        }
        if trace.streamed {
            self.streamed_count += 1;
        }
        self.first_timestamp_unix = self.first_timestamp_unix.min(trace.timestamp_unix);
        if trace.timestamp_unix >= self.last_timestamp_unix {
            self.last_timestamp_unix = trace.timestamp_unix;
            self.last_request_id = trace.request_id.clone();
            self.last_status = trace.final_status;
        }
        self.total_latency_ms = self.total_latency_ms.saturating_add(trace.total_latency_ms);
        self.cost_micros = self.cost_micros.saturating_add(trace.cost_micros);
        self.models.insert(trace.inbound_model);
        for attempt in trace.attempts {
            self.providers.insert(attempt.provider_id);
        }
        if self.tenant.is_none() {
            self.tenant = trace.tenant;
        }
        if self.project.is_none() {
            self.project = trace.project;
        }
    }

    fn add_event(&mut self, event: sb_store::TraceEvent) {
        self.request_count += 1;
        if event.final_status >= 400 {
            self.error_count += 1;
        }
        if event.streamed {
            self.streamed_count += 1;
        }
        let timestamp_unix = (event.created_at_ms.max(0) as u64) / 1000;
        self.first_timestamp_unix = self.first_timestamp_unix.min(timestamp_unix);
        if timestamp_unix >= self.last_timestamp_unix {
            self.last_timestamp_unix = timestamp_unix;
            self.last_request_id = event.request_id;
            self.last_status = event.final_status;
        }
        self.total_latency_ms = self.total_latency_ms.saturating_add(event.total_latency_ms);
        self.cost_micros = self.cost_micros.saturating_add(event.cost_micros);
        self.models.insert(event.inbound_model);
        self.providers.extend(event.attempted_providers);
        if self.tenant.is_none() {
            self.tenant = event.tenant;
        }
        if self.project.is_none() {
            self.project = event.project;
        }
    }
}

impl From<SessionRollupBuilder> for SessionRollup {
    fn from(builder: SessionRollupBuilder) -> Self {
        let avg_latency_ms = if builder.request_count == 0 {
            0
        } else {
            builder.total_latency_ms / builder.request_count as u64
        };
        Self {
            session_id: builder.session_id,
            tenant: builder.tenant,
            project: builder.project,
            request_count: builder.request_count,
            error_count: builder.error_count,
            streamed_count: builder.streamed_count,
            first_timestamp_unix: builder.first_timestamp_unix,
            last_timestamp_unix: builder.last_timestamp_unix,
            last_request_id: builder.last_request_id,
            last_status: builder.last_status,
            total_latency_ms: builder.total_latency_ms,
            avg_latency_ms,
            cost_micros: builder.cost_micros,
            models: builder.models.into_iter().collect(),
            providers: builder.providers.into_iter().collect(),
        }
    }
}

fn store_trace_query(principal: &Principal, q: &TracesQuery, limit: usize) -> sb_store::TraceQuery {
    sb_store::TraceQuery {
        limit,
        tenant: scoped_tenant(principal)
            .map(str::to_string)
            .or_else(|| q.tenant.clone().filter(|tenant| !tenant.is_empty())),
        session_id: q.session_id.clone().filter(|id| !id.is_empty()),
        model: q.model.clone().filter(|model| !model.is_empty()),
        status: q.status,
        since_ms: q.since_ms,
    }
}

fn trace_source(state: &AppState) -> serde_json::Value {
    if state.engine.store().is_some() {
        serde_json::json!({ "kind": "state_store", "metadata_only": true })
    } else {
        serde_json::json!({ "kind": "recent_trace_ring", "metadata_only": true })
    }
}

fn trace_event_json(event: &sb_store::TraceEvent) -> Option<serde_json::Value> {
    serde_json::from_str(&event.trace_json).ok()
}

fn trace_event_visible_to(principal: &Principal, event: &sb_store::TraceEvent) -> bool {
    scoped_tenant(principal)
        .map(|tenant| event.tenant.as_deref() == Some(tenant))
        .unwrap_or(true)
}

fn memory_trace_matches(
    principal: &Principal,
    q: &TracesQuery,
    trace: &sb_trace::TraceRecord,
) -> bool {
    if !trace_visible_to(principal, trace) {
        return false;
    }
    if let Some(tenant) = q.tenant.as_deref().filter(|_| principal.is_admin()) {
        if trace.tenant.as_deref() != Some(tenant) {
            return false;
        }
    }
    if let Some(session_id) = q.session_id.as_deref() {
        if trace.session_id.as_deref() != Some(session_id) {
            return false;
        }
    }
    if let Some(model) = q.model.as_deref() {
        if trace.inbound_model != model {
            return false;
        }
    }
    if let Some(status) = q.status {
        if trace.final_status != status {
            return false;
        }
    }
    if let Some(since_ms) = q.since_ms {
        if (trace.timestamp_unix as i64).saturating_mul(1000) < since_ms {
            return false;
        }
    }
    true
}

fn filtered_memory_trace_records(
    state: &AppState,
    principal: &Principal,
    q: &TracesQuery,
    limit: usize,
) -> Vec<sb_trace::TraceRecord> {
    state
        .traces
        .recent(limit)
        .into_iter()
        .filter(|trace| memory_trace_matches(principal, q, trace))
        .collect()
}

fn filtered_memory_traces(
    state: &AppState,
    principal: &Principal,
    q: &TracesQuery,
    limit: usize,
) -> Vec<serde_json::Value> {
    filtered_memory_trace_records(state, principal, q, limit)
        .into_iter()
        .filter_map(|trace| serde_json::to_value(trace).ok())
        .collect()
}

fn trace_values_for_query(
    state: &AppState,
    principal: &Principal,
    q: &TracesQuery,
    limit: usize,
) -> (Vec<serde_json::Value>, Vec<sb_store::TraceEvent>) {
    if let Some(store) = state.engine.store() {
        match store.query_traces(&store_trace_query(principal, q, limit)) {
            Ok(events) => {
                let traces = events.iter().filter_map(trace_event_json).collect();
                return (traces, events);
            }
            Err(e) => {
                tracing::warn!(error = %e, "state store trace query failed; falling back to ring");
            }
        }
    }
    (
        filtered_memory_traces(state, principal, q, limit),
        Vec::new(),
    )
}

fn add_trace_event_to_sessions(
    event: sb_store::TraceEvent,
    sessions: &mut BTreeMap<String, SessionRollupBuilder>,
    unsessioned_count: &mut usize,
) {
    let Some(session_id) = event.session_id.clone().filter(|id| !id.is_empty()) else {
        *unsessioned_count += 1;
        return;
    };
    sessions
        .entry(session_id.clone())
        .or_insert_with(|| SessionRollupBuilder::new(session_id))
        .add_event(event);
}

fn add_memory_traces_to_sessions(
    traces: Vec<serde_json::Value>,
    sessions: &mut BTreeMap<String, SessionRollupBuilder>,
    unsessioned_count: &mut usize,
) {
    for value in traces {
        let Some(session_id) = value
            .get("session_id")
            .and_then(serde_json::Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_string)
        else {
            *unsessioned_count += 1;
            continue;
        };
        let mut builder = sessions
            .remove(&session_id)
            .unwrap_or_else(|| SessionRollupBuilder::new(session_id.clone()));
        let event = sb_store::TraceEvent {
            request_id: value
                .get("request_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string(),
            revision: value
                .get("revision")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or_default(),
            tenant: value
                .get("tenant")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            project: value
                .get("project")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            session_id: Some(session_id.clone()),
            inbound_model: value
                .get("inbound_model")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string(),
            route: value
                .get("route")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string(),
            selected_target: value
                .pointer("/decision/selected/target_id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            final_status: value
                .get("final_status")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or_default() as u16,
            total_latency_ms: value
                .get("total_latency_ms")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or_default(),
            streamed: value
                .get("streamed")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or_default(),
            cost_micros: value
                .get("cost_micros")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or_default(),
            attempted_providers: value
                .get("attempts")
                .and_then(serde_json::Value::as_array)
                .map(|attempts| {
                    attempts
                        .iter()
                        .filter_map(|attempt| attempt.get("provider_id")?.as_str())
                        .map(str::to_string)
                        .collect::<BTreeSet<_>>()
                        .into_iter()
                        .collect()
                })
                .unwrap_or_default(),
            created_at_ms: value
                .get("timestamp_unix")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or_default()
                .saturating_mul(1000),
            trace_json: String::new(),
        };
        builder.add_event(event);
        sessions.insert(session_id, builder);
    }
}

fn scoped_tenant(principal: &Principal) -> Option<&str> {
    if principal.is_admin() {
        None
    } else {
        principal.tenant.as_deref()
    }
}

fn trace_visible_to(principal: &Principal, trace: &sb_trace::TraceRecord) -> bool {
    scoped_tenant(principal)
        .map(|tenant| trace.tenant.as_deref() == Some(tenant))
        .unwrap_or(true)
}

fn builtin_client_profiles() -> Vec<ClientProfileConfig> {
    vec![
        ClientProfileConfig {
            id: ClientProfileKind::Codex.default_id().to_string(),
            kind: ClientProfileKind::Codex,
            enabled: true,
            models: Vec::new(),
            accounts: Vec::new(),
            description: Some(
                "Codex-compatible OpenAI Responses profile backed by Switchback accounts"
                    .to_string(),
            ),
        },
        ClientProfileConfig {
            id: ClientProfileKind::ClaudeCode.default_id().to_string(),
            kind: ClientProfileKind::ClaudeCode,
            enabled: true,
            models: Vec::new(),
            accounts: Vec::new(),
            description: Some(
                "Claude Code-compatible Anthropic Messages profile backed by Switchback accounts"
                    .to_string(),
            ),
        },
    ]
}

fn effective_client_profiles(config: &Config) -> Vec<ClientProfileConfig> {
    let mut profiles = builtin_client_profiles();
    for configured in &config.client_profiles {
        match profiles
            .iter()
            .position(|profile| profile.id == configured.id)
        {
            Some(index) => profiles[index] = configured.clone(),
            None => profiles.push(configured.clone()),
        }
    }
    profiles
}

fn client_profile_status(
    config: &Config,
    visible_model_set: &HashSet<&str>,
    account_health: &BTreeMap<String, bool>,
    profile: ClientProfileConfig,
) -> serde_json::Value {
    let explicit_models = !profile.models.is_empty();
    let model_checks = profile
        .models
        .iter()
        .map(|model| {
            serde_json::json!({
                "id": model,
                "resolvable": visible_model_set.contains(model.as_str()),
            })
        })
        .collect::<Vec<_>>();
    let models_ready = if explicit_models {
        model_checks
            .iter()
            .all(|check| check["resolvable"].as_bool().unwrap_or(false))
    } else {
        !visible_model_set.is_empty()
    };

    let explicit_accounts = !profile.accounts.is_empty();
    let account_checks = if explicit_accounts {
        profile
            .accounts
            .iter()
            .map(|account_ref| {
                account_ref_status(config, account_health, account_ref).unwrap_or_else(|| {
                    serde_json::json!({
                        "ref": account_ref,
                        "available": false,
                        "reason": "not_visible_or_missing",
                    })
                })
            })
            .collect::<Vec<_>>()
    } else {
        all_account_statuses(config, account_health)
    };
    let accounts_ready = !account_checks.is_empty()
        && account_checks
            .iter()
            .all(|check| check["available"].as_bool().unwrap_or(false));

    let ready = profile.enabled && models_ready && accounts_ready;
    serde_json::json!({
        "id": profile.id,
        "kind": profile.kind,
        "enabled": profile.enabled,
        "ready": ready,
        "protocol": profile.kind.protocol(),
        "base_path": "/v1",
        "required_endpoints": profile.kind.required_endpoints(),
        "session_headers": profile.kind.session_headers(),
        "description": profile.description,
        "models": {
            "mode": if explicit_models { "explicit" } else { "all_visible" },
            "ready": models_ready,
            "checks": model_checks,
        },
        "accounts": {
            "mode": if explicit_accounts { "explicit" } else { "all_visible" },
            "ready": accounts_ready,
            "checks": account_checks,
        },
        "setup": client_profile_setup(profile.kind),
    })
}

fn client_profile_setup(kind: ClientProfileKind) -> serde_json::Value {
    match kind {
        ClientProfileKind::Codex => serde_json::json!({
            "client": "codex",
            "base_url_path": "/v1",
            "primary_endpoint": "/v1/responses",
            "model_listing_endpoint": "/v1/models",
            "auth": "Authorization: Bearer <switchback api key>",
        }),
        ClientProfileKind::ClaudeCode => serde_json::json!({
            "client": "claude-code",
            "base_url_path": "/v1",
            "primary_endpoint": "/v1/messages",
            "count_tokens_endpoint": "/v1/messages/count_tokens",
            "auth": "Authorization: Bearer <switchback api key>",
        }),
    }
}

fn account_health_by_ref(
    config: &Config,
    resolver: &sb_credentials::CredentialResolver,
) -> BTreeMap<String, bool> {
    let mut health = BTreeMap::new();
    for provider in &config.providers {
        for account in resolver.account_health(&provider.id, "") {
            if provider_has_account_for_profile(provider, &account.id) {
                health.insert(format!("{}/{}", provider.id, account.id), account.healthy);
            }
        }
    }
    health
}

fn all_account_statuses(
    config: &Config,
    account_health: &BTreeMap<String, bool>,
) -> Vec<serde_json::Value> {
    config
        .providers
        .iter()
        .flat_map(|provider| provider_account_statuses(provider, account_health).into_iter())
        .collect()
}

fn account_ref_status(
    config: &Config,
    account_health: &BTreeMap<String, bool>,
    account_ref: &str,
) -> Option<serde_json::Value> {
    let (provider_id, account_id) = account_ref.split_once('/')?;
    let provider = config
        .providers
        .iter()
        .find(|provider| provider.id == provider_id)?;
    provider_account_statuses(provider, account_health)
        .into_iter()
        .find(|status| status["ref"].as_str() == Some(account_ref))
        .or_else(|| {
            Some(serde_json::json!({
                "ref": account_ref,
                "provider_id": provider_id,
                "account_id": account_id,
                "available": false,
                "reason": "missing_account",
            }))
        })
}

fn provider_account_statuses(
    provider: &ProviderConfig,
    account_health: &BTreeMap<String, bool>,
) -> Vec<serde_json::Value> {
    if provider.accounts.is_empty() {
        let (auth_kind, auth_sources) = provider_default_auth_summary(provider);
        let account_ref = format!("{}/default", provider.id);
        let healthy = account_health.get(&account_ref).copied().unwrap_or(false);
        return vec![serde_json::json!({
            "ref": account_ref,
            "provider_id": provider.id,
            "account_id": "default",
            "configured": true,
            "healthy": healthy,
            "available": healthy,
            "auth_kind": auth_kind,
            "auth_sources": auth_sources,
            "selection": format!("{:?}", provider.selection).to_lowercase(),
        })];
    }
    provider
        .accounts
        .iter()
        .map(|account| {
            let account_ref = format!("{}/{}", provider.id, account.id);
            let healthy = account_health.get(&account_ref).copied().unwrap_or(false);
            serde_json::json!({
                "ref": account_ref,
                "provider_id": provider.id,
                "account_id": account.id,
                "configured": true,
                "healthy": healthy,
                "available": healthy,
                "auth_kind": auth_kind_name(&account.auth),
                "auth_sources": auth_source_labels(&account.auth),
                "selection": format!("{:?}", provider.selection).to_lowercase(),
                "egress": account.egress.clone(),
            })
        })
        .collect()
}

fn provider_has_account_for_profile(provider: &ProviderConfig, account_id: &str) -> bool {
    if provider.accounts.is_empty() {
        account_id == "default"
    } else {
        provider
            .accounts
            .iter()
            .any(|account| account.id == account_id)
    }
}

fn provider_default_auth_summary(provider: &ProviderConfig) -> (&'static str, Vec<&'static str>) {
    match &provider.kind {
        sb_core::ProviderKind::Mock => ("none", vec!["none"]),
        sb_core::ProviderKind::Bedrock { .. } => ("aws_sigv4", vec!["env"]),
        sb_core::ProviderKind::OpenaiCompatible {
            api_key_env,
            api_key,
            ..
        }
        | sb_core::ProviderKind::Anthropic {
            api_key_env,
            api_key,
            ..
        }
        | sb_core::ProviderKind::Gemini {
            api_key_env,
            api_key,
            ..
        }
        | sb_core::ProviderKind::Vertex {
            api_key_env,
            api_key,
            ..
        } => {
            if api_key_env.is_some() {
                ("api_key", vec!["env"])
            } else if api_key.is_some() {
                ("api_key", vec!["inline"])
            } else {
                ("none", vec!["none"])
            }
        }
    }
}

fn auth_kind_name(auth: &AuthConfig) -> &'static str {
    match auth {
        AuthConfig::None => "none",
        AuthConfig::ApiKey { .. } => "api_key",
        AuthConfig::Oauth { .. } => "oauth",
        AuthConfig::ServiceAccount { .. } => "service_account",
        AuthConfig::AwsSigV4 { .. } => "aws_sigv4",
    }
}

fn auth_source_labels(auth: &AuthConfig) -> Vec<&'static str> {
    match auth {
        AuthConfig::None => vec!["none"],
        AuthConfig::ApiKey { env, inline, vault } => {
            let mut labels = Vec::new();
            if env.is_some() {
                labels.push("env");
            }
            if vault.is_some() {
                labels.push("vault");
            }
            if inline.is_some() {
                labels.push("inline");
            }
            if labels.is_empty() {
                labels.push("missing");
            }
            labels
        }
        AuthConfig::Oauth {
            token_env,
            token,
            token_vault,
            refresh_env,
            refresh,
            refresh_vault,
            client_secret_env,
            client_secret,
            client_secret_vault,
            ..
        } => {
            let mut labels = Vec::new();
            if token_env.is_some() || token.is_some() || token_vault.is_some() {
                labels.push("access_token");
            }
            if refresh_env.is_some() || refresh.is_some() || refresh_vault.is_some() {
                labels.push("refresh_token");
            }
            if refresh_vault.is_some() {
                labels.push("refresh_vault");
            }
            if client_secret_env.is_some()
                || client_secret.is_some()
                || client_secret_vault.is_some()
            {
                labels.push("client_secret");
            }
            if labels.is_empty() {
                labels.push("missing");
            }
            labels
        }
        AuthConfig::ServiceAccount {
            key_file, key_env, ..
        } => {
            let mut labels = Vec::new();
            if key_file.is_some() {
                labels.push("key_file");
            }
            if key_env.is_some() {
                labels.push("key_env");
            }
            if labels.is_empty() {
                labels.push("missing");
            }
            labels
        }
        AuthConfig::AwsSigV4 {
            access_key,
            secret_key,
            session_token,
            session_token_env,
            ..
        } => {
            let mut labels = vec!["env"];
            if access_key.is_some() || secret_key.is_some() {
                labels.push("inline");
            }
            if session_token.is_some() || session_token_env.is_some() {
                labels.push("session_token");
            }
            labels
        }
    }
}

pub(crate) async fn models(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
) -> Json<serde_json::Value> {
    let snap = state.snapshot();
    let scoped = crate::controlplane::scoped_config_for_principal(&snap.config, &principal);
    let ids = model_ids_for_config(&scoped);

    let data: Vec<serde_json::Value> = ids
        .into_iter()
        .map(|id| serde_json::json!({"id": id, "object": "model", "owned_by": "switchback"}))
        .collect();

    Json(serde_json::json!({"object": "list", "data": data}))
}

fn push_model_id(ids: &mut Vec<String>, seen: &mut HashSet<String>, id: impl Into<String>) {
    let id = id.into();
    if seen.insert(id.clone()) {
        ids.push(id);
    }
}

fn model_ids_for_config(config: &Config) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut ids = Vec::new();

    if config.wildcard_route().is_some() {
        for profile in [
            ExecutionProfile::Auto,
            ExecutionProfile::Cheap,
            ExecutionProfile::Fast,
            ExecutionProfile::Coding,
            ExecutionProfile::Private,
            ExecutionProfile::LargeContext,
        ] {
            push_model_id(&mut ids, &mut seen, profile.id());
        }
    }

    for route in &config.routes {
        if let Some(model) = route.match_.model.as_deref().filter(|model| *model != "*") {
            push_model_id(&mut ids, &mut seen, model);
        }
        for target in &route.targets {
            push_model_id(&mut ids, &mut seen, target.clone());
        }
    }

    for name in config.combos.keys() {
        push_model_id(&mut ids, &mut seen, name.clone());
    }

    if let Some(catalog) = &config.catalog {
        for model in &catalog.models {
            push_model_id(
                &mut ids,
                &mut seen,
                format!("{}/{}", model.provider_id, model.id),
            );
        }
    }

    for provider in &config.providers {
        push_model_id(&mut ids, &mut seen, provider.id.clone());
    }

    ids
}
