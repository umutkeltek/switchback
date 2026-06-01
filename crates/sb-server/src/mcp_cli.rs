use std::io::{BufRead, Write};
use std::path::Path;

use sb_core::Config;

use crate::config_cli::config_validate_json;
use crate::provider_cli::provider_certify_config_file;
use crate::provider_preset::provider_presets_json;
use crate::{controlplane, doctor_report_json, route_preview_json};

pub(crate) fn mcp_tools_json() -> serde_json::Value {
    serde_json::json!({
        "schema": "switchback/mcp-tools@1",
        "tools": mcp_tool_defs()
    })
}

fn mcp_tool_defs() -> Vec<serde_json::Value> {
    vec![
        serde_json::json!({
            "name": "switchback_config_validate",
            "description": "Validate the local Switchback config using runtime compile checks.",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false}
        }),
        serde_json::json!({
            "name": "switchback_config_show",
            "description": "Return the redacted local Switchback config.",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false}
        }),
        serde_json::json!({
            "name": "switchback_config_get",
            "description": "Return one redacted config value by dotted path.",
            "inputSchema": {
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"],
                "additionalProperties": false
            }
        }),
        serde_json::json!({
            "name": "switchback_route_preview",
            "description": "Preview a RouteDecision without executing upstream calls.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "model": {"type": "string"},
                    "stream": {"type": "boolean", "default": false}
                },
                "required": ["model"],
                "additionalProperties": false
            }
        }),
        serde_json::json!({
            "name": "switchback_provider_presets",
            "description": "List provider preset defaults and onboarding examples.",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false}
        }),
        serde_json::json!({
            "name": "switchback_provider_certify",
            "description": "Run an end-to-end readiness certification for one provider.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "provider": {"type": "string"},
                    "model": {"type": "string"}
                },
                "required": ["provider"],
                "additionalProperties": false
            }
        }),
        serde_json::json!({
            "name": "switchback_doctor",
            "description": "Return config/provider/route/egress/catalog diagnostics.",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false}
        }),
    ]
}

fn to_pretty(value: &serde_json::Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

fn mcp_content(value: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "content": [{"type": "text", "text": to_pretty(&value)}],
        "structuredContent": value,
    })
}

fn mcp_call_tool(
    config: &Path,
    name: &str,
    args: &serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    let result = match name {
        "switchback_config_validate" => config_validate_json(config)?,
        "switchback_config_show" => {
            let cfg = Config::from_path(config)?;
            controlplane::redact_config(&cfg)
        }
        "switchback_config_get" => {
            let path = args
                .get("path")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("missing required argument `path`"))?;
            let cfg = Config::from_path(config)?;
            let redacted = controlplane::redact_config(&cfg);
            controlplane::pointer_get(&redacted, path)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("no value at `{path}`"))?
        }
        "switchback_route_preview" => {
            let model = args
                .get("model")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("missing required argument `model`"))?;
            let stream = args
                .get("stream")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            route_preview_json(config, model, stream)?
        }
        "switchback_provider_presets" => provider_presets_json(),
        "switchback_provider_certify" => {
            let provider = args
                .get("provider")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("missing required argument `provider`"))?;
            let model = args.get("model").and_then(serde_json::Value::as_str);
            let runtime = tokio::runtime::Handle::current();
            serde_json::to_value(tokio::task::block_in_place(|| {
                runtime.block_on(provider_certify_config_file(config, provider, model))
            })?)?
        }
        "switchback_doctor" => {
            let cfg = Config::from_path(config)?;
            let runtime = tokio::runtime::Handle::current();
            tokio::task::block_in_place(|| runtime.block_on(doctor_report_json(&cfg)))?
        }
        other => anyhow::bail!("unknown tool `{other}`"),
    };
    Ok(mcp_content(result))
}

fn mcp_response(id: serde_json::Value, result: serde_json::Value) -> serde_json::Value {
    serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn mcp_error(id: serde_json::Value, code: i64, message: impl Into<String>) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message.into()}
    })
}

fn mcp_handle_request(config: &Path, req: serde_json::Value) -> Option<serde_json::Value> {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(serde_json::Value::as_str);
    let id_for_response = id.clone().unwrap_or(serde_json::Value::Null);
    let result = match method {
        Some("initialize") => Ok(serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "switchback", "version": env!("CARGO_PKG_VERSION")}
        })),
        Some("tools/list") => Ok(serde_json::json!({"tools": mcp_tool_defs()})),
        Some("tools/call") => {
            let params = req
                .get("params")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));
            let name = params
                .get("name")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("missing tool name"));
            match name {
                Ok(name) => {
                    let args = params
                        .get("arguments")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!({}));
                    mcp_call_tool(config, name, &args)
                }
                Err(e) => Err(e),
            }
        }
        Some(other) => Err(anyhow::anyhow!("method `{other}` is not supported")),
        None => Err(anyhow::anyhow!("missing method")),
    };

    match (id, result) {
        (None, _) => None,
        (Some(id), Ok(result)) => Some(mcp_response(id, result)),
        (Some(_), Err(e)) => Some(mcp_error(id_for_response, -32603, e.to_string())),
    }
}

pub(crate) fn run_mcp_stdio(config: &Path) -> anyhow::Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let parsed: serde_json::Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(e) => {
                writeln!(
                    stdout,
                    "{}",
                    mcp_error(serde_json::Value::Null, -32700, format!("parse error: {e}"))
                )?;
                stdout.flush()?;
                continue;
            }
        };
        if let Some(response) = mcp_handle_request(config, parsed) {
            writeln!(stdout, "{response}")?;
            stdout.flush()?;
        }
    }
    Ok(())
}
