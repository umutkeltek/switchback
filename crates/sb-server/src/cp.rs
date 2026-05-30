//! The `/cp/v1` declarative control plane (Oracle's "control-plane surface").
//!
//! A k8s-style envelope (`apiVersion` / `kind` / `metadata{name,revision,etag}` /
//! `spec`) over the live config, plus a draft → validate → publish lifecycle and
//! a `route-preview` that turns the explainable `RouteDecision` into a product
//! surface — all without touching the YAML file (the API is authoritative; YAML
//! stays bootstrap). The dashboard and the AI-facing CLI are meant to be thin
//! clients over THIS, not second config parsers.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use sb_core::Config;
use serde_json::{json, Value};

use crate::controlplane::redact_config;
use crate::AppState;

const API_VERSION: &str = "cp.switchback.dev/v1";

/// `(url segment, envelope kind, config array key, name field)` for each
/// declarative resource projected from the config.
const KINDS: &[(&str, &str, &str, &str)] = &[
    ("providers", "ProviderEndpoint", "providers", "id"),
    ("routes", "RouteProfile", "routes", "name"),
    ("tenants", "Tenant", "tenants", "id"),
    ("egress", "EgressProfile", "egress", "id"),
    ("plugins", "Plugin", "plugins", "type"),
];

fn kind_for(segment: &str) -> Option<(&'static str, &'static str, &'static str)> {
    KINDS
        .iter()
        .find(|(seg, ..)| *seg == segment)
        .map(|(_, kind, key, name)| (*kind, *key, *name))
}

fn envelope(kind: &str, name: &str, revision: u64, spec: Value) -> Value {
    json!({
        "apiVersion": API_VERSION,
        "kind": kind,
        "metadata": {
            "name": name,
            "revision": revision,
            "etag": format!("W/\"rev-{revision}\""),
        },
        "spec": spec,
    })
}

fn cp_error(status: StatusCode, message: impl Into<String>) -> Response {
    (
        status,
        Json(json!({"error": {"message": message.into(), "type": "cp_error"}})),
    )
        .into_response()
}

/// `GET /cp/v1` — discovery: the resource kinds + the lifecycle/preview verbs.
pub async fn root(State(state): State<AppState>) -> Json<Value> {
    Json(json!({
        "apiVersion": API_VERSION,
        "revision": state.revision(),
        "kinds": KINDS.iter().map(|(seg, kind, ..)| json!({"name": kind, "path": seg})).collect::<Vec<_>>(),
        "verbs": [
            "GET /cp/v1/resources/{kind}", "GET /cp/v1/resources/{kind}/{name}",
            "POST /cp/v1/drafts", "GET /cp/v1/drafts", "GET /cp/v1/drafts/{id}",
            "POST /cp/v1/drafts/{id}/validate", "POST /cp/v1/drafts/{id}/publish",
            "POST /cp/v1/route-preview",
        ],
    }))
}

/// `GET /cp/v1/resources/{kind}` — list the declarative resources of a kind.
pub async fn list_resources(
    State(state): State<AppState>,
    Path(kind_seg): Path<String>,
) -> Response {
    let Some((kind, key, name_field)) = kind_for(&kind_seg) else {
        return cp_error(StatusCode::NOT_FOUND, format!("unknown kind `{kind_seg}`"));
    };
    let snap = state.snapshot();
    let redacted = redact_config(&snap.config);
    let items = redacted
        .get(key)
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let resources: Vec<Value> = items
        .iter()
        .enumerate()
        .map(|(i, spec)| {
            let name = spec
                .get(name_field)
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("{kind_seg}-{i}"));
            envelope(kind, &name, snap.revision, spec.clone())
        })
        .collect();
    Json(json!({ "apiVersion": API_VERSION, "kind": kind, "items": resources })).into_response()
}

/// `GET /cp/v1/resources/{kind}/{name}` — one declarative resource.
pub async fn get_resource(
    State(state): State<AppState>,
    Path((kind_seg, name)): Path<(String, String)>,
) -> Response {
    let Some((kind, key, name_field)) = kind_for(&kind_seg) else {
        return cp_error(StatusCode::NOT_FOUND, format!("unknown kind `{kind_seg}`"));
    };
    let snap = state.snapshot();
    let redacted = redact_config(&snap.config);
    let found = redacted
        .get(key)
        .and_then(|v| v.as_array())
        .and_then(|items| {
            items
                .iter()
                .find(|spec| spec.get(name_field).and_then(|v| v.as_str()) == Some(name.as_str()))
        });
    match found {
        Some(spec) => Json(envelope(kind, &name, snap.revision, spec.clone())).into_response(),
        None => cp_error(StatusCode::NOT_FOUND, format!("no {kind} `{name}`")),
    }
}

/// `POST /cp/v1/route-preview` — the explainable decision for a request, computed
/// without executing it. Body is an OpenAI-shaped chat request.
pub async fn route_preview(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> Response {
    let req = match sb_protocols::openai::request_from_openai_chat(&body) {
        Ok(req) => req,
        Err(msg) => return cp_error(StatusCode::BAD_REQUEST, msg),
    };
    match state.engine.preview_route(&req) {
        Ok((revision, plan)) => Json(json!({
            "revision": revision,
            "decision": plan.decision,
            "candidates": plan.candidates.iter().map(|c| &c.id).collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(e) => (
            StatusCode::from_u16(e.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            Json(json!({"error": {"message": e.message, "type": e.error_type}})),
        )
            .into_response(),
    }
}

// --- Drafts -----------------------------------------------------------------

struct Draft {
    config: Config,
    base_revision: u64,
    created_at_ms: i64,
}

/// In-memory draft store (process-lifetime). Durable drafts (the store) are a
/// follow-up; a first slice keeps proposed configs in memory until published.
#[derive(Clone, Default)]
pub struct DraftStore(Arc<Mutex<HashMap<String, Draft>>>);

impl DraftStore {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Draft>> {
        self.0.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// `POST /cp/v1/drafts` — stage a proposed config (full `Config` as JSON). The
/// draft is validated for shape on create; semantic validation is `/validate`.
pub async fn create_draft(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    let config: Config = match serde_json::from_value(body) {
        Ok(cfg) => cfg,
        Err(e) => return cp_error(StatusCode::BAD_REQUEST, format!("invalid config: {e}")),
    };
    let id = sb_core::new_id("draft");
    let base_revision = state.revision();
    state.drafts.lock().insert(
        id.clone(),
        Draft {
            config,
            base_revision,
            created_at_ms: sb_store::now_millis(),
        },
    );
    (
        StatusCode::CREATED,
        Json(json!({ "id": id, "base_revision": base_revision })),
    )
        .into_response()
}

/// `GET /cp/v1/drafts` — list staged drafts (metadata only).
pub async fn list_drafts(State(state): State<AppState>) -> Json<Value> {
    let drafts = state.drafts.lock();
    let items: Vec<Value> = drafts
        .iter()
        .map(|(id, d)| {
            json!({ "id": id, "base_revision": d.base_revision, "created_at_ms": d.created_at_ms })
        })
        .collect();
    Json(json!({ "drafts": items }))
}

/// `GET /cp/v1/drafts/{id}` — a draft's proposed config, redacted.
pub async fn get_draft(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let drafts = state.drafts.lock();
    match drafts.get(&id) {
        Some(d) => Json(json!({
            "id": id,
            "base_revision": d.base_revision,
            "config": redact_config(&d.config),
        }))
        .into_response(),
        None => cp_error(StatusCode::NOT_FOUND, format!("no draft `{id}`")),
    }
}

/// `POST /cp/v1/drafts/{id}/validate` — compile-check the draft (registry +
/// resolver) without publishing.
pub async fn validate_draft(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let config = {
        let drafts = state.drafts.lock();
        match drafts.get(&id) {
            Some(d) => d.config.clone(),
            None => return cp_error(StatusCode::NOT_FOUND, format!("no draft `{id}`")),
        }
    };
    match sb_runtime::Engine::validate_config(&config) {
        Ok(()) => Json(json!({ "valid": true })).into_response(),
        Err(e) => Json(json!({ "valid": false, "errors": [e] })).into_response(),
    }
}

/// `POST /cp/v1/drafts/{id}/publish` — validate + atomically hot-swap the draft
/// config (a new revision). Optimistic concurrency: if an `If-Match: <revision>`
/// header is present it must equal the current revision, else 409 (someone else
/// published since this draft was based). On success the draft is consumed.
pub async fn publish_draft(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let config = {
        let drafts = state.drafts.lock();
        match drafts.get(&id) {
            Some(d) => d.config.clone(),
            None => return cp_error(StatusCode::NOT_FOUND, format!("no draft `{id}`")),
        }
    };

    // Optimistic concurrency via If-Match (the current revision).
    if let Some(want) = headers.get("if-match").and_then(|v| v.to_str().ok()) {
        let want = want.trim_matches('"').trim_start_matches("W/").trim_matches('"');
        let current = state.revision().to_string();
        if want != current && want != format!("rev-{current}") {
            return cp_error(
                StatusCode::CONFLICT,
                format!("revision changed (If-Match `{want}` != current `{current}`)"),
            );
        }
    }

    if let Err(e) = sb_runtime::Engine::validate_config(&config) {
        return cp_error(StatusCode::UNPROCESSABLE_ENTITY, format!("draft invalid: {e}"));
    }
    match state.engine.reload(config) {
        Ok(revision) => {
            state.drafts.lock().remove(&id);
            Json(json!({ "ok": true, "revision": revision })).into_response()
        }
        Err(e) => cp_error(StatusCode::UNPROCESSABLE_ENTITY, format!("publish failed: {e}")),
    }
}
