use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use sb_bodylog::{
    resolve_keep_days, BodyLogger, BodyLoggerConfig, CaptureBackupReceipt,
    CaptureLegacyBackupReceipt, CaptureReclaimOptions, CaptureReclaimProof, GcOptions,
    DEFAULT_GC_BATCH_SIZE,
};
use sb_core::Config;
use sb_paths::RuntimePaths;
use serde::Serialize;

use crate::body_audit::{
    body_brief, body_logger_config, build_audit, latest_request_id, load_trace_json_from_state,
    open_existing_logger, write_audit_bundle,
};
use crate::config_cli::{
    config_format_file, config_patch_file, config_set_file, config_unset_file,
    config_validate_json, init_config_file, ConfigCmd, InitTemplate,
};
use crate::controlplane;
use crate::doctor_cli::{doctor_report, print_doctor_text};
use crate::eval_cli::{run_eval_cmd, EvalCmd};
use crate::fal_probe::{fal_balance_report, print_fal_balance_text};
use crate::lane_cli::{run_lane_cmd, LaneCmd};
use crate::lane_profile_cli::{run_launch_profile_cmd, LaunchProfileCmd};
use crate::local_probe::{local_capacity_report, print_local_capacity_text};
use crate::mcp_cli::run_mcp_stdio;
use crate::native_cli::{run_native_cmd, NativeCmd};
use crate::otel::{init_tracing, otlp_export_config};
use crate::provider_cli::{
    provider_add_config_file, provider_certify_all_config_file, provider_certify_config_file,
    provider_doctor_config_file, provider_matrix_config_file, provider_models_config_file,
    provider_sync_routes_config_file, provider_test_config_file, ProviderAddRequest, ProviderCmd,
};
use crate::provider_preset::{provider_presets_json, provider_readiness_manifests_json};
use crate::schema_cli::{schema_docs_markdown, schema_json, SchemaCmd};
use crate::serve::{self, route_preview_json};
use crate::setup_cli::{run_setup_cmd, runtime_paths_report, SetupCmd};
use crate::vault_cli::{run_vault_cmd, VaultCmd};

fn default_runtime_config_path() -> PathBuf {
    RuntimePaths::from_env().config_file()
}

fn default_eval_store_path() -> PathBuf {
    RuntimePaths::from_env().eval_store()
}

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
        #[arg(long, default_value_os_t = default_runtime_config_path())]
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
        action: Option<SetupCmd>,
        /// Runtime data root to initialize. Defaults to the shared runtime path contract.
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// Print the Switchback-owned runtime paths.
    Paths {
        /// Resolve paths against this runtime root instead of the environment contract.
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// Serve the Switchback HTTP gateway.
    Serve {
        #[arg(long, default_value_os_t = default_runtime_config_path())]
        config: PathBuf,
        #[arg(long)]
        bind: Option<String>,
    },
    /// Inspect config, provider auth envs, egress reachability, and catalog health.
    Doctor {
        /// Optional specialized probe (`fal`, `local`).
        provider: Option<String>,
        #[arg(long, default_value_os_t = default_runtime_config_path())]
        config: PathBuf,
        /// Specialized provider probe timeout.
        #[arg(long, default_value_t = 5_000)]
        timeout_ms: u64,
    },
    /// Inspect protected raw-body capture index/archive health.
    Body {
        #[command(subcommand)]
        action: BodyCmd,
    },
    /// Ingest and report harness evaluation evidence.
    Eval {
        #[command(subcommand)]
        action: EvalCmd,
        #[arg(long, global = true, default_value_os_t = default_eval_store_path())]
        store: PathBuf,
    },
    /// Preview the route decision for a model without starting the server.
    RoutePreview {
        #[arg(long, default_value_os_t = default_runtime_config_path())]
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
        #[arg(long, global = true, default_value_os_t = default_runtime_config_path())]
        config: PathBuf,
    },
    /// Plan, materialize, and audit Switchback-owned launch profiles.
    Profile {
        #[command(subcommand)]
        action: LaunchProfileCmd,
        #[arg(long, global = true, default_value_os_t = default_runtime_config_path())]
        config: PathBuf,
    },
    /// Inspect native coding-client setup without mutating local state.
    Native {
        #[command(subcommand)]
        action: NativeCmd,
        #[arg(long, global = true, default_value_os_t = default_runtime_config_path())]
        config: PathBuf,
    },
    /// Print machine-readable command/config/MCP schemas for agents.
    Schema {
        #[command(subcommand)]
        action: SchemaCmd,
    },
    /// Run a minimal stdio MCP server over local Switchback control tools.
    Mcp {
        #[arg(long, default_value_os_t = default_runtime_config_path())]
        config: PathBuf,
    },
    /// Add provider config for a supported official/provider-compatible API.
    Provider {
        #[command(subcommand)]
        action: ProviderCmd,
        #[arg(long, global = true, default_value_os_t = default_runtime_config_path())]
        config: PathBuf,
    },
    /// Manage the encrypted credential vault (age file + OS-keychain key).
    Vault {
        #[command(subcommand)]
        action: VaultCmd,
        // global so it's accepted after the subcommand (`vault set X --config Y`).
        #[arg(long, global = true, default_value_os_t = default_runtime_config_path())]
        config: PathBuf,
    },
    /// Inspect the configuration (machine-friendly JSON; for tools and AIs).
    Config {
        #[command(subcommand)]
        action: ConfigCmd,
        #[arg(long, global = true, default_value_os_t = default_runtime_config_path())]
        config: PathBuf,
    },
}

#[derive(Subcommand)]
enum BodyCmd {
    /// Show body index, archive, and spool status.
    Status {
        /// Local hot body index directory.
        #[arg(long)]
        state_dir: Option<PathBuf>,
        /// Compressed long-term archive root.
        #[arg(long)]
        archive_root: Option<PathBuf>,
        /// Compatibility event JSONL path.
        #[arg(long)]
        legacy_jsonl: Option<PathBuf>,
    },
    /// Emit a sealed-manifest-only transfer plan. Never includes SQLite.
    BackupPlan {
        #[arg(long)]
        state_dir: Option<PathBuf>,
        #[arg(long)]
        archive_root: Option<PathBuf>,
        #[arg(long)]
        legacy_jsonl: Option<PathBuf>,
    },
    /// Emit a one-time checksum plan for frozen pre-segment evidence.
    LegacyBackupPlan {
        #[arg(long)]
        state_dir: Option<PathBuf>,
        #[arg(long)]
        archive_root: Option<PathBuf>,
        #[arg(long)]
        legacy_jsonl: Option<PathBuf>,
    },
    /// Accept a transfer receipt only after remote checksums were verified.
    AcceptBackupReceipt {
        receipt: PathBuf,
        #[arg(long)]
        state_dir: Option<PathBuf>,
        #[arg(long)]
        archive_root: Option<PathBuf>,
        #[arg(long)]
        legacy_jsonl: Option<PathBuf>,
    },
    /// Accept exact remote checksum proof for frozen legacy evidence.
    AcceptLegacyBackupReceipt {
        receipt: PathBuf,
        #[arg(long)]
        state_dir: Option<PathBuf>,
        #[arg(long)]
        archive_root: Option<PathBuf>,
        #[arg(long)]
        legacy_jsonl: Option<PathBuf>,
    },
    /// Plan receipt-backed local segment reclaim after a minimum retention window.
    ReclaimPlan {
        #[arg(long)]
        state_dir: Option<PathBuf>,
        #[arg(long)]
        archive_root: Option<PathBuf>,
        #[arg(long)]
        legacy_jsonl: Option<PathBuf>,
        #[arg(long)]
        keep_days: Option<u64>,
    },
    /// Reclaim exact remotely re-verified segments. Mutates only with --confirm.
    Reclaim {
        proof: PathBuf,
        #[arg(long)]
        state_dir: Option<PathBuf>,
        #[arg(long)]
        archive_root: Option<PathBuf>,
        #[arg(long)]
        legacy_jsonl: Option<PathBuf>,
        #[arg(long)]
        keep_days: Option<u64>,
        #[arg(long)]
        confirm: bool,
    },
    /// Restore one remote-only segment from downloaded checksum-proven files.
    Restore {
        segment_sha256: String,
        source_segment: PathBuf,
        source_manifest: PathBuf,
        #[arg(long)]
        state_dir: Option<PathBuf>,
        #[arg(long)]
        archive_root: Option<PathBuf>,
        #[arg(long)]
        legacy_jsonl: Option<PathBuf>,
    },
    /// Render one protected raw-body capture as a readable audit bundle.
    Audit {
        /// Request id to audit, or `latest`.
        request_id: String,
        /// Filter `latest` by client/lane (`claude`, `codex`, or `all`).
        #[arg(long)]
        client: Option<String>,
        /// Output format for stdout (`markdown` writes bundle; `json` prints summary).
        #[arg(long, default_value = "markdown")]
        format: String,
        /// Directory to place the audit bundle in.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Open the generated markdown file with the OS default app.
        #[arg(long)]
        open: bool,
        /// Local hot body index directory.
        #[arg(long)]
        state_dir: Option<PathBuf>,
        /// Compressed long-term archive root.
        #[arg(long)]
        archive_root: Option<PathBuf>,
        /// Compatibility event JSONL path.
        #[arg(long)]
        legacy_jsonl: Option<PathBuf>,
    },
    /// Summarize derived metrics rows into a daily/weekly operator brief.
    Brief {
        /// Brief period label (`daily` or `weekly`).
        period: String,
        /// Local Switchback state directory.
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
    /// Retention GC for the local body index + spool drain (dry-run by default).
    ///
    /// Deletes index rows for UTC days whose archive day dir is absent under a
    /// MOUNTED archive root (exported + pruned), drains the spool into day
    /// partitions, and (optionally) compacts. Mutates only with `--confirm`.
    Gc {
        /// Local hot body index directory.
        #[arg(long)]
        state_dir: Option<PathBuf>,
        /// Compressed long-term archive root.
        #[arg(long)]
        archive_root: Option<PathBuf>,
        /// Frozen compatibility event JSONL path (for status/plumbing only).
        #[arg(long)]
        legacy_jsonl: Option<PathBuf>,
        /// Keep this many recent UTC days (default 3, env SWITCHBACK_BODY_KEEP_DAYS).
        #[arg(long)]
        keep_days: Option<u64>,
        /// Actually mutate (delete rows / drain spool). Without it: dry-run only.
        #[arg(long)]
        confirm: bool,
        /// Only drain the spool into day partitions; skip retention deletes.
        #[arg(long)]
        drain_only: bool,
        /// After GC, compact the index (VACUUM INTO + atomic replace). Guarded;
        /// requires `--confirm` and refuses if any process holds the DB open.
        #[arg(long)]
        compact: bool,
        /// Bounded-batch size for retention deletes.
        #[arg(long)]
        batch_size: Option<u64>,
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
        Cmd::Setup { action, root } => run_setup_cmd(action, root, json)?,
        Cmd::Paths { root } => print_json(&runtime_paths_report(root))?,
        Cmd::Vault { action, config } => run_vault_cmd(action, &config, json)?,
        Cmd::Doctor {
            provider,
            config,
            timeout_ms,
        } => {
            let cfg = Config::from_path(&config)?;
            match provider.as_deref() {
                None => {
                    let report = doctor_report(&cfg).await;
                    if json {
                        print_json(&report)?;
                    } else {
                        print_doctor_text(&report);
                    }
                }
                Some("fal") => {
                    let report = fal_balance_report(&cfg, timeout_ms).await;
                    if json {
                        print_json(&report)?;
                    } else {
                        print_fal_balance_text(&report);
                    }
                }
                Some("local") => {
                    let report = local_capacity_report(&cfg, timeout_ms).await;
                    if json {
                        print_json(&report)?;
                    } else {
                        print_local_capacity_text(&report);
                    }
                }
                Some(provider) => {
                    anyhow::bail!(
                        "unsupported specialized doctor `{provider}`; supported: fal, local"
                    )
                }
            }
        }
        Cmd::Body { action } => run_body_cmd(action, json)?,
        Cmd::Eval { action, store } => run_eval_cmd(action, &store, json)?,
        Cmd::RoutePreview {
            config,
            model,
            stream,
        } => {
            print_json(&route_preview_json(&config, &model, stream)?)?;
        }
        Cmd::Lane { action, config } => run_lane_cmd(action, &config, json)?,
        Cmd::Profile { action, config } => run_launch_profile_cmd(action, &config, json)?,
        Cmd::Native { action, config } => run_native_cmd(action, &config, json).await?,
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
fn run_body_cmd(action: BodyCmd, json: bool) -> anyhow::Result<()> {
    match action {
        BodyCmd::Status {
            state_dir,
            archive_root,
            legacy_jsonl,
        } => {
            let state_dir = state_dir.unwrap_or_else(default_body_state_dir);
            let legacy_jsonl = legacy_jsonl.unwrap_or_else(|| state_dir.join("tap-bodies.jsonl"));
            let mut config = BodyLoggerConfig::from_legacy_sink(legacy_jsonl);
            config.state_dir = state_dir.clone();
            config.archive_root =
                archive_root.unwrap_or_else(|| default_body_archive_root(&state_dir));
            let status = BodyLogger::status_for_config(config)?;
            if json {
                print_json(&status)?;
            } else {
                println!("body log: {}", status.status);
                println!("index: {}", status.index_path);
                println!(
                    "index bytes: {} (reclaimable {})",
                    status.index_bytes, status.index_reclaimable_bytes
                );
                println!(
                    "archive: {} ({})",
                    status.archive_root,
                    if status.archive_available {
                        "available"
                    } else {
                        "unavailable; using spool"
                    }
                );
                let approx = if status.counts_approximate {
                    " (approx, MAX(rowid))"
                } else {
                    ""
                };
                println!("events: {}{approx}", status.events);
                println!("blobs: {}{approx}", status.blobs);
                println!(
                    "capture 1m: {} events / {} body bytes",
                    status.capture_events_last_minute, status.capture_body_bytes_last_minute
                );
                println!(
                    "segments: {} local / {} unbacked bytes",
                    status.local_segment_count, status.segment_backlog_bytes
                );
                println!(
                    "capture queue: {} depth / {} drops",
                    status.capture_queue_depth, status.capture_queue_drops
                );
                println!(
                    "capture mode: {:?}{}",
                    status.pressure.mode,
                    if status.pressure.reasons.is_empty() {
                        String::new()
                    } else {
                        format!(" ({})", status.pressure.reasons.join(","))
                    }
                );
                // `healthy_backup_cycles: 0` is the outcome of a held gate, not a
                // cause. Print the gate, or the operator debugs the counter.
                if !status.pressure.resume_blockers.is_empty() {
                    println!(
                        "capture resume blocked by: {} ({}/{} healthy backup cycles)",
                        status.pressure.resume_blockers.join(","),
                        status.pressure.healthy_backup_cycles,
                        status.pressure.thresholds.healthy_backup_cycles_to_resume
                    );
                }
                println!(
                    "backup: age={} verified-through={}",
                    status
                        .backup_age_ms
                        .map_or_else(|| "unknown".to_string(), |age| format!("{age}ms")),
                    status.verified_through_day.as_deref().unwrap_or("unknown")
                );
                if status.spool_backlog_exact {
                    println!("spool backlog: {}", status.spool_backlog);
                } else {
                    println!("spool backlog: unknown (filesystem walk failed)");
                }
                println!("retention cutoff: {}", status.retention_cutoff_day);
                print!("local archive days: {}", status.local_archive_day_dirs);
                if let Some(oldest) = &status.oldest_local_day_dir {
                    print!(" (oldest {oldest})");
                }
                println!();
                if let Some(bytes) = status.legacy_jsonl_bytes {
                    println!("legacy jsonl (frozen): {bytes} bytes");
                }
                println!("protected:");
                for path in status.protected_paths {
                    println!("  {path}");
                }
            }
        }
        BodyCmd::BackupPlan {
            state_dir,
            archive_root,
            legacy_jsonl,
        } => {
            let state_dir = state_dir.unwrap_or_else(default_body_state_dir);
            let legacy_jsonl = legacy_jsonl.unwrap_or_else(|| state_dir.join("tap-bodies.jsonl"));
            let mut config = BodyLoggerConfig::from_legacy_sink(legacy_jsonl);
            config.state_dir = state_dir.clone();
            config.archive_root =
                archive_root.unwrap_or_else(|| default_body_archive_root(&state_dir));
            let logger = BodyLogger::open_existing(config)?
                .ok_or_else(|| anyhow::anyhow!("body index does not exist"))?;
            let plan = logger.backup_plan()?;
            if json {
                print_json(&plan)?;
            } else {
                println!(
                    "backup generation {}: {} sealed segment(s), {} bytes",
                    plan.next_generation,
                    plan.segments.len(),
                    plan.total_segment_bytes
                );
                for segment in plan.segments {
                    println!(
                        "{} {} {}",
                        segment.utc_day, segment.segment_sha256, segment.segment_path
                    );
                }
            }
        }
        BodyCmd::LegacyBackupPlan {
            state_dir,
            archive_root,
            legacy_jsonl,
        } => {
            let state_dir = state_dir.unwrap_or_else(default_body_state_dir);
            let legacy_jsonl = legacy_jsonl.unwrap_or_else(|| state_dir.join("tap-bodies.jsonl"));
            let mut config = BodyLoggerConfig::from_legacy_sink(legacy_jsonl);
            config.state_dir = state_dir.clone();
            config.archive_root =
                archive_root.unwrap_or_else(|| default_body_archive_root(&state_dir));
            let logger = BodyLogger::open_existing(config)?
                .ok_or_else(|| anyhow::anyhow!("body index does not exist"))?;
            let plan = logger.legacy_backup_plan()?;
            if json {
                print_json(&plan)?;
            } else {
                println!(
                    "{} pending legacy artifact(s), {} bytes",
                    plan.artifacts.len(),
                    plan.total_artifact_bytes
                );
                for artifact in plan.artifacts {
                    println!(
                        "{} {} {}",
                        artifact.artifact_id, artifact.sha256, artifact.local_path
                    );
                }
                for blocker in plan.blockers {
                    println!(
                        "blocked {} {} {}",
                        blocker.artifact_id, blocker.code, blocker.remediation
                    );
                }
            }
        }
        BodyCmd::AcceptBackupReceipt {
            receipt,
            state_dir,
            archive_root,
            legacy_jsonl,
        } => {
            let state_dir = state_dir.unwrap_or_else(default_body_state_dir);
            let legacy_jsonl = legacy_jsonl.unwrap_or_else(|| state_dir.join("tap-bodies.jsonl"));
            let mut config = BodyLoggerConfig::from_legacy_sink(legacy_jsonl);
            config.state_dir = state_dir.clone();
            config.archive_root =
                archive_root.unwrap_or_else(|| default_body_archive_root(&state_dir));
            let logger = BodyLogger::open_existing(config)?
                .ok_or_else(|| anyhow::anyhow!("body index does not exist"))?;
            let receipt: CaptureBackupReceipt = serde_json::from_slice(&std::fs::read(&receipt)?)?;
            let generation = receipt.generation;
            logger.accept_backup_receipt(receipt)?;
            if json {
                print_json(&serde_json::json!({
                    "schema": "switchback/capture-backup-accept@1",
                    "accepted": true,
                    "generation": generation,
                }))?;
            } else {
                println!("accepted backup receipt generation {generation}");
            }
        }
        BodyCmd::AcceptLegacyBackupReceipt {
            receipt,
            state_dir,
            archive_root,
            legacy_jsonl,
        } => {
            let state_dir = state_dir.unwrap_or_else(default_body_state_dir);
            let legacy_jsonl = legacy_jsonl.unwrap_or_else(|| state_dir.join("tap-bodies.jsonl"));
            let mut config = BodyLoggerConfig::from_legacy_sink(legacy_jsonl);
            config.state_dir = state_dir.clone();
            config.archive_root =
                archive_root.unwrap_or_else(|| default_body_archive_root(&state_dir));
            let logger = BodyLogger::open_existing(config)?
                .ok_or_else(|| anyhow::anyhow!("body index does not exist"))?;
            let receipt: CaptureLegacyBackupReceipt =
                serde_json::from_slice(&std::fs::read(&receipt)?)?;
            let artifacts = receipt.artifacts.len();
            logger.accept_legacy_backup_receipt(receipt)?;
            if json {
                print_json(&serde_json::json!({
                    "schema": "switchback/capture-legacy-backup-accept@1",
                    "accepted": true,
                    "artifacts": artifacts,
                }))?;
            } else {
                println!("accepted legacy backup receipt for {artifacts} artifact(s)");
            }
        }
        BodyCmd::ReclaimPlan {
            state_dir,
            archive_root,
            legacy_jsonl,
            keep_days,
        } => {
            let state_dir = state_dir.unwrap_or_else(default_body_state_dir);
            let legacy_jsonl = legacy_jsonl.unwrap_or_else(|| state_dir.join("tap-bodies.jsonl"));
            let mut config = BodyLoggerConfig::from_legacy_sink(legacy_jsonl);
            config.state_dir = state_dir.clone();
            config.archive_root =
                archive_root.unwrap_or_else(|| default_body_archive_root(&state_dir));
            let logger = BodyLogger::open_existing(config)?
                .ok_or_else(|| anyhow::anyhow!("body index does not exist"))?;
            let plan = logger.reclaim_plan(resolve_keep_days(keep_days))?;
            if json {
                print_json(&plan)?;
            } else {
                println!(
                    "{} reclaimable segment(s), {} bytes after {} local day(s)",
                    plan.segments.len(),
                    plan.total_segment_bytes,
                    plan.keep_days
                );
            }
        }
        BodyCmd::Reclaim {
            proof,
            state_dir,
            archive_root,
            legacy_jsonl,
            keep_days,
            confirm,
        } => {
            let state_dir = state_dir.unwrap_or_else(default_body_state_dir);
            let legacy_jsonl = legacy_jsonl.unwrap_or_else(|| state_dir.join("tap-bodies.jsonl"));
            let mut config = BodyLoggerConfig::from_legacy_sink(legacy_jsonl);
            config.state_dir = state_dir.clone();
            config.archive_root =
                archive_root.unwrap_or_else(|| default_body_archive_root(&state_dir));
            let logger = BodyLogger::open_existing(config)?
                .ok_or_else(|| anyhow::anyhow!("body index does not exist"))?;
            let proof: CaptureReclaimProof = serde_json::from_slice(&std::fs::read(proof)?)?;
            let report = logger.reclaim_verified_segments(
                CaptureReclaimOptions {
                    keep_days: resolve_keep_days(keep_days),
                    confirm,
                },
                proof,
            )?;
            if json {
                print_json(&report)?;
            } else {
                println!(
                    "{} {} segment(s), {} bytes",
                    if report.dry_run {
                        "would reclaim"
                    } else {
                        "reclaimed"
                    },
                    if report.dry_run {
                        report.candidate_segments
                    } else {
                        report.reclaimed_segments
                    },
                    if report.dry_run {
                        report.candidate_bytes
                    } else {
                        report.reclaimed_bytes
                    }
                );
            }
        }
        BodyCmd::Restore {
            segment_sha256,
            source_segment,
            source_manifest,
            state_dir,
            archive_root,
            legacy_jsonl,
        } => {
            let state_dir = state_dir.unwrap_or_else(default_body_state_dir);
            let legacy_jsonl = legacy_jsonl.unwrap_or_else(|| state_dir.join("tap-bodies.jsonl"));
            let mut config = BodyLoggerConfig::from_legacy_sink(legacy_jsonl);
            config.state_dir = state_dir.clone();
            config.archive_root =
                archive_root.unwrap_or_else(|| default_body_archive_root(&state_dir));
            let logger = BodyLogger::open_existing(config)?
                .ok_or_else(|| anyhow::anyhow!("body index does not exist"))?;
            logger.restore_remote_segment(&segment_sha256, &source_segment, &source_manifest)?;
            if json {
                print_json(&serde_json::json!({
                    "schema": "switchback/capture-restore@1",
                    "restored": true,
                    "segment_sha256": segment_sha256,
                }))?;
            } else {
                println!("restored capture segment {segment_sha256}");
            }
        }
        BodyCmd::Audit {
            request_id,
            client,
            format,
            out,
            open,
            state_dir,
            archive_root,
            legacy_jsonl,
        } => {
            let state_dir = state_dir.unwrap_or_else(default_body_state_dir);
            let config = body_logger_config(state_dir.clone(), archive_root, legacy_jsonl);
            let logger = open_existing_logger(config)?;
            let request_id = if request_id == "latest" {
                latest_request_id(&logger, client.as_deref())?
            } else {
                request_id
            };
            let trace = load_trace_json_from_state(&state_dir, &request_id);
            let audit = build_audit(&logger, &request_id, trace)?;
            let write = write_audit_bundle(&state_dir, out.as_deref(), &audit, open)?;
            if json || format == "json" {
                print_json(&serde_json::json!({
                    "audit": audit,
                    "files": write,
                }))?;
            } else {
                println!("audit: {}", write.markdown_path);
                println!("bundle: {}", write.dir);
                println!("metrics: {}", write.metrics_path);
                println!("daily: {}", write.daily_rollup_path);
            }
        }
        BodyCmd::Brief { period, state_dir } => {
            let state_dir = state_dir.unwrap_or_else(default_body_state_dir);
            let brief = body_brief(&state_dir, &period)?;
            if json {
                print_json(&serde_json::json!({
                    "period": period,
                    "markdown": brief,
                }))?;
            } else {
                print!("{brief}");
            }
        }
        BodyCmd::Gc {
            state_dir,
            archive_root,
            legacy_jsonl,
            keep_days,
            confirm,
            drain_only,
            compact,
            batch_size,
        } => {
            let state_dir = state_dir.unwrap_or_else(default_body_state_dir);
            let config = body_logger_config(state_dir, archive_root, legacy_jsonl);
            let logger = open_existing_logger(config)?;
            let report = logger.gc(GcOptions {
                keep_days: resolve_keep_days(keep_days),
                confirm,
                drain_only,
                batch_size: batch_size.unwrap_or(DEFAULT_GC_BATCH_SIZE),
            })?;
            // Compaction is only meaningful (and only mutates) with --confirm.
            let compact_report = if compact {
                Some(logger.compact(confirm)?)
            } else {
                None
            };
            if json {
                print_json(&serde_json::json!({
                    "gc": report,
                    "compact": compact_report,
                }))?;
            } else {
                print_gc_report(&report);
                if let Some(compact) = &compact_report {
                    if let Some(reason) = &compact.refused {
                        println!("compact: REFUSED — {reason}");
                    } else {
                        println!(
                            "compact: {} -> {} bytes ({} events, {} blobs preserved)",
                            compact.bytes_before,
                            compact.bytes_after,
                            compact.events_after,
                            compact.blobs_after
                        );
                    }
                }
            }
        }
    }
    Ok(())
}

fn print_gc_report(report: &sb_bodylog::GcReport) {
    if let Some(reason) = &report.refused {
        println!("gc: REFUSED — {reason}");
        return;
    }
    let mode = if report.dry_run {
        "dry-run"
    } else {
        "confirmed"
    };
    println!(
        "gc: {mode} (keep {} days, cutoff {}, archive {})",
        report.keep_days,
        report.cutoff_day,
        if report.archive_available {
            "available"
        } else {
            "unavailable"
        }
    );
    if report.candidate_days.is_empty() {
        println!("  candidate days: none");
    } else {
        println!("  candidate days:");
        for day in &report.candidate_days {
            println!("    {} — {} event rows", day.day, day.event_rows);
        }
    }
    if report.dry_run {
        println!(
            "  would drain: {} spool segments, {} legacy spool blobs, {} legacy spool day-files",
            report.spool_segments_drained,
            report.spool_blobs_drained,
            report.spool_day_files_drained
        );
        println!("  (dry-run: pass --confirm to mutate)");
    } else {
        println!(
            "  deleted: {} events, {} blobs",
            report.events_deleted, report.blobs_deleted
        );
        println!(
            "  drained: {} spool segments, {} legacy spool blobs, {} legacy spool day-files",
            report.spool_segments_drained,
            report.spool_blobs_drained,
            report.spool_day_files_drained
        );
    }
}

fn default_body_state_dir() -> PathBuf {
    std::env::var_os("SWITCHBACK_BODY_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| RuntimePaths::from_env().state_root())
}

fn default_body_archive_root(state_dir: &Path) -> PathBuf {
    std::env::var_os("SWITCHBACK_BODY_ARCHIVE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| state_dir.join("body").join("archive"))
}

fn to_pretty(value: &serde_json::Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

pub(crate) fn print_json(value: &impl Serialize) -> anyhow::Result<()> {
    println!("{}", to_pretty(&serde_json::to_value(value)?));
    Ok(())
}
