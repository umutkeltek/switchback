use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use clap::{Args, ValueEnum};
use sb_core::Config;
use serde::Serialize;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

const LANE_RECORD_SCHEMA: &str = "switchback/claude-lane@1";
const AUDIT_SCHEMA: &str = "switchback/claude-lane-audit@1";
const DEFINE_SCHEMA: &str = "switchback/claude-lane-define@1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum LaneHarness {
    ClaudeCode,
}

impl LaneHarness {
    fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LaneTransport {
    Gateway,
    Tap,
    Headroom,
}

impl LaneTransport {
    fn as_str(self) -> &'static str {
        match self {
            Self::Gateway => "gateway",
            Self::Tap => "tap",
            Self::Headroom => "headroom",
        }
    }

    fn requires_port(self) -> bool {
        matches!(self, Self::Tap | Self::Headroom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NativeEffort {
    Default,
    Low,
    Medium,
    High,
    Max,
    Xhigh,
    Ultra,
}

impl NativeEffort {
    fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Max => "max",
            Self::Xhigh => "xhigh",
            Self::Ultra => "ultra",
        }
    }
}

#[derive(Args, Debug, Clone)]
pub(crate) struct ClaudeLaneDefineArgs {
    /// Stable lane/profile name.
    pub(crate) name: String,
    /// Inbound Switchback model requested by the harness.
    #[arg(long)]
    pub(crate) model: String,
    /// Exact Switchback route to bind. Defaults to --model.
    #[arg(long)]
    pub(crate) route: Option<String>,
    /// Additional exact route aliases that must resolve to identical targets.
    #[arg(long = "alias")]
    pub(crate) aliases: Vec<String>,
    /// Harness that consumes the materialized profile.
    #[arg(long, value_enum, default_value = "claude-code")]
    pub(crate) harness: LaneHarness,
    /// Executable transport used by the harness.
    #[arg(long, value_enum)]
    pub(crate) transport: LaneTransport,
    /// Native harness effort. Ultra is explicit per profile and never inherited.
    #[arg(long, value_enum)]
    pub(crate) effort: NativeEffort,
    /// Local Anthropic-compatible tap/proxy port (required for tap/headroom).
    #[arg(long)]
    pub(crate) anthropic_port: Option<u16>,
    /// Env-var name used for the local gateway token; the value is never read or copied.
    #[arg(long, default_value = "SWITCHBACK_SCOUT_API_KEY")]
    pub(crate) key_env: String,
    /// Claude profile directory label. Defaults to the lane name.
    #[arg(long)]
    pub(crate) profile_label: Option<String>,
    /// Display name shown by Claude Code.
    #[arg(long)]
    pub(crate) display_name: Option<String>,
    /// Description shown by Claude Code.
    #[arg(long)]
    pub(crate) description: Option<String>,
    /// Required number of ordered fallback targets after the primary.
    #[arg(long, default_value_t = 1)]
    pub(crate) min_fallbacks: usize,
    /// Existing Switchback lane-record root.
    #[arg(long)]
    pub(crate) lane_root: Option<PathBuf>,
    /// Existing Claude provider-profile root.
    #[arg(long)]
    pub(crate) profile_root: Option<PathBuf>,
    /// Apply the transaction. Without this flag the command is a read-only plan.
    #[arg(long)]
    pub(crate) apply: bool,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct ClaudeLaneAuditArgs {
    /// Stable lane/profile name.
    pub(crate) name: String,
    /// Existing Switchback lane-record root.
    #[arg(long)]
    pub(crate) lane_root: Option<PathBuf>,
    /// Existing Claude provider-profile root.
    #[arg(long)]
    pub(crate) profile_root: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize)]
struct ClaudeLaneDefinition {
    schema: &'static str,
    name: String,
    harness: &'static str,
    model: String,
    route: String,
    transport: &'static str,
    native_effort: &'static str,
    aliases: Vec<String>,
    targets: Vec<String>,
    revision: String,
    profile_label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    anthropic_port: Option<u16>,
    key_env: String,
    display_name: String,
    description: String,
    min_fallbacks: usize,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ClaudeLaneAuditReport {
    schema: &'static str,
    pub(crate) ok: bool,
    config: String,
    lane_record: String,
    settings: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    definition: Option<ClaudeLaneDefinition>,
    checks: Vec<AuditCheck>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    next_actions: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct AuditCheck {
    name: &'static str,
    ok: bool,
    expected: Value,
    actual: Value,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ClaudeLaneDefineReport {
    schema: &'static str,
    pub(crate) ok: bool,
    changed: bool,
    dry_run: bool,
    applied: bool,
    apply_mode: &'static str,
    replaced_surfaces: Vec<&'static str>,
    definition: ClaudeLaneDefinition,
    audit: ClaudeLaneAuditReport,
}

pub(crate) fn define_claude_lane(
    cfg: &Config,
    config_path: &Path,
    args: ClaudeLaneDefineArgs,
) -> anyhow::Result<ClaudeLaneDefineReport> {
    validate_safe_name(&args.name, "lane name")?;
    validate_model_token(&args.model, "model")?;
    validate_env_name(&args.key_env)?;
    if args.transport.requires_port() && args.anthropic_port.is_none() {
        anyhow::bail!(
            "transport {} requires --anthropic-port",
            args.transport.as_str()
        );
    }
    if !args.transport.requires_port() && args.anthropic_port.is_some() {
        anyhow::bail!(
            "transport {} does not accept --anthropic-port",
            args.transport.as_str()
        );
    }

    let route = args.route.clone().unwrap_or_else(|| args.model.clone());
    validate_model_token(&route, "route")?;
    let aliases = normalized_aliases(&route, &args.aliases)?;
    let route_cfg = cfg
        .exact_route_for(&route)
        .ok_or_else(|| anyhow::anyhow!("exact route `{route}` is not configured"))?;
    validate_route_targets(cfg, &route, &route_cfg.targets, args.min_fallbacks)?;
    if args.model != route {
        validate_alias_routes(cfg, std::slice::from_ref(&args.model), &route_cfg.targets)
            .map_err(|error| anyhow::anyhow!("model route is not coherent: {error}"))?;
    }
    validate_alias_routes(cfg, &aliases, &route_cfg.targets)?;

    let profile_label = args
        .profile_label
        .clone()
        .unwrap_or_else(|| args.name.clone());
    validate_safe_name(&profile_label, "profile label")?;
    let display_name = args
        .display_name
        .clone()
        .unwrap_or_else(|| args.name.clone());
    let description = args.description.clone().unwrap_or_else(|| {
        format!(
            "Claude Code via Switchback route {route}; native effort {}; {} ordered fallback(s).",
            args.effort.as_str(),
            route_cfg.targets.len().saturating_sub(1)
        )
    });

    let mut definition = ClaudeLaneDefinition {
        schema: LANE_RECORD_SCHEMA,
        name: args.name.clone(),
        harness: args.harness.as_str(),
        model: args.model.clone(),
        route,
        transport: args.transport.as_str(),
        native_effort: args.effort.as_str(),
        aliases,
        targets: route_cfg.targets.clone(),
        revision: String::new(),
        profile_label,
        anthropic_port: args.anthropic_port,
        key_env: args.key_env.clone(),
        display_name,
        description,
        min_fallbacks: args.min_fallbacks,
    };
    definition.revision = definition_revision(&definition)?;

    let (lane_root, profile_root) = roots(args.lane_root, args.profile_root);
    let lane_record = lane_root.join(format!("{}.env", definition.name));
    let settings = profile_root
        .join(&definition.profile_label)
        .join("settings.json");
    let record_after = render_lane_record(&definition);
    let settings_before = read_optional_text(&settings)?;
    let settings_after = render_settings(settings_before.as_deref(), &definition)?;
    let record_before = read_optional_text(&lane_record)?;
    let changed = record_before.as_deref() != Some(record_after.as_str())
        || settings_before.as_deref() != Some(settings_after.as_str());

    let desired_audit = audit_materialized(
        cfg,
        config_path,
        &lane_record,
        &settings,
        &record_after,
        &settings_after,
    );
    if !desired_audit.ok {
        anyhow::bail!("generated lane definition failed its own audit");
    }

    let mut audit = desired_audit;
    if args.apply && changed {
        write_pair_transaction(
            &lane_record,
            record_before.as_deref(),
            &record_after,
            &settings,
            settings_before.as_deref(),
            &settings_after,
        )?;
        audit = audit_claude_lane(
            cfg,
            config_path,
            ClaudeLaneAuditArgs {
                name: definition.name.clone(),
                lane_root: Some(lane_root),
                profile_root: Some(profile_root),
            },
        )?;
        if !audit.ok {
            rollback_pair(
                &lane_record,
                record_before.as_deref(),
                &settings,
                settings_before.as_deref(),
            )?;
            anyhow::bail!("post-apply audit failed; both files were rolled back");
        }
    }

    Ok(ClaudeLaneDefineReport {
        schema: DEFINE_SCHEMA,
        ok: audit.ok,
        changed,
        dry_run: !args.apply,
        applied: args.apply && changed,
        apply_mode: "atomic_per_file_with_cross_file_rollback",
        replaced_surfaces: vec![
            "direct Claude provider settings.json edits",
            "ad-hoc Switchback route/profile shell writes",
        ],
        definition,
        audit,
    })
}

pub(crate) fn audit_claude_lane(
    cfg: &Config,
    config_path: &Path,
    args: ClaudeLaneAuditArgs,
) -> anyhow::Result<ClaudeLaneAuditReport> {
    validate_safe_name(&args.name, "lane name")?;
    let (lane_root, profile_root) = roots(args.lane_root, args.profile_root);
    let lane_record = lane_root.join(format!("{}.env", args.name));
    let record = match std::fs::read_to_string(&lane_record) {
        Ok(value) => value,
        Err(error) => {
            return Ok(missing_audit_report(
                config_path,
                lane_record,
                profile_root.join(&args.name).join("settings.json"),
                format!("lane record is unavailable: {error}"),
            ));
        }
    };
    let fields = match parse_lane_record(&record) {
        Ok(value) => value,
        Err(error) => {
            return Ok(missing_audit_report(
                config_path,
                lane_record,
                profile_root.join(&args.name).join("settings.json"),
                format!("lane record is invalid: {error}"),
            ));
        }
    };
    let profile_label = fields
        .get("SB_LANE_CLAUDE_PROFILE_LABEL")
        .cloned()
        .unwrap_or_else(|| args.name.clone());
    let settings = profile_root.join(profile_label).join("settings.json");
    let settings_text = match std::fs::read_to_string(&settings) {
        Ok(value) => value,
        Err(error) => {
            return Ok(missing_audit_report(
                config_path,
                lane_record,
                settings,
                format!("settings.json is unavailable: {error}"),
            ));
        }
    };
    Ok(audit_materialized(
        cfg,
        config_path,
        &lane_record,
        &settings,
        &record,
        &settings_text,
    ))
}

fn audit_materialized(
    cfg: &Config,
    config_path: &Path,
    lane_record: &Path,
    settings: &Path,
    record_text: &str,
    settings_text: &str,
) -> ClaudeLaneAuditReport {
    let mut checks = Vec::new();
    let fields = match parse_lane_record(record_text) {
        Ok(fields) => fields,
        Err(error) => {
            return missing_audit_report(
                config_path,
                lane_record.to_path_buf(),
                settings.to_path_buf(),
                format!("lane record is invalid: {error}"),
            );
        }
    };

    let field = |key: &str| fields.get(key).cloned().unwrap_or_default();
    let name = field("SB_LANE_NAME");
    let harness = field("SB_LANE_HARNESS");
    let model = field("SB_LANE_MODEL");
    let route = field("SB_LANE_ROUTE");
    let transport = field("SB_LANE_TRANSPORT");
    let effort = field("SB_LANE_CLAUDE_EFFORT");
    let aliases = split_words(&field("SB_LANE_ALIASES"));
    let recorded_targets = split_words(&field("SB_LANE_TARGETS"));
    let profile_label = field("SB_LANE_CLAUDE_PROFILE_LABEL");
    let key_env = field("SB_LANE_KEY_ENV");
    let display_name = field("SB_LANE_CLAUDE_CUSTOM_MODEL_NAME");
    let description = field("SB_LANE_CLAUDE_CUSTOM_MODEL_DESCRIPTION");
    let min_fallbacks = field("SB_LANE_MIN_FALLBACKS")
        .parse::<usize>()
        .unwrap_or(usize::MAX);
    let anthropic_port = field("SB_LANE_ANTHROPIC_TAP").parse::<u16>().ok();
    let route_targets = cfg
        .exact_route_for(&route)
        .map(|configured| configured.targets.clone())
        .unwrap_or_default();

    push_check(
        &mut checks,
        "record.schema",
        json!(LANE_RECORD_SCHEMA),
        json!(field("SB_LANE_SCHEMA")),
    );
    push_check(
        &mut checks,
        "record.harness",
        json!("claude-code"),
        json!(harness),
    );
    push_check(
        &mut checks,
        "record.name",
        json!(lane_record
            .file_stem()
            .and_then(|v| v.to_str())
            .unwrap_or("")),
        json!(name),
    );
    push_check(
        &mut checks,
        "route.exists",
        json!(true),
        json!(cfg.exact_route_for(&route).is_some()),
    );
    push_check(
        &mut checks,
        "route.targets",
        json!(route_targets),
        json!(recorded_targets),
    );
    let model_targets = cfg
        .exact_route_for(&model)
        .map(|configured| configured.targets.clone())
        .unwrap_or_default();
    push_check(
        &mut checks,
        "route.model",
        json!(route_targets),
        json!(model_targets),
    );
    let fallback_ok = route_targets.len().saturating_sub(1) >= min_fallbacks;
    push_check(
        &mut checks,
        "route.fallbacks",
        json!(true),
        json!(fallback_ok),
    );
    let provider_ok = validate_provider_targets(cfg, &route_targets).is_ok();
    push_check(
        &mut checks,
        "route.providers",
        json!(true),
        json!(provider_ok),
    );
    let aliases_ok = validate_alias_routes(cfg, &aliases, &route_targets).is_ok();
    push_check(&mut checks, "route.aliases", json!(true), json!(aliases_ok));
    let transport_ok = match transport.as_str() {
        "gateway" => anthropic_port.is_none(),
        "tap" | "headroom" => anthropic_port.is_some(),
        _ => false,
    };
    push_check(&mut checks, "transport", json!(true), json!(transport_ok));
    push_check(
        &mut checks,
        "server.retry",
        json!(true),
        json!(cfg.server.retry.max_retries >= 1),
    );
    push_check(
        &mut checks,
        "server.circuit_breaker",
        json!(true),
        json!(
            cfg.server.circuit_breaker.enabled
                && cfg.server.circuit_breaker.failure_threshold > 0
                && cfg.server.circuit_breaker.open_secs > 0
        ),
    );

    let parsed_settings = serde_json::from_str::<Value>(settings_text).ok();
    let setting = |pointer: &str| {
        parsed_settings
            .as_ref()
            .and_then(|value| value.pointer(pointer))
            .cloned()
            .unwrap_or(Value::Null)
    };
    push_check(
        &mut checks,
        "settings.model",
        json!(model),
        setting("/model"),
    );
    push_check(
        &mut checks,
        "settings.effort",
        json!(effort),
        setting("/effortLevel"),
    );
    for (name, pointer, expected) in [
        (
            "settings.opus_model",
            "/env/ANTHROPIC_DEFAULT_OPUS_MODEL",
            model.as_str(),
        ),
        (
            "settings.sonnet_model",
            "/env/ANTHROPIC_DEFAULT_SONNET_MODEL",
            model.as_str(),
        ),
        (
            "settings.haiku_model",
            "/env/ANTHROPIC_DEFAULT_HAIKU_MODEL",
            model.as_str(),
        ),
        (
            "settings.fable_model",
            "/env/ANTHROPIC_DEFAULT_FABLE_MODEL",
            model.as_str(),
        ),
        (
            "settings.fast_model",
            "/env/ANTHROPIC_SMALL_FAST_MODEL",
            model.as_str(),
        ),
        (
            "settings.option",
            "/env/ANTHROPIC_CUSTOM_MODEL_OPTION",
            model.as_str(),
        ),
        (
            "settings.option_name",
            "/env/ANTHROPIC_CUSTOM_MODEL_OPTION_NAME",
            display_name.as_str(),
        ),
        (
            "settings.option_description",
            "/env/ANTHROPIC_CUSTOM_MODEL_OPTION_DESCRIPTION",
            description.as_str(),
        ),
        (
            "settings.discovery",
            "/env/CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY",
            "1",
        ),
    ] {
        push_check(&mut checks, name, json!(expected), setting(pointer));
    }

    let mut definition = ClaudeLaneDefinition {
        schema: LANE_RECORD_SCHEMA,
        name,
        harness: "claude-code",
        model,
        route,
        transport: match transport.as_str() {
            "gateway" => "gateway",
            "tap" => "tap",
            "headroom" => "headroom",
            _ => "invalid",
        },
        native_effort: match effort.as_str() {
            "default" => "default",
            "low" => "low",
            "medium" => "medium",
            "high" => "high",
            "max" => "max",
            "xhigh" => "xhigh",
            "ultra" => "ultra",
            _ => "invalid",
        },
        aliases,
        targets: route_targets,
        revision: String::new(),
        profile_label,
        anthropic_port,
        key_env,
        display_name,
        description,
        min_fallbacks,
    };
    let expected_revision = definition_revision(&definition).unwrap_or_default();
    let actual_revision = field("SB_LANE_REVISION");
    push_check(
        &mut checks,
        "record.revision",
        json!(expected_revision),
        json!(actual_revision),
    );
    definition.revision = expected_revision;

    let ok = checks.iter().all(|check| check.ok);
    let next_actions = if ok {
        Vec::new()
    } else {
        vec![format!(
            "Run `sb lane define {} ... --apply` from the reviewed executable tuple",
            definition.name
        )]
    };
    ClaudeLaneAuditReport {
        schema: AUDIT_SCHEMA,
        ok,
        config: config_path.display().to_string(),
        lane_record: lane_record.display().to_string(),
        settings: settings.display().to_string(),
        definition: Some(definition),
        checks,
        next_actions,
    }
}

fn missing_audit_report(
    config_path: &Path,
    lane_record: PathBuf,
    settings: PathBuf,
    problem: String,
) -> ClaudeLaneAuditReport {
    ClaudeLaneAuditReport {
        schema: AUDIT_SCHEMA,
        ok: false,
        config: config_path.display().to_string(),
        lane_record: lane_record.display().to_string(),
        settings: settings.display().to_string(),
        definition: None,
        checks: vec![AuditCheck {
            name: "materialized_files",
            ok: false,
            expected: json!("readable typed lane record and Claude settings"),
            actual: json!(problem),
        }],
        next_actions: vec!["Run `sb lane define ... --apply` with the intended tuple".to_string()],
    }
}

fn roots(lane_root: Option<PathBuf>, profile_root: Option<PathBuf>) -> (PathBuf, PathBuf) {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    (
        lane_root.unwrap_or_else(|| home.join(".config/switchback/lanes")),
        profile_root.unwrap_or_else(|| home.join(".config/switchback/claude/_providers")),
    )
}

fn normalized_aliases(route: &str, aliases: &[String]) -> anyhow::Result<Vec<String>> {
    let mut out = BTreeSet::new();
    for alias in aliases {
        validate_model_token(alias, "alias")?;
        if alias != route {
            out.insert(alias.clone());
        }
    }
    Ok(out.into_iter().collect())
}

fn validate_route_targets(
    cfg: &Config,
    route: &str,
    targets: &[String],
    min_fallbacks: usize,
) -> anyhow::Result<()> {
    if targets.is_empty() {
        anyhow::bail!("route `{route}` has no targets");
    }
    let fallback_count = targets.len().saturating_sub(1);
    if fallback_count < min_fallbacks {
        anyhow::bail!("route `{route}` has {fallback_count} fallback(s), requires {min_fallbacks}");
    }
    validate_provider_targets(cfg, targets)
}

fn validate_provider_targets(cfg: &Config, targets: &[String]) -> anyhow::Result<()> {
    let providers = cfg
        .providers
        .iter()
        .map(|provider| provider.id.as_str())
        .collect::<HashSet<_>>();
    for target in targets {
        let (provider, model) = target
            .split_once('/')
            .ok_or_else(|| anyhow::anyhow!("target `{target}` must be provider/model"))?;
        if provider.is_empty() || model.is_empty() {
            anyhow::bail!("target `{target}` must be provider/model");
        }
        if !providers.contains(provider) {
            anyhow::bail!("target `{target}` references unknown provider `{provider}`");
        }
    }
    Ok(())
}

fn validate_alias_routes(
    cfg: &Config,
    aliases: &[String],
    targets: &[String],
) -> anyhow::Result<()> {
    for alias in aliases {
        let alias_route = cfg
            .exact_route_for(alias)
            .ok_or_else(|| anyhow::anyhow!("alias route `{alias}` is not configured"))?;
        if alias_route.targets != targets {
            anyhow::bail!("alias route `{alias}` has different ordered targets");
        }
    }
    Ok(())
}

fn definition_revision(definition: &ClaudeLaneDefinition) -> anyhow::Result<String> {
    let canonical = json!({
        "schema": definition.schema,
        "name": definition.name,
        "harness": definition.harness,
        "model": definition.model,
        "route": definition.route,
        "transport": definition.transport,
        "native_effort": definition.native_effort,
        "aliases": definition.aliases,
        "targets": definition.targets,
        "profile_label": definition.profile_label,
        "anthropic_port": definition.anthropic_port,
        "key_env": definition.key_env,
        "display_name": definition.display_name,
        "description": definition.description,
        "min_fallbacks": definition.min_fallbacks,
    });
    let encoded = serde_json::to_vec(&canonical)?;
    Ok(format!("sha256:{:x}", Sha256::digest(encoded)))
}

fn render_lane_record(definition: &ClaudeLaneDefinition) -> String {
    let mut fields = vec![
        ("SB_LANE_SCHEMA", definition.schema.to_string()),
        ("SB_LANE_NAME", definition.name.clone()),
        ("SB_LANE_HARNESS", definition.harness.to_string()),
        ("SB_LANE_TRANSPORT", definition.transport.to_string()),
        ("SB_LANE_MODEL", definition.model.clone()),
        ("SB_LANE_ROUTE", definition.route.clone()),
        ("SB_LANE_KEY_ENV", definition.key_env.clone()),
        ("SB_LANE_WIRE_API", "anthropic_messages".to_string()),
        (
            "SB_LANE_CLAUDE_PROFILE_LABEL",
            definition.profile_label.clone(),
        ),
        (
            "SB_LANE_CLAUDE_EFFORT",
            definition.native_effort.to_string(),
        ),
        (
            "SB_LANE_CLAUDE_CUSTOM_MODEL_NAME",
            definition.display_name.clone(),
        ),
        (
            "SB_LANE_CLAUDE_CUSTOM_MODEL_DESCRIPTION",
            definition.description.clone(),
        ),
        ("SB_LANE_ALIASES", definition.aliases.join(" ")),
        ("SB_LANE_TARGETS", definition.targets.join(" ")),
        (
            "SB_LANE_MIN_FALLBACKS",
            definition.min_fallbacks.to_string(),
        ),
        ("SB_LANE_REVISION", definition.revision.clone()),
    ];
    if let Some(port) = definition.anthropic_port {
        fields.push(("SB_LANE_ANTHROPIC_TAP", port.to_string()));
    } else {
        fields.push(("SB_LANE_ANTHROPIC_TAP", String::new()));
    }
    fields.push((
        "SB_LANE_HEADROOM",
        if definition.transport == "headroom" {
            "1"
        } else {
            "0"
        }
        .to_string(),
    ));
    fields.push(("SB_LANE_CLAUDE_HEADROOM_BYPASS", "0".to_string()));

    let mut out = String::from("# Generated by `sb lane define`; edit through that owner.\n");
    for (key, value) in fields {
        out.push_str(key);
        out.push('=');
        out.push_str(&shell_single_quote(&value));
        out.push('\n');
    }
    out
}

fn render_settings(
    existing: Option<&str>,
    definition: &ClaudeLaneDefinition,
) -> anyhow::Result<String> {
    let mut root = match existing {
        Some(text) if !text.trim().is_empty() => serde_json::from_str::<Value>(text)
            .map_err(|error| anyhow::anyhow!("parse existing settings.json: {error}"))?,
        _ => Value::Object(Map::new()),
    };
    let object = root
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("existing settings.json top level must be an object"))?;
    object.insert("model".to_string(), json!(definition.model));
    object.insert("effortLevel".to_string(), json!(definition.native_effort));
    let env = object
        .entry("env".to_string())
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("existing settings.json env must be an object"))?;
    for key in [
        "ANTHROPIC_DEFAULT_OPUS_MODEL",
        "ANTHROPIC_DEFAULT_SONNET_MODEL",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL",
        "ANTHROPIC_DEFAULT_FABLE_MODEL",
        "ANTHROPIC_SMALL_FAST_MODEL",
        "ANTHROPIC_CUSTOM_MODEL_OPTION",
    ] {
        env.insert(key.to_string(), json!(definition.model));
    }
    env.insert(
        "ANTHROPIC_CUSTOM_MODEL_OPTION_NAME".to_string(),
        json!(definition.display_name),
    );
    env.insert(
        "ANTHROPIC_CUSTOM_MODEL_OPTION_DESCRIPTION".to_string(),
        json!(definition.description),
    );
    env.insert(
        "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY".to_string(),
        json!("1"),
    );
    let mut rendered = serde_json::to_string_pretty(&root)?;
    rendered.push('\n');
    Ok(rendered)
}

fn parse_lane_record(text: &str) -> anyhow::Result<BTreeMap<String, String>> {
    let mut fields = BTreeMap::new();
    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, raw_value) = line
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("line {} is not KEY=VALUE", index + 1))?;
        if !key.starts_with("SB_LANE_") || fields.contains_key(key) {
            anyhow::bail!("line {} has invalid or duplicate key `{key}`", index + 1);
        }
        let value = parse_shell_literal(raw_value)
            .ok_or_else(|| anyhow::anyhow!("line {} has an unsupported value", index + 1))?;
        fields.insert(key.to_string(), value);
    }
    Ok(fields)
}

fn parse_shell_literal(value: &str) -> Option<String> {
    if value.len() >= 2 && value.starts_with('\'') && value.ends_with('\'') {
        let inner = &value[1..value.len() - 1];
        return Some(inner.replace("'\\''", "'"));
    }
    if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
        return serde_json::from_str(value).ok();
    }
    (!value.chars().any(char::is_whitespace)).then(|| value.to_string())
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn split_words(value: &str) -> Vec<String> {
    value.split_whitespace().map(ToString::to_string).collect()
}

fn push_check(checks: &mut Vec<AuditCheck>, name: &'static str, expected: Value, actual: Value) {
    checks.push(AuditCheck {
        name,
        ok: expected == actual,
        expected,
        actual,
    });
}

fn validate_safe_name(value: &str, label: &str) -> anyhow::Result<()> {
    if value.is_empty()
        || !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
    {
        anyhow::bail!("{label} must contain only letters, digits, dot, underscore, or dash");
    }
    Ok(())
}

fn validate_model_token(value: &str, label: &str) -> anyhow::Result<()> {
    if value.is_empty()
        || !value.chars().all(|ch| {
            ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-' | '/' | ':' | '@' | '+')
        })
    {
        anyhow::bail!("{label} contains unsupported characters");
    }
    Ok(())
}

fn validate_env_name(value: &str) -> anyhow::Result<()> {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        anyhow::bail!("key env name is empty");
    };
    if !(first.is_ascii_alphabetic() || first == '_')
        || !chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
    {
        anyhow::bail!("key env name is invalid");
    }
    Ok(())
}

fn read_optional_text(path: &Path) -> anyhow::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(anyhow::anyhow!("read {}: {error}", path.display())),
    }
}

fn write_pair_transaction(
    first: &Path,
    first_before: Option<&str>,
    first_after: &str,
    second: &Path,
    second_before: Option<&str>,
    second_after: &str,
) -> anyhow::Result<()> {
    crate::config_cli::write_file_atomic(first, first_after)?;
    set_private_permissions(first)?;
    if let Err(error) = crate::config_cli::write_file_atomic(second, second_after)
        .and_then(|()| set_private_permissions(second))
    {
        restore_file(first, first_before)?;
        return Err(error.context("second file failed; first file rolled back"));
    }
    if let Err(error) = std::fs::read_to_string(first)
        .map_err(anyhow::Error::from)
        .and_then(|actual| {
            (actual == first_after)
                .then_some(())
                .ok_or_else(|| anyhow::anyhow!("first file verification mismatch"))
        })
        .and_then(|()| std::fs::read_to_string(second).map_err(anyhow::Error::from))
        .and_then(|actual| {
            (actual == second_after)
                .then_some(())
                .ok_or_else(|| anyhow::anyhow!("second file verification mismatch"))
        })
    {
        rollback_pair(first, first_before, second, second_before)?;
        return Err(error.context("post-write verification failed; both files rolled back"));
    }
    Ok(())
}

fn rollback_pair(
    first: &Path,
    first_before: Option<&str>,
    second: &Path,
    second_before: Option<&str>,
) -> anyhow::Result<()> {
    let first_result = restore_file(first, first_before);
    let second_result = restore_file(second, second_before);
    first_result.and(second_result)
}

fn restore_file(path: &Path, before: Option<&str>) -> anyhow::Result<()> {
    match before {
        Some(contents) => {
            crate::config_cli::write_file_atomic(path, contents)?;
            set_private_permissions(path)
        }
        None => match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(anyhow::anyhow!(
                "remove {} during rollback: {error}",
                path.display()
            )),
        },
    }
}

#[cfg(unix)]
fn set_private_permissions(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|error| anyhow::anyhow!("chmod {}: {error}", path.display()))
}

#[cfg(not(unix))]
fn set_private_permissions(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

pub(crate) fn print_claude_lane_audit_text(report: &ClaudeLaneAuditReport) {
    println!(
        "claude lane audit {}",
        if report.ok { "ok" } else { "not-ok" }
    );
    println!("lane_record {}", report.lane_record);
    println!("settings {}", report.settings);
    for check in &report.checks {
        println!(
            "{} {} expected={} actual={}",
            if check.ok { "pass" } else { "fail" },
            check.name,
            check.expected,
            check.actual
        );
    }
}

pub(crate) fn print_claude_lane_define_text(report: &ClaudeLaneDefineReport) {
    println!(
        "claude lane define {}",
        if report.ok { "ok" } else { "not-ok" }
    );
    println!("dry_run {}", report.dry_run);
    println!("changed {}", report.changed);
    println!("applied {}", report.applied);
    println!("revision {}", report.definition.revision);
    println!("route {}", report.definition.route);
    println!("targets {}", report.definition.targets.join(" -> "));
    if report.dry_run && report.changed {
        println!("next rerun with --apply to materialize both files");
    }
}
