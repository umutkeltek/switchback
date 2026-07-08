use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context};
use sb_bodylog::{BodyEventQuery, BodyLogger, BodyLoggerConfig, BodyRecord, CaptureStage};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct BodyAudit {
    pub request_id: String,
    pub generated_at_unix_ms: i64,
    pub trace: Option<Value>,
    pub timeline: Vec<BodyEventSummary>,
    pub request: BodyArtifact,
    pub response: Option<BodyArtifact>,
    pub response_events: Vec<Value>,
    pub assistant_text: String,
    pub context: ContextBreakdown,
    pub metrics: MetricsRow,
    pub suggested_actions: Vec<String>,
    #[serde(skip_serializing)]
    pub request_raw: Vec<u8>,
    #[serde(skip_serializing)]
    pub response_raw: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct BodyArtifact {
    pub stage: String,
    pub content_type: Option<String>,
    pub body_bytes: u64,
    pub body_sha256: String,
    pub archive_path: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct BodyEventSummary {
    pub event_id: String,
    pub request_id: String,
    pub observed_at_unix_ms: i64,
    pub capture_stage: String,
    pub protocol: String,
    pub upstream: Option<String>,
    pub model: Option<String>,
    pub status: Option<u16>,
    pub content_type: Option<String>,
    pub body_bytes: u64,
    pub metadata: Value,
}

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct ContextBreakdown {
    pub request_bytes: u64,
    pub estimated_tokens: u64,
    pub categories: Vec<ContextCategory>,
    pub top_tools: Vec<TopTool>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ContextCategory {
    pub name: String,
    pub bytes: u64,
    pub pct: f64,
    pub estimated_tokens: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TopTool {
    pub name: String,
    pub bytes: u64,
    pub pct: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MetricsRow {
    pub request_id: String,
    pub session_id: Option<String>,
    pub client: Option<String>,
    pub lane: Option<String>,
    pub cwd: Option<String>,
    pub project: Option<String>,
    pub model: Option<String>,
    pub path: Option<String>,
    pub status: Option<u16>,
    pub error_kind: Option<String>,
    pub request_bytes: u64,
    pub response_bytes: u64,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_creation_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub cost_micros: u64,
    pub total_latency_ms: Option<u64>,
    pub ttft_ms: Option<u64>,
    pub upstream_ms: Option<u64>,
    pub route: Option<String>,
    pub selected_upstream: Option<String>,
    pub headroom_used: bool,
    pub context_breakdown: BTreeMap<String, u64>,
    pub top_tools: Vec<TopTool>,
    pub observed_at_unix_ms: i64,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct AuditWriteResult {
    pub dir: String,
    pub markdown_path: String,
    pub request_raw_path: String,
    pub request_pretty_path: Option<String>,
    pub response_raw_path: Option<String>,
    pub response_events_path: Option<String>,
    pub assistant_text_path: Option<String>,
    pub metrics_path: String,
    pub daily_rollup_path: String,
}

pub(crate) fn body_logger_config(
    state_dir: PathBuf,
    archive_root: Option<PathBuf>,
    legacy_jsonl: Option<PathBuf>,
) -> BodyLoggerConfig {
    let legacy_jsonl = legacy_jsonl.unwrap_or_else(|| state_dir.join("tap-bodies.jsonl"));
    let mut config = BodyLoggerConfig::from_legacy_sink(legacy_jsonl);
    config.state_dir = state_dir.clone();
    if let Some(archive_root) = archive_root {
        config.archive_root = archive_root;
    }
    config
}

pub(crate) fn open_existing_logger(config: BodyLoggerConfig) -> anyhow::Result<BodyLogger> {
    BodyLogger::open_existing(config)?.ok_or_else(|| {
        anyhow!("body log index does not exist; enable capture and record traffic first")
    })
}

pub(crate) fn latest_request_id(
    logger: &BodyLogger,
    client: Option<&str>,
) -> anyhow::Result<String> {
    let events = logger.query_events(BodyEventQuery {
        capture_stage: Some(CaptureStage::ClientInbound),
        limit: 1000,
        ..BodyEventQuery::default()
    })?;
    events
        .into_iter()
        .find(|event| client_matches(event, client))
        .map(|event| event.request_id)
        .ok_or_else(|| anyhow!("no captured request matched client filter"))
}

pub(crate) fn build_audit(
    logger: &BodyLogger,
    request_id: &str,
    trace: Option<Value>,
) -> anyhow::Result<BodyAudit> {
    let mut events = logger.events_for_request(request_id)?;
    if events.is_empty() {
        return Err(anyhow!("no body events found for request `{request_id}`"));
    }
    events.sort_by_key(|event| event.observed_at_unix_ms);
    let request_event = events
        .iter()
        .find(|event| is_request_stage(&event.capture_stage))
        .or_else(|| events.first())
        .ok_or_else(|| anyhow!("no request body event found for `{request_id}`"))?;
    let response_event = events
        .iter()
        .rev()
        .find(|event| is_response_stage(&event.capture_stage));

    let request_raw = logger
        .read_blob(&request_event.body_sha256)
        .with_context(|| format!("read request blob for `{request_id}`"))?;
    let response_raw = match response_event {
        Some(event) => Some(
            logger
                .read_blob(&event.body_sha256)
                .with_context(|| format!("read response blob for `{request_id}`"))?,
        ),
        None => None,
    };
    let response_events = response_raw
        .as_ref()
        .map(|raw| parse_sse_events(raw))
        .unwrap_or_default();
    let assistant_text = response_raw
        .as_ref()
        .map(|raw| {
            assistant_text(
                raw,
                response_event.and_then(|event| event.content_type.as_deref()),
            )
        })
        .unwrap_or_default();
    let context = analyze_context(&request_raw);
    let timeline = events.iter().map(event_summary).collect::<Vec<_>>();
    let metrics = metrics_row(
        request_id,
        request_event,
        response_event,
        &request_raw,
        response_raw.as_deref(),
        &context,
        trace.as_ref(),
    );
    let suggested_actions = suggested_actions(&context, &metrics, trace.as_ref(), response_event);

    Ok(BodyAudit {
        request_id: request_id.to_string(),
        generated_at_unix_ms: now_unix_ms(),
        trace,
        timeline,
        request: artifact_summary(request_event),
        response: response_event.map(artifact_summary),
        response_events,
        assistant_text,
        context,
        metrics,
        suggested_actions,
        request_raw,
        response_raw,
    })
}

pub(crate) fn write_audit_bundle(
    state_dir: &Path,
    out_root: Option<&Path>,
    audit: &BodyAudit,
    open: bool,
) -> anyhow::Result<AuditWriteResult> {
    let safe_id = safe_file_component(&audit.request_id);
    let dir = out_root
        .map(Path::to_path_buf)
        .unwrap_or_else(|| state_dir.join("audits"))
        .join(format!("{}_{}", audit.generated_at_unix_ms, safe_id));
    fs::create_dir_all(&dir)?;

    let request_raw_path = dir.join("request.raw.json");
    write_private_file(&request_raw_path, &audit.request_raw)?;
    let request_pretty_path = pretty_json_bytes(&audit.request_raw).map(|pretty| {
        let path = dir.join("request.pretty.json");
        write_private_file(&path, pretty)?;
        Ok::<PathBuf, anyhow::Error>(path)
    });
    let request_pretty_path = match request_pretty_path {
        Some(path) => Some(path?),
        None => None,
    };

    let mut response_raw_path = None;
    let mut response_events_path = None;
    let mut assistant_text_path = None;
    if let Some(raw) = &audit.response_raw {
        let ext = if is_sse(
            audit
                .response
                .as_ref()
                .and_then(|r| r.content_type.as_deref()),
            raw,
        ) {
            "sse"
        } else {
            "json"
        };
        let path = dir.join(format!("response.raw.{ext}"));
        write_private_file(&path, raw)?;
        response_raw_path = Some(path);
        if !audit.response_events.is_empty() {
            let path = dir.join("response.events.json");
            write_private_file(&path, serde_json::to_vec_pretty(&audit.response_events)?)?;
            response_events_path = Some(path);
        }
        let path = dir.join("response.assistant_text.txt");
        write_private_file(&path, &audit.assistant_text)?;
        assistant_text_path = Some(path);
    }

    let markdown = render_markdown(audit);
    let markdown_path = dir.join(format!("{}_{}.md", audit.generated_at_unix_ms, safe_id));
    write_private_file(&markdown_path, markdown)?;
    let (metrics_path, daily_rollup_path) = write_metrics(state_dir, &audit.metrics)?;

    if open {
        open_path(&markdown_path);
    }

    Ok(AuditWriteResult {
        dir: dir.to_string_lossy().into_owned(),
        markdown_path: markdown_path.to_string_lossy().into_owned(),
        request_raw_path: request_raw_path.to_string_lossy().into_owned(),
        request_pretty_path: request_pretty_path.map(|p| p.to_string_lossy().into_owned()),
        response_raw_path: response_raw_path.map(|p| p.to_string_lossy().into_owned()),
        response_events_path: response_events_path.map(|p| p.to_string_lossy().into_owned()),
        assistant_text_path: assistant_text_path.map(|p| p.to_string_lossy().into_owned()),
        metrics_path: metrics_path.to_string_lossy().into_owned(),
        daily_rollup_path: daily_rollup_path.to_string_lossy().into_owned(),
    })
}

pub(crate) fn render_markdown(audit: &BodyAudit) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# Switchback Body Audit: `{}`\n\n",
        audit.request_id
    ));
    out.push_str("## Verdict\n\n");
    for action in &audit.suggested_actions {
        out.push_str(&format!("- {action}\n"));
    }
    if audit.suggested_actions.is_empty() {
        out.push_str("- No obvious action from captured evidence.\n");
    }
    out.push_str("\n## Route / Meta\n\n");
    out.push_str(&format!("- request_id: `{}`\n", audit.request_id));
    out.push_str(&format!(
        "- client: `{}`\n",
        audit.metrics.client.as_deref().unwrap_or("unknown")
    ));
    out.push_str(&format!(
        "- model: `{}`\n",
        audit.metrics.model.as_deref().unwrap_or("unknown")
    ));
    out.push_str(&format!(
        "- route: `{}`\n",
        audit.metrics.route.as_deref().unwrap_or("missing")
    ));
    out.push_str(&format!(
        "- selected_upstream: `{}`\n",
        audit
            .metrics
            .selected_upstream
            .as_deref()
            .unwrap_or("missing")
    ));
    out.push_str(&format!(
        "- status: `{}`\n",
        audit
            .metrics
            .status
            .map(|status| status.to_string())
            .unwrap_or_else(|| "missing".to_string())
    ));
    out.push_str("\n## Exact Paths\n\n");
    out.push_str(&format!(
        "- request blob: `{}`\n",
        audit.request.archive_path
    ));
    if let Some(response) = &audit.response {
        out.push_str(&format!("- response blob: `{}`\n", response.archive_path));
    } else {
        out.push_str("- response blob: missing\n");
    }
    out.push_str("\n## Usage / Cost / Latency\n\n");
    out.push_str(&format!(
        "- input_tokens: `{}`\n",
        audit.metrics.input_tokens
    ));
    out.push_str(&format!(
        "- cached_input_tokens: `{}`\n",
        audit.metrics.cached_input_tokens
    ));
    out.push_str(&format!(
        "- output_tokens: `{}`\n",
        audit.metrics.output_tokens
    ));
    out.push_str(&format!(
        "- reasoning_tokens: `{}`\n",
        audit.metrics.reasoning_tokens
    ));
    out.push_str(&format!(
        "- total_latency_ms: `{}`\n",
        audit
            .metrics
            .total_latency_ms
            .map(|v| v.to_string())
            .unwrap_or_else(|| "missing".to_string())
    ));
    out.push_str("\n## Context Eaters\n\n");
    out.push_str("| Rank | Category | Bytes | Est. tokens | Share |\n");
    out.push_str("|---:|---|---:|---:|---:|\n");
    for (idx, category) in audit.context.categories.iter().enumerate() {
        out.push_str(&format!(
            "| {} | {} | {} | {} | {:.1}% |\n",
            idx + 1,
            category.name,
            category.bytes,
            category.estimated_tokens,
            category.pct
        ));
    }
    out.push_str("\n## Top Tools\n\n");
    if audit.context.top_tools.is_empty() {
        out.push_str("No top-level tool schemas detected.\n");
    } else {
        out.push_str("| Rank | Tool | Bytes | Share |\n");
        out.push_str("|---:|---|---:|---:|\n");
        for (idx, tool) in audit.context.top_tools.iter().enumerate() {
            out.push_str(&format!(
                "| {} | `{}` | {} | {:.1}% |\n",
                idx + 1,
                tool.name,
                tool.bytes,
                tool.pct
            ));
        }
    }
    out.push_str("\n\n## Timeline\n\n");
    for event in &audit.timeline {
        out.push_str(&format!(
            "- `{}` `{}` status={} bytes={} upstream=`{}` path=`{}`\n",
            event.capture_stage,
            event.protocol,
            event
                .status
                .map(|status| status.to_string())
                .unwrap_or_else(|| "none".to_string()),
            event.body_bytes,
            event.upstream.as_deref().unwrap_or("unknown"),
            event
                .metadata
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
        ));
    }
    out.push_str("\n## Response\n\n");
    if audit.assistant_text.trim().is_empty() {
        out.push_str("No assistant text reconstructed from the captured response.\n");
    } else {
        out.push_str("```text\n");
        out.push_str(&audit.assistant_text);
        if !audit.assistant_text.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("```\n");
    }
    out
}

pub(crate) fn load_trace_json_from_state(state_dir: &Path, request_id: &str) -> Option<Value> {
    let path = state_dir.join("traces.jsonl");
    let text = fs::read_to_string(path).ok()?;
    text.lines().rev().find_map(|line| {
        let value = serde_json::from_str::<Value>(line).ok()?;
        (value.get("request_id").and_then(Value::as_str) == Some(request_id)).then_some(value)
    })
}

fn write_metrics(state_dir: &Path, row: &MetricsRow) -> anyhow::Result<(PathBuf, PathBuf)> {
    let metrics_dir = state_dir.join("metrics");
    fs::create_dir_all(metrics_dir.join("daily"))?;
    let requests_path = metrics_dir.join("requests.jsonl");
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&requests_path)?;
    writeln!(file, "{}", serde_json::to_string(row)?)?;

    let day = day_string(row.observed_at_unix_ms);
    let mut rows = Vec::new();
    if let Ok(text) = fs::read_to_string(&requests_path) {
        for line in text.lines() {
            let Ok(existing) = serde_json::from_str::<MetricsRow>(line) else {
                continue;
            };
            if day_string(existing.observed_at_unix_ms) == day {
                rows.push(existing);
            }
        }
    }
    let daily_path = metrics_dir.join("daily").join(format!("{day}.json"));
    fs::write(
        &daily_path,
        serde_json::to_vec_pretty(&daily_rollup(&day, &rows))?,
    )?;
    Ok((requests_path, daily_path))
}

fn daily_rollup(day: &str, rows: &[MetricsRow]) -> Value {
    let mut by_client: BTreeMap<String, u64> = BTreeMap::new();
    let mut by_route: BTreeMap<String, u64> = BTreeMap::new();
    let mut total_cost_micros = 0u64;
    let mut total_request_bytes = 0u64;
    let mut total_response_bytes = 0u64;
    let mut total_latency_ms = 0u64;
    let mut latency_count = 0u64;
    let mut failure_count = 0u64;
    let mut slowest: Option<&MetricsRow> = None;
    for row in rows {
        *by_client
            .entry(row.client.clone().unwrap_or_else(|| "unknown".to_string()))
            .or_default() += 1;
        *by_route
            .entry(row.route.clone().unwrap_or_else(|| "unknown".to_string()))
            .or_default() += 1;
        total_request_bytes += row.request_bytes;
        total_response_bytes += row.response_bytes;
        total_cost_micros += row.cost_micros;
        if let Some(latency) = row.total_latency_ms {
            total_latency_ms += latency;
            latency_count += 1;
        }
        if is_failure(row) {
            failure_count += 1;
        }
        if row.total_latency_ms.unwrap_or(0) > slowest.and_then(|r| r.total_latency_ms).unwrap_or(0)
        {
            slowest = Some(row);
        }
    }
    serde_json::json!({
        "day": day,
        "requests": rows.len(),
        "total_request_bytes": total_request_bytes,
        "total_response_bytes": total_response_bytes,
        "total_cost_micros": total_cost_micros,
        "avg_request_bytes": average(total_request_bytes, rows.len() as u64),
        "avg_response_bytes": average(total_response_bytes, rows.len() as u64),
        "avg_latency_ms": average(total_latency_ms, latency_count),
        "failure_count": failure_count,
        "failure_rate_pct": percent(failure_count, rows.len() as u64),
        "by_client": by_client,
        "by_route": by_route,
        "slowest": slowest.map(|row| serde_json::json!({
            "request_id": row.request_id,
            "latency_ms": row.total_latency_ms,
            "route": row.route,
            "model": row.model,
        })),
    })
}

fn metrics_row(
    request_id: &str,
    request: &BodyRecord,
    response: Option<&BodyRecord>,
    request_raw: &[u8],
    response_raw: Option<&[u8]>,
    context: &ContextBreakdown,
    trace: Option<&Value>,
) -> MetricsRow {
    let usage = trace.and_then(|trace| trace.get("usage"));
    let mut context_breakdown = BTreeMap::new();
    for category in &context.categories {
        context_breakdown.insert(category.name.clone(), category.bytes);
    }
    for comparable_key in ["tools", "system", "messages", "tool_results", "images"] {
        context_breakdown
            .entry(comparable_key.to_string())
            .or_insert(0);
    }
    MetricsRow {
        request_id: request_id.to_string(),
        session_id: trace
            .and_then(|trace| trace.get("session_id"))
            .and_then(Value::as_str)
            .map(str::to_string),
        client: infer_client(request),
        lane: infer_lane(request),
        cwd: request
            .metadata
            .get("cwd")
            .and_then(Value::as_str)
            .map(str::to_string),
        project: trace
            .and_then(|trace| trace.get("project"))
            .and_then(Value::as_str)
            .map(str::to_string),
        model: request.model.clone().or_else(|| {
            trace
                .and_then(|trace| trace.get("inbound_model"))
                .and_then(Value::as_str)
                .map(str::to_string)
        }),
        path: request
            .metadata
            .get("path")
            .and_then(Value::as_str)
            .map(str::to_string),
        status: response.and_then(|response| response.status),
        error_kind: trace_error_kind(trace),
        request_bytes: request_raw.len() as u64,
        response_bytes: response_raw.map(|raw| raw.len() as u64).unwrap_or(0),
        input_tokens: usage_u64(usage, "input_tokens"),
        cached_input_tokens: usage_u64(usage, "cached_input_tokens"),
        cache_creation_tokens: usage_u64(usage, "cache_creation_tokens"),
        output_tokens: usage_u64(usage, "output_tokens"),
        reasoning_tokens: usage_u64(usage, "reasoning_tokens"),
        cost_micros: trace
            .and_then(|trace| trace.get("cost_micros"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        total_latency_ms: trace
            .and_then(|trace| trace.get("total_latency_ms"))
            .and_then(Value::as_u64),
        ttft_ms: trace_ttft_ms(trace),
        upstream_ms: trace_upstream_ms(trace),
        route: trace
            .and_then(|trace| trace.get("route"))
            .and_then(Value::as_str)
            .map(str::to_string),
        selected_upstream: selected_upstream(request, trace),
        headroom_used: uses_headroom(request),
        context_breakdown,
        top_tools: context.top_tools.clone(),
        observed_at_unix_ms: request.observed_at_unix_ms,
    }
}

fn analyze_context(raw: &[u8]) -> ContextBreakdown {
    let request_bytes = raw.len() as u64;
    let Ok(json) = serde_json::from_slice::<Value>(raw) else {
        return ContextBreakdown {
            request_bytes,
            estimated_tokens: estimated_tokens(request_bytes),
            categories: vec![category("unknown", request_bytes, request_bytes)],
            top_tools: Vec::new(),
        };
    };
    let mut buckets: BTreeMap<&'static str, u64> = BTreeMap::new();
    if let Some(system) = json.get("system") {
        add_bucket(&mut buckets, "system", json_size(system));
    }
    if let Some(tools) = json.get("tools") {
        add_bucket(&mut buckets, "tools", json_size(tools));
    }
    if let Some(messages) = json.get("messages").and_then(Value::as_array) {
        for message in messages {
            add_message_buckets(&mut buckets, message);
        }
    }
    if let Some(input) = json.get("input") {
        add_bucket(&mut buckets, "messages", json_size(input));
    }
    for key in ["attachments", "files"] {
        if let Some(value) = json.get(key) {
            add_bucket(&mut buckets, "attachments", json_size(value));
        }
    }
    let known = buckets.values().copied().sum::<u64>();
    if request_bytes > known {
        add_bucket(&mut buckets, "other", request_bytes - known);
    }
    let mut categories = buckets
        .into_iter()
        .map(|(name, bytes)| category(name, bytes, request_bytes))
        .collect::<Vec<_>>();
    categories.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.name.cmp(&b.name)));
    ContextBreakdown {
        request_bytes,
        estimated_tokens: estimated_tokens(request_bytes),
        categories,
        top_tools: top_tools(&json, request_bytes),
    }
}

fn add_message_buckets(buckets: &mut BTreeMap<&'static str, u64>, message: &Value) {
    if message.get("role").and_then(Value::as_str) == Some("tool") {
        add_bucket(buckets, "tool_results", json_size(message));
        return;
    }
    let Some(content) = message.get("content") else {
        add_bucket(buckets, "messages", json_size(message));
        return;
    };
    if let Some(parts) = content.as_array() {
        for part in parts {
            let part_type = part.get("type").and_then(Value::as_str).unwrap_or_default();
            if part_type.contains("tool_result") || part.get("tool_use_id").is_some() {
                add_bucket(buckets, "tool_results", json_size(part));
            } else if part_type.contains("image")
                || part.get("image_url").is_some()
                || part.get("source").is_some_and(looks_like_image_source)
            {
                add_bucket(buckets, "images", json_size(part));
            } else {
                add_bucket(buckets, "messages", json_size(part));
            }
        }
    } else {
        add_bucket(buckets, "messages", json_size(message));
    }
}

fn top_tools(json: &Value, request_bytes: u64) -> Vec<TopTool> {
    let Some(tools) = json.get("tools").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut top = tools
        .iter()
        .map(|tool| {
            let name = tool
                .get("name")
                .or_else(|| tool.pointer("/function/name"))
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            let bytes = json_size(tool);
            TopTool {
                name,
                bytes,
                pct: pct(bytes, request_bytes),
            }
        })
        .collect::<Vec<_>>();
    top.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.name.cmp(&b.name)));
    top.truncate(25);
    top
}

fn parse_sse_events(raw: &[u8]) -> Vec<Value> {
    let text = String::from_utf8_lossy(raw);
    let mut events = Vec::new();
    for block in text.split("\n\n") {
        let mut event_name = None;
        let mut data = String::new();
        for line in block.lines() {
            if let Some(rest) = line.strip_prefix("event:") {
                event_name = Some(rest.trim().to_string());
            } else if let Some(rest) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(rest.trim());
            }
        }
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let parsed = serde_json::from_str::<Value>(&data).unwrap_or_else(|_| {
            serde_json::json!({
                "event": event_name,
                "data": data,
            })
        });
        events.push(parsed);
    }
    events
}

fn assistant_text(raw: &[u8], content_type: Option<&str>) -> String {
    if is_sse(content_type, raw) {
        let mut out = String::new();
        for event in parse_sse_events(raw) {
            append_text_delta(&mut out, &event);
        }
        return out;
    }
    let Ok(json) = serde_json::from_slice::<Value>(raw) else {
        return String::from_utf8_lossy(raw).into_owned();
    };
    assistant_text_from_json(&json)
}

fn append_text_delta(out: &mut String, value: &Value) {
    if let Some(text) = value.pointer("/delta/text").and_then(Value::as_str) {
        out.push_str(text);
    }
    if let Some(text) = value.pointer("/delta/content").and_then(Value::as_str) {
        out.push_str(text);
    }
    if let Some(text) = value.get("delta").and_then(Value::as_str) {
        out.push_str(text);
    }
    if let Some(choices) = value.get("choices").and_then(Value::as_array) {
        for choice in choices {
            if let Some(text) = choice.pointer("/delta/content").and_then(Value::as_str) {
                out.push_str(text);
            }
        }
    }
}

fn assistant_text_from_json(value: &Value) -> String {
    if let Some(choices) = value.get("choices").and_then(Value::as_array) {
        let mut out = String::new();
        for choice in choices {
            if let Some(text) = choice.pointer("/message/content").and_then(Value::as_str) {
                out.push_str(text);
            }
        }
        if !out.is_empty() {
            return out;
        }
    }
    if let Some(content) = value.get("content").and_then(Value::as_array) {
        let mut out = String::new();
        for part in content {
            if part.get("type").and_then(Value::as_str) == Some("text") {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    out.push_str(text);
                }
            }
        }
        if !out.is_empty() {
            return out;
        }
    }
    value
        .pointer("/output_text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn render_brief(rows: &[MetricsRow], label: &str) -> String {
    let mut out = String::new();
    out.push_str(&format!("# Switchback {label} Brief\n\n"));
    if rows.is_empty() {
        out.push_str("No metrics rows found.\n");
        return out;
    }
    let (current, previous) = period_rows(rows, label);
    let comparable_rows = if current.is_empty() {
        rows.iter().collect::<Vec<_>>()
    } else {
        current
    };
    let current_stats = brief_stats(&comparable_rows);
    let previous_stats = brief_stats(&previous);
    let biggest = comparable_rows
        .iter()
        .copied()
        .max_by_key(|row| row.request_bytes);
    let costliest = comparable_rows
        .iter()
        .copied()
        .max_by_key(|row| row.cost_micros);
    let slowest = comparable_rows
        .iter()
        .copied()
        .max_by_key(|row| row.total_latency_ms.unwrap_or(0));
    let slowest_route = slowest_route(&comparable_rows);
    let failing_surface = most_common_failure_surface(&comparable_rows);
    let headroom_rows = comparable_rows
        .iter()
        .filter(|row| row.headroom_used)
        .count();

    out.push_str("## Verdict\n\n");
    out.push_str(&format!(
        "- Current window: {} requests, avg latency {} ms, failure rate {:.1}%, total cost ${:.6}.\n",
        current_stats.requests,
        current_stats.avg_latency_ms(),
        current_stats.failure_rate_pct(),
        current_stats.total_cost_micros as f64 / 1_000_000.0
    ));
    if previous_stats.requests > 0 {
        out.push_str(&format!(
            "- Outcome vs previous {label}: cost {}, avg latency {}, failure rate {}, avg request bytes {}.\n",
            signed_i64(
                current_stats.total_cost_micros as i64 - previous_stats.total_cost_micros as i64,
                " micro-USD"
            ),
            signed_i64(
                current_stats.avg_latency_ms() as i64 - previous_stats.avg_latency_ms() as i64,
                " ms"
            ),
            signed_f64(
                current_stats.failure_rate_pct() - previous_stats.failure_rate_pct(),
                " pp"
            ),
            signed_i64(
                current_stats.avg_request_bytes() as i64
                    - previous_stats.avg_request_bytes() as i64,
                " bytes"
            )
        ));
    } else {
        out.push_str(&format!(
            "- Outcome vs previous {label}: no prior window yet; rerun after a routing/compression change to measure cost, latency, and success.\n"
        ));
    }
    if let Some(row) = biggest {
        out.push_str(&format!(
            "- Biggest context request: `{}` with {} request bytes.\n",
            row.request_id, row.request_bytes
        ));
    }
    if let Some(row) = costliest {
        out.push_str(&format!(
            "- Biggest cost offender: `{}` at ${:.6}.\n",
            row.request_id,
            row.cost_micros as f64 / 1_000_000.0
        ));
    }
    if let Some(row) = slowest {
        out.push_str(&format!(
            "- Slowest request: `{}` at {} ms.\n",
            row.request_id,
            row.total_latency_ms.unwrap_or(0)
        ));
    }
    if let Some((route, avg_latency, count)) = slowest_route {
        out.push_str(&format!(
            "- Slowest route: `{route}` averaged {avg_latency} ms across {count} requests.\n"
        ));
    }
    if let Some((surface, count)) = failing_surface {
        out.push_str(&format!(
            "- Most common failing surface: `{surface}` with {count} failures.\n"
        ));
    }
    if headroom_rows > 0 {
        out.push_str(&format!(
            "- Compression loop: {headroom_rows} requests used Headroom; compare context-byte deltas after the next Headroom/default change.\n"
        ));
    } else {
        out.push_str("- Compression loop: no Headroom path rows in this window, so no compression win is measurable yet.\n");
    }
    out.push_str("- Default to change next: inspect the biggest/slowest offender with `sb body audit <request_id>` and trim its top context category or move the slowest route behind a cheaper/faster default.\n");
    out
}

pub(crate) fn body_brief(state_dir: &Path, label: &str) -> anyhow::Result<String> {
    let path = state_dir.join("metrics/requests.jsonl");
    let text = fs::read_to_string(&path)
        .with_context(|| format!("read metrics rows from `{}`", path.display()))?;
    let mut rows = Vec::new();
    for line in text.lines() {
        if let Ok(row) = serde_json::from_str::<MetricsRow>(line) {
            rows.push(row);
        }
    }
    Ok(render_brief(&rows, label))
}

fn event_summary(event: &BodyRecord) -> BodyEventSummary {
    BodyEventSummary {
        event_id: event.event_id.clone(),
        request_id: event.request_id.clone(),
        observed_at_unix_ms: event.observed_at_unix_ms,
        capture_stage: event.capture_stage.clone(),
        protocol: event.protocol.clone(),
        upstream: event.upstream.clone(),
        model: event.model.clone(),
        status: event.status,
        content_type: event.content_type.clone(),
        body_bytes: event.body_bytes,
        metadata: event.metadata.clone(),
    }
}

fn artifact_summary(event: &BodyRecord) -> BodyArtifact {
    BodyArtifact {
        stage: event.capture_stage.clone(),
        content_type: event.content_type.clone(),
        body_bytes: event.body_bytes,
        body_sha256: event.body_sha256.clone(),
        archive_path: event.archive_path.clone(),
    }
}

fn suggested_actions(
    context: &ContextBreakdown,
    metrics: &MetricsRow,
    trace: Option<&Value>,
    response: Option<&BodyRecord>,
) -> Vec<String> {
    let mut actions = Vec::new();
    if trace.is_none() {
        actions.push(
            "Trace metadata is missing; verify trace_log/state_store configuration for this lane."
                .to_string(),
        );
    }
    if response.is_none() {
        actions.push("Response body is missing; verify capture completed or the client did not disconnect before upstream response.".to_string());
    }
    if metrics.status.is_some_and(|status| status >= 400) {
        actions.push(format!(
            "Request failed with status {}; inspect route and upstream error class.",
            metrics.status.unwrap()
        ));
    }
    if let Some(category) = context.categories.first() {
        if category.pct >= 30.0 && category.bytes >= 8_192 {
            actions.push(format!(
                "Cut `{}` first: it consumes {:.1}% of the captured request.",
                category.name, category.pct
            ));
        }
    }
    if let Some(tool) = context.top_tools.first() {
        if tool.bytes >= 8_192 {
            actions.push(format!(
                "Largest tool schema is `{}` at {} bytes; consider lazy-loading or trimming it.",
                tool.name, tool.bytes
            ));
        }
    }
    if metrics.headroom_used {
        actions.push(
            "Headroom was on the path; compare before/after compression effect for this request."
                .to_string(),
        );
    }
    actions
}

fn client_matches(event: &BodyRecord, client: Option<&str>) -> bool {
    let Some(client) = client.filter(|client| *client != "all") else {
        return true;
    };
    let haystack = format!(
        "{} {} {} {} {}",
        event.protocol,
        event.upstream.as_deref().unwrap_or_default(),
        event.model.as_deref().unwrap_or_default(),
        event
            .metadata
            .get("tap")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        event
            .metadata
            .get("proxy_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
    )
    .to_ascii_lowercase();
    match client {
        "claude" => haystack.contains("claude") || haystack.contains("forward-proxy"),
        "codex" => haystack.contains("codex"),
        other => haystack.contains(&other.to_ascii_lowercase()),
    }
}

fn is_request_stage(stage: &str) -> bool {
    matches!(stage, "client_inbound" | "headroom_inbound")
}

fn is_response_stage(stage: &str) -> bool {
    matches!(
        stage,
        "client_response" | "upstream_response" | "headroom_outbound" | "client_session"
    )
}

fn pretty_json_bytes(raw: &[u8]) -> Option<Vec<u8>> {
    let value = serde_json::from_slice::<Value>(raw).ok()?;
    serde_json::to_vec_pretty(&value).ok()
}

fn safe_file_component(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn open_path(path: &Path) {
    #[cfg(target_os = "macos")]
    let cmd = "open";
    #[cfg(all(unix, not(target_os = "macos")))]
    let cmd = "xdg-open";
    #[cfg(windows)]
    let cmd = "cmd";
    #[cfg(windows)]
    let _ = Command::new(cmd)
        .args(["/C", "start", ""])
        .arg(path)
        .spawn();
    #[cfg(not(windows))]
    let _ = Command::new(cmd).arg(path).spawn();
}

fn write_private_file(path: &Path, contents: impl AsRef<[u8]>) -> anyhow::Result<()> {
    fs::write(path, contents)?;
    set_private_permissions(path)?;
    Ok(())
}

#[cfg(unix)]
fn set_private_permissions(path: &Path) -> anyhow::Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_permissions(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

fn is_sse(content_type: Option<&str>, raw: &[u8]) -> bool {
    content_type
        .unwrap_or_default()
        .to_ascii_lowercase()
        .contains("text/event-stream")
        || raw.starts_with(b"event:")
        || raw.starts_with(b"data:")
}

fn json_size(value: &Value) -> u64 {
    serde_json::to_vec(value)
        .map(|v| v.len() as u64)
        .unwrap_or(0)
}

fn add_bucket(buckets: &mut BTreeMap<&'static str, u64>, name: &'static str, bytes: u64) {
    *buckets.entry(name).or_default() += bytes;
}

fn category(name: &str, bytes: u64, total: u64) -> ContextCategory {
    ContextCategory {
        name: name.to_string(),
        bytes,
        pct: pct(bytes, total),
        estimated_tokens: estimated_tokens(bytes),
    }
}

fn pct(bytes: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        bytes as f64 * 100.0 / total as f64
    }
}

fn average(total: u64, count: u64) -> u64 {
    total.checked_div(count).unwrap_or(0)
}

fn percent(count: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        count as f64 * 100.0 / total as f64
    }
}

fn is_failure(row: &MetricsRow) -> bool {
    row.error_kind.is_some()
        || row
            .status
            .is_some_and(|status| !(200..400).contains(&status))
}

const DAY_MS: i64 = 86_400_000;

fn period_rows<'a>(
    rows: &'a [MetricsRow],
    label: &str,
) -> (Vec<&'a MetricsRow>, Vec<&'a MetricsRow>) {
    let window_ms = if label.eq_ignore_ascii_case("weekly") {
        DAY_MS * 7
    } else {
        DAY_MS
    };
    let latest = rows
        .iter()
        .map(|row| row.observed_at_unix_ms)
        .max()
        .unwrap_or(0);
    let current_start = latest.saturating_sub(window_ms);
    let previous_start = latest.saturating_sub(window_ms * 2);
    let current = rows
        .iter()
        .filter(|row| row.observed_at_unix_ms >= current_start)
        .collect::<Vec<_>>();
    let previous = rows
        .iter()
        .filter(|row| {
            row.observed_at_unix_ms >= previous_start && row.observed_at_unix_ms < current_start
        })
        .collect::<Vec<_>>();
    (current, previous)
}

#[derive(Default)]
struct BriefStats {
    requests: usize,
    failures: u64,
    total_cost_micros: u64,
    total_request_bytes: u64,
    total_latency_ms: u64,
    latency_count: u64,
}

impl BriefStats {
    fn avg_latency_ms(&self) -> u64 {
        average(self.total_latency_ms, self.latency_count)
    }

    fn failure_rate_pct(&self) -> f64 {
        percent(self.failures, self.requests as u64)
    }

    fn avg_request_bytes(&self) -> u64 {
        average(self.total_request_bytes, self.requests as u64)
    }
}

fn brief_stats(rows: &[&MetricsRow]) -> BriefStats {
    let mut stats = BriefStats {
        requests: rows.len(),
        ..BriefStats::default()
    };
    for row in rows {
        stats.total_cost_micros += row.cost_micros;
        stats.total_request_bytes += row.request_bytes;
        if is_failure(row) {
            stats.failures += 1;
        }
        if let Some(latency) = row.total_latency_ms {
            stats.total_latency_ms += latency;
            stats.latency_count += 1;
        }
    }
    stats
}

fn slowest_route(rows: &[&MetricsRow]) -> Option<(String, u64, usize)> {
    let mut by_route: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    for row in rows {
        let Some(latency) = row.total_latency_ms else {
            continue;
        };
        let route = row.route.clone().unwrap_or_else(|| "unknown".to_string());
        let entry = by_route.entry(route).or_default();
        entry.0 += latency;
        entry.1 += 1;
    }
    by_route
        .into_iter()
        .map(|(route, (latency, count))| (route, average(latency, count), count as usize))
        .max_by_key(|(_, avg_latency, count)| (*avg_latency, *count))
}

fn most_common_failure_surface(rows: &[&MetricsRow]) -> Option<(String, u64)> {
    let mut failures: BTreeMap<String, u64> = BTreeMap::new();
    for row in rows.iter().filter(|row| is_failure(row)) {
        let surface = row
            .error_kind
            .clone()
            .or_else(|| row.path.clone())
            .or_else(|| row.route.clone())
            .unwrap_or_else(|| "unknown".to_string());
        *failures.entry(surface).or_default() += 1;
    }
    failures
        .into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(&a.0)))
}

fn signed_i64(value: i64, unit: &str) -> String {
    format!("{value:+}{unit}")
}

fn signed_f64(value: f64, unit: &str) -> String {
    format!("{value:+.1}{unit}")
}

fn estimated_tokens(bytes: u64) -> u64 {
    bytes.div_ceil(4)
}

fn looks_like_image_source(value: &Value) -> bool {
    value
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind.contains("image"))
        || value
            .get("media_type")
            .and_then(Value::as_str)
            .is_some_and(|kind| kind.starts_with("image/"))
}

fn usage_u64(usage: Option<&Value>, key: &str) -> u64 {
    usage
        .and_then(|usage| usage.get(key))
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

fn trace_ttft_ms(trace: Option<&Value>) -> Option<u64> {
    trace
        .and_then(|trace| trace.get("events"))
        .and_then(Value::as_array)?
        .iter()
        .find_map(|event| {
            let kind = event
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or_default();
            (kind.contains("first") || kind.contains("ttft"))
                .then(|| event.get("latency_ms").and_then(Value::as_u64))
                .flatten()
        })
}

fn trace_upstream_ms(trace: Option<&Value>) -> Option<u64> {
    trace
        .and_then(|trace| trace.get("attempts"))
        .and_then(Value::as_array)?
        .iter()
        .filter_map(|attempt| attempt.get("latency_ms").and_then(Value::as_u64))
        .max()
}

fn trace_error_kind(trace: Option<&Value>) -> Option<String> {
    trace?
        .get("attempts")?
        .as_array()?
        .iter()
        .rev()
        .find_map(|attempt| {
            attempt
                .get("class")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
}

fn selected_upstream(request: &BodyRecord, trace: Option<&Value>) -> Option<String> {
    request
        .metadata
        .get("selected_upstream")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            trace?
                .pointer("/decision/selected/target_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .or_else(|| request.upstream.clone())
}

fn infer_client(request: &BodyRecord) -> Option<String> {
    let tap = request.metadata.get("tap").and_then(Value::as_str);
    let proxy = request.metadata.get("proxy_id").and_then(Value::as_str);
    tap.or(proxy).map(|id| {
        if id.contains("claude") || request.protocol == "forward-proxy" {
            "claude".to_string()
        } else if id.contains("codex") {
            "codex".to_string()
        } else {
            id.to_string()
        }
    })
}

fn infer_lane(request: &BodyRecord) -> Option<String> {
    request
        .metadata
        .get("tap")
        .or_else(|| request.metadata.get("proxy_id"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn uses_headroom(request: &BodyRecord) -> bool {
    request
        .metadata
        .get("selected_upstream")
        .and_then(Value::as_str)
        .is_some_and(|upstream| upstream.contains("127.0.0.1:8787"))
        || request
            .upstream
            .as_deref()
            .is_some_and(|upstream| upstream.contains("127.0.0.1:8787"))
}

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn day_string(unix_ms: i64) -> String {
    let seconds = unix_ms.div_euclid(1000);
    let dt = time::OffsetDateTime::from_unix_timestamp(seconds)
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    format!(
        "{:04}-{:02}-{:02}",
        dt.year(),
        u8::from(dt.month()),
        dt.day()
    )
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use sb_bodylog::{BodyEventInput, BodyLogger, BodyLoggerConfig, CaptureStage};

    use super::*;

    static NEXT_ID: AtomicU64 = AtomicU64::new(1);

    fn temp_root(tag: &str) -> PathBuf {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "switchback-body-audit-{tag}-{}-{id}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn logger(root: &std::path::Path) -> BodyLogger {
        BodyLogger::new(BodyLoggerConfig {
            state_dir: root.join("state"),
            archive_root: root.join("archive"),
            legacy_jsonl: Some(root.join("state/tap-bodies.jsonl")),
            inline_threshold_bytes: 16,
        })
        .unwrap()
    }

    fn record_request(logger: &BodyLogger) {
        let request = serde_json::json!({
            "model": "claude-test",
            "system": "keep concise",
            "tools": [
                {"name": "large_tool", "description": "x".repeat(9000), "input_schema": {"type": "object"}}
            ],
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "hello"}]},
                {"role": "tool", "content": [{"type": "tool_result", "content": "tool result"}]}
            ]
        });
        logger
            .record(BodyEventInput {
                request_id: "req_audit".to_string(),
                capture_stage: CaptureStage::ClientInbound,
                protocol: "forward-proxy".to_string(),
                upstream: Some("api.anthropic.com".to_string()),
                model: Some("claude-test".to_string()),
                status: None,
                content_type: Some("application/json".to_string()),
                metadata: serde_json::json!({
                    "proxy_id": "mode-d",
                    "path": "/v1/messages?beta=true",
                    "selected_upstream": "http://127.0.0.1:8787",
                }),
                body: serde_json::to_vec(&request).unwrap(),
            })
            .unwrap();
        logger
            .record(BodyEventInput {
                request_id: "req_audit".to_string(),
                capture_stage: CaptureStage::UpstreamResponse,
                protocol: "forward-proxy".to_string(),
                upstream: Some("api.anthropic.com".to_string()),
                model: Some("claude-test".to_string()),
                status: Some(200),
                content_type: Some("text/event-stream".to_string()),
                metadata: serde_json::json!({
                    "proxy_id": "mode-d",
                    "path": "/v1/messages?beta=true",
                    "selected_upstream": "http://127.0.0.1:8787",
                }),
                body: b"event: content_block_delta\ndata: {\"delta\":{\"text\":\"hi\"}}\n\n"
                    .to_vec(),
            })
            .unwrap();
    }

    #[test]
    fn audit_bundle_preserves_hierarchy() {
        let root = temp_root("bundle");
        let logger = logger(&root);
        record_request(&logger);
        let trace = serde_json::json!({
            "request_id": "req_audit",
            "session_id": "s1",
            "project": "switchback",
            "route": "default",
            "inbound_model": "claude-test",
            "final_status": 200,
            "total_latency_ms": 123,
            "cost_micros": 42,
            "usage": {
                "input_tokens": 10,
                "cached_input_tokens": 2,
                "cache_creation_tokens": 0,
                "output_tokens": 3,
                "reasoning_tokens": 1
            },
            "decision": {"selected": {"target_id": "anthropic/claude-test"}},
            "attempts": [{"latency_ms": 99}]
        });

        let audit = build_audit(&logger, "req_audit", Some(trace)).unwrap();
        assert_eq!(audit.assistant_text, "hi");
        assert!(audit.context.request_bytes > 0);
        assert_eq!(audit.metrics.cost_micros, 42);
        assert!(audit.metrics.headroom_used);
        assert!(audit
            .suggested_actions
            .iter()
            .any(|action| action.contains("Largest tool schema")));

        let write = write_audit_bundle(&root.join("state"), None, &audit, false).unwrap();
        assert!(PathBuf::from(&write.markdown_path).exists());
        assert!(PathBuf::from(&write.request_raw_path).exists());
        assert!(PathBuf::from(&write.metrics_path).exists());
        assert!(PathBuf::from(&write.daily_rollup_path).exists());
        let markdown = fs::read_to_string(write.markdown_path).unwrap();
        assert!(markdown.contains("## Route / Meta"));
        assert!(markdown.contains("## Context Eaters"));
    }

    #[test]
    fn body_brief_reports_outcome_loop_inputs() {
        let root = temp_root("brief");
        fs::create_dir_all(root.join("state/metrics")).unwrap();
        let current_ts = 1_725_086_400_000;
        let previous_ts = current_ts - DAY_MS - 1;
        let previous = MetricsRow {
            request_id: "req_old_failure".to_string(),
            session_id: None,
            client: Some("claude".to_string()),
            lane: Some("mode-d".to_string()),
            cwd: None,
            project: Some("switchback".to_string()),
            model: Some("claude-test".to_string()),
            path: Some("/v1/messages".to_string()),
            status: Some(500),
            error_kind: Some("upstream_error".to_string()),
            request_bytes: 2000,
            response_bytes: 10,
            input_tokens: 1,
            cached_input_tokens: 0,
            cache_creation_tokens: 0,
            output_tokens: 1,
            reasoning_tokens: 0,
            cost_micros: 20,
            total_latency_ms: Some(4000),
            ttft_ms: None,
            upstream_ms: Some(3900),
            route: Some("default".to_string()),
            selected_upstream: Some("anthropic/claude-test".to_string()),
            headroom_used: false,
            context_breakdown: BTreeMap::new(),
            top_tools: Vec::new(),
            observed_at_unix_ms: previous_ts,
        };
        let mut current = previous.clone();
        current.request_id = "req_current".to_string();
        current.status = Some(200);
        current.error_kind = None;
        current.request_bytes = 1000;
        current.cost_micros = 9;
        current.total_latency_ms = Some(2500);
        current.upstream_ms = Some(2400);
        current.headroom_used = true;
        current.observed_at_unix_ms = current_ts;
        let rows = [previous, current];
        let metrics = rows
            .iter()
            .map(|row| serde_json::to_string(row).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(
            root.join("state/metrics/requests.jsonl"),
            format!("{metrics}\n"),
        )
        .unwrap();

        let brief = body_brief(&root.join("state"), "daily").unwrap();
        assert!(brief.contains("Outcome vs previous daily"));
        assert!(brief.contains("failure rate -100.0 pp"));
        assert!(brief.contains("Biggest context request"));
        assert!(brief.contains("Slowest request"));
        assert!(brief.contains("Compression loop: 1 requests used Headroom"));
        assert!(brief.contains("Default to change next"));
    }
}
