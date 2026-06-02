use std::collections::HashSet;

use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use sb_core::{Config, ExecutionProfile};
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
