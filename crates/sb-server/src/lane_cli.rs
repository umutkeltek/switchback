use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use clap::{Args, Subcommand};
use sb_core::{ClientProfileKind, ComboConfig, Config, ProviderKind, RouteConfig, RouteRequire};
use serde::Serialize;

use crate::lane_profile_cli::{
    audit_claude_lane, define_claude_lane, print_claude_lane_audit_text,
    print_claude_lane_define_text, ClaudeLaneAuditArgs, ClaudeLaneDefineArgs,
};

#[derive(Subcommand)]
pub(crate) enum LaneCmd {
    /// Inspect local lane identity, defaults, and fail-closed native state.
    Doctor,
    /// Plan or materialize one typed Claude Code lane and provider profile.
    Define(ClaudeLaneDefineArgs),
    /// Audit local client configuration against a named lane contract.
    Audit {
        #[command(subcommand)]
        target: LaneAuditCmd,
    },
    /// Install or repair local client configuration for a named lane.
    Install {
        #[command(subcommand)]
        target: LaneInstallCmd,
    },
}

#[derive(Subcommand)]
pub(crate) enum LaneAuditCmd {
    /// Check that Codex's scout profile points at the Switchback scout/code lane.
    CodexScout(CodexScoutAuditArgs),
    /// Check a materialized Claude Code lane against live Switchback configuration.
    ClaudeProfile(ClaudeLaneAuditArgs),
}

#[derive(Args, Clone)]
pub(crate) struct CodexScoutAuditArgs {
    /// Codex config path. Defaults to $HOME/.codex/config.toml.
    #[arg(long)]
    codex_config: Option<PathBuf>,
    /// Codex profile name to check.
    #[arg(long, default_value = "switchback-scout")]
    profile: String,
    /// Codex model provider id to check.
    #[arg(long, default_value = "switchback-scout")]
    provider: String,
    /// Expected Switchback lane model.
    #[arg(long, default_value = "scout/code")]
    model: String,
    /// Expected reasoning effort for the scout lane.
    #[arg(long, default_value = "xhigh")]
    reasoning_effort: String,
    /// Expected provider base URL. Defaults to http://<config server.bind>/v1.
    #[arg(long)]
    base_url: Option<String>,
    /// Expected auth env key name.
    #[arg(long, default_value = "SWITCHBACK_SCOUT_API_KEY")]
    env_key: String,
}

#[derive(Subcommand)]
pub(crate) enum LaneInstallCmd {
    /// Write or repair Codex's scout profile for the Switchback scout/code lane.
    CodexScout(CodexScoutInstallArgs),
}

#[derive(Args)]
pub(crate) struct CodexScoutInstallArgs {
    #[command(flatten)]
    audit: CodexScoutAuditArgs,
    /// Show the repaired config/audit result without writing.
    #[arg(long)]
    dry_run: bool,
    /// Skip creating a timestamped backup before replacing an existing file.
    #[arg(long)]
    no_backup: bool,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct LaneDoctorReport {
    schema: &'static str,
    ok: bool,
    config: String,
    bind: String,
    lanes: Vec<LaneReport>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    problems: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    next_actions: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct LaneReport {
    id: String,
    state: LaneState,
    surface: String,
    execution_class: String,
    cost_policy: String,
    resume_scope: String,
    source: LaneSource,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    aliases: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    primary_target: Option<String>,
    fallback_count: usize,
    /// Per-capability requirement map declared by the route (true = the route
    /// requires this capability). Only non-null `RouteRequire` fields surface.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    capability_requirements: BTreeMap<String, bool>,
    /// Per-capability coverage map: `saturating/total` (e.g. `0/4`, `3/4`).
    /// Computed statically from `ProviderConfig.capabilities` overrides;
    /// a zero-coverage entry signals `state: red`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    capability_coverage: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    problems: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum LaneState {
    Green,
    Yellow,
    Red,
    Manual,
}

impl LaneState {
    fn is_problem(self) -> bool {
        matches!(self, Self::Red)
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum LaneSource {
    ExactRoute {
        name: String,
    },
    LegacyCombo {
        name: String,
        canonical_route: &'static str,
    },
    NativeRelayGate {
        provider_count: usize,
    },
    ManualHandoff,
    Missing {
        expected: Vec<&'static str>,
    },
}

pub(crate) fn run_lane_cmd(action: LaneCmd, config: &Path, json: bool) -> anyhow::Result<()> {
    match action {
        LaneCmd::Doctor => {
            let cfg = Config::from_path(config)?;
            let report = lane_doctor_report(&cfg, config);
            if json {
                crate::print_json(&report)?;
            } else {
                print_lane_doctor_text(&report);
                if !report.ok {
                    std::process::exit(1);
                }
            }
        }
        LaneCmd::Define(args) => {
            let cfg = Config::from_path(config)?;
            let report = define_claude_lane(&cfg, config, args)?;
            if json {
                crate::print_json(&report)?;
            } else {
                print_claude_lane_define_text(&report);
                if !report.ok {
                    std::process::exit(1);
                }
            }
        }
        LaneCmd::Audit { target } => {
            let cfg = Config::from_path(config)?;
            match target {
                LaneAuditCmd::CodexScout(args) => {
                    let report = codex_scout_audit_report(&cfg, args)?;
                    if json {
                        crate::print_json(&report)?;
                    } else {
                        print_codex_scout_audit_text(&report);
                        if !report.ok {
                            std::process::exit(1);
                        }
                    }
                }
                LaneAuditCmd::ClaudeProfile(args) => {
                    let report = audit_claude_lane(&cfg, config, args)?;
                    if json {
                        crate::print_json(&report)?;
                    } else {
                        print_claude_lane_audit_text(&report);
                        if !report.ok {
                            std::process::exit(1);
                        }
                    }
                }
            }
        }
        LaneCmd::Install { target } => {
            let cfg = Config::from_path(config)?;
            match target {
                LaneInstallCmd::CodexScout(args) => {
                    let report = codex_scout_install_report(&cfg, args)?;
                    if json {
                        crate::print_json(&report)?;
                    } else {
                        print_codex_scout_install_text(&report);
                        if !report.ok {
                            std::process::exit(1);
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
struct CodexScoutAuditReport {
    schema: &'static str,
    ok: bool,
    codex_config: String,
    profile: String,
    provider: String,
    expected_model: String,
    expected_base_url: String,
    checks: Vec<CodexScoutAuditCheck>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    next_actions: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct CodexScoutAuditCheck {
    name: &'static str,
    ok: bool,
    expected: serde_json::Value,
    actual: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
struct CodexScoutInstallReport {
    schema: &'static str,
    ok: bool,
    changed: bool,
    dry_run: bool,
    codex_config: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    backup: Option<String>,
    audit: CodexScoutAuditReport,
}

fn codex_scout_audit_report(
    cfg: &Config,
    args: CodexScoutAuditArgs,
) -> anyhow::Result<CodexScoutAuditReport> {
    let codex_config = args
        .codex_config
        .clone()
        .unwrap_or_else(default_codex_config_path);
    let text = std::fs::read_to_string(&codex_config)
        .map_err(|e| anyhow::anyhow!("read {}: {e}", codex_config.display()))?;
    codex_scout_audit_report_from_text(cfg, &args, &codex_config, &text)
}

fn codex_scout_audit_report_from_text(
    cfg: &Config,
    args: &CodexScoutAuditArgs,
    codex_config: &Path,
    text: &str,
) -> anyhow::Result<CodexScoutAuditReport> {
    let expected_base_url = expected_codex_base_url(cfg, args);
    let parsed = text
        .parse::<toml::Value>()
        .map_err(|e| anyhow::anyhow!("parse {}: {e}", codex_config.display()))?;

    let profile_path = ["profiles", args.profile.as_str()];
    let provider_path = ["model_providers", args.provider.as_str()];
    let profile = table_at(&parsed, &profile_path);
    let provider = table_at(&parsed, &provider_path);

    let checks = vec![
        check_bool("profile_exists", true, Some(profile.is_some())),
        check_bool("provider_exists", true, Some(provider.is_some())),
        check_string(
            "profile.model_provider",
            &args.provider,
            profile.and_then(|table| string_field(table, "model_provider")),
        ),
        check_string(
            "profile.model",
            &args.model,
            profile.and_then(|table| string_field(table, "model")),
        ),
        check_string(
            "profile.model_reasoning_effort",
            &args.reasoning_effort,
            profile.and_then(|table| string_field(table, "model_reasoning_effort")),
        ),
        check_string(
            "provider.base_url",
            &expected_base_url,
            provider.and_then(|table| string_field(table, "base_url")),
        ),
        check_string(
            "provider.wire_api",
            "responses",
            provider.and_then(|table| string_field(table, "wire_api")),
        ),
        check_string(
            "provider.env_key",
            &args.env_key,
            provider.and_then(|table| string_field(table, "env_key")),
        ),
        check_bool(
            "provider.requires_openai_auth",
            false,
            provider.and_then(|table| bool_field(table, "requires_openai_auth")),
        ),
    ];

    let ok = checks.iter().all(|check| check.ok);
    let next_actions = if ok {
        Vec::new()
    } else {
        vec![format!(
            "Set profile `{}` to provider `{}`, model `{}`, reasoning `{}`, base_url `{}`",
            args.profile, args.provider, args.model, args.reasoning_effort, expected_base_url
        )]
    };

    Ok(CodexScoutAuditReport {
        schema: "switchback/lane-codex-scout-audit@1",
        ok,
        codex_config: codex_config.display().to_string(),
        profile: args.profile.clone(),
        provider: args.provider.clone(),
        expected_model: args.model.clone(),
        expected_base_url,
        checks,
        next_actions,
    })
}

fn codex_scout_install_report(
    cfg: &Config,
    args: CodexScoutInstallArgs,
) -> anyhow::Result<CodexScoutInstallReport> {
    let codex_config = args
        .audit
        .codex_config
        .clone()
        .unwrap_or_else(default_codex_config_path);
    let before = match std::fs::read_to_string(&codex_config) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => anyhow::bail!("read {}: {e}", codex_config.display()),
    };
    if !before.trim().is_empty() {
        before
            .parse::<toml::Value>()
            .map_err(|e| anyhow::anyhow!("parse {}: {e}", codex_config.display()))?;
        let current_audit =
            codex_scout_audit_report_from_text(cfg, &args.audit, &codex_config, &before)?;
        if current_audit.ok {
            return Ok(CodexScoutInstallReport {
                schema: "switchback/lane-codex-scout-install@1",
                ok: true,
                changed: false,
                dry_run: args.dry_run,
                codex_config: codex_config.display().to_string(),
                backup: None,
                audit: current_audit,
            });
        }
    }

    let after = apply_codex_scout_contract(cfg, &args.audit, &before);
    let changed = before != after;

    let mut backup = None;
    if changed && !args.dry_run {
        if !before.is_empty() && !args.no_backup {
            let path = backup_path_for(&codex_config);
            std::fs::copy(&codex_config, &path).map_err(|e| {
                anyhow::anyhow!(
                    "backup {} -> {}: {e}",
                    codex_config.display(),
                    path.display()
                )
            })?;
            backup = Some(path.display().to_string());
        }
        crate::config_cli::write_file_atomic(&codex_config, &after)?;
    }

    let audit = if args.dry_run {
        codex_scout_audit_report_from_text(cfg, &args.audit, &codex_config, &after)?
    } else {
        codex_scout_audit_report(cfg, args.audit.clone())?
    };

    Ok(CodexScoutInstallReport {
        schema: "switchback/lane-codex-scout-install@1",
        ok: audit.ok,
        changed,
        dry_run: args.dry_run,
        codex_config: codex_config.display().to_string(),
        backup,
        audit,
    })
}

fn apply_codex_scout_contract(cfg: &Config, args: &CodexScoutAuditArgs, before: &str) -> String {
    let expected_base_url = expected_codex_base_url(cfg, args);
    let profile_table = format!("profiles.{}", toml_key(&args.profile));
    let provider_table = format!("model_providers.{}", toml_key(&args.provider));
    let mut out = remove_toml_tables(before, &[profile_table.as_str(), provider_table.as_str()])
        .trim_end()
        .to_string();
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    out.push_str(&format!(
        "[{}]\nmodel_provider = {}\nmodel = {}\nmodel_reasoning_effort = {}\n\n[{}]\nname = {}\nbase_url = {}\nwire_api = \"responses\"\nenv_key = {}\nrequires_openai_auth = false\n",
        profile_table,
        toml_string(&args.provider),
        toml_string(&args.model),
        toml_string(&args.reasoning_effort),
        provider_table,
        toml_string("Switchback Scout"),
        toml_string(&expected_base_url),
        toml_string(&args.env_key),
    ));
    out
}

fn expected_codex_base_url(cfg: &Config, args: &CodexScoutAuditArgs) -> String {
    args.base_url
        .clone()
        .unwrap_or_else(|| format!("http://{}/v1", cfg.server.bind))
}

fn default_codex_config_path() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".codex")
        .join("config.toml")
}

fn table_at<'a>(value: &'a toml::Value, path: &[&str]) -> Option<&'a toml::value::Table> {
    let mut current = value;
    for segment in path {
        current = current.get(*segment)?;
    }
    current.as_table()
}

fn string_field(table: &toml::value::Table, key: &str) -> Option<String> {
    table.get(key)?.as_str().map(ToString::to_string)
}

fn bool_field(table: &toml::value::Table, key: &str) -> Option<bool> {
    table.get(key)?.as_bool()
}

fn check_string(
    name: &'static str,
    expected: &str,
    actual: Option<String>,
) -> CodexScoutAuditCheck {
    let actual_json = actual
        .as_ref()
        .map(|value| serde_json::json!(value))
        .unwrap_or(serde_json::Value::Null);
    CodexScoutAuditCheck {
        name,
        ok: actual.as_deref() == Some(expected),
        expected: serde_json::json!(expected),
        actual: actual_json,
    }
}

fn check_bool(name: &'static str, expected: bool, actual: Option<bool>) -> CodexScoutAuditCheck {
    let actual_json = actual
        .map(|value| serde_json::json!(value))
        .unwrap_or(serde_json::Value::Null);
    CodexScoutAuditCheck {
        name,
        ok: actual == Some(expected),
        expected: serde_json::json!(expected),
        actual: actual_json,
    }
}

fn backup_path_for(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config.toml");
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    path.with_file_name(format!(
        "{file_name}.bak-switchback-lane-{}-{nanos}",
        std::process::id()
    ))
}

fn remove_toml_tables(input: &str, table_paths: &[&str]) -> String {
    let mut output = String::new();
    let mut skip = false;
    for line in input.lines() {
        if let Some(header) = toml_table_header(line) {
            skip = table_paths.contains(&header);
        }
        if !skip {
            output.push_str(line);
            output.push('\n');
        }
    }
    output
}

fn toml_table_header(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    if trimmed.starts_with("[[") || !trimmed.starts_with('[') || !trimmed.ends_with(']') {
        return None;
    }
    Some(trimmed.trim_start_matches('[').trim_end_matches(']').trim())
}

fn toml_key(value: &str) -> String {
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        value.to_string()
    } else {
        toml_string(value)
    }
}

fn toml_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}

pub(crate) fn lane_doctor_report(cfg: &Config, config_path: &Path) -> LaneDoctorReport {
    let mut lanes = vec![
        lane_from_route_or_combo(
            cfg,
            LaneSpec {
                id: "scout/code",
                surface: "openai_responses",
                execution_class: "cheap_scout",
                cost_policy: "free_first_hard_ceiling",
                resume_scope: "codex_profile:switchback-scout",
                exact_route: "scout/code",
                legacy_combo: Some("nonstop-code"),
                aliases: vec!["scout", "auto/scout-code"],
            },
        ),
        lane_from_route_or_combo(
            cfg,
            LaneSpec {
                id: "scout/chat",
                surface: "openai_chat_or_responses",
                execution_class: "cheap_scout",
                cost_policy: "free_first_hard_ceiling",
                resume_scope: "switchback_session",
                exact_route: "scout/chat",
                legacy_combo: Some("nonstop-chat"),
                aliases: vec!["auto/scout-chat"],
            },
        ),
        codex_api_lane(cfg),
        codex_native_lane(cfg),
        build_lane_report(
            cfg,
            "pro/manual",
            "manual_pro",
            "external_manual",
            "subscription_native",
            "not_applicable",
            vec!["oracle", "chatgpt-pro"],
            LaneState::Manual,
            LaneSource::ManualHandoff,
            &[],
            &RouteRequire::default(),
            Vec::new(),
            vec![
                "ChatGPT Pro is a creative handoff lane, not an automatic router provider"
                    .to_string(),
            ],
        ),
    ];

    // Data-driven pass: enumerate every route and combo in the config so
    // user-defined lanes (e.g. `wpcom/gpt-5.6-sol`) get the same capability
    // coverage check as the 5 stable lanes above. Stable lanes that match a
    // route by `match.model` are skipped to avoid double-listing.
    let stable_match_models = [
        Some("scout/code".to_string()),
        Some("scout/chat".to_string()),
        Some("codex/api".to_string()),
        Some("codex-native".to_string()),
    ];
    for route in &cfg.routes {
        if stable_match_models.contains(&route.match_.model) {
            continue;
        }
        lanes.push(lane_for_user_route(cfg, route));
    }
    let stable_combos = ["nonstop-code", "nonstop-chat"];
    for (name, combo) in &cfg.combos {
        if stable_combos.contains(&name.as_str()) {
            continue;
        }
        lanes.push(lane_for_user_combo(cfg, name, combo));
    }

    let mut warnings = Vec::new();
    let mut problems = Vec::new();
    if wildcard_default_is_thin(cfg) && cfg.combos.contains_key("nonstop-code") {
        warnings.push(
            "default wildcard route has a single target while nonstop-code has a richer pool"
                .to_string(),
        );
    }
    for lane in &lanes {
        for problem in &lane.problems {
            problems.push(format!("{}: {problem}", lane.id));
        }
        for warning in &lane.warnings {
            warnings.push(format!("{}: {warning}", lane.id));
        }
    }

    let mut next_actions = Vec::new();
    if lanes
        .iter()
        .any(|lane| matches!(lane.source, LaneSource::LegacyCombo { .. }))
    {
        next_actions.push(
            "Promote legacy combos into exact lane routes in the model-router generator"
                .to_string(),
        );
    }
    if cfg.exact_route_for("codex-native").is_none() {
        next_actions.push(
            "Keep codex-native fail-closed until native relay conformance is green".to_string(),
        );
    }
    if wildcard_default_is_thin(cfg) {
        next_actions
            .push("Make default map to a named scout lane or reject unknown aliases".to_string());
    }

    let ok = lanes
        .iter()
        .filter(|lane| lane.id != "codex-native")
        .all(|lane| !lane.state.is_problem());

    lanes.sort_by_key(|lane| match lane.id.as_str() {
        "scout/code" => 0,
        "scout/chat" => 1,
        "codex/api" => 2,
        "codex-native" => 3,
        "pro/manual" => 4,
        _ => 99,
    });

    LaneDoctorReport {
        schema: "switchback/lane-doctor@1",
        ok,
        config: config_path.display().to_string(),
        bind: cfg.server.bind.clone(),
        lanes,
        problems,
        warnings,
        next_actions,
    }
}

struct LaneSpec {
    id: &'static str,
    surface: &'static str,
    execution_class: &'static str,
    cost_policy: &'static str,
    resume_scope: &'static str,
    exact_route: &'static str,
    legacy_combo: Option<&'static str>,
    aliases: Vec<&'static str>,
}

fn lane_from_route_or_combo(cfg: &Config, spec: LaneSpec) -> LaneReport {
    if let Some(route) = cfg.exact_route_for(spec.exact_route) {
        return build_lane_report(
            cfg,
            spec.id,
            spec.surface,
            spec.execution_class,
            spec.cost_policy,
            spec.resume_scope,
            spec.aliases,
            LaneState::Green,
            LaneSource::ExactRoute {
                name: route.name.clone(),
            },
            &route.targets,
            &route.require,
            Vec::new(),
            Vec::new(),
        );
    }

    if let Some(combo_name) = spec.legacy_combo {
        if let Some(combo) = cfg.combo_for(combo_name) {
            let warning = format!(
                "using legacy combo `{combo_name}`; promote to exact route `{}` for durable lane identity",
                spec.exact_route
            );
            let source = LaneSource::LegacyCombo {
                name: combo_name.to_string(),
                canonical_route: spec.exact_route,
            };
            return build_lane_report(
                cfg,
                spec.id,
                spec.surface,
                spec.execution_class,
                spec.cost_policy,
                spec.resume_scope,
                spec.aliases,
                LaneState::Yellow,
                source,
                &combo.models,
                &combo.require,
                Vec::new(),
                vec![warning],
            );
        }
    }

    let missing_problem = format!(
        "missing exact route `{}`{}",
        spec.exact_route,
        spec.legacy_combo
            .map(|combo| format!(" or legacy combo `{combo}`"))
            .unwrap_or_default()
    );
    build_lane_report(
        cfg,
        spec.id,
        spec.surface,
        spec.execution_class,
        spec.cost_policy,
        spec.resume_scope,
        spec.aliases,
        LaneState::Red,
        LaneSource::Missing {
            expected: spec
                .legacy_combo
                .map(|combo| vec![spec.exact_route, combo])
                .unwrap_or_else(|| vec![spec.exact_route]),
        },
        &[],
        &RouteRequire::default(),
        vec![missing_problem],
        Vec::new(),
    )
}

fn codex_api_lane(cfg: &Config) -> LaneReport {
    let spec = LaneSpec {
        id: "codex/api",
        surface: "openai_responses",
        execution_class: "codex_compatible_scout_pool",
        cost_policy: "free_first_hard_ceiling",
        resume_scope: "codex_profile:api",
        exact_route: "codex/api",
        legacy_combo: Some("nonstop-code"),
        aliases: vec!["codex-api"],
    };
    let mut lane = lane_from_route_or_combo(cfg, spec);
    if !matches!(lane.state, LaneState::Red) {
        lane.state = LaneState::Yellow;
    }
    lane.warnings.push(
        "`codex/api` is a Codex-compatible API surface backed by the scout pool here; the interactive `codex` shell command uses `scout/code`, and native Codex stays `codex-native`"
            .to_string(),
    );
    let codex_profiles = cfg
        .client_profiles
        .iter()
        .filter(|profile| profile.kind == ClientProfileKind::Codex)
        .collect::<Vec<_>>();
    if codex_profiles.is_empty() {
        lane.warnings
            .push("no Codex client profile is declared in config".to_string());
    }
    lane
}

fn codex_native_lane(cfg: &Config) -> LaneReport {
    let provider_count = cfg
        .providers
        .iter()
        .filter(|provider| matches!(provider.kind, ProviderKind::CodexNativeRelay { .. }))
        .count();
    let route = cfg.exact_route_for("codex-native");
    let mut problems = Vec::new();
    let warnings = Vec::new();

    let (state, source, targets) = match (route, provider_count) {
        (Some(route), count) if count > 0 => (
            LaneState::Yellow,
            LaneSource::ExactRoute {
                name: route.name.clone(),
            },
            route.targets.as_slice(),
        ),
        (Some(route), _) => {
            problems.push(
                "codex-native route exists but no codex_native_relay provider is configured"
                    .to_string(),
            );
            (
                LaneState::Red,
                LaneSource::ExactRoute {
                    name: route.name.clone(),
                },
                route.targets.as_slice(),
            )
        }
        (None, count) if count > 0 => (
            LaneState::Yellow,
            LaneSource::NativeRelayGate {
                provider_count: count,
            },
            &[][..],
        ),
        (None, _) => (
            LaneState::Red,
            LaneSource::NativeRelayGate { provider_count: 0 },
            &[][..],
        ),
    };

    if route.is_none() {
        problems.push(
            "codex-native intentionally has no executable route; keep it fail-closed until relay conformance is green"
                .to_string(),
        );
    }

    build_lane_report(
        cfg,
        "codex-native",
        "openai_responses",
        "native_relay",
        "subscription_native",
        "codex_profile:native",
        vec!["codex-native"],
        state,
        source,
        targets,
        &RouteRequire::default(),
        problems,
        warnings,
    )
}

/// Build a `LaneReport` and compute capability coverage from `require` against
/// `targets`. A target string is `provider_id/model_id`; the provider's
/// `CapabilityOverrides` is the source of truth here. A non-null require field
/// with zero satisfying targets flips the lane to `Red` and emits a problem.
#[allow(clippy::too_many_arguments)]
fn build_lane_report(
    cfg: &Config,
    id: &str,
    surface: &str,
    execution_class: &str,
    cost_policy: &str,
    resume_scope: &str,
    aliases: Vec<&str>,
    state: LaneState,
    source: LaneSource,
    targets: &[String],
    require: &RouteRequire,
    mut problems: Vec<String>,
    warnings: Vec<String>,
) -> LaneReport {
    let (capability_requirements, capability_coverage, capability_problems) =
        capability_requirements_and_coverage(cfg, targets, require);
    let capability_failure = !capability_problems.is_empty();
    problems.extend(capability_problems);
    let computed_state = if capability_failure && matches!(state, LaneState::Green) {
        LaneState::Red
    } else {
        state
    };
    LaneReport {
        id: id.to_string(),
        state: computed_state,
        surface: surface.to_string(),
        execution_class: execution_class.to_string(),
        cost_policy: cost_policy.to_string(),
        resume_scope: resume_scope.to_string(),
        source,
        aliases: aliases.into_iter().map(str::to_string).collect(),
        primary_target: targets.first().cloned(),
        fallback_count: targets.len().saturating_sub(1),
        capability_requirements,
        capability_coverage,
        problems,
        warnings,
    }
}

/// Walk `RouteRequire` fields; for each non-null field, count how many of
/// `targets` have a provider whose `CapabilityOverrides` declares that field as
/// `Some(true)`. Returns (requirements, coverage `"saturating/total"`,
/// problems-with-zero-coverage).
fn capability_requirements_and_coverage(
    cfg: &Config,
    targets: &[String],
    require: &RouteRequire,
) -> (
    BTreeMap<String, bool>,
    BTreeMap<String, String>,
    Vec<String>,
) {
    let mut requirements = BTreeMap::new();
    let mut coverage = BTreeMap::new();
    let mut problems = Vec::new();
    let mut record = |field: &str, on: bool| {
        if !on {
            return;
        }
        requirements.insert(field.to_string(), true);
        let mut satisfying = 0usize;
        for target in targets {
            let provider_id = target.split('/').next().unwrap_or("");
            if provider_declares_capability(cfg, provider_id, field) {
                satisfying += 1;
            }
        }
        coverage.insert(field.to_string(), format!("{satisfying}/{}", targets.len()));
        if satisfying == 0 && !targets.is_empty() {
            problems.push(format!(
                "{field} required by route but 0/{} targets declare it",
                targets.len()
            ));
        }
    };
    record("streaming", require.streaming.unwrap_or(false));
    record("tool_calling", require.tool_calling.unwrap_or(false));
    record("server_tools", require.server_tools.unwrap_or(false));
    record("vision_in", require.vision_in.unwrap_or(false));
    record("audio_in", require.audio_in.unwrap_or(false));
    record("file_in", require.file_in.unwrap_or(false));
    record("image_out", require.image_out.unwrap_or(false));
    record(
        "reasoning_summary",
        require.reasoning_summary.unwrap_or(false),
    );
    record("json_schema", require.json_schema.unwrap_or(false));
    (requirements, coverage, problems)
}

/// Return `true` when the provider's `CapabilityOverrides` declares `field =
/// Some(true)`. Negative overrides (`Some(false)`) count as false; absent
/// fields are unknown and count as false (fail-closed). A missing provider
/// counts as false. The routing layer resolves this authoritatively at
/// request time; this static path lets the lane doctor work without
/// credentials.
fn provider_declares_capability(cfg: &Config, provider_id: &str, field: &str) -> bool {
    let Some(provider) = cfg.providers.iter().find(|p| p.id == provider_id) else {
        return false;
    };
    match field {
        "streaming" => provider.capabilities.streaming == Some(true),
        "tool_calling" => provider.capabilities.tool_calling == Some(true),
        "server_tools" => provider.capabilities.tool_calling == Some(true),
        "vision_in" => provider.capabilities.vision_in == Some(true),
        "audio_in" => provider.capabilities.audio_in == Some(true),
        "file_in" => provider.capabilities.file_in == Some(true),
        "image_out" => provider.capabilities.image_out == Some(true),
        "reasoning_summary" => provider.capabilities.reasoning_summary == Some(true),
        "json_schema" => provider.capabilities.json_schema == Some(true),
        _ => false,
    }
}

/// Build a `LaneReport` for a user-defined route (not one of the 5 stable
/// lanes). The id is `route/<match.model>` so it never collides with the
/// stable lanes.
fn lane_for_user_route(cfg: &Config, route: &RouteConfig) -> LaneReport {
    let id = format!(
        "route/{}",
        route.match_.model.as_deref().unwrap_or(&route.name)
    );
    let aliases = [route.name.clone()];
    let surface = "user_defined_route";
    let execution_class = "user_route";
    let cost_policy = "inherited";
    let resume_scope = "user_route";
    build_lane_report(
        cfg,
        &id,
        surface,
        execution_class,
        cost_policy,
        resume_scope,
        aliases.iter().map(String::as_str).collect(),
        LaneState::Green,
        LaneSource::ExactRoute {
            name: route.name.clone(),
        },
        &route.targets,
        &route.require,
        Vec::new(),
        Vec::new(),
    )
}

/// Build a `LaneReport` for a user-defined combo (not one of the 5 stable
/// lanes' legacy combos). The id is `combo/<name>`.
fn lane_for_user_combo(cfg: &Config, name: &str, combo: &ComboConfig) -> LaneReport {
    let id = format!("combo/{name}");
    let aliases = [name.to_string()];
    build_lane_report(
        cfg,
        &id,
        "user_defined_combo",
        "user_combo",
        "inherited",
        "user_combo",
        aliases.iter().map(String::as_str).collect(),
        LaneState::Green,
        LaneSource::LegacyCombo {
            name: name.to_string(),
            canonical_route: "",
        },
        &combo.models,
        &combo.require,
        Vec::new(),
        Vec::new(),
    )
}

fn wildcard_default_is_thin(cfg: &Config) -> bool {
    cfg.wildcard_route()
        .is_some_and(|route: &RouteConfig| route.targets.len() <= 1)
}

fn print_lane_doctor_text(report: &LaneDoctorReport) {
    println!("lane doctor {}", if report.ok { "ok" } else { "not-ok" });
    println!("config {}", report.config);
    println!("bind {}", report.bind);
    for lane in &report.lanes {
        println!(
            "lane {} state={:?} surface={} class={} cost={} primary={} fallbacks={}",
            lane.id,
            lane.state,
            lane.surface,
            lane.execution_class,
            lane.cost_policy,
            lane.primary_target.as_deref().unwrap_or("-"),
            lane.fallback_count
        );
        for problem in &lane.problems {
            println!("problem {} {}", lane.id, problem);
        }
        for warning in &lane.warnings {
            println!("warning {} {}", lane.id, warning);
        }
    }
    for problem in &report.problems {
        println!("problem {problem}");
    }
    for warning in &report.warnings {
        println!("warning {warning}");
    }
    for action in &report.next_actions {
        println!("next {action}");
    }
}

fn print_codex_scout_audit_text(report: &CodexScoutAuditReport) {
    println!(
        "codex scout audit {}",
        if report.ok { "ok" } else { "not-ok" }
    );
    println!("codex_config {}", report.codex_config);
    println!("profile {}", report.profile);
    println!("provider {}", report.provider);
    println!("expected_model {}", report.expected_model);
    println!("expected_base_url {}", report.expected_base_url);
    for check in &report.checks {
        println!(
            "{} {} expected={} actual={}",
            if check.ok { "pass" } else { "fail" },
            check.name,
            check.expected,
            check.actual
        );
    }
    for action in &report.next_actions {
        println!("next {action}");
    }
}

fn print_codex_scout_install_text(report: &CodexScoutInstallReport) {
    println!(
        "codex scout install {}",
        if report.ok { "ok" } else { "not-ok" }
    );
    println!("codex_config {}", report.codex_config);
    println!("changed {}", report.changed);
    println!("dry_run {}", report.dry_run);
    if let Some(backup) = &report.backup {
        println!("backup {backup}");
    }
    print_codex_scout_audit_text(&report.audit);
}
