use std::collections::{BTreeMap, BTreeSet, HashSet};

use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use sb_core::{AiRequest, Config, ExecutionProfile};
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

#[derive(Deserialize)]
pub(crate) struct TracesQuery {
    limit: Option<usize>,
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
    let mut recent = state.traces.recent(q.limit.unwrap_or(50).min(1000));
    if let Some(tenant) = scoped_tenant(&principal) {
        recent.retain(|trace| trace.tenant.as_deref() == Some(tenant));
    }
    Json(serde_json::json!({ "count": recent.len(), "traces": recent }))
}

/// One trace by request id.
pub(crate) async fn trace_by_id(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<String>,
) -> Response {
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
    let mut recent = state.traces.recent(trace_limit);
    if let Some(tenant) = scoped_tenant(&principal) {
        recent.retain(|trace| trace.tenant.as_deref() == Some(tenant));
    }

    let mut unsessioned_count = 0usize;
    let mut sessions: BTreeMap<String, SessionRollupBuilder> = BTreeMap::new();
    for trace in recent {
        let Some(session_id) = trace.session_id.clone().filter(|id| !id.is_empty()) else {
            unsessioned_count += 1;
            continue;
        };
        sessions
            .entry(session_id.clone())
            .or_insert_with(|| SessionRollupBuilder::new(session_id))
            .add(trace);
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
        "source": {
            "kind": "recent_trace_ring",
            "trace_limit": trace_limit,
            "metadata_only": true,
        },
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
    let Some(trace) = state
        .traces
        .get(&id)
        .filter(|trace| trace_visible_to(&principal, trace))
    else {
        return (
            StatusCode::NOT_FOUND,
            Json(openai_error(&format!("no trace `{id}`"), "not_found")),
        )
            .into_response();
    };

    let mut req = AiRequest::new(trace.inbound_model.clone(), Vec::new());
    req.id = sb_core::new_id("preview");
    req.stream = trace.streamed;
    req.tenant = trace.tenant.clone();
    req.project = trace.project.clone();
    if let Some(session_id) = &trace.session_id {
        req.metadata
            .insert("session_id".to_string(), session_id.clone());
    }

    match state.engine.preview_route(&req) {
        Ok((revision, plan)) => Json(serde_json::json!({
            "source_request_id": trace.request_id,
            "revision": revision,
            "original_revision": trace.revision,
            "principal": {
                "tenant": req.tenant,
                "project": req.project,
                "session_id": trace.session_id,
            },
            "decision": plan.decision,
            "candidates": plan.candidates.iter().map(|c| &c.id).collect::<Vec<_>>(),
            "assumptions": [
                "metadata_only_trace: request body is not stored",
                "preview uses inbound_model, stream flag, tenant, project, and session_id only"
            ],
        }))
        .into_response(),
        Err(e) => (
            StatusCode::from_u16(e.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            Json(serde_json::json!({"error": {"message": e.message, "type": e.error_type}})),
        )
            .into_response(),
    }
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

    fn add(&mut self, trace: sb_trace::TraceRecord) {
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
