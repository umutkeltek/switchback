use clap::Subcommand;

use crate::config_cli::config_schema_json;
use crate::mcp_cli::mcp_tools_json;

#[derive(Subcommand)]
pub(crate) enum SchemaCmd {
    /// List stable CLI commands, outputs, and examples.
    Commands,
    /// List common config paths that agents can inspect or mutate.
    Config,
    /// List MCP tools exposed by `switchback mcp`.
    Mcp,
}

pub(crate) fn schema_json(action: SchemaCmd) -> serde_json::Value {
    match action {
        SchemaCmd::Commands => command_schema_json(),
        SchemaCmd::Config => config_schema_json(),
        SchemaCmd::Mcp => mcp_tools_json(),
    }
}

fn command_schema_json() -> serde_json::Value {
    serde_json::json!({
        "schema": "switchback/commands@1",
        "stdout": "JSON for schema/config/provider diagnostic commands; human text only for serve and non-json init/provider add/vault commands",
        "commands": [
            {"name": "init", "writes_config": true, "output": "text or JSON with --json", "example": "switchback --json init --config switchback.yaml"},
            {"name": "serve", "writes_config": false, "output": "long-running HTTP server", "example": "switchback serve --config switchback.yaml"},
            {"name": "doctor", "writes_config": false, "output": "text or JSON with --json", "example": "switchback --json doctor --config switchback.yaml"},
            {"name": "route-preview", "writes_config": false, "output": "JSON RouteDecision preview", "example": "switchback route-preview --config switchback.yaml --model auto/coding"},
            {"name": "schema commands", "writes_config": false, "output": "JSON command schema", "example": "switchback schema commands"},
            {"name": "schema config", "writes_config": false, "output": "JSON config path schema", "example": "switchback schema config"},
            {"name": "schema mcp", "writes_config": false, "output": "JSON MCP tool schema", "example": "switchback schema mcp"},
            {"name": "mcp", "writes_config": false, "output": "stdio JSON-RPC MCP server", "example": "switchback mcp --config switchback.yaml"},
            {"name": "provider presets", "writes_config": false, "output": "JSON provider preset matrix", "example": "switchback provider presets"},
            {"name": "provider readiness", "writes_config": false, "output": "JSON provider readiness manifests", "example": "switchback provider readiness openai"},
            {"name": "provider add", "writes_config": true, "output": "text or JSON with --json", "example": "switchback --json provider add openai --config switchback.yaml --model gpt-4.1-mini"},
            {"name": "provider models", "writes_config": false, "output": "JSON discovered model list", "example": "switchback provider models openai --config switchback.yaml"},
            {"name": "provider sync-routes", "writes_config": true, "output": "JSON route import summary", "example": "switchback provider sync-routes openai --config switchback.yaml"},
            {"name": "provider test", "writes_config": false, "output": "JSON request smoke-test summary", "example": "switchback provider test openai --config switchback.yaml"},
            {"name": "provider doctor", "writes_config": false, "output": "JSON provider diagnostic report", "example": "switchback provider doctor openai --config switchback.yaml"},
            {"name": "provider certify", "writes_config": false, "output": "JSON provider readiness certification report", "example": "switchback provider certify openai --config switchback.yaml"},
            {"name": "provider certify-all", "writes_config": false, "output": "JSON readiness certification report for every configured provider", "example": "switchback provider certify-all --config switchback.yaml"},
            {"name": "provider matrix", "writes_config": false, "output": "JSON all-provider diagnostic report", "example": "switchback provider matrix --config switchback.yaml"},
            {"name": "config show", "writes_config": false, "output": "JSON redacted config", "example": "switchback config show --config switchback.yaml"},
            {"name": "config get", "writes_config": false, "output": "JSON value", "example": "switchback config get server.bind --config switchback.yaml"},
            {"name": "config set", "writes_config": true, "output": "JSON write summary", "example": "switchback config set server.cost_aware true --config switchback.yaml"},
            {"name": "config unset", "writes_config": true, "output": "JSON write summary", "example": "switchback config unset server.default_provider --config switchback.yaml"},
            {"name": "config patch", "writes_config": true, "output": "JSON write summary", "example": "switchback config patch --from-file patch.yaml --config switchback.yaml"},
            {"name": "config format", "writes_config": true, "output": "JSON write summary", "example": "switchback config format --config switchback.yaml"},
            {"name": "vault", "writes_config": false, "output": "text or JSON with --json; never prints secret values", "example": "switchback --json vault list --config switchback.yaml"}
        ]
    })
}
