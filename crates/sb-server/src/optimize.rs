//! The speed report (`GET /v1/optimize/report`, `sb ai doctor`): per local
//! target, measured decode and prompt speed, time to first token, the share
//! of its ceiling, model swaps, prefix-cache hits, speculative acceptance,
//! empty answers after thinking, queue waits, wakes and last use; and
//! findings, each with its evidence and one suggested action.
//!
//! Read-only and machine-free: it reads the usage ledger, the settings
//! registry and its history, the speed-trick evidence the runtime keeps,
//! the lane reports and the engine control's last listing. It never wakes
//! a machine and never calls an engine.
//!
//! Method credit: ceilings first, then measure against them, after
//! <https://github.com/sybil-solutions/inference-optimisation-skill>.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::Deserialize;
use serde_json::{json, Value};

use sb_core::{HostSpec, ModelSettingsDoc, ReportThresholds};
use sb_ledger::UsageRecord;
use sb_runtime::optimize::TargetObservations;

use crate::tenancy::Principal;
use crate::AppState;

const DAY_MS: i64 = 86_400_000;

/// One local target the report covers.
#[derive(Debug, Clone, Default)]
pub(crate) struct TargetInfo {
    pub target: String,
    pub lane: String,
    pub host: String,
    /// Configured on a route, combo or as a direct target (else only seen
    /// in the engine's listing).
    pub configured: bool,
}

/// The settings side of one target: its document, revision and the time
/// of its latest revision.
#[derive(Debug, Clone, Default)]
pub(crate) struct SettingsSide {
    pub doc: ModelSettingsDoc,
    pub revision: u64,
    pub revision_at_ms: Option<i64>,
}

/// Everything the report reads, gathered first so the computation is pure.
#[derive(Default)]
pub(crate) struct ReportInput {
    pub now_ms: i64,
    pub targets: Vec<TargetInfo>,
    pub records: Vec<UsageRecord>,
    pub settings: HashMap<String, SettingsSide>,
    pub observations: BTreeMap<String, TargetObservations>,
    pub hosts: BTreeMap<String, HostSpec>,
    pub thresholds: ReportThresholds,
    pub queue_waits: HashMap<String, u64>,
    /// Per lane: measured wakes and their EWMA.
    pub wakes: HashMap<String, (u64, Option<u64>)>,
    pub stickiness_on: bool,
    /// The client view: measurements from the key's own records only, no
    /// gateway-wide counters and no findings drawn from them.
    pub client: bool,
}

fn median(values: &mut [f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = values.len() / 2;
    Some(if values.len() % 2 == 1 {
        values[mid]
    } else {
        (values[mid - 1] + values[mid]) / 2.0
    })
}

fn round1(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

#[derive(Default)]
struct Measured {
    decode: Vec<f64>,
    prompt: Vec<f64>,
    ttft: Vec<f64>,
    last_seen_ms: Option<i64>,
}

fn measured_of<'a>(records: impl Iterator<Item = &'a UsageRecord>) -> Measured {
    let mut out = Measured::default();
    for record in records {
        let at = record.timestamp_unix as i64 * 1000;
        out.last_seen_ms = Some(out.last_seen_ms.map_or(at, |seen| seen.max(at)));
        if let Some(tps) = record
            .speed
            .decode_tps
            .filter(|t| t.is_finite() && *t > 0.0)
        {
            out.decode.push(tps);
        }
        if let Some(tps) = record
            .speed
            .prompt_tps
            .filter(|t| t.is_finite() && *t > 0.0)
        {
            out.prompt.push(tps);
        }
        if let Some(ms) = record.speed.ttft_ms {
            out.ttft.push(ms as f64);
        }
    }
    out
}

/// A target's decode ceiling and where it came from.
#[derive(Debug, Clone, PartialEq)]
struct CeilingOf {
    decode_tps: f64,
    source: String,
}

/// Build the report. Pure: everything comes from `input`.
pub(crate) fn build_report(input: &ReportInput) -> Value {
    let t = &input.thresholds;
    let mut by_target: HashMap<String, Vec<&UsageRecord>> = HashMap::new();
    for record in &input.records {
        by_target
            .entry(format!("{}/{}", record.provider_id, record.model))
            .or_default()
            .push(record);
    }
    let oldest_ms = input
        .records
        .iter()
        .map(|r| r.timestamp_unix as i64 * 1000)
        .min();
    let mut measured: HashMap<String, Measured> = HashMap::new();
    for info in &input.targets {
        let records = by_target
            .get(&info.target)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        measured.insert(info.target.clone(), measured_of(records.iter().copied()));
    }
    let mut p50_decode: HashMap<String, f64> = HashMap::new();
    for (target, m) in &mut measured {
        if let Some(p50) = median(&mut m.decode) {
            p50_decode.insert(target.clone(), p50);
        }
    }

    // Host bandwidth: configured, else calibrated from the best measured
    // target on the host (decode x bytes per token).
    let bytes_per_token = |target: &str| -> Option<u64> {
        input
            .settings
            .get(target)
            .and_then(|s| s.doc.ceiling.as_ref())
            .and_then(|c| c.bytes_per_token)
    };
    let mut hosts_out = Vec::new();
    let mut bandwidth: HashMap<String, (f64, String)> = HashMap::new();
    let host_names: BTreeSet<&str> = input.targets.iter().map(|t| t.host.as_str()).collect();
    for host in host_names {
        if let Some(gbs) = input.hosts.get(host).and_then(|spec| spec.bandwidth_gbs) {
            bandwidth.insert(host.to_string(), (gbs * 1e9, "configured".to_string()));
            hosts_out.push(json!({"host": host, "bandwidth_gbs": gbs, "source": "configured",
                                  "peak_tflops": input.hosts.get(host).and_then(|s| s.peak_tflops)}));
            continue;
        }
        let best = input
            .targets
            .iter()
            .filter(|info| info.host == host)
            .filter_map(|info| {
                let bpt = bytes_per_token(&info.target)? as f64;
                let decode = *p50_decode.get(&info.target)?;
                Some((decode * bpt, info.target.clone()))
            })
            .max_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        if let Some((bytes_per_sec, from)) = best {
            bandwidth.insert(
                host.to_string(),
                (bytes_per_sec, format!("calibrated from {from}")),
            );
            hosts_out.push(json!({"host": host, "bandwidth_gbs": round1(bytes_per_sec / 1e9),
                                  "source": "calibrated", "calibrated_from": from,
                                  "note": "an effective bandwidth from the best measured target on \
                                           this host, so that target reads 100%; set \
                                           server.optimize.hosts.HOST.bandwidth_gbs for a real ceiling"}));
        }
    }
    let ceiling_of = |info: &TargetInfo| -> Option<CeilingOf> {
        let doc = input.settings.get(&info.target).map(|s| &s.doc);
        if let Some(decode) = doc
            .and_then(|d| d.ceiling.as_ref())
            .and_then(|c| c.decode_tps)
        {
            let source = doc
                .and_then(|d| d.ceiling.as_ref())
                .and_then(|c| c.source.clone())
                .unwrap_or_else(|| "settings".to_string());
            return Some(CeilingOf {
                decode_tps: decode,
                source,
            });
        }
        let bpt = bytes_per_token(&info.target)? as f64;
        let (bytes_per_sec, source) = bandwidth.get(&info.host)?;
        Some(CeilingOf {
            decode_tps: bytes_per_sec / bpt,
            source: format!("bandwidth {} / bytes per token", source),
        })
    };

    let mut targets_out = Vec::new();
    let mut findings = Vec::new();
    let mut finding = |target: &str, kind: &str, evidence: String, action: String| {
        findings
            .push(json!({"target": target, "kind": kind, "evidence": evidence, "action": action}));
    };
    for info in &input.targets {
        let m = measured
            .get_mut(&info.target)
            .expect("measured for every target");
        let decode_samples = m.decode.len();
        let decode = p50_decode.get(&info.target).copied();
        let prompt = median(&mut m.prompt);
        let ttft = median(&mut m.ttft);
        let side = input.settings.get(&info.target);
        let doc = side.map(|s| &s.doc);
        let ceiling = ceiling_of(info);
        let pct = match (decode, &ceiling) {
            (Some(decode), Some(ceiling)) if ceiling.decode_tps > 0.0 => {
                Some(round1(decode / ceiling.decode_tps * 100.0))
            }
            _ => None,
        };
        let obs = (!input.client)
            .then(|| input.observations.get(&info.target))
            .flatten();
        let last_used = [m.last_seen_ms, obs.and_then(|o| o.last_used_ms)]
            .into_iter()
            .flatten()
            .max();
        let guard_on = doc.and_then(|d| d.active_reasoning_guard()).is_some();
        let mut row = json!({
            "target": info.target,
            "lane": info.lane,
            "host": info.host,
            "configured": info.configured,
            "requests": by_target.get(&info.target).map(Vec::len).unwrap_or(0),
            "decode_tps_p50": decode.map(round1),
            // Every served request at whatever concurrency it ran, so not
            // a single-stream (c1) figure.
            "decode_basis": "all requests, median",
            "decode_samples": decode_samples,
            "prompt_tps_p50": prompt.map(round1),
            "ttft_ms_p50": ttft.map(|v| v.round()),
            "ceiling": ceiling.as_ref().map(|c| json!({
                "decode_tps": round1(c.decode_tps),
                "source": c.source,
                "bytes_per_token": bytes_per_token(&info.target),
                "prefill_tps": doc.and_then(|d| d.ceiling.as_ref()).and_then(|c| c.prefill_tps),
            })),
            "pct_of_ceiling": pct,
            "settings_revision": side.map(|s| s.revision).unwrap_or(0),
            "reasoning_guard": guard_on,
            "last_used_ms": last_used,
        });
        if let Some(o) = obs {
            let hit_rate = (o.prompt_tokens > 0)
                .then(|| round1(o.cached_tokens as f64 / o.prompt_tokens as f64 * 100.0));
            let repeat_rate = (o.repeat_prompt_requests > 0).then(|| {
                round1(o.repeat_prompt_hits as f64 / o.repeat_prompt_requests as f64 * 100.0)
            });
            let acceptance = (o.draft_tokens > 0)
                .then(|| round1(o.draft_accepted as f64 / o.draft_tokens as f64 * 100.0));
            row["swaps_last_hour"] = json!(o.swaps_last_hour);
            row["swaps_last_day"] = json!(o.swaps_last_day);
            row["cache"] = json!({
                "cached_tokens": o.cached_tokens, "prompt_tokens": o.prompt_tokens,
                "hit_requests": o.cache_hit_requests, "token_hit_pct": hit_rate,
                "repeat_prompt_requests": o.repeat_prompt_requests,
                "repeat_prompt_hit_pct": repeat_rate,
            });
            row["speculative"] = json!({
                "requests": o.spec_requests, "draft_tokens": o.draft_tokens,
                "accepted_tokens": o.draft_accepted, "acceptance_pct": acceptance,
            });
            row["empty_after_thinking"] = json!(o.empty_after_thinking);
            row["reasoning_guard_applied"] = json!(o.reasoning_guard_applied);
            row["sticky_slot_pins"] = json!(o.sticky_kept);
        }
        if !input.client {
            row["queue_waits"] = json!(input.queue_waits.get(&info.lane).copied().unwrap_or(0));
            if let Some((count, ewma)) = input.wakes.get(&info.lane) {
                row["wake"] = json!({"count": count, "ewma_ms": ewma});
            }
        }

        // Findings.
        if let (Some(pct), Some(ceiling), Some(decode)) = (pct, &ceiling, decode) {
            if decode_samples >= t.regression_samples as usize && pct < t.decode_ceiling_pct {
                finding(
                    &info.target,
                    "decode_below_ceiling",
                    format!(
                        "decode {:.1} tok/s (all requests, median of {decode_samples}) is {pct:.0}% \
                         of its ceiling {:.1} tok/s ({})",
                        decode, ceiling.decode_tps, ceiling.source
                    ),
                    format!(
                        "likely layers on the CPU or a slow KV type; run `sb ai tune {}`",
                        info.target
                    ),
                );
            }
        }
        if let Some(o) = obs {
            if o.swaps_last_hour > u64::from(t.swaps_per_hour) {
                let others: Vec<String> = o.swapped_out_last_hour.keys().cloned().collect();
                finding(
                    &info.target,
                    "swap_thrash",
                    format!(
                        "{} model swaps to {} in the last hour (limit {}), replacing {}",
                        o.swaps_last_hour,
                        info.target,
                        t.swaps_per_hour,
                        if others.is_empty() {
                            "another model".to_string()
                        } else {
                            others.join(", ")
                        }
                    ),
                    "point both names at one model, or raise the router's models-max if memory \
                     allows (check with `sb ai fit`)"
                        .to_string(),
                );
            }
            if o.repeat_prompt_requests >= u64::from(t.min_requests) {
                let rate = o.repeat_prompt_hits as f64 / o.repeat_prompt_requests as f64 * 100.0;
                if rate < t.cache_hit_pct {
                    finding(
                        &info.target,
                        "cache_cold",
                        format!(
                            "{} requests repeated a system prompt the target had seen, and {:.0}% \
                             of them reused the prefix cache",
                            o.repeat_prompt_requests, rate
                        ),
                        if input.stickiness_on {
                            "stickiness is on: check that the engine keeps cache_prompt on and has \
                             enough slots (load parallel)"
                                .to_string()
                        } else {
                            "turn on stickiness (server.optimize.stickiness: true)".to_string()
                        },
                    );
                }
            }
            if o.spec_requests >= u64::from(t.min_requests) && o.draft_tokens > 0 {
                let rate = o.draft_accepted as f64 / o.draft_tokens as f64 * 100.0;
                if rate < t.acceptance_pct {
                    finding(
                        &info.target,
                        "low_acceptance",
                        format!(
                            "speculative acceptance {:.0}% ({} of {} drafted tokens over {} requests)",
                            rate, o.draft_accepted, o.draft_tokens, o.spec_requests
                        ),
                        format!(
                            "try a shorter draft (`sb ai tune {} --levers draft_max`) or no MTP",
                            info.target
                        ),
                    );
                }
            }
            if o.empty_after_thinking > 0 && !guard_on {
                finding(
                    &info.target,
                    "empty_after_thinking",
                    format!(
                        "{} answers came back empty: the token limit ended them while the model \
                         was still thinking",
                        o.empty_after_thinking
                    ),
                    format!(
                        "turn on the reasoning guard: `sb ai set {} reasoning=true \
                         reasoning_guard.min_tokens=1024`",
                        info.target
                    ),
                );
            }
        }
        if !input.client {
            let unused_ms = i64::from(t.unused_days) * DAY_MS;
            let unused = match last_used {
                Some(last) => (input.now_ms - last > unused_ms)
                    .then(|| format!("last used {} days ago", (input.now_ms - last) / DAY_MS)),
                None => oldest_ms
                    .filter(|oldest| input.now_ms - oldest > unused_ms)
                    .map(|oldest| {
                        format!(
                            "no use in the usage window ({} days)",
                            (input.now_ms - oldest) / DAY_MS
                        )
                    }),
            };
            if let Some(evidence) = unused {
                finding(
                    &info.target,
                    "unused",
                    evidence,
                    "removal candidate (Switchback deletes nothing)".to_string(),
                );
            }
        }
        // Regression after the latest settings revision.
        if let Some((revision, at_ms)) = side
            .filter(|s| s.revision > 1)
            .and_then(|s| s.revision_at_ms.map(|at| (s.revision, at)))
        {
            let records = by_target
                .get(&info.target)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let before = measured_of(
                records
                    .iter()
                    .copied()
                    .filter(|r| (r.timestamp_unix as i64) * 1000 < at_ms),
            );
            let after = measured_of(
                records
                    .iter()
                    .copied()
                    .filter(|r| (r.timestamp_unix as i64) * 1000 >= at_ms),
            );
            let need = t.regression_samples as usize;
            let factor = t.regression_pct / 100.0;
            let mut worse = Vec::new();
            let (mut b, mut a) = (before.decode.clone(), after.decode.clone());
            if b.len() >= need && a.len() >= need {
                if let (Some(b), Some(a)) = (median(&mut b), median(&mut a)) {
                    if a < b * (1.0 - factor) {
                        worse.push(format!(
                            "decode {:.1} -> {:.1} tok/s ({:.0}%)",
                            b,
                            a,
                            (a / b - 1.0) * 100.0
                        ));
                    }
                }
            }
            let (mut b, mut a) = (before.ttft.clone(), after.ttft.clone());
            if b.len() >= need && a.len() >= need {
                if let (Some(b), Some(a)) = (median(&mut b), median(&mut a)) {
                    if b > 0.0 && a > b * (1.0 + factor) {
                        worse.push(format!(
                            "ttft {:.0} -> {:.0} ms (+{:.0}%)",
                            b,
                            a,
                            (a / b - 1.0) * 100.0
                        ));
                    }
                }
            }
            if !worse.is_empty() {
                finding(
                    &info.target,
                    "regression",
                    format!(
                        "after settings revision {revision}: {} ({} samples before, {} after)",
                        worse.join(", "),
                        before.decode.len().max(before.ttft.len()),
                        after.decode.len().max(after.ttft.len())
                    ),
                    format!(
                        "undo it with `sb ai revert {} {}`",
                        info.target,
                        revision - 1
                    ),
                );
            }
        }
        targets_out.push(row);
    }
    json!({
        "object": "optimize.report",
        "view": if input.client { "client" } else { "operator" },
        "generated_at_ms": input.now_ms,
        "window": {"records": input.records.len(), "oldest_ms": oldest_ms},
        "hosts": if input.client { Value::Array(Vec::new()) } else { Value::Array(hosts_out) },
        "targets": targets_out,
        "findings": findings,
        "method": "https://github.com/sybil-solutions/inference-optimisation-skill",
    })
}

#[derive(Debug, Deserialize)]
pub(crate) struct ReportQuery {
    /// Usage records to read (default 5000, the most).
    limit: Option<usize>,
    /// Only this target (`provider/model`) or public name's targets.
    model: Option<String>,
}

/// The local targets: every target of a provider that has a managed lane
/// (from routes, combos and the engine's last listing).
fn local_targets(state: &AppState, scoped: &sb_core::Config, operator: bool) -> Vec<TargetInfo> {
    let snap = state.snapshot();
    let lanes: HashMap<&str, &sb_core::LocalExecutorConfig> = snap
        .config
        .local_executors
        .iter()
        .map(|lane| (lane.name.as_str(), lane))
        .collect();
    let mut out: BTreeMap<String, TargetInfo> = BTreeMap::new();
    let mut add = |target: &str, configured: bool| {
        let Some((provider, _)) = target.split_once('/') else {
            return;
        };
        let Some(lane) = lanes.get(provider) else {
            return;
        };
        let entry = out.entry(target.to_string()).or_insert_with(|| TargetInfo {
            target: target.to_string(),
            lane: lane.name.clone(),
            host: lane.host.clone().unwrap_or_else(|| lane.name.clone()),
            configured,
        });
        entry.configured |= configured;
    };
    for route in &scoped.routes {
        for target in &route.targets {
            add(target, true);
        }
    }
    for combo in scoped.combos.values() {
        for target in &combo.models {
            add(target, true);
        }
    }
    if operator {
        for (target, _) in state.engine.model_settings().all() {
            add(&target, true);
        }
        for lane in &snap.config.local_executors {
            if crate::engine_control::engine_lane(&snap.config, &lane.name).is_none() {
                continue;
            }
            if let Some((_, models)) = state.engines.cached_models(&lane.name) {
                for row in models {
                    if let Some(model) = row["id"].as_str() {
                        add(&format!("{}/{model}", lane.name), false);
                    }
                }
            }
        }
        for record in state.ledger.recent(5_000) {
            add(&format!("{}/{}", record.provider_id, record.model), true);
        }
    }
    out.into_values().collect()
}

/// `GET /v1/optimize/report`: the speed report. Operator and admin keys
/// see every local target and the findings; a client key sees the targets
/// its tenant reaches, measured from its own records only.
pub(crate) async fn report(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Query(query): Query<ReportQuery>,
) -> Response {
    let operator = principal.is_operator_or_admin();
    let snap = state.snapshot();
    let scoped = if principal.is_admin() || operator {
        (*snap.config).clone()
    } else {
        crate::controlplane::scoped_config_for_principal(&snap.config, &principal)
    };
    let mut targets = local_targets(&state, &scoped, operator);
    if let Some(model) = query.model.as_deref() {
        let wanted: HashSet<String> = scoped
            .routes
            .iter()
            .filter(|route| route.match_.model.as_deref() == Some(model) || route.name == model)
            .flat_map(|route| route.targets.iter().cloned())
            .chain(
                scoped
                    .combos
                    .get(model)
                    .into_iter()
                    .flat_map(|c| c.models.iter().cloned()),
            )
            .chain(std::iter::once(model.to_string()))
            .collect();
        targets.retain(|t| wanted.contains(&t.target));
    }
    let limit = query.limit.unwrap_or(5_000).clamp(1, 5_000);
    let mut records = state.ledger.recent(limit);
    if !operator {
        let tenant = principal.tenant.as_deref();
        let visible: HashSet<&str> = targets.iter().map(|t| t.target.as_str()).collect();
        records.retain(|record| {
            tenant.is_some()
                && record.tenant.as_deref() == tenant
                && visible.contains(format!("{}/{}", record.provider_id, record.model).as_str())
        });
        if tenant.is_none() {
            targets.clear();
        }
    }
    let registry = state.engine.model_settings();
    let mut settings = HashMap::new();
    for info in &targets {
        if let Some(entry) = registry.get(&info.target) {
            let revision_at_ms = registry
                .history(&info.target, 1)
                .ok()
                .and_then(|history| history.first().map(|change| change.at_ms));
            settings.insert(
                info.target.clone(),
                SettingsSide {
                    doc: (*entry.doc).clone(),
                    revision: entry.revision,
                    revision_at_ms,
                },
            );
        }
    }
    let optimize = &snap.config.server.optimize;
    let mut wakes = HashMap::new();
    for report in state.capacity.reports() {
        if report.wake_configured {
            wakes.insert(
                report.name.clone(),
                (report.wake_samples, report.wake_duration_ewma_ms),
            );
        }
    }
    let input = ReportInput {
        now_ms: sb_store::now_millis(),
        targets,
        records,
        settings,
        observations: state.engine.optimize().observations(),
        hosts: optimize.hosts.clone(),
        thresholds: optimize.report.clone(),
        queue_waits: state.engine.optimize().queue_waits(),
        wakes,
        stickiness_on: optimize.stickiness
            || snap
                .config
                .routes
                .iter()
                .any(|r| r.optimize.is_some_and(|o| o.stickiness == Some(true))),
        client: !operator,
    };
    let mut report = build_report(&input);
    if operator {
        report["quiet_windows"] = json!(optimize
            .windows()
            .map(|(window, tz)| window.label(tz))
            .collect::<Vec<_>>());
        report["in_quiet_window"] = json!(sb_runtime::optimize::quiet_window_now(
            optimize,
            input.now_ms
        ));
    }
    Json(report).into_response()
}

fn refuse(status: axum::http::StatusCode, kind: &str, message: String) -> Response {
    (
        status,
        Json(json!({"error": {"message": message, "type": kind, "reason": kind}})),
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LockBody {
    target: String,
    #[serde(default)]
    ttl_secs: Option<u64>,
    #[serde(default)]
    reason: Option<String>,
    /// The run's hold on the lane's machine: it ends no later than the lock.
    #[serde(default)]
    hold_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExtendBody {
    ttl_secs: u64,
}

/// The longest a tuning lock may last (it is extended while a run goes on).
const MAX_LOCK_SECS: u64 = 4 * 3600;

/// `POST /v1/optimize/locks {"target", "ttl_secs", "reason", "hold_id"}`
/// (operator): take a lane's tuning lock for `sb ai tune`. Refused (409,
/// typed `reason`) inside a quiet window (`quiet_window`), while the lane
/// has a request in flight or queued (`lane_busy`), or while another run
/// holds it (`locked`). Never wakes anything.
///
/// The lock, and the run's hold named by `hold_id`, end no later than the
/// next quiet window's start, so a tuner that dies leaves nothing that
/// reaches into the window. The answer's `hold` is that hold after the
/// cap (`null` when no such hold is on the lane's machine).
pub(crate) async fn lock_create(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Json(body): Json<LockBody>,
) -> Response {
    use axum::http::StatusCode;
    if !principal.is_operator_or_admin() {
        return crate::tenancy::forbidden();
    }
    let snap = state.snapshot();
    let Some((provider, _)) = body.target.split_once('/') else {
        return refuse(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            format!("`{}` is not provider/model", body.target),
        );
    };
    let Some(lane) = crate::engine_control::engine_lane(&snap.config, provider) else {
        return refuse(
            StatusCode::BAD_REQUEST,
            "no_engine_control",
            format!(
                "`{}` has no engine control (its lane needs an `engine:` block)",
                body.target
            ),
        );
    };
    let now = sb_store::now_millis();
    let optimize = &snap.config.server.optimize;
    if let Some(window) = sb_runtime::optimize::quiet_window_now(optimize, now) {
        return refuse(
            StatusCode::CONFLICT,
            "quiet_window",
            format!(
                "inside the quiet window {window} (server.optimize.quiet_windows); try after it"
            ),
        );
    }
    let report = state
        .capacity
        .reports()
        .into_iter()
        .find(|report| report.name == lane.name);
    let busy = state
        .engine
        .model_settings()
        .in_flight_on_provider(&lane.name)
        + report
            .as_ref()
            .map(|r| r.in_flight as usize + r.queue_depth as usize)
            .unwrap_or(0);
    if busy > 0 {
        return refuse(
            StatusCode::CONFLICT,
            "lane_busy",
            format!(
                "lane `{}` has {busy} request(s) in flight or queued; try when it is idle",
                lane.name
            ),
        );
    }
    let host = snap
        .config
        .local_executors
        .iter()
        .find(|l| l.name == lane.name)
        .and_then(|l| l.host.clone())
        .unwrap_or_else(|| lane.name.clone());
    let ttl = body.ttl_secs.unwrap_or(1800).clamp(1, MAX_LOCK_SECS);
    let mut expires_at_ms = now + (ttl as i64) * 1000;
    if let Some(start) = sb_runtime::optimize::next_quiet_window_start(optimize, now) {
        expires_at_ms = expires_at_ms.min(start);
    }
    let lock = sb_runtime::optimize::TuneLock {
        id: sb_core::new_id("tune"),
        target: body.target.clone(),
        lane: lane.name.clone(),
        host: host.clone(),
        reason: body.reason.unwrap_or_default().chars().take(200).collect(),
        created_ms: now,
        expires_at_ms,
    };
    match state.engine.optimize().take_lock(lock) {
        Ok(lock) => {
            tracing::info!(lane = %lock.lane, target = %lock.target, id = %lock.id, "tuning lock taken");
            let mut answer = json!(lock);
            if let Some(hold_id) = &body.hold_id {
                answer["hold"] = cap_hold(&state, &host, hold_id, lock.expires_at_ms, now)
                    .map(|hold| crate::handlers::hosts::hold_json(&hold))
                    .unwrap_or(Value::Null);
            }
            (StatusCode::CREATED, Json(answer)).into_response()
        }
        Err(held) => refuse(
            StatusCode::CONFLICT,
            "locked",
            format!(
                "lane `{}` is under another tuning run ({}, {})",
                held.lane, held.target, held.id
            ),
        ),
    }
}

/// End the hold `hold_id` on `host` no later than `until_ms` (epoch ms on
/// the lock's clock, `now_ms` being that clock's now). `None` when no such
/// hold is active.
fn cap_hold(
    state: &AppState,
    host: &str,
    hold_id: &str,
    until_ms: i64,
    now_ms: i64,
) -> Option<crate::holds::Hold> {
    let holds = state.capacity.holds();
    let hold_now = state.capacity.now_ms();
    let hold = holds.get(host, hold_id, hold_now)?;
    let left_ms = u64::try_from(until_ms.saturating_sub(now_ms))
        .unwrap_or(0)
        .max(1_000);
    if hold.expires_at_ms <= hold_now.saturating_add(left_ms) {
        return Some(hold);
    }
    holds.extend(host, hold_id, left_ms, hold_now)
}

/// `GET /v1/optimize/locks` (operator): the live tuning locks.
pub(crate) async fn lock_list(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
) -> Response {
    if !principal.is_operator_or_admin() {
        return crate::tenancy::forbidden();
    }
    Json(json!({"object": "list", "data": state.engine.optimize().locks()})).into_response()
}

/// `PATCH /v1/optimize/locks/{id} {"ttl_secs"}` (operator): end it that
/// many seconds from now. The answer adds `quiet_window` (the window in
/// force, or null). A 404 means the lock is gone (expired, released, or
/// the gateway restarted): the run stops.
pub(crate) async fn lock_extend(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(body): Json<ExtendBody>,
) -> Response {
    if !principal.is_operator_or_admin() {
        return crate::tenancy::forbidden();
    }
    let expires = sb_store::now_millis() + (body.ttl_secs.clamp(1, MAX_LOCK_SECS) as i64) * 1000;
    match state.engine.optimize().extend_lock(&id, expires) {
        // The tuner's heartbeat: it also learns whether a quiet window
        // began, so it stops at its next safe point.
        Some(lock) => {
            let snap = state.snapshot();
            let mut view = json!(lock);
            view["quiet_window"] = json!(sb_runtime::optimize::quiet_window_now(
                &snap.config.server.optimize,
                sb_store::now_millis()
            ));
            Json(view).into_response()
        }
        None => refuse(
            axum::http::StatusCode::NOT_FOUND,
            "not_found",
            format!("no tuning lock {id}"),
        ),
    }
}

/// `DELETE /v1/optimize/locks/{id}` (operator): end it.
pub(crate) async fn lock_delete(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    if !principal.is_operator_or_admin() {
        return crate::tenancy::forbidden();
    }
    match state.engine.optimize().release_lock(&id) {
        Some(lock) => {
            tracing::info!(lane = %lock.lane, id = %lock.id, "tuning lock released");
            Json(json!({"id": lock.id, "deleted": true})).into_response()
        }
        None => refuse(
            axum::http::StatusCode::NOT_FOUND,
            "not_found",
            format!("no tuning lock {id}"),
        ),
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct ExperimentsQuery {
    model: Option<String>,
    limit: Option<usize>,
}

/// `POST /v1/optimize/experiments` (operator): append one row of the
/// tuning ledger. The row is metadata (lever, value, metrics, verdict), at
/// most 16 KiB, and must name its `target` and `run_id`.
pub(crate) async fn experiment_append(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Json(row): Json<Value>,
) -> Response {
    use axum::http::StatusCode;
    if !principal.is_operator_or_admin() {
        return crate::tenancy::forbidden();
    }
    let target = row["target"].as_str().unwrap_or("").to_string();
    let run_id = row["run_id"].as_str().unwrap_or("").to_string();
    if target.is_empty() || run_id.is_empty() || !row.is_object() {
        return refuse(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "an experiment row is an object with `target` and `run_id`".to_string(),
        );
    }
    let text = row.to_string();
    if text.len() > 16 * 1024 {
        return refuse(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "an experiment row is at most 16 KiB".to_string(),
        );
    }
    let record = sb_store::ExperimentRecord {
        id: 0,
        target,
        run_id,
        at_ms: sb_store::now_millis(),
        row: text,
    };
    let (id, durable) = match state.engine.store() {
        Some(store) => match store.optimize_experiment_append(&record) {
            Ok(id) => (id, true),
            Err(e) => {
                return refuse(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "store_error",
                    format!("state store: {}", e.0),
                )
            }
        },
        None => (state.engine.optimize().remember_experiment(record), false),
    };
    (
        StatusCode::CREATED,
        Json(json!({"id": id, "durable": durable})),
    )
        .into_response()
}

/// `GET /v1/optimize/experiments?model=TARGET&limit=N` (operator): the
/// tuning ledger, newest first.
pub(crate) async fn experiment_list(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Query(query): Query<ExperimentsQuery>,
) -> Response {
    if !principal.is_operator_or_admin() {
        return crate::tenancy::forbidden();
    }
    let limit = query.limit.unwrap_or(200).clamp(1, 10_000);
    let target = query.model.as_deref();
    let (records, durable) = match state.engine.store() {
        Some(store) => (
            store
                .optimize_experiment_list(target, limit)
                .unwrap_or_default(),
            true,
        ),
        None => (
            state
                .engine
                .optimize()
                .remembered_experiments(target, limit),
            false,
        ),
    };
    let data: Vec<Value> = records
        .into_iter()
        .map(|record| {
            let mut row: Value = serde_json::from_str(&record.row).unwrap_or(json!({}));
            row["id"] = json!(record.id);
            row["at_ms"] = json!(record.at_ms);
            row
        })
        .collect();
    Json(json!({"object": "list", "durable": durable, "data": data})).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sb_core::{Ceiling, ReasoningGuard, Usage};
    use sb_ledger::RequestSpeed;

    const NOW: i64 = 1_800_000_000_000;

    fn record(target: &str, at_ms: i64, decode: f64, ttft: u64) -> UsageRecord {
        let (provider, model) = target.split_once('/').unwrap();
        let mut record =
            UsageRecord::priced("r", provider, model, None, Usage::default(), 10, false, 0);
        record.timestamp_unix = (at_ms / 1000) as u64;
        record.speed = RequestSpeed {
            decode_tps: Some(decode),
            ttft_ms: Some(ttft),
            prompt_tps: Some(1000.0),
            ..RequestSpeed::default()
        };
        record
    }

    fn input(targets: &[(&str, &str)]) -> ReportInput {
        ReportInput {
            now_ms: NOW,
            targets: targets
                .iter()
                .map(|(target, host)| TargetInfo {
                    target: target.to_string(),
                    lane: target.split('/').next().unwrap().to_string(),
                    host: host.to_string(),
                    configured: true,
                })
                .collect(),
            ..ReportInput::default()
        }
    }

    fn kinds(report: &Value) -> Vec<String> {
        report["findings"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["kind"].as_str().unwrap().to_string())
            .collect()
    }

    fn ceiling_doc(decode: Option<f64>, bpt: Option<u64>) -> SettingsSide {
        SettingsSide {
            doc: ModelSettingsDoc {
                ceiling: Some(Ceiling {
                    decode_tps: decode,
                    bytes_per_token: bpt,
                    ..Ceiling::default()
                }),
                ..ModelSettingsDoc::default()
            },
            revision: 1,
            revision_at_ms: None,
        }
    }

    #[test]
    fn decode_far_below_its_ceiling_is_a_finding() {
        let mut inp = input(&[("box/q", "box")]);
        inp.records = (0..8)
            .map(|i| record("box/q", NOW - i * 1000, 30.0, 200))
            .collect();
        inp.settings
            .insert("box/q".into(), ceiling_doc(Some(100.0), None));
        let report = build_report(&inp);
        assert_eq!(report["targets"][0]["pct_of_ceiling"], json!(30.0));
        assert_eq!(kinds(&report), ["decode_below_ceiling"]);
        assert!(report["findings"][0]["action"]
            .as_str()
            .unwrap()
            .contains("sb ai tune box/q"));
        // At 50% of the ceiling: no finding.
        inp.records = (0..8)
            .map(|i| record("box/q", NOW - i * 1000, 50.0, 200))
            .collect();
        assert!(kinds(&build_report(&inp)).is_empty());
    }

    #[test]
    fn bandwidth_is_calibrated_from_the_best_target_on_a_host() {
        let mut inp = input(&[("box/fast", "box"), ("box/slow", "box")]);
        inp.records = (0..8)
            .flat_map(|i| {
                [
                    record("box/fast", NOW - i * 1000, 100.0, 100),
                    record("box/slow", NOW - i * 1000, 20.0, 100),
                ]
            })
            .collect();
        // Same bytes per token: the slow one is at 20% of what the host showed.
        inp.settings
            .insert("box/fast".into(), ceiling_doc(None, Some(1_000_000_000)));
        inp.settings
            .insert("box/slow".into(), ceiling_doc(None, Some(1_000_000_000)));
        let report = build_report(&inp);
        assert_eq!(report["hosts"][0]["source"], "calibrated");
        assert_eq!(report["hosts"][0]["bandwidth_gbs"], json!(100.0));
        let slow = report["targets"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["target"] == "box/slow")
            .unwrap()
            .clone();
        assert_eq!(slow["pct_of_ceiling"], json!(20.0));
        assert!(slow["ceiling"]["source"]
            .as_str()
            .unwrap()
            .contains("calibrated"));
        // A configured bandwidth wins.
        inp.hosts.insert(
            "box".into(),
            HostSpec {
                bandwidth_gbs: Some(400.0),
                peak_tflops: None,
            },
        );
        let report = build_report(&inp);
        assert_eq!(report["hosts"][0]["source"], "configured");
        let fast = &report["targets"][0];
        assert_eq!(fast["pct_of_ceiling"], json!(25.0));
    }

    #[test]
    fn swap_thrash_is_a_finding() {
        let mut inp = input(&[("box/a", "box")]);
        let mut obs = TargetObservations {
            swaps_last_hour: 9,
            ..TargetObservations::default()
        };
        obs.swapped_out_last_hour.insert("b".into(), 9);
        inp.observations.insert("box/a".into(), obs);
        let report = build_report(&inp);
        assert_eq!(kinds(&report), ["swap_thrash"]);
        let evidence = report["findings"][0]["evidence"].as_str().unwrap();
        assert!(
            evidence.contains("9 model swaps") && evidence.contains("replacing b"),
            "{evidence}"
        );
        // At the limit: none.
        inp.observations.get_mut("box/a").unwrap().swaps_last_hour = 6;
        assert!(kinds(&build_report(&inp)).is_empty());
    }

    #[test]
    fn a_cold_cache_on_repeated_prompts_suggests_stickiness() {
        let mut inp = input(&[("box/a", "box")]);
        inp.observations.insert(
            "box/a".into(),
            TargetObservations {
                repeat_prompt_requests: 20,
                repeat_prompt_hits: 1,
                ..TargetObservations::default()
            },
        );
        let report = build_report(&inp);
        assert_eq!(kinds(&report), ["cache_cold"]);
        assert!(report["findings"][0]["action"]
            .as_str()
            .unwrap()
            .contains("turn on stickiness"));
        inp.stickiness_on = true;
        let report = build_report(&inp);
        assert!(report["findings"][0]["action"]
            .as_str()
            .unwrap()
            .contains("cache_prompt"));
        // Too few requests: no finding.
        inp.observations
            .get_mut("box/a")
            .unwrap()
            .repeat_prompt_requests = 5;
        assert!(kinds(&build_report(&inp)).is_empty());
    }

    #[test]
    fn low_speculative_acceptance_is_a_finding() {
        let mut inp = input(&[("box/a", "box")]);
        inp.observations.insert(
            "box/a".into(),
            TargetObservations {
                spec_requests: 12,
                draft_tokens: 300,
                draft_accepted: 90,
                ..TargetObservations::default()
            },
        );
        let report = build_report(&inp);
        assert_eq!(kinds(&report), ["low_acceptance"]);
        assert_eq!(
            report["targets"][0]["speculative"]["acceptance_pct"],
            json!(30.0)
        );
        inp.observations.get_mut("box/a").unwrap().draft_accepted = 200;
        assert!(kinds(&build_report(&inp)).is_empty());
    }

    #[test]
    fn empty_answers_after_thinking_suggest_the_guard_until_it_is_on() {
        let mut inp = input(&[("box/a", "box")]);
        inp.observations.insert(
            "box/a".into(),
            TargetObservations {
                empty_after_thinking: 3,
                ..TargetObservations::default()
            },
        );
        let report = build_report(&inp);
        assert_eq!(kinds(&report), ["empty_after_thinking"]);
        assert!(report["findings"][0]["action"]
            .as_str()
            .unwrap()
            .contains("reasoning_guard"));
        inp.settings.insert(
            "box/a".into(),
            SettingsSide {
                doc: ModelSettingsDoc {
                    reasoning: true,
                    reasoning_guard: Some(ReasoningGuard::default()),
                    ..ModelSettingsDoc::default()
                },
                revision: 1,
                revision_at_ms: None,
            },
        );
        assert!(kinds(&build_report(&inp)).is_empty());
    }

    #[test]
    fn a_model_unused_for_two_weeks_is_a_removal_candidate() {
        let mut inp = input(&[("box/old", "box"), ("box/new", "box")]);
        inp.records = vec![
            record("box/old", NOW - 20 * DAY_MS, 40.0, 100),
            record("box/new", NOW - 1000, 40.0, 100),
        ];
        let report = build_report(&inp);
        assert_eq!(kinds(&report), ["unused"]);
        assert_eq!(report["findings"][0]["target"], "box/old");
        assert!(report["findings"][0]["action"]
            .as_str()
            .unwrap()
            .contains("deletes nothing"));
        // Never used, and the window is older than two weeks: also a candidate.
        let mut inp = input(&[("box/never", "box"), ("box/new", "box")]);
        inp.records = vec![
            record("box/new", NOW - 30 * DAY_MS, 40.0, 100),
            record("box/new", NOW - 1000, 40.0, 100),
        ];
        assert_eq!(kinds(&build_report(&inp)), ["unused"]);
        // A young window says nothing about a model never seen.
        inp.records = vec![record("box/new", NOW - 1000, 40.0, 100)];
        assert!(kinds(&build_report(&inp)).is_empty());
    }

    #[test]
    fn a_regression_after_a_settings_revision_needs_six_samples_a_side() {
        let mut inp = input(&[("box/a", "box")]);
        let at = NOW - 3_600_000;
        let mut records: Vec<UsageRecord> = (0..6)
            .map(|i| record("box/a", at - (i + 1) * 60_000, 100.0, 200))
            .collect();
        records.extend((0..6).map(|i| record("box/a", at + (i + 1) * 60_000, 85.0, 200)));
        inp.records = records.clone();
        inp.settings.insert(
            "box/a".into(),
            SettingsSide {
                doc: ModelSettingsDoc::default(),
                revision: 4,
                revision_at_ms: Some(at),
            },
        );
        let report = build_report(&inp);
        assert_eq!(kinds(&report), ["regression"]);
        let finding = &report["findings"][0];
        assert!(finding["evidence"]
            .as_str()
            .unwrap()
            .contains("revision 4: decode 100.0 -> 85.0"));
        assert!(finding["action"]
            .as_str()
            .unwrap()
            .contains("sb ai revert box/a 3"));
        // Five samples after: not enough.
        inp.records = records[..11].to_vec();
        assert!(kinds(&build_report(&inp)).is_empty());
        // Within 10%: no regression.
        inp.records = records
            .iter()
            .map(|r| {
                let mut r = r.clone();
                if r.speed.decode_tps == Some(85.0) {
                    r.speed.decode_tps = Some(95.0);
                }
                r
            })
            .collect();
        assert!(kinds(&build_report(&inp)).is_empty());
        // TTFT worse by more than 10% is one too.
        inp.records = records
            .iter()
            .map(|r| {
                let mut r = r.clone();
                r.speed.decode_tps = Some(100.0);
                if (r.timestamp_unix as i64) * 1000 >= at {
                    r.speed.ttft_ms = Some(300);
                }
                r
            })
            .collect();
        let report = build_report(&inp);
        assert!(report["findings"][0]["evidence"]
            .as_str()
            .unwrap()
            .contains("ttft 200 -> 300 ms"));
    }

    #[test]
    fn the_client_view_has_no_gateway_wide_counters_or_findings_from_them() {
        let mut inp = input(&[("box/a", "box")]);
        inp.observations.insert(
            "box/a".into(),
            TargetObservations {
                swaps_last_hour: 50,
                empty_after_thinking: 4,
                ..TargetObservations::default()
            },
        );
        inp.client = true;
        let report = build_report(&inp);
        assert_eq!(report["view"], "client");
        assert!(kinds(&report).is_empty());
        assert!(report["targets"][0].get("swaps_last_hour").is_none());
        assert!(report["targets"][0].get("queue_waits").is_none());
    }
}
