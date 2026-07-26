use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Args, Subcommand, ValueEnum};
use sb_core::Config;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

const LANE_RECORD_SCHEMA: &str = "switchback/claude-lane@2";
const AUDIT_SCHEMA: &str = "switchback/claude-lane-audit@1";
const DEFINE_SCHEMA: &str = "switchback/claude-lane-define@1";
const LAUNCH_PROFILES_SCHEMA: &str = "switchback/launch-profiles@1";
const PROVIDER_LANE_SCHEMA: &str = "switchback/provider-lane@1";
const LAUNCH_PROFILE_RECORD_SCHEMA: &str = "switchback/launch-profile-record@1";
const PROFILE_CONFORMANCE_SCHEMA: &str = "switchback/profile-conformance@1";
const PROFILE_WRAPPER_OWNER_MARKER: &str = "# switchback-owned: launch-profile-wrapper@1";
const PROFILE_OWNED_PROVIDER_LANE_FIELDS: &[&str] = &[
    "SB_LANE_SCHEMA",
    "SB_LANE_NAME",
    "SB_LANE_MODEL",
    "SB_LANE_ROUTE",
    "SB_LANE_TRANSPORT",
    "SB_LANE_TARGETS",
    "SB_LANE_MIN_FALLBACKS",
    "SB_LANE_REVISION",
    "SB_LANE_KEY_ENV",
    "SB_LANE_VAULT_REF",
    "SB_LANE_ANTHROPIC_TAP",
    "SB_LANE_HEADROOM",
    "SB_LANE_HEADROOM_PORT",
    "SB_LANE_CLAUDE_VIA_TAP",
    "SB_LANE_CLAUDE_HEADROOM_BYPASS",
];

#[derive(Subcommand, Debug)]
pub(crate) enum LaunchProfileCmd {
    /// List validated profiles from the Switchback-owned authority.
    List(LaunchProfilePathsArgs),
    /// Show one resolved, non-secret launch profile.
    Show(LaunchProfileNameArgs),
    /// Plan all generated artifacts without writing.
    Plan(LaunchProfileNameArgs),
    /// Compare generated artifacts with the current local materialization.
    Diff(LaunchProfileNameArgs),
    /// Materialize one profile transactionally and verify conformance.
    Apply(LaunchProfileNameArgs),
    /// Audit one profile, or every profile when NAME is omitted.
    Doctor(LaunchProfileDoctorArgs),
}

#[derive(Args, Debug, Clone)]
pub(crate) struct LaunchProfilePathsArgs {
    /// Private Switchback launch-profile authority.
    #[arg(long)]
    pub(crate) authority: Option<PathBuf>,
    /// Generated provider/profile lane-record root.
    #[arg(long)]
    pub(crate) lane_root: Option<PathBuf>,
    /// Generated harness settings root.
    #[arg(long)]
    pub(crate) profile_root: Option<PathBuf>,
    /// Generated executable wrapper root.
    #[arg(long)]
    pub(crate) wrapper_root: Option<PathBuf>,
    /// Generated non-secret conformance projection root.
    #[arg(long)]
    pub(crate) projection_root: Option<PathBuf>,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct LaunchProfileNameArgs {
    /// Stable launch-profile id.
    pub(crate) name: String,
    #[command(flatten)]
    pub(crate) paths: LaunchProfilePathsArgs,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct LaunchProfileDoctorArgs {
    /// Stable launch-profile id. Omit to audit every profile.
    pub(crate) name: Option<String>,
    #[command(flatten)]
    pub(crate) paths: LaunchProfilePathsArgs,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct LaunchProfilesDocument {
    schema: String,
    provider_lanes: BTreeMap<String, ProviderLaneSpec>,
    harness_presets: BTreeMap<String, HarnessPresetSpec>,
    capture_policies: BTreeMap<String, CapturePolicySpec>,
    launch_profiles: BTreeMap<String, LaunchProfileSpec>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ProviderLaneSpec {
    route: String,
    requested_model: String,
    transport: LaneTransport,
    credential_ref: CredentialReference,
    #[serde(default)]
    anthropic_tap_port: Option<u16>,
    #[serde(default)]
    headroom_port: Option<u16>,
    #[serde(default)]
    claude_via_tap: bool,
    #[serde(default)]
    aliases: Vec<String>,
    #[serde(default = "default_min_fallbacks")]
    min_fallbacks: usize,
}

fn default_min_fallbacks() -> usize {
    1
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum CredentialReference {
    Env { name: String },
    Vault { name: String },
}

impl CredentialReference {
    fn validate(&self) -> anyhow::Result<()> {
        match self {
            Self::Env { name } => validate_env_name(name),
            Self::Vault { name } => validate_safe_name(name, "vault credential reference"),
        }
    }

    fn lane_fields(&self) -> (&'static str, &str) {
        match self {
            Self::Env { name } => ("SB_LANE_KEY_ENV", name),
            Self::Vault { name } => ("SB_LANE_VAULT_REF", name),
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum HarnessKind {
    ClaudeCode,
    Codex,
}

impl HarnessKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
        }
    }

    fn run_token(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude",
            Self::Codex => "codex",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct HarnessPresetSpec {
    harness: HarnessKind,
    native_effort: NativeEffort,
    #[serde(default)]
    model_aliases: HarnessModelAliases,
    #[serde(default)]
    compaction_window: Option<u64>,
    permissions_mode: PermissionsMode,
    mcp_mode: McpMode,
    skills_mode: SkillsMode,
    settings_mode: SettingsMode,
    #[serde(default)]
    launch_args: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct HarnessModelAliases {
    #[serde(default)]
    default: Option<String>,
    #[serde(default)]
    opus: Option<String>,
    #[serde(default)]
    sonnet: Option<String>,
    #[serde(default)]
    haiku: Option<String>,
    #[serde(default)]
    subagent: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum PermissionsMode {
    InheritAllowlisted,
    Minimal,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum McpMode {
    None,
    Selected,
    All,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum SkillsMode {
    Disabled,
    Enabled,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum SettingsMode {
    Minimal,
    InheritAllowlisted,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CapturePolicySpec {
    mode: LaunchCaptureMode,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum LaunchCaptureMode {
    SegmentedFullWire,
    MetadataOnly,
    Off,
}

impl LaunchCaptureMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::SegmentedFullWire => "segmented_full_wire",
            Self::MetadataOnly => "metadata_only",
            Self::Off => "off",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct LaunchProfileSpec {
    provider_lane: String,
    harness_preset: String,
    capture_policy: String,
    #[serde(default)]
    profile_label: Option<String>,
    #[serde(default)]
    wrappers: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct ResolvedLaunchProfile {
    id: String,
    provider_lane: String,
    harness_preset: String,
    capture_policy: String,
    profile_label: String,
    harness: &'static str,
    route: String,
    requested_model: String,
    requested_effort: &'static str,
    transport: &'static str,
    capture: ResolvedCapturePolicy,
    model_aliases: HarnessModelAliases,
    #[serde(skip_serializing_if = "Option::is_none")]
    compaction_window: Option<u64>,
    permissions_mode: PermissionsMode,
    mcp_mode: McpMode,
    skills_mode: SkillsMode,
    settings_mode: SettingsMode,
    launch_args: Vec<String>,
    wrappers: Vec<String>,
    targets: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct ResolvedCapturePolicy {
    id: String,
    mode: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, ValueEnum)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, ValueEnum)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, ValueEnum)]
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

    fn claude_code_effort(self) -> &'static str {
        match self {
            // Ultra is a Switchback lane request. Claude Code's current native
            // vocabulary tops out at `max`; writing the unsupported `ultra`
            // value into settings silently falls back to `high`.
            Self::Ultra => Self::Max.as_str(),
            _ => self.as_str(),
        }
    }
}

fn claude_code_effort(requested: &str) -> &str {
    match requested {
        "ultra" => "max",
        "default" | "low" | "medium" | "high" | "max" | "xhigh" => requested,
        _ => "invalid",
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
    /// Requested lane effort. Ultra is explicit per profile and maps to Claude Code `max`.
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
    requested_effort: &'static str,
    claude_effort: &'static str,
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
            "Claude Code via Switchback route {route}; requested effort {} maps to Claude Code {}; {} ordered fallback(s).",
            args.effort.as_str(),
            args.effort.claude_code_effort(),
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
        requested_effort: args.effort.as_str(),
        claude_effort: args.effort.claude_code_effort(),
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
    let requested_effort = field("SB_LANE_REQUESTED_EFFORT");
    let claude_effort = field("SB_LANE_CLAUDE_EFFORT");
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
        "effort.translation",
        json!(claude_code_effort(&requested_effort)),
        json!(claude_effort.as_str()),
    );
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
        json!(claude_effort.as_str()),
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
        requested_effort: match requested_effort.as_str() {
            "default" => "default",
            "low" => "low",
            "medium" => "medium",
            "high" => "high",
            "max" => "max",
            "xhigh" => "xhigh",
            "ultra" => "ultra",
            _ => "invalid",
        },
        claude_effort: match claude_effort.as_str() {
            "default" => "default",
            "low" => "low",
            "medium" => "medium",
            "high" => "high",
            "max" => "max",
            "xhigh" => "xhigh",
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
        "requested_effort": definition.requested_effort,
        "claude_effort": definition.claude_effort,
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
            "SB_LANE_REQUESTED_EFFORT",
            definition.requested_effort.to_string(),
        ),
        (
            "SB_LANE_CLAUDE_EFFORT",
            definition.claude_effort.to_string(),
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
    object.insert("effortLevel".to_string(), json!(definition.claude_effort));
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

#[derive(Debug, Clone)]
struct ResolvedProfileBundle {
    profile: ResolvedLaunchProfile,
    provider: ProviderLaneSpec,
    preset: HarnessPresetSpec,
    revision: String,
    provider_revision: String,
}

#[derive(Debug, Clone)]
struct ProfilePaths {
    authority: PathBuf,
    lane_root: PathBuf,
    profile_root: PathBuf,
    wrapper_root: PathBuf,
    projection_root: PathBuf,
}

#[derive(Debug, Clone)]
struct PlannedProfileArtifact {
    kind: &'static str,
    path: PathBuf,
    contents: String,
    mode: u32,
}

#[derive(Debug, Clone, Serialize)]
struct ProfileArtifactStatus {
    kind: &'static str,
    path: String,
    exists: bool,
    changed: bool,
    current_sha256: Option<String>,
    desired_sha256: String,
    mode: String,
    actual_mode: Option<String>,
    mode_matches: bool,
}

#[derive(Debug, Clone, Copy)]
enum ProfileDoctorScope {
    Materialized,
    Live,
}

#[derive(Debug, Clone, Serialize)]
struct ProfileAuthorityProjection {
    owner: &'static str,
    compound_role: &'static str,
    projection: &'static str,
}

fn profile_authority_projection() -> ProfileAuthorityProjection {
    ProfileAuthorityProjection {
        owner: "switchback",
        compound_role: "consumer",
        projection: "non_secret_profile_conformance",
    }
}

#[derive(Debug, Clone, Serialize)]
struct LaunchProfileSummary {
    id: String,
    harness: &'static str,
    provider_lane: String,
    route: String,
    requested_model: String,
    requested_effort: &'static str,
    capture_mode: &'static str,
    revision: String,
}

#[derive(Debug, Clone, Serialize)]
struct LaunchProfileListReport {
    schema: &'static str,
    authority: ProfileAuthorityProjection,
    authority_path: String,
    authority_revision: String,
    profiles: Vec<LaunchProfileSummary>,
}

#[derive(Debug, Clone, Serialize)]
struct LaunchProfileShowReport {
    schema: &'static str,
    authority: ProfileAuthorityProjection,
    authority_revision: String,
    revision: String,
    profile: ResolvedLaunchProfile,
}

#[derive(Debug, Clone, Serialize)]
struct LaunchProfilePlanReport {
    schema: &'static str,
    authority: ProfileAuthorityProjection,
    authority_revision: String,
    revision: String,
    profile: ResolvedLaunchProfile,
    changed: bool,
    artifacts: Vec<ProfileArtifactStatus>,
}

#[derive(Debug, Clone, Serialize)]
struct LaunchProfileDoctorReport {
    schema: &'static str,
    authority: ProfileAuthorityProjection,
    ok: bool,
    authority_revision: String,
    revision: String,
    profile: ResolvedLaunchProfile,
    checks: Vec<AuditCheck>,
    artifacts: Vec<ProfileArtifactStatus>,
}

#[derive(Debug, Clone, Serialize)]
struct LaunchProfileDoctorAllReport {
    schema: &'static str,
    authority: ProfileAuthorityProjection,
    ok: bool,
    authority_revision: String,
    profiles: Vec<LaunchProfileDoctorReport>,
}

#[derive(Debug, Clone, Serialize)]
struct LaunchProfileApplyReport {
    schema: &'static str,
    authority: ProfileAuthorityProjection,
    revision: String,
    changed: bool,
    applied: bool,
    doctor: LaunchProfileDoctorReport,
}

pub(crate) fn run_launch_profile_cmd(
    action: LaunchProfileCmd,
    config_path: &Path,
    json_output: bool,
) -> anyhow::Result<()> {
    let cfg = Config::from_path(config_path)?;
    match action {
        LaunchProfileCmd::List(args) => {
            let paths = resolve_profile_paths(&args);
            let (document, authority_revision) = load_profile_authority(&paths.authority)?;
            let mut profiles = Vec::with_capacity(document.launch_profiles.len());
            for name in document.launch_profiles.keys() {
                let bundle = resolve_launch_profile(&document, &cfg, name)?;
                profiles.push(profile_summary(&bundle));
            }
            let report = LaunchProfileListReport {
                schema: "switchback/launch-profile-list@1",
                authority: profile_authority_projection(),
                authority_path: paths.authority.display().to_string(),
                authority_revision,
                profiles,
            };
            print_profile_report(&report, json_output, || {
                println!("launch profiles {}", report.profiles.len());
                for profile in &report.profiles {
                    println!(
                        "{} {} {} {} {}",
                        profile.id,
                        profile.harness,
                        profile.route,
                        profile.requested_effort,
                        profile.capture_mode
                    );
                }
            })?;
        }
        LaunchProfileCmd::Show(args) => {
            let paths = resolve_profile_paths(&args.paths);
            let (document, authority_revision) = load_profile_authority(&paths.authority)?;
            let bundle = resolve_launch_profile(&document, &cfg, &args.name)?;
            let report = LaunchProfileShowReport {
                schema: "switchback/launch-profile@1",
                authority: profile_authority_projection(),
                authority_revision,
                revision: bundle.revision.clone(),
                profile: bundle.profile,
            };
            print_profile_report(&report, json_output, || {
                println!("profile {}", report.profile.id);
                println!("revision {}", report.revision);
                println!("harness {}", report.profile.harness);
                println!("route {}", report.profile.route);
                println!("capture {}", report.profile.capture.mode);
            })?;
        }
        LaunchProfileCmd::Plan(args) => {
            let report = launch_profile_plan(&cfg, &args, "switchback/launch-profile-plan@1")?;
            print_profile_report(&report, json_output, || {
                println!("profile {}", report.profile.id);
                println!("revision {}", report.revision);
                println!("changed {}", report.changed);
                for artifact in &report.artifacts {
                    println!(
                        "{} {} {}",
                        if artifact.changed { "change" } else { "same" },
                        artifact.kind,
                        artifact.path
                    );
                }
            })?;
        }
        LaunchProfileCmd::Diff(args) => {
            let report = launch_profile_plan(&cfg, &args, "switchback/launch-profile-diff@1")?;
            print_profile_report(&report, json_output, || {
                println!("profile {}", report.profile.id);
                println!("revision {}", report.revision);
                println!("changed {}", report.changed);
                for artifact in &report.artifacts {
                    println!(
                        "{} {} {}",
                        if artifact.changed { "change" } else { "same" },
                        artifact.kind,
                        artifact.path
                    );
                }
            })?;
        }
        LaunchProfileCmd::Apply(args) => {
            let paths = resolve_profile_paths(&args.paths);
            let (document, authority_revision) = load_profile_authority(&paths.authority)?;
            let bundle = resolve_launch_profile(&document, &cfg, &args.name)?;
            let artifacts = build_profile_artifacts(&paths, &bundle)?;
            let before = artifact_statuses(&artifacts)?;
            let artifacts_changed = before.iter().any(|artifact| artifact.changed);
            let authority_mode_changed =
                path_mode(&paths.authority)?.is_some_and(|mode| mode != 0o600);
            let changed = artifacts_changed || authority_mode_changed;
            if artifacts_changed {
                apply_profile_artifacts(&artifacts)?;
            }
            if authority_mode_changed {
                set_mode(&paths.authority, 0o600)?;
            }
            let doctor = profile_doctor_report(
                &cfg,
                &authority_revision,
                &paths.authority,
                &bundle,
                &artifacts,
                ProfileDoctorScope::Materialized,
            )?;
            if !doctor.ok {
                anyhow::bail!("post-apply profile conformance failed");
            }
            let report = LaunchProfileApplyReport {
                schema: "switchback/launch-profile-apply@1",
                authority: profile_authority_projection(),
                revision: bundle.revision,
                changed,
                applied: changed,
                doctor,
            };
            print_profile_report(&report, json_output, || {
                println!("profile apply {}", report.doctor.profile.id);
                println!("revision {}", report.revision);
                println!("changed {}", report.changed);
                println!("applied {}", report.applied);
            })?;
        }
        LaunchProfileCmd::Doctor(args) => {
            let paths = resolve_profile_paths(&args.paths);
            let (document, authority_revision) = load_profile_authority(&paths.authority)?;
            if let Some(name) = args.name {
                let bundle = resolve_launch_profile(&document, &cfg, &name)?;
                let artifacts = build_profile_artifacts(&paths, &bundle)?;
                let report = profile_doctor_report(
                    &cfg,
                    &authority_revision,
                    &paths.authority,
                    &bundle,
                    &artifacts,
                    ProfileDoctorScope::Live,
                )?;
                print_profile_report(&report, json_output, || {
                    println!(
                        "profile doctor {} {}",
                        report.profile.id,
                        if report.ok { "ok" } else { "not-ok" }
                    );
                    for check in &report.checks {
                        println!("{} {}", if check.ok { "pass" } else { "fail" }, check.name);
                    }
                })?;
                if !json_output && !report.ok {
                    std::process::exit(1);
                }
            } else {
                let mut profiles = Vec::with_capacity(document.launch_profiles.len());
                for name in document.launch_profiles.keys() {
                    let bundle = resolve_launch_profile(&document, &cfg, name)?;
                    let artifacts = build_profile_artifacts(&paths, &bundle)?;
                    profiles.push(profile_doctor_report(
                        &cfg,
                        &authority_revision,
                        &paths.authority,
                        &bundle,
                        &artifacts,
                        ProfileDoctorScope::Live,
                    )?);
                }
                let report = LaunchProfileDoctorAllReport {
                    schema: "switchback/launch-profile-doctor-all@1",
                    authority: profile_authority_projection(),
                    ok: profiles.iter().all(|profile| profile.ok),
                    authority_revision,
                    profiles,
                };
                print_profile_report(&report, json_output, || {
                    println!("profile doctor {}", if report.ok { "ok" } else { "not-ok" });
                    for profile in &report.profiles {
                        println!(
                            "{} {}",
                            profile.profile.id,
                            if profile.ok { "ok" } else { "not-ok" }
                        );
                    }
                })?;
                if !json_output && !report.ok {
                    std::process::exit(1);
                }
            }
        }
    }
    Ok(())
}

fn print_profile_report<T: Serialize>(
    report: &T,
    json_output: bool,
    print_text: impl FnOnce(),
) -> anyhow::Result<()> {
    if json_output {
        crate::print_json(report)
    } else {
        print_text();
        Ok(())
    }
}

fn resolve_profile_paths(args: &LaunchProfilePathsArgs) -> ProfilePaths {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    ProfilePaths {
        authority: args
            .authority
            .clone()
            .unwrap_or_else(|| home.join(".config/switchback/launch-profiles.json")),
        lane_root: args
            .lane_root
            .clone()
            .unwrap_or_else(|| home.join(".config/switchback/lanes")),
        profile_root: args
            .profile_root
            .clone()
            .unwrap_or_else(|| home.join(".config/switchback/claude/_providers")),
        wrapper_root: args
            .wrapper_root
            .clone()
            .unwrap_or_else(|| home.join(".local/bin")),
        projection_root: args
            .projection_root
            .clone()
            .unwrap_or_else(|| home.join(".local/state/switchback/profile-conformance")),
    }
}

fn load_profile_authority(path: &Path) -> anyhow::Result<(LaunchProfilesDocument, String)> {
    let raw = std::fs::read_to_string(path).map_err(|error| {
        anyhow::anyhow!("read launch-profile authority {}: {error}", path.display())
    })?;
    let document: LaunchProfilesDocument = serde_json::from_str(&raw).map_err(|error| {
        anyhow::anyhow!("parse launch-profile authority {}: {error}", path.display())
    })?;
    if document.schema != LAUNCH_PROFILES_SCHEMA {
        anyhow::bail!(
            "unsupported launch-profile authority schema `{}`",
            document.schema
        );
    }
    for name in document.provider_lanes.keys() {
        validate_safe_name(name, "provider lane")?;
    }
    for name in document.harness_presets.keys() {
        validate_safe_name(name, "harness preset")?;
    }
    for name in document.capture_policies.keys() {
        validate_safe_name(name, "capture policy")?;
    }
    for name in document.launch_profiles.keys() {
        validate_safe_name(name, "launch profile")?;
    }
    let canonical = serde_json::to_vec(&document)?;
    Ok((document, format!("sha256:{:x}", Sha256::digest(canonical))))
}

fn resolve_launch_profile(
    document: &LaunchProfilesDocument,
    cfg: &Config,
    name: &str,
) -> anyhow::Result<ResolvedProfileBundle> {
    validate_safe_name(name, "launch profile")?;
    let spec = document
        .launch_profiles
        .get(name)
        .ok_or_else(|| anyhow::anyhow!("unknown launch profile `{name}`"))?;
    let provider = document
        .provider_lanes
        .get(&spec.provider_lane)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "launch profile `{name}` references unknown provider lane `{}`",
                spec.provider_lane
            )
        })?
        .clone();
    let preset = document
        .harness_presets
        .get(&spec.harness_preset)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "launch profile `{name}` references unknown harness preset `{}`",
                spec.harness_preset
            )
        })?
        .clone();
    let capture = document
        .capture_policies
        .get(&spec.capture_policy)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "launch profile `{name}` references unknown capture policy `{}`",
                spec.capture_policy
            )
        })?
        .clone();
    validate_model_token(&provider.route, "provider lane route")?;
    validate_model_token(&provider.requested_model, "provider lane model")?;
    provider.credential_ref.validate()?;
    match provider.transport {
        LaneTransport::Gateway => {
            if provider.anthropic_tap_port.is_some() || provider.headroom_port.is_some() {
                anyhow::bail!(
                    "provider lane `{}` gateway transport does not accept local endpoint ports",
                    spec.provider_lane
                );
            }
        }
        LaneTransport::Tap => {
            if provider.anthropic_tap_port.is_none() {
                anyhow::bail!(
                    "provider lane `{}` tap transport requires anthropic_tap_port",
                    spec.provider_lane
                );
            }
            if provider.headroom_port.is_some() {
                anyhow::bail!(
                    "provider lane `{}` tap transport does not accept headroom_port",
                    spec.provider_lane
                );
            }
        }
        LaneTransport::Headroom => {
            if provider.headroom_port.is_none() {
                anyhow::bail!(
                    "provider lane `{}` headroom transport requires headroom_port",
                    spec.provider_lane
                );
            }
        }
    }
    if provider.claude_via_tap && provider.anthropic_tap_port.is_none() {
        anyhow::bail!(
            "provider lane `{}` claude_via_tap requires anthropic_tap_port",
            spec.provider_lane
        );
    }
    if preset.harness == HarnessKind::ClaudeCode
        && provider.transport == LaneTransport::Headroom
        && capture.mode == LaunchCaptureMode::SegmentedFullWire
        && !provider.claude_via_tap
    {
        anyhow::bail!("observed Claude Code headroom profiles must set claude_via_tap=true");
    }
    if preset.harness == HarnessKind::ClaudeCode
        && !matches!(provider.credential_ref, CredentialReference::Env { .. })
    {
        anyhow::bail!("Claude Code launch profile `{name}` requires an env credential reference");
    }
    validate_route_targets(
        cfg,
        &provider.route,
        &cfg.exact_route_for(&provider.route)
            .ok_or_else(|| anyhow::anyhow!("exact route `{}` is not configured", provider.route))?
            .targets,
        provider.min_fallbacks,
    )?;
    let route_targets = cfg
        .exact_route_for(&provider.route)
        .expect("route checked above")
        .targets
        .clone();
    validate_alias_routes(cfg, &provider.aliases, &route_targets)?;
    validate_model_aliases(&preset.model_aliases)?;
    validate_launch_args(&preset.launch_args)?;
    if preset.harness == HarnessKind::Codex {
        anyhow::bail!(
            "Codex launch-profile materialization is not yet supported; refusing to leave native_effort declarative-only"
        );
    }
    if matches!(preset.mcp_mode, McpMode::Selected) {
        anyhow::bail!("mcp_mode selected requires an explicit server selection");
    }
    let mut launch_args = preset.launch_args.clone();
    match preset.mcp_mode {
        McpMode::None => push_launch_arg(&mut launch_args, "--no-mcp"),
        McpMode::All => push_launch_arg(&mut launch_args, "--mcp-all"),
        McpMode::Selected => unreachable!("selected MCP mode rejected above"),
    }
    match preset.skills_mode {
        SkillsMode::Disabled => push_launch_arg(&mut launch_args, "--no-skills"),
        SkillsMode::Enabled => push_launch_arg(&mut launch_args, "--skills"),
    }
    let profile_label = spec
        .profile_label
        .clone()
        .unwrap_or_else(|| name.to_string());
    validate_safe_name(&profile_label, "profile label")?;
    let wrappers = if spec.wrappers.is_empty() {
        vec![name.to_string()]
    } else {
        spec.wrappers.clone()
    };
    for wrapper in &wrappers {
        validate_safe_name(wrapper, "wrapper")?;
    }

    let profile = ResolvedLaunchProfile {
        id: name.to_string(),
        provider_lane: spec.provider_lane.clone(),
        harness_preset: spec.harness_preset.clone(),
        capture_policy: spec.capture_policy.clone(),
        profile_label,
        harness: preset.harness.as_str(),
        route: provider.route.clone(),
        requested_model: provider.requested_model.clone(),
        requested_effort: preset.native_effort.as_str(),
        transport: provider.transport.as_str(),
        capture: ResolvedCapturePolicy {
            id: spec.capture_policy.clone(),
            mode: capture.mode.as_str(),
        },
        model_aliases: preset.model_aliases.clone(),
        compaction_window: preset.compaction_window,
        permissions_mode: preset.permissions_mode,
        mcp_mode: preset.mcp_mode,
        skills_mode: preset.skills_mode,
        settings_mode: preset.settings_mode,
        launch_args,
        wrappers,
        targets: route_targets,
    };
    let revision = stable_json_revision(&json!({
        "schema": LAUNCH_PROFILES_SCHEMA,
        "profile": profile,
        "credential_ref": provider.credential_ref,
    }))?;
    let provider_revision = stable_json_revision(&json!({
        "schema": PROVIDER_LANE_SCHEMA,
        "id": spec.provider_lane,
        "provider": provider,
    }))?;
    Ok(ResolvedProfileBundle {
        profile,
        provider,
        preset,
        revision,
        provider_revision,
    })
}

fn validate_model_aliases(aliases: &HarnessModelAliases) -> anyhow::Result<()> {
    for (label, value) in [
        ("default model alias", aliases.default.as_deref()),
        ("opus model alias", aliases.opus.as_deref()),
        ("sonnet model alias", aliases.sonnet.as_deref()),
        ("haiku model alias", aliases.haiku.as_deref()),
        ("subagent model alias", aliases.subagent.as_deref()),
    ] {
        if let Some(value) = value {
            if value.is_empty()
                || !value.chars().all(|ch| {
                    ch.is_ascii_alphanumeric()
                        || matches!(ch, '.' | '_' | '-' | '/' | ':' | '@' | '+' | '[' | ']')
                })
            {
                anyhow::bail!("{label} contains unsupported characters");
            }
        }
    }
    Ok(())
}

fn validate_launch_args(args: &[String]) -> anyhow::Result<()> {
    for arg in args {
        if !arg.starts_with("--")
            || !arg.chars().all(|ch| {
                ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '=' | ',' | ':' | '/' | '.')
            })
        {
            anyhow::bail!("launch arg `{arg}` is not a safe option token");
        }
    }
    Ok(())
}

fn push_launch_arg(args: &mut Vec<String>, value: &str) {
    if !args.iter().any(|arg| arg == value) {
        args.push(value.to_string());
    }
}

fn stable_json_revision(value: &Value) -> anyhow::Result<String> {
    Ok(format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(value)?)
    ))
}

fn profile_summary(bundle: &ResolvedProfileBundle) -> LaunchProfileSummary {
    LaunchProfileSummary {
        id: bundle.profile.id.clone(),
        harness: bundle.profile.harness,
        provider_lane: bundle.profile.provider_lane.clone(),
        route: bundle.profile.route.clone(),
        requested_model: bundle.profile.requested_model.clone(),
        requested_effort: bundle.profile.requested_effort,
        capture_mode: bundle.profile.capture.mode,
        revision: bundle.revision.clone(),
    }
}

fn launch_profile_plan(
    cfg: &Config,
    args: &LaunchProfileNameArgs,
    schema: &'static str,
) -> anyhow::Result<LaunchProfilePlanReport> {
    let paths = resolve_profile_paths(&args.paths);
    let (document, authority_revision) = load_profile_authority(&paths.authority)?;
    let bundle = resolve_launch_profile(&document, cfg, &args.name)?;
    let artifacts = build_profile_artifacts(&paths, &bundle)?;
    let statuses = artifact_statuses(&artifacts)?;
    let authority_mode_changed = path_mode(&paths.authority)?.is_some_and(|mode| mode != 0o600);
    Ok(LaunchProfilePlanReport {
        schema,
        authority: profile_authority_projection(),
        authority_revision,
        revision: bundle.revision,
        profile: bundle.profile,
        changed: authority_mode_changed || statuses.iter().any(|artifact| artifact.changed),
        artifacts: statuses,
    })
}

fn build_profile_artifacts(
    paths: &ProfilePaths,
    bundle: &ResolvedProfileBundle,
) -> anyhow::Result<Vec<PlannedProfileArtifact>> {
    let mut artifacts = Vec::new();
    let provider_lane_path = paths
        .lane_root
        .join(format!("{}.env", bundle.profile.provider_lane));
    let existing_provider_lane = read_optional_text(&provider_lane_path)?;
    artifacts.push(PlannedProfileArtifact {
        kind: "provider_lane_record",
        path: provider_lane_path,
        contents: render_provider_lane_record(existing_provider_lane.as_deref(), bundle),
        mode: 0o600,
    });
    artifacts.push(PlannedProfileArtifact {
        kind: "launch_profile_record",
        path: paths
            .lane_root
            .join("profiles")
            .join(format!("{}.env", bundle.profile.id)),
        contents: render_launch_profile_record(bundle),
        mode: 0o600,
    });
    if bundle.preset.harness == HarnessKind::ClaudeCode {
        let settings_path = paths
            .profile_root
            .join(&bundle.profile.profile_label)
            .join("settings.json");
        let existing = read_optional_text(&settings_path)?;
        let settings = render_launch_profile_settings(existing.as_deref(), bundle)?;
        artifacts.push(PlannedProfileArtifact {
            kind: "harness_settings",
            path: settings_path,
            contents: settings,
            mode: 0o600,
        });
    }
    for wrapper in &bundle.profile.wrappers {
        artifacts.push(PlannedProfileArtifact {
            kind: "wrapper",
            path: paths.wrapper_root.join(wrapper),
            contents: render_profile_wrapper(bundle),
            mode: 0o700,
        });
    }
    artifacts.push(PlannedProfileArtifact {
        kind: "conformance_projection",
        path: paths
            .projection_root
            .join(format!("{}.json", bundle.profile.id)),
        contents: render_profile_conformance(bundle)?,
        mode: 0o600,
    });
    Ok(artifacts)
}

fn render_provider_lane_record(existing: Option<&str>, bundle: &ResolvedProfileBundle) -> String {
    let (credential_key, credential_value) = bundle.provider.credential_ref.lane_fields();
    let mut fields = vec![
        ("SB_LANE_SCHEMA", PROVIDER_LANE_SCHEMA.to_string()),
        ("SB_LANE_NAME", bundle.profile.provider_lane.clone()),
        ("SB_LANE_MODEL", bundle.provider.requested_model.clone()),
        ("SB_LANE_ROUTE", bundle.provider.route.clone()),
        (
            "SB_LANE_TRANSPORT",
            bundle.provider.transport.as_str().to_string(),
        ),
        ("SB_LANE_TARGETS", bundle.profile.targets.join(" ")),
        (
            "SB_LANE_MIN_FALLBACKS",
            bundle.provider.min_fallbacks.to_string(),
        ),
        ("SB_LANE_REVISION", bundle.provider_revision.clone()),
        (credential_key, credential_value.to_string()),
        (
            "SB_LANE_ANTHROPIC_TAP",
            bundle
                .provider
                .anthropic_tap_port
                .map(|port| port.to_string())
                .unwrap_or_default(),
        ),
        (
            "SB_LANE_HEADROOM",
            if bundle.provider.transport == LaneTransport::Headroom {
                "1"
            } else {
                "0"
            }
            .to_string(),
        ),
        (
            "SB_LANE_HEADROOM_PORT",
            bundle
                .provider
                .headroom_port
                .map(|port| port.to_string())
                .unwrap_or_default(),
        ),
        (
            "SB_LANE_CLAUDE_VIA_TAP",
            if bundle.provider.claude_via_tap {
                "1"
            } else {
                "0"
            }
            .to_string(),
        ),
        ("SB_LANE_CLAUDE_HEADROOM_BYPASS", "0".to_string()),
    ];
    if credential_key != "SB_LANE_KEY_ENV" {
        fields.push(("SB_LANE_KEY_ENV", String::new()));
    }
    let mut rendered = render_shell_record(
        "# Generated from switchback/launch-profiles@1 provider_lanes; do not hand-edit.\n",
        fields,
    );
    let preserved = preserved_provider_lane_fields(existing);
    if !preserved.is_empty() {
        rendered.push_str("\n# Preserved compatibility fields outside provider_lanes authority.\n");
        for line in preserved.into_values() {
            rendered.push_str(&line);
            rendered.push('\n');
        }
    }
    rendered
}

fn preserved_provider_lane_fields(existing: Option<&str>) -> BTreeMap<String, String> {
    let mut preserved = BTreeMap::new();
    let Some(existing) = existing else {
        return preserved;
    };
    for raw_line in existing.lines() {
        let line = raw_line.trim();
        let Some((key, _)) = line.split_once('=') else {
            continue;
        };
        if !key.starts_with("SB_LANE_")
            || !key
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
            || PROFILE_OWNED_PROVIDER_LANE_FIELDS.contains(&key)
        {
            continue;
        }
        preserved.insert(key.to_string(), line.to_string());
    }
    preserved
}

fn render_launch_profile_record(bundle: &ResolvedProfileBundle) -> String {
    let aliases = &bundle.profile.model_aliases;
    let fields = vec![
        (
            "SB_LAUNCH_PROFILE_SCHEMA",
            LAUNCH_PROFILE_RECORD_SCHEMA.to_string(),
        ),
        ("SB_LAUNCH_PROFILE_ID", bundle.profile.id.clone()),
        (
            "SB_LAUNCH_PROFILE_REF",
            format!("switchback://launch-profiles/{}", bundle.profile.id),
        ),
        ("SB_LAUNCH_PROFILE_REVISION", bundle.revision.clone()),
        ("SB_LAUNCH_HARNESS", bundle.profile.harness.to_string()),
        (
            "SB_LAUNCH_PROVIDER_LANE",
            bundle.profile.provider_lane.clone(),
        ),
        (
            "SB_LAUNCH_REQUESTED_MODEL",
            bundle.profile.requested_model.clone(),
        ),
        (
            "SB_LAUNCH_REQUESTED_EFFORT",
            bundle.profile.requested_effort.to_string(),
        ),
        (
            "SB_LAUNCH_CAPTURE_POLICY",
            bundle.profile.capture.mode.to_string(),
        ),
        (
            "SB_LAUNCH_PROFILE_LABEL",
            bundle.profile.profile_label.clone(),
        ),
        (
            "SB_LAUNCH_MODEL_DEFAULT",
            aliases.default.clone().unwrap_or_default(),
        ),
        (
            "SB_LAUNCH_MODEL_OPUS",
            aliases.opus.clone().unwrap_or_default(),
        ),
        (
            "SB_LAUNCH_MODEL_SONNET",
            aliases.sonnet.clone().unwrap_or_default(),
        ),
        (
            "SB_LAUNCH_MODEL_HAIKU",
            aliases.haiku.clone().unwrap_or_default(),
        ),
        (
            "SB_LAUNCH_MODEL_SUBAGENT",
            aliases.subagent.clone().unwrap_or_default(),
        ),
        (
            "SB_LAUNCH_COMPACTION_WINDOW",
            bundle
                .profile
                .compaction_window
                .map(|value| value.to_string())
                .unwrap_or_default(),
        ),
    ];
    render_shell_record(
        "# Generated from switchback/launch-profiles@1 launch_profiles; do not hand-edit.\n",
        fields,
    )
}

fn render_shell_record(header: &str, fields: Vec<(&str, String)>) -> String {
    let mut out = String::from(header);
    for (key, value) in fields {
        out.push_str(key);
        out.push('=');
        out.push_str(&shell_single_quote(&value));
        out.push('\n');
    }
    out
}

fn render_launch_profile_settings(
    existing: Option<&str>,
    bundle: &ResolvedProfileBundle,
) -> anyhow::Result<String> {
    let mut root = match existing {
        Some(text) if !text.trim().is_empty() => serde_json::from_str::<Value>(text)
            .map_err(|error| anyhow::anyhow!("parse existing settings.json: {error}"))?,
        _ => Value::Object(Map::new()),
    };
    if !root.is_object() {
        anyhow::bail!("existing settings.json top level must be an object");
    }
    merge_allowlisted_user_settings(
        &mut root,
        bundle.preset.permissions_mode,
        bundle.preset.settings_mode,
    )?;
    let object = root.as_object_mut().expect("settings object checked above");
    let aliases = &bundle.profile.model_aliases;
    let default_model = aliases
        .default
        .as_deref()
        .unwrap_or(&bundle.profile.requested_model);
    object.insert("model".to_string(), json!(default_model));
    object.insert(
        "effortLevel".to_string(),
        json!(bundle.preset.native_effort.claude_code_effort()),
    );
    let env = object
        .entry("env".to_string())
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("existing settings.json env must be an object"))?;
    for (key, value) in [
        ("ANTHROPIC_DEFAULT_OPUS_MODEL", aliases.opus.as_deref()),
        ("ANTHROPIC_DEFAULT_SONNET_MODEL", aliases.sonnet.as_deref()),
        ("ANTHROPIC_DEFAULT_HAIKU_MODEL", aliases.haiku.as_deref()),
        ("CLAUDE_CODE_SUBAGENT_MODEL", aliases.subagent.as_deref()),
    ] {
        if let Some(value) = value {
            env.insert(key.to_string(), json!(value));
        } else {
            env.remove(key);
        }
    }
    if let Some(window) = bundle.profile.compaction_window {
        env.insert(
            "CLAUDE_CODE_AUTO_COMPACT_WINDOW".to_string(),
            json!(window.to_string()),
        );
    } else {
        env.remove("CLAUDE_CODE_AUTO_COMPACT_WINDOW");
    }
    env.insert(
        "ANTHROPIC_CUSTOM_MODEL_OPTION".to_string(),
        json!(default_model),
    );
    env.insert(
        "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY".to_string(),
        json!("1"),
    );
    let mut rendered = serde_json::to_string_pretty(&root)?;
    rendered.push('\n');
    Ok(rendered)
}

fn merge_allowlisted_user_settings(
    root: &mut Value,
    permissions_mode: PermissionsMode,
    settings_mode: SettingsMode,
) -> anyhow::Result<()> {
    let root_object = root
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("generated settings top level must be an object"))?;
    for key in [
        "permissions",
        "skipAutoPermissionPrompt",
        "skipDangerousModePermissionPrompt",
        "autoMode",
        "skipWorkflowUsageWarning",
        "statusLine",
    ] {
        root_object.remove(key);
    }
    if matches!(permissions_mode, PermissionsMode::Minimal)
        && matches!(settings_mode, SettingsMode::Minimal)
    {
        return Ok(());
    }

    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let path = home.join(".claude/settings.json");
    let Some(text) = read_optional_text(&path)? else {
        return Ok(());
    };
    let user: Value = serde_json::from_str(&text).map_err(|error| {
        anyhow::anyhow!("parse user Claude settings {}: {error}", path.display())
    })?;
    let Some(user_object) = user.as_object() else {
        anyhow::bail!("user Claude settings top level must be an object");
    };
    let permission_keys: &[&str] = match permissions_mode {
        PermissionsMode::InheritAllowlisted => &[
            "permissions",
            "skipAutoPermissionPrompt",
            "skipDangerousModePermissionPrompt",
        ],
        PermissionsMode::Minimal => &[],
    };
    let setting_keys: &[&str] = match settings_mode {
        SettingsMode::InheritAllowlisted => &["autoMode", "skipWorkflowUsageWarning", "statusLine"],
        SettingsMode::Minimal => &[],
    };
    for key in permission_keys.iter().chain(setting_keys.iter()) {
        if let Some(value) = user_object.get(*key) {
            root_object.insert((*key).to_string(), value.clone());
        }
    }
    Ok(())
}

fn render_profile_wrapper(bundle: &ResolvedProfileBundle) -> String {
    let mut out = format!("#!/bin/zsh\n{PROFILE_WRAPPER_OWNER_MARKER}\nset -eu\n");
    for (key, value) in [
        ("SB_LAUNCH_PROFILE_ID", bundle.profile.id.as_str()),
        ("SB_LAUNCH_PROFILE_REVISION", bundle.revision.as_str()),
        ("SB_LAUNCH_HARNESS", bundle.profile.harness),
        ("SB_LAUNCH_CAPTURE_POLICY", bundle.profile.capture.mode),
        (
            "SB_LAUNCH_PROFILE_LABEL",
            bundle.profile.profile_label.as_str(),
        ),
        ("SB_LANE_REQUESTED_EFFORT", bundle.profile.requested_effort),
    ] {
        out.push_str("export ");
        out.push_str(key);
        out.push('=');
        out.push_str(&shell_single_quote(value));
        out.push('\n');
    }
    for (key, value) in [
        (
            "SB_LANE_CLAUDE_MODEL",
            bundle.profile.model_aliases.default.as_deref(),
        ),
        (
            "SB_LANE_CLAUDE_OPUS_MODEL",
            bundle.profile.model_aliases.opus.as_deref(),
        ),
        (
            "SB_LANE_CLAUDE_SONNET_MODEL",
            bundle.profile.model_aliases.sonnet.as_deref(),
        ),
        (
            "SB_LANE_CLAUDE_HAIKU_MODEL",
            bundle.profile.model_aliases.haiku.as_deref(),
        ),
    ] {
        if let Some(value) = value {
            out.push_str("export ");
            out.push_str(key);
            out.push('=');
            out.push_str(&shell_single_quote(value));
            out.push('\n');
        }
    }
    if let Some(window) = bundle.profile.compaction_window {
        out.push_str("export SB_LANE_CLAUDE_AUTO_COMPACT_WINDOW=");
        out.push_str(&shell_single_quote(&window.to_string()));
        out.push('\n');
    }
    out.push_str("exec sb run ");
    out.push_str(bundle.preset.harness.run_token());
    out.push_str(" --with ");
    out.push_str(&bundle.profile.provider_lane);
    for arg in &bundle.profile.launch_args {
        out.push(' ');
        out.push_str(arg);
    }
    out.push_str(" \"$@\"\n");
    out
}

fn render_profile_conformance(bundle: &ResolvedProfileBundle) -> anyhow::Result<String> {
    let value = json!({
        "schema": PROFILE_CONFORMANCE_SCHEMA,
        "authority": profile_authority_projection(),
        "launch_profile_ref": format!(
            "switchback://launch-profiles/{}",
            bundle.profile.id
        ),
        "conformance_revision": bundle.revision,
        "profile": {
            "id": bundle.profile.id,
            "harness": bundle.profile.harness,
            "provider_lane": bundle.profile.provider_lane,
            "route": bundle.profile.route,
            "requested_model": bundle.profile.requested_model,
            "requested_effort": bundle.profile.requested_effort,
            "capture_policy": bundle.profile.capture,
        },
    });
    let mut rendered = serde_json::to_string_pretty(&value)?;
    rendered.push('\n');
    Ok(rendered)
}

fn artifact_statuses(
    artifacts: &[PlannedProfileArtifact],
) -> anyhow::Result<Vec<ProfileArtifactStatus>> {
    artifacts
        .iter()
        .map(|artifact| {
            let current = match std::fs::read(&artifact.path) {
                Ok(value) => Some(value),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => {
                    return Err(anyhow::anyhow!(
                        "read generated artifact {}: {error}",
                        artifact.path.display()
                    ))
                }
            };
            let desired = artifact.contents.as_bytes();
            let actual_mode = path_mode(&artifact.path)?;
            let mode_matches = actual_mode.map_or(cfg!(not(unix)), |mode| mode == artifact.mode);
            Ok(ProfileArtifactStatus {
                kind: artifact.kind,
                path: artifact.path.display().to_string(),
                exists: current.is_some(),
                changed: current.as_deref() != Some(desired) || !mode_matches,
                current_sha256: current
                    .as_deref()
                    .map(|bytes| format!("sha256:{:x}", Sha256::digest(bytes))),
                desired_sha256: format!("sha256:{:x}", Sha256::digest(desired)),
                mode: format!("{:04o}", artifact.mode),
                actual_mode: actual_mode.map(|mode| format!("{mode:04o}")),
                mode_matches,
            })
        })
        .collect()
}

fn profile_doctor_report(
    cfg: &Config,
    authority_revision: &str,
    authority_path: &Path,
    bundle: &ResolvedProfileBundle,
    artifacts: &[PlannedProfileArtifact],
    scope: ProfileDoctorScope,
) -> anyhow::Result<LaunchProfileDoctorReport> {
    let statuses = artifact_statuses(artifacts)?;
    let mut checks = Vec::new();
    push_check(
        &mut checks,
        "route.exists",
        json!(true),
        json!(cfg.exact_route_for(&bundle.profile.route).is_some()),
    );
    push_check(
        &mut checks,
        "route.targets",
        json!(bundle.profile.targets),
        json!(cfg
            .exact_route_for(&bundle.profile.route)
            .map(|route| route.targets.clone())
            .unwrap_or_default()),
    );
    push_check(
        &mut checks,
        "artifacts.current",
        json!(true),
        json!(!statuses.iter().any(|artifact| artifact.changed)),
    );
    push_check(
        &mut checks,
        "authority.mode",
        json!("0600"),
        json!(path_mode(authority_path)?.map(|mode| format!("{mode:04o}"))),
    );
    // A Headroom process fronts exactly ONE Anthropic provider: the target is
    // pinned from ANTHROPIC_TARGET_API_URL when the process starts, and the
    // `/v1/messages` handler has no per-request override — `x-headroom-base-url`
    // is honored only on OpenAI-shaped paths. So an Anthropic lane's tap must
    // forward to that lane's OWN Headroom port. Point it at a shared instance
    // and the lane's traffic reaches whichever provider that process was pinned
    // to, which surfaces as a 401 from the wrong vendor — while the listener
    // checks below stay green, because both ports really are up. Liveness can
    // never catch this; only comparing the binding can.
    if bundle.provider.transport == LaneTransport::Headroom
        && (bundle.provider.claude_via_tap || bundle.provider.transport == LaneTransport::Tap)
    {
        if let (Some(tap_port), Some(headroom_port)) = (
            bundle.provider.anthropic_tap_port,
            bundle.provider.headroom_port,
        ) {
            // Only when this config actually declares the lane's tap. Whether a
            // tap exists at all is the listener check's business below; this one
            // answers where a declared tap points.
            if let Some(tap) = cfg
                .server
                .taps
                .iter()
                .find(|tap| tap_bind_port(&tap.bind) == Some(tap_port))
            {
                push_check(
                    &mut checks,
                    "tap.headroom_binding",
                    json!(format!("http://127.0.0.1:{headroom_port}")),
                    json!(tap.upstream.trim_end_matches('/')),
                );
                // A no-op on this lane's path that reads like provider routing,
                // so it invites exactly the misbinding above. Absence is the
                // contract.
                push_check(
                    &mut checks,
                    "tap.no_openai_base_url_override",
                    json!(false),
                    json!(tap
                        .headers
                        .keys()
                        .any(|name| name.eq_ignore_ascii_case("x-headroom-base-url"))),
                );
            }
        }
    }
    if matches!(scope, ProfileDoctorScope::Live) {
        if bundle.provider.claude_via_tap || bundle.provider.transport == LaneTransport::Tap {
            let port = bundle.provider.anthropic_tap_port.ok_or_else(|| {
                anyhow::anyhow!(
                    "profile {} requires an Anthropic tap listener",
                    bundle.profile.id
                )
            })?;
            push_check(
                &mut checks,
                "listener.anthropic_tap",
                json!(true),
                json!(local_listener_ready(port)),
            );
        }
        if bundle.provider.transport == LaneTransport::Headroom {
            let port = bundle.provider.headroom_port.ok_or_else(|| {
                anyhow::anyhow!("profile {} requires a Headroom listener", bundle.profile.id)
            })?;
            push_check(
                &mut checks,
                "listener.headroom",
                json!(true),
                json!(local_listener_ready(port)),
            );
        }
        // Live ports and a correct binding still say nothing about whether the
        // provider accepts this lane's credential — the failure that actually
        // reaches a session. Assert it only when the credential is in the
        // environment: a doctor that failed because it could not find a key
        // would train everyone to ignore it, and one that passed without asking
        // the provider would be claiming something it never checked.
        if bundle.provider.claude_via_tap {
            if let (Some(port), CredentialReference::Env { name }) = (
                bundle.provider.anthropic_tap_port,
                &bundle.provider.credential_ref,
            ) {
                if let Some(credential) = std::env::var(name)
                    .ok()
                    .filter(|value| !value.trim().is_empty())
                {
                    if let Some(accepted) =
                        tap_credential_accepted(port, &bundle.profile.requested_model, &credential)
                    {
                        push_check(
                            &mut checks,
                            "preflight.provider_accepts_credential",
                            json!(true),
                            json!(accepted),
                        );
                    }
                }
            }
        }
    }
    Ok(LaunchProfileDoctorReport {
        schema: "switchback/launch-profile-doctor@1",
        authority: profile_authority_projection(),
        ok: checks.iter().all(|check| check.ok),
        authority_revision: authority_revision.to_string(),
        revision: bundle.revision.clone(),
        profile: bundle.profile.clone(),
        checks,
        artifacts: statuses,
    })
}

/// Port a tap listens on, from a `host:port` bind string.
fn tap_bind_port(bind: &str) -> Option<u16> {
    bind.rsplit_once(':')
        .and_then(|(_, port)| port.parse::<u16>().ok())
}

/// One authenticated round trip through a lane's own tap, reporting whether the
/// provider accepted the credential. Ports can listen and bindings can be right
/// while the far end still rejects every request — a wrong key, a revoked plan,
/// or a tap wired to a provider this key isn't for. Without this, the first
/// thing that discovers it is a real session.
///
/// Raw HTTP/1.1 over loopback on purpose: this runs on the doctor's synchronous
/// path, so borrowing an async client would mean standing up a runtime inside a
/// health check. `Some(true)`/`Some(false)` mean the provider answered and did
/// or did not accept us; `None` means the exchange never completed, which the
/// listener checks already describe and this must not restate as an auth verdict.
fn tap_credential_accepted(port: u16, model: &str, credential: &str) -> Option<bool> {
    use std::io::{Read as _, Write as _};

    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_millis(400)).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .ok()?;
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .ok()?;
    let body = format!(
        r#"{{"model":"{model}","max_tokens":1,"messages":[{{"role":"user","content":"ping"}}]}}"#
    );
    let request = format!(
        "POST /v1/messages HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\n\
         anthropic-version: 2023-06-01\r\nx-api-key: {credential}\r\n\
         Authorization: Bearer {credential}\r\nConnection: close\r\n\
         Content-Length: {len}\r\n\r\n{body}",
        len = body.len()
    );
    stream.write_all(request.as_bytes()).ok()?;
    let mut response = Vec::new();
    // Only the status line decides this check; a provider error body adds
    // nothing and could carry prompt content into a health report.
    let mut chunk = [0u8; 512];
    while response.len() < 512 {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => response.extend_from_slice(&chunk[..read]),
            Err(_) => break,
        }
        if response.contains(&b'\n') {
            break;
        }
    }
    let status_line = String::from_utf8_lossy(&response);
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())?;
    Some(!matches!(status, 401 | 403))
}

fn local_listener_ready(port: u16) -> bool {
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    TcpStream::connect_timeout(&address, Duration::from_millis(150)).is_ok()
}

fn apply_profile_artifacts(artifacts: &[PlannedProfileArtifact]) -> anyhow::Result<()> {
    #[derive(Clone)]
    struct Snapshot {
        path: PathBuf,
        contents: Option<Vec<u8>>,
        mode: Option<u32>,
    }

    let mut snapshots = Vec::with_capacity(artifacts.len());
    for artifact in artifacts {
        let contents = match std::fs::read(&artifact.path) {
            Ok(value) => Some(value),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(anyhow::anyhow!(
                    "read {} before profile apply: {error}",
                    artifact.path.display()
                ))
            }
        };
        if artifact.kind == "wrapper"
            && contents
                .as_deref()
                .is_some_and(|existing| !switchback_owns_wrapper(existing, &artifact.contents))
        {
            anyhow::bail!(
                "refusing to overwrite unowned wrapper {}",
                artifact.path.display()
            );
        }
        #[cfg(unix)]
        let mode = std::fs::metadata(&artifact.path).ok().map(|metadata| {
            use std::os::unix::fs::PermissionsExt;
            metadata.permissions().mode() & 0o777
        });
        #[cfg(not(unix))]
        let mode = None;
        snapshots.push(Snapshot {
            path: artifact.path.clone(),
            contents,
            mode,
        });
    }

    let apply_result = (|| -> anyhow::Result<()> {
        for artifact in artifacts {
            if let Some(parent) = artifact.path.parent() {
                create_private_parent_dirs(parent)?;
            }
            crate::config_cli::write_file_atomic(&artifact.path, &artifact.contents)?;
            set_mode(&artifact.path, artifact.mode)?;
            let actual = std::fs::read_to_string(&artifact.path)?;
            if actual != artifact.contents {
                anyhow::bail!(
                    "post-write verification mismatch for {}",
                    artifact.path.display()
                );
            }
        }
        Ok(())
    })();
    if let Err(error) = apply_result {
        for snapshot in snapshots.iter().rev() {
            match &snapshot.contents {
                Some(contents) => {
                    let text = String::from_utf8(contents.clone()).map_err(|utf8_error| {
                        anyhow::anyhow!(
                            "rollback {} is not UTF-8: {utf8_error}",
                            snapshot.path.display()
                        )
                    })?;
                    crate::config_cli::write_file_atomic(&snapshot.path, &text)?;
                    if let Some(mode) = snapshot.mode {
                        set_mode(&snapshot.path, mode)?;
                    }
                }
                None => match std::fs::remove_file(&snapshot.path) {
                    Ok(()) => {}
                    Err(remove_error) if remove_error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(remove_error) => {
                        return Err(anyhow::anyhow!(
                            "rollback remove {}: {remove_error}",
                            snapshot.path.display()
                        ))
                    }
                },
            }
        }
        return Err(error.context("profile apply failed; generated artifacts rolled back"));
    }
    Ok(())
}

fn switchback_owns_wrapper(existing: &[u8], desired: &str) -> bool {
    let Ok(existing) = std::str::from_utf8(existing) else {
        return false;
    };
    if existing
        .lines()
        .any(|line| line == PROFILE_WRAPPER_OWNER_MARKER)
    {
        return true;
    }

    let marker_line = format!("{PROFILE_WRAPPER_OWNER_MARKER}\n");
    desired.replacen(&marker_line, "", 1) == existing
}

fn create_private_parent_dirs(parent: &Path) -> anyhow::Result<()> {
    let mut missing = Vec::new();
    let mut cursor = Some(parent);
    while let Some(path) = cursor {
        if path.exists() {
            break;
        }
        missing.push(path.to_path_buf());
        cursor = path.parent();
    }

    for path in missing.iter().rev() {
        match std::fs::create_dir(path) {
            Ok(()) => set_mode(path, 0o700)?,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(anyhow::anyhow!("create {}: {error}", path.display()));
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|error| anyhow::anyhow!("chmod {}: {error}", path.display()))
}

#[cfg(unix)]
fn path_mode(path: &Path) -> anyhow::Result<Option<u32>> {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(Some(metadata.permissions().mode() & 0o777)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(anyhow::anyhow!("stat {}: {error}", path.display())),
    }
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(not(unix))]
fn path_mode(_path: &Path) -> anyhow::Result<Option<u32>> {
    Ok(None)
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
