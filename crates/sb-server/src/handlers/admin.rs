//! `/admin/*` — loopback-only operator surfaces.
//!
//! `GET /admin/lanes` is the machine-readable lane-health API: one entry per
//! configured route (a "lane" in the routing contract — e.g. route `scout-code`
//! serving model `scout/code`), enriched with live runtime state: per-target
//! circuit position (read-only — polling never consumes the breaker's
//! half-open probe), account-pool health, rolling p50/p95 latency, last error,
//! and request counts from the recent trace ring. Metadata only, as always.
//!
//! Consumers: `run_packet.py` / `probe_free_pool.ts` (umut-os) pick healthy
//! lanes at runtime instead of routing on a static registry. The CLI
//! `switchback lane doctor` stays the *contract* auditor (static config); this
//! endpoint is the *live* view.

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use sb_core::RouteConfig;
use sb_credentials::CircuitState;
use sb_trace::{AttemptOutcome, TraceRecord};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::http_response::openai_error;
use crate::AppState;

#[derive(Default, Deserialize)]
pub(crate) struct LanesQuery {
    /// Override the "today" boundary (unix seconds). Defaults to UTC midnight.
    since: Option<u64>,
}

/// Middleware for the `/admin` subtree: loopback peers only, regardless of
/// auth — these surfaces exist for local orchestrators. The real listener
/// provides connect info; in-process callers (tests) have none and pass.
pub(crate) async fn require_loopback(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0);
    if peer.is_some_and(|peer| !peer.ip().is_loopback()) {
        return (
            StatusCode::FORBIDDEN,
            Json(openai_error(
                "/admin endpoints are loopback-only",
                "forbidden",
            )),
        )
            .into_response();
    }
    next.run(req).await
}

/// `GET /admin/lanes` — loopback-only lane health.
pub(crate) async fn lanes(State(state): State<AppState>, Query(q): Query<LanesQuery>) -> Response {
    let snap = state.snapshot();
    let now_unix = sb_trace::now_unix();
    let today_start = q.since.unwrap_or(now_unix - now_unix % 86_400);

    // One pass over the trace ring, bucketed by route name.
    let traces = state.traces.recent(state.traces.len());
    let oldest_unix = traces.iter().map(|t| t.timestamp_unix).min();

    let lanes: Vec<Value> = snap
        .config
        .routes
        .iter()
        .map(|route| lane_json(&snap, route, &traces, today_start))
        .collect();

    Json(json!({
        "schema": "switchback/admin-lanes@1",
        "revision": snap.revision,
        "now_unix": now_unix,
        "today_start_unix": today_start,
        "source": {
            "kind": "recent_trace_ring",
            "len": traces.len(),
            "oldest_unix": oldest_unix,
            "metadata_only": true,
        },
        "lanes": lanes,
    }))
    .into_response()
}

fn lane_json(
    snap: &sb_runtime::Snapshot,
    route: &RouteConfig,
    traces: &[TraceRecord],
    today_start: u64,
) -> Value {
    let lane = route.match_.model.as_deref().unwrap_or("*");

    let mut blocked = 0usize;
    let mut attention = 0usize; // half-open circuits: recovering, watch them
    let mut known = 0usize;
    let targets: Vec<Value> = route
        .targets
        .iter()
        .map(|target| {
            let provider_id = provider_for_target(snap, target);
            match provider_id {
                Some(provider_id) => {
                    known += 1;
                    let model = target
                        .strip_prefix(provider_id)
                        .and_then(|rest| rest.strip_prefix('/'))
                        .unwrap_or("");
                    let circuit = snap.resolver.circuit_view(provider_id);
                    let pool = snap.resolver.pool_health(provider_id, model);
                    if circuit.state == CircuitState::Open || pool.healthy == 0 {
                        blocked += 1;
                    } else if circuit.state == CircuitState::HalfOpen {
                        attention += 1;
                    }
                    json!({
                        "id": target,
                        "provider_id": provider_id,
                        "model": model,
                        "circuit": circuit,
                        "accounts": { "total": pool.total, "healthy": pool.healthy },
                    })
                }
                None => json!({
                    "id": target,
                    "provider_id": Value::Null,
                    "circuit": Value::Null,
                    "accounts": Value::Null,
                }),
            }
        })
        .collect();

    let status = if route.targets.is_empty() {
        "unroutable"
    } else if known > 0 && blocked == known {
        "down"
    } else if blocked > 0 || attention > 0 {
        "degraded"
    } else {
        "healthy"
    };

    let stats = LaneStats::collect(&route.name, traces, today_start);

    json!({
        "route": route.name,
        "lane": lane,
        "status": status,
        "targets": targets,
        "requests": {
            "window": stats.window,
            "today": stats.today,
            "errors_window": stats.errors_window,
            "errors_today": stats.errors_today,
        },
        "latency_ms": {
            "p50": stats.p50,
            "p95": stats.p95,
            "samples": stats.samples,
        },
        "last_request_unix": stats.last_request_unix,
        "last_error": stats.last_error,
    })
}

/// Resolve a route target (`provider/model`) to its provider id by the longest
/// configured-provider prefix. `None` = unknown provider (passthrough target).
fn provider_for_target<'a>(snap: &'a sb_runtime::Snapshot, target: &str) -> Option<&'a str> {
    snap.config
        .providers
        .iter()
        .filter(|p| target == p.id || target.starts_with(&format!("{}/", p.id)))
        .max_by_key(|p| p.id.len())
        .map(|p| p.id.as_str())
}

#[derive(Default)]
struct LaneStats {
    window: usize,
    today: usize,
    errors_window: usize,
    errors_today: usize,
    samples: usize,
    p50: Option<u64>,
    p95: Option<u64>,
    last_request_unix: Option<u64>,
    last_error: Value,
}

impl LaneStats {
    /// Aggregate one lane's traces. `traces` is newest-first (the ring order).
    fn collect(route_name: &str, traces: &[TraceRecord], today_start: u64) -> Self {
        let mut stats = LaneStats {
            last_error: Value::Null,
            ..LaneStats::default()
        };
        let mut latencies: Vec<u64> = Vec::new();
        for trace in traces.iter().filter(|t| t.route == route_name) {
            stats.window += 1;
            let today = trace.timestamp_unix >= today_start;
            if today {
                stats.today += 1;
            }
            if stats.last_request_unix.is_none() {
                stats.last_request_unix = Some(trace.timestamp_unix);
            }
            let errored = trace.final_status >= 400;
            if errored {
                stats.errors_window += 1;
                if today {
                    stats.errors_today += 1;
                }
            } else {
                latencies.push(trace.total_latency_ms);
            }
            if stats.last_error.is_null() {
                stats.last_error = last_error_json(trace);
            }
        }
        latencies.sort_unstable();
        stats.samples = latencies.len();
        stats.p50 = percentile(&latencies, 0.50);
        stats.p95 = percentile(&latencies, 0.95);
        stats
    }
}

/// The newest error evidence in a trace: a failed attempt (even when the
/// request later succeeded via fallback — that is exactly the signal a breaker
/// investigation needs) or an error final status. `Null` when clean.
fn last_error_json(trace: &TraceRecord) -> Value {
    let failed_attempt = trace.attempts.iter().rev().find_map(|a| match &a.outcome {
        AttemptOutcome::Failed { class, fell_over } => Some(json!({
            "timestamp_unix": trace.timestamp_unix,
            "request_id": trace.request_id,
            "status": trace.final_status,
            "class": class,
            "target_id": a.target_id,
            "fell_over": fell_over,
        })),
        AttemptOutcome::Success => None,
    });
    match failed_attempt {
        Some(value) => value,
        None if trace.final_status >= 400 => json!({
            "timestamp_unix": trace.timestamp_unix,
            "request_id": trace.request_id,
            "status": trace.final_status,
            "class": format!("http_{}", trace.final_status),
            "target_id": Value::Null,
            "fell_over": false,
        }),
        None => Value::Null,
    }
}

fn percentile(sorted: &[u64], q: f64) -> Option<u64> {
    if sorted.is_empty() {
        return None;
    }
    let idx = ((sorted.len() - 1) as f64 * q).round() as usize;
    sorted.get(idx).copied()
}
