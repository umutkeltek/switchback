use std::path::PathBuf;

use clap::{Parser, Subcommand};
use sb_core::Config;
use serde::Serialize;

use crate::config_cli::{
    config_format_file, config_patch_file, config_set_file, config_unset_file,
    config_validate_json, init_config_file, ConfigCmd, InitTemplate,
};
use crate::controlplane;
use crate::doctor_cli::{doctor_report, print_doctor_text};
use crate::lane_cli::{run_lane_cmd, LaneCmd};
use crate::mcp_cli::run_mcp_stdio;
use crate::otel::{init_tracing, otlp_export_config};
use crate::provider_cli::{
    provider_add_config_file, provider_certify_all_config_file, provider_certify_config_file,
    provider_doctor_config_file, provider_matrix_config_file, provider_models_config_file,
    provider_sync_routes_config_file, provider_test_config_file, ProviderAddRequest, ProviderCmd,
};
use crate::provider_preset::{provider_presets_json, provider_readiness_manifests_json};
use crate::schema_cli::{schema_docs_markdown, schema_json, SchemaCmd};
use crate::serve::{self, route_preview_json};
use crate::setup_cli::{run_setup_cmd, SetupCmd};
use crate::vault_cli::{run_vault_cmd, VaultCmd};

#[derive(Parser)]
struct Cli {
    /// Emit machine-readable JSON for commands that otherwise default to text.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a starter local config that works with no provider credentials.
    Init {
        #[arg(long, default_value = "switchback.yaml")]
        config: PathBuf,
        /// Replace the config file if it already exists.
        #[arg(long)]
        force: bool,
        /// Use the Codex + Claude Code native-client starter template.
        #[arg(long)]
        native_clients: bool,
    },
    /// Guided first-run setup and setup-pack installation.
    Setup {
        #[command(subcommand)]
        action: SetupCmd,
    },
    /// Serve the Switchback HTTP gateway.
    Serve {
        #[arg(long, default_value = "config/switchback.example.yaml")]
        config: PathBuf,
        #[arg(long)]
        bind: Option<String>,
    },
    /// Inspect config, provider auth envs, egress reachability, and catalog health.
    Doctor {
        #[arg(long, default_value = "config/switchback.example.yaml")]
        config: PathBuf,
    },
    /// Preview the route decision for a model without starting the server.
    RoutePreview {
        #[arg(long, default_value = "config/switchback.example.yaml")]
        config: PathBuf,
        /// Inbound model/profile/combo to preview.
        #[arg(long)]
        model: String,
        /// Simulate a streaming request.
        #[arg(long)]
        stream: bool,
    },
    /// Inspect named local lanes such as scout/code, codex/api, and pro/manual.
    Lane {
        #[command(subcommand)]
        action: LaneCmd,
        #[arg(long, global = true, default_value = "config/switchback.example.yaml")]
        config: PathBuf,
    },
    /// Print machine-readable command/config/MCP schemas for agents.
    Schema {
        #[command(subcommand)]
        action: SchemaCmd,
    },
    /// Run a minimal stdio MCP server over local Switchback control tools.
    Mcp {
        #[arg(long, default_value = "config/switchback.example.yaml")]
        config: PathBuf,
    },
    /// Add provider config for a supported official/provider-compatible API.
    Provider {
        #[command(subcommand)]
        action: ProviderCmd,
        #[arg(long, global = true, default_value = "switchback.yaml")]
        config: PathBuf,
    },
    /// Manage the encrypted credential vault (age file + OS-keychain key).
    Vault {
        #[command(subcommand)]
        action: VaultCmd,
        // global so it's accepted after the subcommand (`vault set X --config Y`).
        #[arg(long, global = true, default_value = "config/switchback.example.yaml")]
        config: PathBuf,
    },
    /// Inspect the configuration (machine-friendly JSON; for tools and AIs).
    Config {
        #[command(subcommand)]
        action: ConfigCmd,
        #[arg(long, global = true, default_value = "config/switchback.example.yaml")]
        config: PathBuf,
    },
}

pub fn run() -> anyhow::Result<()> {
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async_run())
}

async fn async_run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let json = cli.json;
    // Pre-load the serve config so tracing init can wire the OTLP exporter from
    // `server.otel_endpoint` before any spans are emitted.
    let serve_cfg = match &cli.cmd {
        Cmd::Serve { config, .. } => Some(Config::from_path(config)?),
        _ => None,
    };
    init_tracing(otlp_export_config(serve_cfg.as_ref()));

    match cli.cmd {
        Cmd::Init {
            config,
            force,
            native_clients,
        } => {
            let template = if native_clients {
                InitTemplate::NativeClients
            } else {
                InitTemplate::Quickstart
            };
            init_config_file(&config, force, template)?;
            let next_commands = template.next_commands(&config);
            if json {
                print_json(&serde_json::json!({
                    "ok": true,
                    "config": config,
                    "template": template.id(),
                    "next": next_commands[0],
                    "next_commands": next_commands,
                }))?;
            } else {
                println!("created {}", config.display());
                println!("template: {}", template.id());
                for command in next_commands {
                    println!("next: {command}");
                }
            }
        }
        Cmd::Serve { bind, config } => {
            let cfg = serve_cfg.expect("serve config pre-loaded above");
            serve::serve_gateway(config, bind, cfg).await?;
        }
        Cmd::Setup { action } => run_setup_cmd(action, json)?,
        Cmd::Vault { action, config } => run_vault_cmd(action, &config, json)?,
        Cmd::Doctor { config } => {
            let cfg = Config::from_path(&config)?;
            let report = doctor_report(&cfg).await;
            if json {
                print_json(&report)?;
            } else {
                print_doctor_text(&report);
            }
        }
        Cmd::RoutePreview {
            config,
            model,
            stream,
        } => {
            print_json(&route_preview_json(&config, &model, stream)?)?;
        }
        Cmd::Lane { action, config } => run_lane_cmd(action, &config, json)?,
        Cmd::Schema {
            action: SchemaCmd::Docs,
        } => println!("{}", schema_docs_markdown()),
        Cmd::Schema { action } => print_json(&schema_json(action))?,
        Cmd::Mcp { config } => {
            run_mcp_stdio(&config)?;
        }
        Cmd::Provider { action, config } => match action {
            ProviderCmd::Presets => {
                print_json(&provider_presets_json())?;
            }
            ProviderCmd::Readiness { preset } => {
                print_json(&provider_readiness_manifests_json(preset))?;
            }
            ProviderCmd::Add {
                preset,
                id,
                base_url,
                api_key_env,
                model,
                route,
                force,
            } => {
                let summary = provider_add_config_file(
                    &config,
                    ProviderAddRequest {
                        preset,
                        id,
                        base_url,
                        api_key_env,
                        model,
                        route,
                        force,
                    },
                )?;
                if json {
                    print_json(&serde_json::json!({
                        "ok": true,
                        "config": config,
                        "provider_id": summary.provider_id,
                        "api_key_env": summary.api_key_env,
                        "route_model": summary.route_model,
                        "target": summary.target,
                    }))?;
                } else {
                    println!(
                        "added provider `{}` to {}",
                        summary.provider_id,
                        config.display()
                    );
                    if let Some(env) = summary.api_key_env.as_deref() {
                        if std::env::var(env).is_err() {
                            println!("set {env} before serve/route-preview");
                        }
                    }
                    if let (Some(route_model), Some(target)) = (summary.route_model, summary.target)
                    {
                        println!("added route `{route_model}` -> `{target}`");
                        match summary.api_key_env.as_deref() {
                            Some(env) if std::env::var(env).is_err() => {}
                            _ => println!(
                                "preview: switchback route-preview --config {} --model {}",
                                config.display(),
                                route_model
                            ),
                        }
                    } else {
                        println!(
                            "next: add a route with --model, or request an explicit provider/model"
                        );
                    }
                }
            }
            ProviderCmd::Test {
                provider,
                model,
                stream,
            } => {
                let summary =
                    provider_test_config_file(&config, &provider, model.as_deref(), stream).await?;
                println!("{}", to_pretty(&serde_json::to_value(summary)?));
            }
            ProviderCmd::Models { provider } => {
                let summary = provider_models_config_file(&config, &provider).await?;
                println!("{}", to_pretty(&serde_json::to_value(summary)?));
            }
            ProviderCmd::SyncRoutes {
                provider,
                prefix,
                force,
            } => {
                let summary =
                    provider_sync_routes_config_file(&config, &provider, prefix.as_deref(), force)
                        .await?;
                println!("{}", to_pretty(&serde_json::to_value(summary)?));
            }
            ProviderCmd::Doctor { provider, model } => {
                let summary =
                    provider_doctor_config_file(&config, &provider, model.as_deref()).await?;
                println!("{}", to_pretty(&serde_json::to_value(summary)?));
            }
            ProviderCmd::Certify { provider, model } => {
                let summary =
                    provider_certify_config_file(&config, &provider, model.as_deref()).await?;
                println!("{}", to_pretty(&serde_json::to_value(summary)?));
            }
            ProviderCmd::CertifyAll { skip_missing_env } => {
                let summary = provider_certify_all_config_file(&config, skip_missing_env).await?;
                println!("{}", to_pretty(&serde_json::to_value(summary)?));
            }
            ProviderCmd::Matrix => {
                let summary = provider_matrix_config_file(&config).await?;
                println!("{}", to_pretty(&serde_json::to_value(summary)?));
            }
        },
        Cmd::Config { action, config } => match action {
            ConfigCmd::Show => {
                let cfg = Config::from_path(&config)?;
                println!("{}", to_pretty(&controlplane::redact_config(&cfg)));
            }
            ConfigCmd::Get { pointer } => {
                let cfg = Config::from_path(&config)?;
                let v = controlplane::redact_config(&cfg);
                match controlplane::pointer_get(&v, &pointer) {
                    Some(found) => println!("{}", to_pretty(found)),
                    None => {
                        eprintln!("no value at `{pointer}`");
                        std::process::exit(1);
                    }
                }
            }
            ConfigCmd::Set { pointer, value } => {
                let parsed = config_set_file(&config, &pointer, &value)?;
                print_json(&serde_json::json!({
                    "ok": true,
                    "config": config,
                    "path": pointer,
                    "value": parsed,
                }))?;
            }
            ConfigCmd::Unset { pointer } => {
                let removed = config_unset_file(&config, &pointer)?;
                print_json(&serde_json::json!({
                    "ok": true,
                    "config": config,
                    "path": pointer,
                    "removed": removed,
                }))?;
            }
            ConfigCmd::Patch { from_file } => {
                config_patch_file(&config, &from_file)?;
                print_json(&serde_json::json!({
                    "ok": true,
                    "config": config,
                    "patch": from_file,
                }))?;
            }
            ConfigCmd::Format => {
                config_format_file(&config)?;
                print_json(&serde_json::json!({
                    "ok": true,
                    "config": config,
                }))?;
            }
            ConfigCmd::Validate => {
                let report = config_validate_json(&config)?;
                let ok = report
                    .get("ok")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                println!("{}", to_pretty(&report));
                if !ok {
                    std::process::exit(1);
                }
            }
            ConfigCmd::Providers => {
                let cfg = Config::from_path(&config)?;
                let providers: Vec<serde_json::Value> = cfg
                    .providers
                    .iter()
                    .map(|p| {
                        serde_json::json!({
                            "id": p.id,
                            "type": controlplane::provider_type_name(&p.kind),
                            "egress": p.egress,
                            "accounts": p.accounts.iter().map(|a| a.id.clone()).collect::<Vec<_>>(),
                        })
                    })
                    .collect();
                println!(
                    "{}",
                    to_pretty(&serde_json::json!({ "providers": providers }))
                );
            }
            ConfigCmd::Routes => {
                let cfg = Config::from_path(&config)?;
                let routes: Vec<serde_json::Value> = cfg
                    .routes
                    .iter()
                    .map(|r| serde_json::json!({ "name": r.name, "targets": r.targets }))
                    .collect();
                let combos: Vec<serde_json::Value> = cfg
                    .combos
                    .iter()
                    .map(|(name, combo)| {
                        serde_json::json!({
                            "name": name,
                            "strategy": combo.strategy.as_str(),
                            "targets": combo.models.clone(),
                        })
                    })
                    .collect();
                println!(
                    "{}",
                    to_pretty(&serde_json::json!({ "routes": routes, "combos": combos }))
                );
            }
        },
    }

    Ok(())
}

/// Pretty JSON for CLI output (falls back to compact on the impossible error).
fn to_pretty(value: &serde_json::Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

pub(crate) fn print_json(value: &impl Serialize) -> anyhow::Result<()> {
    println!("{}", to_pretty(&serde_json::to_value(value)?));
    Ok(())
}
