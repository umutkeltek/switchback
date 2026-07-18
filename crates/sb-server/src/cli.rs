use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use sb_bodylog::{
    resolve_keep_days, BodyLogger, BodyLoggerConfig, GcOptions, DEFAULT_GC_BATCH_SIZE,
};
use sb_core::Config;
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
        /// Optional specialized probe (`fal`, `local`).
        provider: Option<String>,
        #[arg(long, default_value = "config/switchback.example.yaml")]
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
        #[arg(long, global = true, default_value = ".switchback/eval.sqlite")]
        store: PathBuf,
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
    /// Inspect native coding-client setup without mutating local state.
    Native {
        #[command(subcommand)]
        action: NativeCmd,
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
        /// Keep this many recent UTC days (default 14, env SWITCHBACK_BODY_KEEP_DAYS).
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
        Cmd::Setup { action } => run_setup_cmd(action, json)?,
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
            "  would drain: {} spool blobs, {} spool day-files",
            report.spool_blobs_drained, report.spool_day_files_drained
        );
        println!("  (dry-run: pass --confirm to mutate)");
    } else {
        println!(
            "  deleted: {} events, {} blobs",
            report.events_deleted, report.blobs_deleted
        );
        println!(
            "  drained: {} spool blobs, {} spool day-files",
            report.spool_blobs_drained, report.spool_day_files_drained
        );
    }
}

fn default_body_state_dir() -> PathBuf {
    std::env::var_os("SWITCHBACK_BODY_STATE_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("SB_RUNTIME_ROOT").map(|root| PathBuf::from(root).join("state"))
        })
        .or_else(|| {
            std::env::var_os("SWITCHBACK_ROOT")
                .map(PathBuf::from)
                .map(|root| root.join(".switchback").join("state"))
        })
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| home.join("Projects/systems/switchback/.switchback/state"))
        })
        .unwrap_or_else(|| PathBuf::from(".switchback/state"))
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
