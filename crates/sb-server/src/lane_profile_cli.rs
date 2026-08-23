use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use clap::{Args, Subcommand, ValueEnum};
use sb_core::{ApiKeyRole, ClientProfileKind, Config};
use sb_paths::RuntimePaths;
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
    "SB_LANE_OPENAI_TAP",
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
    /// OpenAI-compatible, body-capturing tap used by direct headless harnesses.
    /// Separate from the Anthropic Messages tap used by Claude Code.
    #[serde(default)]
    openai_tap_port: Option<u16>,
    #[serde(default)]
    headroom_port: Option<u16>,
    #[serde(default)]
    claude_via_tap: bool,
    #[serde(default)]
    aliases: Vec<String>,
    #[serde(default = "default_min_fallbacks")]
    min_fallbacks: usize,
    // Everything below was previously reachable only by hand-editing the lane
    // record, so the authority could not own it: the generator wrote its 15
    // fields and let the rest survive as "preserved compatibility". Preserved
    // means unvalidated and unregenerated, which is how a record ended up
    // claiming one Headroom port while the tap used another. Declare them here
    // and the authority owns them; leave one out and the legacy value is still
    // preserved, so lanes migrate one field at a time instead of all at once.
    #[serde(default)]
    wire_api: Option<LaneWireApi>,
    /// Anthropic-compatible upstream used when materializing a fresh lane or
    /// starting its lane-scoped Headroom process.
    #[serde(default)]
    anthropic_url: Option<String>,
    #[serde(default)]
    fast_model: Option<String>,
    #[serde(default)]
    codex_route: Option<String>,
    #[serde(default)]
    direct_anthropic_tap_port: Option<u16>,
    #[serde(default)]
    direct_route: Option<String>,
    /// Whether the harness asks Headroom to pass this lane through untouched.
    /// Previously hardcoded to `0` in the emitted record regardless of what the
    /// harness settings actually injected, so the two could disagree.
    #[serde(default)]
    headroom_bypass: Option<bool>,
    /// Whether Headroom may replace the harness tool set with its hosted
    /// tool-search declaration. Keep this explicit for translated provider
    /// lanes: the injected Anthropic server tool narrows Switchback routing to
    /// targets that advertise that protocol.
    #[serde(default)]
    headroom_tool_search: Option<bool>,
    /// Why this lane is configured the way it is. A record comment cannot
    /// survive regeneration — only `KEY=VALUE` lines are preserved — so
    /// operator reasoning that lives in the legacy file (for example a
    /// compaction window chosen to stay under a provider's price cliff) needs a
    /// field, or migrating the lane deletes it.
    #[serde(default)]
    notes: Option<String>,
}

/// Wire format the lane's provider speaks. The launch-profiles emitter used to
/// hardcode `anthropic_messages`, which would silently convert a chat-wire lane
/// on migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum LaneWireApi {
    AnthropicMessages,
    Chat,
    Responses,
}

impl LaneWireApi {
    fn as_str(self) -> &'static str {
        match self {
            Self::AnthropicMessages => "anthropic_messages",
            Self::Chat => "chat",
            Self::Responses => "responses",
        }
    }
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
    /// Prime-Agent owns a distinct provider artifact and launcher contract.
    PrimeAgent,
    /// Oh My Pi (OMP) is in the pi-coding-agent family, but deliberately does
    /// not inherit Prime-Agent's flags or config-root contract.
    Omp,
    /// Qwen Code is Gemini-CLI lineage and owns a QWEN_HOME settings artifact.
    QwenCode,
    /// DeepSeek Harness is the Cordis pre-1.0 product launcher.
    DeepseekHarness,
}

impl HarnessKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
            Self::PrimeAgent => "prime-agent",
            Self::Omp => "omp",
            Self::QwenCode => "qwen-code",
            Self::DeepseekHarness => "deepseek-harness",
        }
    }

    fn run_token(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude",
            Self::Codex => "codex",
            Self::PrimeAgent => "prime",
            Self::Omp => "omp",
            Self::QwenCode => "qwen",
            Self::DeepseekHarness => "dsh",
        }
    }

    fn executable(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude",
            Self::Codex => "codex",
            Self::PrimeAgent => "prime-agent",
            Self::Omp => "omp",
            Self::QwenCode => "qwen",
            Self::DeepseekHarness => "dsh",
        }
    }
    fn compound_identity(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude_code",
            Self::Codex => "codex",
            Self::PrimeAgent => "prime_agent",
            Self::Omp => "oh_my_pi",
            Self::QwenCode => "qwen_code",
            Self::DeepseekHarness => "deepseek_harness",
        }
    }

    fn is_direct_headless(self) -> bool {
        matches!(self, Self::Omp | Self::QwenCode | Self::DeepseekHarness)
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
    /// Explicit MCP server ids when `mcp_mode` is `selected`.
    #[serde(default)]
    mcp_servers: Vec<String>,
    skills_mode: SkillsMode,
    settings_mode: SettingsMode,
    #[serde(default)]
    launch_args: Vec<String>,
    /// Exact harness version this source-defined preset was verified against.
    /// Required for direct headless harnesses; older profile kinds may omit it.
    #[serde(default)]
    expected_version: Option<String>,
    /// Label and blurb Claude Code shows for this lane's model. The typed
    /// `lane define` path has always accepted these (`--display-name`,
    /// `--description`); the launch-profiles authority could not express them,
    /// so lanes carried them as preserved legacy fields instead.
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    description: Option<String>,
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
    /// Leave whatever the lane already carries. The permission region stops
    /// tracking `~/.claude/settings.json`, so a lane can stay stricter than the
    /// operator's global default without `apply` loosening it back.
    Pinned,
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
    /// Leave whatever the lane already carries — see [`PermissionsMode::Pinned`].
    Pinned,
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
    /// Optional Switchback client profile. This is the account-policy binding:
    /// the selected client profile constrains provider accounts fail-closed.
    #[serde(default)]
    client_profile: Option<String>,
    /// Client-side credential presented to the Switchback gateway/tap. This is
    /// distinct from the provider credential Switchback uses upstream.
    #[serde(default)]
    client_credential_ref: Option<CredentialReference>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    client_profile: Option<String>,
    /// Canonical Compound harness identity consumed by list/conformance clients.
    harness: &'static str,
    /// Raw Switchback renderer kind retained for launch and diagnostics.
    harness_kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_credential_env: Option<String>,
    expected_executable: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    expected_version: Option<String>,
    route: String,
    /// Model token the harness actually places on the request wire.
    request_model: String,
    requested_model: String,
    requested_effort: &'static str,
    transport: &'static str,
    capture: ResolvedCapturePolicy,
    #[serde(skip_serializing_if = "Option::is_none")]
    capture_endpoint: Option<String>,
    headless: bool,
    workspace_mode: &'static str,
    output_mode: &'static str,
    permission_posture: &'static str,
    model_aliases: HarnessModelAliases,
    #[serde(skip_serializing_if = "Option::is_none")]
    compaction_window: Option<u64>,
    permissions_mode: PermissionsMode,
    mcp_mode: McpMode,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    mcp_servers: Vec<String>,
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
    /// Launch-profile authority, consulted only to name the owner of a lane
    /// this command does not own.
    #[arg(long)]
    pub(crate) authority: Option<PathBuf>,
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
    /// Why `ok` is what it is. `audited` means this command actually checked the
    /// lane; `delegated` means the lane belongs to the launch-profile authority,
    /// so this command has no verdict to give and `next_actions` names the owner
    /// that does. `ok` stays false when delegated — reporting green for a lane
    /// nothing verified would be a false all-clear.
    status: LaneAuditStatus,
    config: String,
    lane_record: String,
    settings: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    definition: Option<ClaudeLaneDefinition>,
    checks: Vec<AuditCheck>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    next_actions: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum LaneAuditStatus {
    Audited,
    Delegated,
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
        // A lane record carrying the current provider-lane shape belongs to the
        // launch-profile authority. Rewriting it here would silently downgrade a
        // healthy lane to the retired claude-lane shape and drop the fields that
        // only the profile owner writes.
        if existing_record_schema(record_before.as_deref()).as_deref() == Some(PROVIDER_LANE_SCHEMA)
        {
            anyhow::bail!(
                "lane `{}` is owned by the launch-profile authority (record schema {PROVIDER_LANE_SCHEMA}); \
                 `sb lane define --apply` writes the retired {LANE_RECORD_SCHEMA} shape and would replace it. \
                 Use `sb profile apply <profile>` instead.",
                definition.name
            );
        }
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
                authority: None,
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
    // Two generations of owner write lane records. This command audits the
    // retired claude-lane shape; the current provider-lane shape is materialized
    // by the launch-profile authority, which also owns the profile label, the
    // harness, and the requested effort. Auditing one with the other reports
    // failures that describe the schema gap rather than the lane.
    if fields.get("SB_LANE_SCHEMA").map(String::as_str) == Some(PROVIDER_LANE_SCHEMA) {
        let authority = args
            .authority
            .clone()
            .unwrap_or_else(|| RuntimePaths::from_env().launch_profiles_file());
        return Ok(foreign_owner_audit_report(
            config_path,
            lane_record,
            &authority,
            &profile_root,
            &args.name,
        ));
    }

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
        status: LaneAuditStatus::Audited,
        config: config_path.display().to_string(),
        lane_record: lane_record.display().to_string(),
        settings: settings.display().to_string(),
        definition: Some(definition),
        checks,
        next_actions,
    }
}

fn existing_record_schema(record: Option<&str>) -> Option<String> {
    parse_lane_record(record?)
        .ok()?
        .get("SB_LANE_SCHEMA")
        .cloned()
}

/// Launch profiles that materialize `lane`, as `(profile_id, profile_label)`.
/// Read leniently: this is used to name the right owner in an error path, so a
/// malformed or absent authority degrades to a generic pointer, never a panic.
fn launch_profiles_for_lane(authority: &Path, lane: &str) -> Vec<(String, String)> {
    let Ok(raw) = std::fs::read_to_string(authority) else {
        return Vec::new();
    };
    let Ok(document) = serde_json::from_str::<Value>(&raw) else {
        return Vec::new();
    };
    let Some(profiles) = document.get("launch_profiles").and_then(Value::as_object) else {
        return Vec::new();
    };
    profiles
        .iter()
        .filter(|(_, spec)| spec.get("provider_lane").and_then(Value::as_str) == Some(lane))
        .map(|(id, spec)| {
            let label = spec
                .get("profile_label")
                .and_then(Value::as_str)
                .unwrap_or(id.as_str())
                .to_string();
            (id.clone(), label)
        })
        .collect()
}

/// The lane exists and may be perfectly healthy — this command simply is not its
/// auditor. Report the ownership gap and point at the owner, never at
/// `sb lane define --apply`, which would replace the record it cannot read.
fn foreign_owner_audit_report(
    config_path: &Path,
    lane_record: PathBuf,
    authority: &Path,
    profile_root: &Path,
    lane: &str,
) -> ClaudeLaneAuditReport {
    let profiles = launch_profiles_for_lane(authority, lane);
    let settings = profile_root
        .join(
            profiles
                .first()
                .map(|(_, label)| label.as_str())
                .unwrap_or(lane),
        )
        .join("settings.json");
    let next_actions = if profiles.is_empty() {
        vec![format!(
            "Audit this lane through its owner, the launch-profile authority at {}: `sb profile doctor`",
            authority.display()
        )]
    } else {
        profiles
            .iter()
            .map(|(id, _)| format!("sb profile doctor {id}"))
            .collect()
    };
    ClaudeLaneAuditReport {
        schema: AUDIT_SCHEMA,
        ok: false,
        status: LaneAuditStatus::Delegated,
        config: config_path.display().to_string(),
        lane_record: lane_record.display().to_string(),
        settings: settings.display().to_string(),
        definition: None,
        checks: vec![AuditCheck {
            name: "record.owner",
            ok: false,
            expected: json!(LANE_RECORD_SCHEMA),
            actual: json!(format!(
                "{PROVIDER_LANE_SCHEMA} (owned by the launch-profile authority, not `sb lane define`)"
            )),
        }],
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
        status: LaneAuditStatus::Audited,
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
    let paths = RuntimePaths::from_env();
    (
        lane_root.unwrap_or_else(|| paths.lanes_root()),
        profile_root.unwrap_or_else(|| paths.claude_profiles_root().join("_providers")),
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

fn validate_http_endpoint(value: &str, label: &str) -> anyhow::Result<()> {
    if !(value.starts_with("http://") || value.starts_with("https://"))
        || value
            .chars()
            .any(|ch| ch.is_whitespace() || ch.is_control())
    {
        anyhow::bail!("{label} must be an http(s) URL without whitespace");
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
    client_credential_ref: Option<CredentialReference>,
    revision: String,
    provider_revision: String,
}

#[derive(Debug, Clone)]
struct ProfilePaths {
    authority: PathBuf,
    lane_root: PathBuf,
    profile_root: PathBuf,
    /// Generated prime-agent provider-artifact root. Mirrors the layout
    /// `claude_profiles_root()` uses for Claude Code: `<config_root>/prime`
    /// holds the `_providers/<label>/models.json` files `prime-agent`
    /// launches are pointed at via `PRIME_AGENT_CODING_AGENT_DIR`.
    prime_profiles_root: PathBuf,
    /// Switchback-owned isolated OMP agent roots. Each profile gets an
    /// `agent/models.yml`; the wrapper points `PI_CODING_AGENT_DIR` at it.
    omp_profiles_root: PathBuf,
    /// Switchback-owned isolated Qwen homes. Each profile gets a complete
    /// `settings.json`; the wrapper points `QWEN_HOME` at it.
    qwen_profiles_root: PathBuf,
    /// Isolated DSH homes; DSH initializes its shipped headless profile here on
    /// first real launch, outside the user's ambient DSH state.
    dsh_profiles_root: PathBuf,
    wrapper_root: PathBuf,
    projection_root: PathBuf,
}

/// How conformance decides an artifact has drifted.
///
/// Hashing raw bytes is right for records Switchback writes end-to-end, and
/// wrong for a document it only partly owns: a harness settings file is also
/// written by Claude Code and mirrors keys from the operator's global settings,
/// so byte equality reports drift that no `apply` can durably fix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ArtifactComparison {
    /// Whole file, byte for byte — shell records and wrappers.
    Bytes,
    /// Whole document, key order and whitespace normalized.
    CanonicalJson,
    /// Only the Switchback-owned keys, canonicalized.
    OwnedJsonRegion,
}

#[derive(Debug, Clone)]
struct PlannedProfileArtifact {
    kind: &'static str,
    path: PathBuf,
    contents: String,
    mode: u32,
    comparison: ArtifactComparison,
}

/// Recursively sort object keys so two documents that differ only in key order
/// or whitespace hash identically. Done explicitly rather than relying on
/// `serde_json`'s map backing, which flips to insertion order if any crate in
/// the graph turns on `preserve_order`.
fn canonicalize_json(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let sorted: std::collections::BTreeMap<String, Value> = map
                .iter()
                .map(|(key, nested)| (key.clone(), canonicalize_json(nested)))
                .collect();
            Value::Object(sorted.into_iter().collect())
        }
        Value::Array(items) => Value::Array(items.iter().map(canonicalize_json).collect()),
        other => other.clone(),
    }
}

/// Project the Switchback-owned region out of a harness settings document.
/// Absent keys are simply absent, so a lane that legitimately has no subagent
/// alias compares equal to a desired document that also omits it.
fn owned_settings_region(document: &Value) -> Value {
    let mut owned = Map::new();
    let Some(object) = document.as_object() else {
        return Value::Object(owned);
    };
    for key in OWNED_SETTINGS_ROOT_KEYS {
        if let Some(value) = object.get(*key) {
            owned.insert((*key).to_string(), canonicalize_json(value));
        }
    }
    if let Some(env) = object.get("env").and_then(Value::as_object) {
        let mut owned_env = Map::new();
        for key in OWNED_SETTINGS_ENV_KEYS {
            if let Some(value) = env.get(*key) {
                owned_env.insert((*key).to_string(), canonicalize_json(value));
            }
        }
        if !owned_env.is_empty() {
            owned.insert("env".to_string(), Value::Object(owned_env));
        }
    }
    Value::Object(owned)
}

/// Bytes an artifact is compared and hashed on, under its comparison policy.
/// Unparseable JSON falls back to raw bytes: a hand-mangled file should surface
/// as drift, not as a hard error that blocks the whole report.
fn comparable_bytes(contents: &[u8], comparison: ArtifactComparison) -> Vec<u8> {
    match comparison {
        ArtifactComparison::Bytes => contents.to_vec(),
        ArtifactComparison::CanonicalJson | ArtifactComparison::OwnedJsonRegion => {
            let Ok(parsed) = serde_json::from_slice::<Value>(contents) else {
                return contents.to_vec();
            };
            let projected = if comparison == ArtifactComparison::OwnedJsonRegion {
                owned_settings_region(&parsed)
            } else {
                canonicalize_json(&parsed)
            };
            serde_json::to_vec(&projected).unwrap_or_else(|_| contents.to_vec())
        }
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
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
    /// Which rule decided `changed`. Additive for consumers that predate it.
    comparison: ArtifactComparison,
    /// Hashes over the compared region only. Equal to the whole-file hashes
    /// when `comparison` is `bytes`.
    compared_current_sha256: Option<String>,
    compared_desired_sha256: String,
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
    harness_kind: &'static str,
    provider_lane: String,
    route: String,
    requested_model: String,
    request_model: String,
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
    /// Live provenance for the mirrored region. Reported, never asserted — the
    /// source is outside this authority, so divergence is information, not a
    /// conformance failure.
    derived_settings: DerivedSettingsStatus,
}

#[derive(Debug, Clone, Serialize)]
struct DerivedSettingsStatus {
    applicable: bool,
    source: String,
    source_present: bool,
    source_sha256: Option<String>,
    permissions_mode: PermissionsMode,
    settings_mode: SettingsMode,
    permission_keys: &'static [&'static str],
    setting_keys: &'static [&'static str],
}

fn derived_settings_status(bundle: &ResolvedProfileBundle) -> DerivedSettingsStatus {
    if bundle.preset.harness != HarnessKind::ClaudeCode {
        return DerivedSettingsStatus {
            applicable: false,
            source: String::new(),
            source_present: false,
            source_sha256: None,
            permissions_mode: bundle.preset.permissions_mode,
            settings_mode: bundle.preset.settings_mode,
            permission_keys: &[],
            setting_keys: &[],
        };
    }
    let path = user_claude_settings_path();
    let source = std::fs::read(&path).ok();
    DerivedSettingsStatus {
        applicable: true,
        source: path.display().to_string(),
        source_present: source.is_some(),
        source_sha256: source.as_deref().map(sha256_hex),
        permissions_mode: bundle.preset.permissions_mode,
        settings_mode: bundle.preset.settings_mode,
        permission_keys: derived_permission_keys(bundle.preset.permissions_mode),
        setting_keys: derived_setting_keys(bundle.preset.settings_mode),
    }
}

#[derive(Debug, Clone, Serialize)]
struct LaunchProfileDoctorAllReport {
    schema: &'static str,
    authority: ProfileAuthorityProjection,
    ok: bool,
    authority_revision: String,
    profiles: Vec<LaunchProfileDoctorReport>,
    // A profile that cannot even RESOLVE has no bundle, so it cannot produce a
    // check list -- but it must still appear. `doctor` over all profiles used to
    // propagate the first resolve error and abandon the loop, so one broken lane
    // hid every lane after it: the operator fixed one, re-ran, discovered the
    // next, and paid a round trip per fault to learn what the command already
    // knew. Surveying everything is the whole point of the no-argument form.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    unresolvable: Vec<LaunchProfileResolveFailure>,
}

#[derive(Debug, Clone, Serialize)]
struct LaunchProfileResolveFailure {
    id: String,
    error: String,
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
                let mut unresolvable = Vec::new();
                for name in document.launch_profiles.keys() {
                    // Record the fault and keep surveying. A resolve failure is a
                    // finding about ONE profile, not a reason to stop inspecting
                    // the others -- the targeted `doctor <name>` form below still
                    // returns the error directly, which is where a caller asking
                    // about a single profile wants it.
                    let bundle = match resolve_launch_profile(&document, &cfg, name) {
                        Ok(bundle) => bundle,
                        Err(err) => {
                            unresolvable.push(LaunchProfileResolveFailure {
                                id: name.clone(),
                                error: err.to_string(),
                            });
                            continue;
                        }
                    };
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
                    ok: profiles.iter().all(|profile| profile.ok) && unresolvable.is_empty(),
                    authority_revision,
                    profiles,
                    unresolvable,
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
                    for failure in &report.unresolvable {
                        println!("{} unresolvable: {}", failure.id, failure.error);
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
    let paths = RuntimePaths::from_env();
    ProfilePaths {
        authority: args
            .authority
            .clone()
            .unwrap_or_else(|| paths.launch_profiles_file()),
        lane_root: args.lane_root.clone().unwrap_or_else(|| paths.lanes_root()),
        profile_root: args
            .profile_root
            .clone()
            .unwrap_or_else(|| paths.claude_profiles_root().join("_providers")),
        prime_profiles_root: args
            .profile_root
            .clone()
            .map(|root| {
                root.parent()
                    .map(|p| p.join("prime/_providers"))
                    .unwrap_or_else(|| paths.config_root().join("prime/_providers"))
            })
            .unwrap_or_else(|| paths.config_root().join("prime/_providers")),
        omp_profiles_root: args
            .profile_root
            .clone()
            .and_then(|root| root.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| paths.config_root())
            .join("omp/profiles"),
        qwen_profiles_root: args
            .profile_root
            .clone()
            .and_then(|root| root.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| paths.config_root())
            .join("qwen/profiles"),
        dsh_profiles_root: args
            .profile_root
            .clone()
            .and_then(|root| root.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| paths.config_root())
            .join("dsh/profiles"),
        wrapper_root: args
            .wrapper_root
            .clone()
            .unwrap_or_else(|| home.join(".local/bin")),
        projection_root: args
            .projection_root
            .clone()
            .unwrap_or_else(|| paths.profile_projection_root()),
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
    if let Some(endpoint) = provider.anthropic_url.as_deref() {
        validate_http_endpoint(endpoint, "provider lane anthropic_url")?;
    }
    if matches!(
        preset.harness,
        HarnessKind::PrimeAgent
            | HarnessKind::Omp
            | HarnessKind::QwenCode
            | HarnessKind::DeepseekHarness
    ) && spec.client_profile.is_some()
    {
        anyhow::bail!(
            "launch profile `{name}` {} lanes must not declare client_profile; \
             this harness has no compatible Switchback client-profile kind",
            preset.harness.as_str()
        );
    }
    if preset.harness == HarnessKind::ClaudeCode && preset.model_aliases.subagent.is_none() {
        anyhow::bail!(
            "launch profile `{name}` requires an explicit subagent model alias for Claude Code"
        );
    }
    if let Some(client_profile_id) = spec.client_profile.as_deref() {
        validate_safe_name(client_profile_id, "client profile")?;
        let client_profile = cfg
            .client_profiles
            .iter()
            .find(|profile| profile.id == client_profile_id)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "launch profile `{name}` references unknown client profile `{client_profile_id}`"
                )
            })?;
        if !client_profile.enabled {
            anyhow::bail!(
                "launch profile `{name}` references disabled client profile `{client_profile_id}`"
            );
        }
        let expected_kind = match preset.harness {
            HarnessKind::ClaudeCode => ClientProfileKind::ClaudeCode,
            HarnessKind::Codex => ClientProfileKind::Codex,
            HarnessKind::PrimeAgent
            | HarnessKind::Omp
            | HarnessKind::QwenCode
            | HarnessKind::DeepseekHarness => {
                unreachable!("non-client-profile harness fence already bailed above")
            }
        };
        if client_profile.kind != expected_kind {
            anyhow::bail!(
                "launch profile `{name}` client profile `{client_profile_id}` is not compatible with harness `{}`",
                preset.harness.as_str()
            );
        }
        if preset.harness == HarnessKind::ClaudeCode && !client_profile.models.is_empty() {
            for (alias_name, alias_model) in [
                ("default", preset.model_aliases.default.as_deref()),
                ("opus", preset.model_aliases.opus.as_deref()),
                ("sonnet", preset.model_aliases.sonnet.as_deref()),
                ("haiku", preset.model_aliases.haiku.as_deref()),
                ("subagent", preset.model_aliases.subagent.as_deref()),
            ] {
                let alias_model = alias_model.ok_or_else(|| {
                    anyhow::anyhow!(
                        "launch profile `{name}` with client profile `{client_profile_id}` requires an explicit {alias_name} model alias"
                    )
                })?;
                if !client_profile
                    .models
                    .iter()
                    .any(|model| model == alias_model)
                {
                    anyhow::bail!(
                        "launch profile `{name}` {alias_name} model alias `{alias_model}` is denied by client profile `{client_profile_id}`"
                    );
                }
            }
        }
        if !client_profile.models.is_empty()
            && !client_profile
                .models
                .iter()
                .any(|model| model == &provider.route)
        {
            anyhow::bail!(
                "launch profile `{name}` route `{}` is denied by client profile `{client_profile_id}`",
                provider.route
            );
        }
    }
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
    if provider.transport != LaneTransport::Headroom && provider.headroom_tool_search.is_some() {
        anyhow::bail!(
            "provider lane `{}` headroom_tool_search requires headroom transport",
            spec.provider_lane
        );
    }
    if provider.claude_via_tap && provider.anthropic_tap_port.is_none() {
        anyhow::bail!(
            "provider lane `{}` claude_via_tap requires anthropic_tap_port",
            spec.provider_lane
        );
    }
    // There is deliberately NO rule here forcing `claude_via_tap` onto headroom
    // transport. One stood here and was wrong: it reasoned that Claude Code
    // authenticates with `x-api-key`, which the gateway rejects with 401, so a
    // tap-transport lane could never serve Claude Code. The premise is false.
    // Claude Code sends `x-api-key` only under `ANTHROPIC_API_KEY`; this binary
    // launches it with `ANTHROPIC_AUTH_TOKEN` (see `native_cli.rs`,
    // `ClientProfileKind::ClaudeCode`), and that env var makes it send
    // `Authorization: Bearer` — exactly what the gateway honours.
    //
    // Measured 2026-07-27 against the live gateway, same key and body, header form
    // the only variable: `x-api-key` -> 401, `Authorization: Bearer` -> 200 with a
    // real completion, on both `gpt56-sol-wpcom` (tap :18781) and a Headroom chain.
    // The rule therefore failed closed on a lane in daily use and, because it bailed
    // at resolve time, took `profile list` and `profile doctor` down with it — a
    // guard that hid the fleet to protect it from a 401 that never happens.
    //
    // The auth form is a property of the LAUNCH CONFIG, not of the harness. Any
    // future rule here must read the env the launcher actually sets; a probe with
    // hand-written curl headers proves nothing about it.
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
    validate_direct_headless_preset(
        name,
        &provider,
        &preset,
        capture.mode,
        spec.client_credential_ref.as_ref(),
    )?;
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
    if matches!(preset.mcp_mode, McpMode::Selected) && preset.mcp_servers.is_empty() {
        anyhow::bail!("mcp_mode selected requires an explicit server selection");
    }
    if !matches!(preset.mcp_mode, McpMode::Selected) && !preset.mcp_servers.is_empty() {
        anyhow::bail!("mcp_servers is only valid when mcp_mode is selected");
    }
    let mut mcp_server_names = BTreeSet::new();
    for server in &preset.mcp_servers {
        validate_safe_name(server, "MCP server")?;
        if !mcp_server_names.insert(server) {
            anyhow::bail!("MCP server `{server}` is selected more than once");
        }
    }
    let mut launch_args = preset.launch_args.clone();
    if !preset.harness.is_direct_headless() {
        match preset.mcp_mode {
            McpMode::None => push_launch_arg(&mut launch_args, "--no-mcp"),
            McpMode::All => push_launch_arg(&mut launch_args, "--mcp-all"),
            McpMode::Selected => push_launch_arg(
                &mut launch_args,
                &format!("--mcp={}", preset.mcp_servers.join(",")),
            ),
        }
        match preset.skills_mode {
            SkillsMode::Disabled => push_launch_arg(&mut launch_args, "--no-skills"),
            SkillsMode::Enabled => push_launch_arg(&mut launch_args, "--skills"),
        }
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
        client_profile: spec.client_profile.clone(),
        harness: preset.harness.compound_identity(),
        harness_kind: preset.harness.as_str(),
        client_credential_env: spec
            .client_credential_ref
            .as_ref()
            .map(|reference| reference.lane_fields().1.to_string()),
        expected_executable: preset.harness.executable(),
        expected_version: preset.expected_version.clone(),
        route: provider.route.clone(),
        requested_model: provider.requested_model.clone(),
        requested_effort: preset.native_effort.as_str(),
        transport: provider.transport.as_str(),
        request_model: match preset.harness {
            HarnessKind::Omp | HarnessKind::QwenCode => provider.route.clone(),
            _ => provider.requested_model.clone(),
        },
        capture: ResolvedCapturePolicy {
            id: spec.capture_policy.clone(),
            mode: capture.mode.as_str(),
        },
        capture_endpoint: direct_capture_endpoint(&provider, &preset, capture.mode)?,
        headless: preset.harness.is_direct_headless(),
        workspace_mode: harness_workspace_mode(preset.harness),
        output_mode: harness_output_mode(preset.harness),
        permission_posture: harness_permission_posture(preset.harness),
        model_aliases: preset.model_aliases.clone(),
        compaction_window: preset.compaction_window,
        permissions_mode: preset.permissions_mode,
        mcp_mode: preset.mcp_mode,
        mcp_servers: preset.mcp_servers.clone(),
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
        "client_credential_ref": spec.client_credential_ref,
    }))?;
    let provider_revision = stable_json_revision(&json!({
        "schema": PROVIDER_LANE_SCHEMA,
        "id": spec.provider_lane,
        "provider": provider,
    }))?;
    Ok(ResolvedProfileBundle {
        profile,
        provider,
        client_credential_ref: spec.client_credential_ref.clone(),
        preset,
        revision,
        provider_revision,
    })
}

fn validate_direct_headless_preset(
    name: &str,
    provider: &ProviderLaneSpec,
    preset: &HarnessPresetSpec,
    capture_mode: LaunchCaptureMode,
    client_credential_ref: Option<&CredentialReference>,
) -> anyhow::Result<()> {
    if !preset.harness.is_direct_headless() {
        if client_credential_ref.is_some() {
            anyhow::bail!(
                "launch profile `{name}` client_credential_ref is only valid for direct headless harnesses"
            );
        }
        return Ok(());
    }
    let client_credential_ref = client_credential_ref.ok_or_else(|| {
        anyhow::anyhow!(
            "launch profile `{name}` {} requires client_credential_ref for the \
             Switchback gateway; provider credentials are upstream-only",
            preset.harness.as_str()
        )
    })?;
    client_credential_ref.validate()?;
    if !matches!(client_credential_ref, CredentialReference::Env { .. }) {
        anyhow::bail!(
            "launch profile `{name}` {} requires an env client_credential_ref; \
             gateway vault-to-process resolution is not implemented",
            preset.harness.as_str()
        );
    }
    if client_credential_ref.lane_fields() == provider.credential_ref.lane_fields() {
        anyhow::bail!(
            "launch profile `{name}` {} client_credential_ref must be distinct \
             from the provider credential; use a Switchback gateway client key",
            preset.harness.as_str()
        );
    }
    let version = preset.expected_version.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "launch profile `{name}` {} preset requires expected_version",
            preset.harness.as_str()
        )
    })?;
    if version.is_empty()
        || !version
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '+'))
    {
        anyhow::bail!("launch profile `{name}` expected_version is not a safe version token");
    }
    if capture_mode != LaunchCaptureMode::SegmentedFullWire {
        anyhow::bail!(
            "launch profile `{name}` {} requires segmented_full_wire capture; \
             direct or metadata-only endpoints are not rendered",
            preset.harness.as_str()
        );
    }
    let tap_port = provider.openai_tap_port.ok_or_else(|| {
        anyhow::anyhow!(
            "launch profile `{name}` {} requires a Switchback-owned tap endpoint",
            preset.harness.as_str()
        )
    })?;
    if tap_port == 18765 {
        anyhow::bail!(
            "launch profile `{name}` refuses bare gateway :18765; \
             configure a body-capturing Switchback tap"
        );
    }
    if !preset.launch_args.is_empty() {
        anyhow::bail!(
            "launch profile `{name}` {} does not accept launch_args; \
             its renderer owns the complete supported headless flag contract",
            preset.harness.as_str()
        );
    }
    if !matches!(preset.mcp_mode, McpMode::None) || !preset.mcp_servers.is_empty() {
        anyhow::bail!(
            "launch profile `{name}` {} MCP is not adopted; keep mcp_mode=none \
             until a Switchback-owned harness artifact carries the server definitions",
            preset.harness.as_str()
        );
    }
    if !matches!(preset.skills_mode, SkillsMode::Disabled) {
        anyhow::bail!(
            "launch profile `{name}` {} skills are not installed by this renderer; \
             use skills_mode=disabled",
            preset.harness.as_str()
        );
    }
    if !matches!(preset.settings_mode, SettingsMode::Minimal)
        || !matches!(preset.permissions_mode, PermissionsMode::Minimal)
    {
        anyhow::bail!(
            "launch profile `{name}` {} refuses inherited or pinned posture; \
             use settings_mode=minimal and permissions_mode=minimal",
            preset.harness.as_str()
        );
    }
    if preset.model_aliases.default.is_some()
        || preset.model_aliases.opus.is_some()
        || preset.model_aliases.sonnet.is_some()
        || preset.model_aliases.haiku.is_some()
        || preset.model_aliases.subagent.is_some()
        || preset.compaction_window.is_some()
    {
        anyhow::bail!(
            "launch profile `{name}` {} does not support Claude model aliases or compaction_window",
            preset.harness.as_str()
        );
    }
    match preset.harness {
        HarnessKind::Omp if preset.native_effort == NativeEffort::Ultra => anyhow::bail!(
            "launch profile `{name}` OMP does not accept Switchback effort `ultra`; \
             use OMP's supported max tier or lower"
        ),
        HarnessKind::QwenCode | HarnessKind::DeepseekHarness
            if preset.native_effort != NativeEffort::Default =>
        {
            anyhow::bail!(
                "launch profile `{name}` {} has no verified CLI effort flag; \
                 native_effort must remain default",
                preset.harness.as_str()
            )
        }
        HarnessKind::DeepseekHarness if provider.requested_model != "deepseek-v4-flash" => {
            anyhow::bail!(
                "launch profile `{name}` DeepSeek Harness headless profile is pinned to \
                 `deepseek-v4-flash`; DSH has no model flag, so requested_model `{}` is unsupported",
                provider.requested_model
            )
        }
        HarnessKind::DeepseekHarness if provider.route != provider.requested_model => {
            anyhow::bail!(
                "launch profile `{name}` DeepSeek Harness sends request model `{}` and \
                 cannot exact-match route `{}`; declare the route as `deepseek-v4-flash`",
                provider.requested_model,
                provider.route
            )
        }
        _ => {}
    }
    Ok(())
}

fn direct_capture_endpoint(
    provider: &ProviderLaneSpec,
    preset: &HarnessPresetSpec,
    capture_mode: LaunchCaptureMode,
) -> anyhow::Result<Option<String>> {
    if !preset.harness.is_direct_headless() {
        return Ok(None);
    }
    if capture_mode != LaunchCaptureMode::SegmentedFullWire {
        anyhow::bail!("direct headless harness resolved without full-wire capture");
    }
    let port = provider
        .openai_tap_port
        .ok_or_else(|| anyhow::anyhow!("direct headless harness resolved without a tap port"))?;
    Ok(Some(format!("http://127.0.0.1:{port}/v1")))
}

fn harness_workspace_mode(kind: HarnessKind) -> &'static str {
    match kind {
        HarnessKind::Omp => "explicit_cwd_flag",
        HarnessKind::QwenCode | HarnessKind::DeepseekHarness => "process_cwd",
        _ => "harness_managed",
    }
}

fn harness_output_mode(kind: HarnessKind) -> &'static str {
    match kind {
        HarnessKind::Omp => "text",
        HarnessKind::QwenCode => "stream_json",
        HarnessKind::DeepseekHarness => "final_text_only",
        _ => "interactive",
    }
}

fn harness_permission_posture(kind: HarnessKind) -> &'static str {
    match kind {
        HarnessKind::Omp => "always_ask",
        HarnessKind::QwenCode => "default",
        HarnessKind::DeepseekHarness => "workspace_write_ask",
        _ => "harness_preset",
    }
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
        harness_kind: bundle.profile.harness_kind,
        provider_lane: bundle.profile.provider_lane.clone(),
        route: bundle.profile.route.clone(),
        requested_model: bundle.profile.requested_model.clone(),
        request_model: bundle.profile.request_model.clone(),
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
        comparison: ArtifactComparison::Bytes,
    });
    artifacts.push(PlannedProfileArtifact {
        kind: "launch_profile_record",
        path: paths
            .lane_root
            .join("profiles")
            .join(format!("{}.env", bundle.profile.id)),
        contents: render_launch_profile_record(bundle),
        mode: 0o600,
        comparison: ArtifactComparison::Bytes,
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
            // Shared document: Switchback owns some keys, the derived mirror
            // supplies others, Claude Code writes the rest at runtime.
            comparison: ArtifactComparison::OwnedJsonRegion,
        });
    }
    if bundle.preset.harness == HarnessKind::PrimeAgent {
        // Prime-agent's `models.json` provider artifact. Lives at
        // `<prime_root>/_providers/<profile_label>/models.json`, the layout
        // `PRIME_AGENT_CODING_AGENT_DIR` reads under its config-root
        // relocation. The wrapper points prime-agent at the parent of
        // `_providers` via `SB_LANE_PRIME_CONFIG_DIR`.
        let models_path = paths
            .prime_profiles_root
            .join(&bundle.profile.profile_label)
            .join("models.json");
        let existing = read_optional_text(&models_path)?;
        let models = render_prime_models_json(existing.as_deref(), bundle)?;
        artifacts.push(PlannedProfileArtifact {
            kind: "prime_provider_models",
            path: models_path,
            contents: models,
            mode: 0o600,
            // Whole file, canonicalized. Prime-agent may add its own keys
            // (model defaults, etc.) at runtime; canonicalizing means a
            // re-emit only when Switchback's owned region actually drifts.
            comparison: ArtifactComparison::CanonicalJson,
        });
    }
    if bundle.preset.harness == HarnessKind::Omp {
        let models_path = paths
            .omp_profiles_root
            .join(&bundle.profile.profile_label)
            .join("agent/models.yml");
        artifacts.push(PlannedProfileArtifact {
            kind: "omp_provider_models",
            path: models_path,
            contents: render_omp_models_yaml(bundle)?,
            mode: 0o600,
            comparison: ArtifactComparison::Bytes,
        });
    }
    if bundle.preset.harness == HarnessKind::QwenCode {
        let settings_path = paths
            .qwen_profiles_root
            .join(&bundle.profile.profile_label)
            .join("settings.json");
        artifacts.push(PlannedProfileArtifact {
            kind: "qwen_profile_settings",
            path: settings_path,
            contents: render_qwen_settings(bundle)?,
            mode: 0o600,
            comparison: ArtifactComparison::CanonicalJson,
        });
    }
    for wrapper in &bundle.profile.wrappers {
        artifacts.push(PlannedProfileArtifact {
            kind: "wrapper",
            path: paths.wrapper_root.join(wrapper),
            contents: render_profile_wrapper(paths, bundle),
            mode: 0o700,
            comparison: ArtifactComparison::Bytes,
        });
    }
    artifacts.push(PlannedProfileArtifact {
        kind: "conformance_projection",
        path: paths
            .projection_root
            .join(format!("{}.json", bundle.profile.id)),
        contents: render_profile_conformance(bundle)?,
        mode: 0o600,
        comparison: ArtifactComparison::CanonicalJson,
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
            "SB_LANE_OPENAI_TAP",
            bundle
                .provider
                .openai_tap_port
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
        (
            "SB_LANE_CLAUDE_HEADROOM_BYPASS",
            if bundle.provider.headroom_bypass.unwrap_or(false) {
                "1"
            } else {
                "0"
            }
            .to_string(),
        ),
    ];
    if credential_key != "SB_LANE_KEY_ENV" {
        fields.push(("SB_LANE_KEY_ENV", String::new()));
    }
    // Emit only what this lane's spec actually declares. An undeclared field is
    // left to the preserve pass below, so bringing a lane under the authority is
    // additive: nothing is dropped because the spec has not caught up yet.
    let aliases = &bundle.preset.model_aliases;
    let optional: [(&'static str, Option<String>); 17] = [
        (
            "SB_LANE_WIRE_API",
            bundle.provider.wire_api.map(|api| api.as_str().to_string()),
        ),
        (
            "SB_LANE_ANTHROPIC_URL",
            bundle.provider.anthropic_url.clone(),
        ),
        ("SB_LANE_FAST_MODEL", bundle.provider.fast_model.clone()),
        ("SB_LANE_CODEX_ROUTE", bundle.provider.codex_route.clone()),
        (
            "SB_LANE_DIRECT_ANTHROPIC_TAP",
            bundle
                .provider
                .direct_anthropic_tap_port
                .map(|port| port.to_string()),
        ),
        ("SB_LANE_DIRECT_ROUTE", bundle.provider.direct_route.clone()),
        (
            "SB_LANE_HEADROOM_TOOL_SEARCH",
            bundle
                .provider
                .headroom_tool_search
                .map(|enabled| if enabled { "1" } else { "0" }.to_string()),
        ),
        ("SB_LANE_NOTES", bundle.provider.notes.clone()),
        // The preset has always known both of these; the record scavenged them
        // from the legacy file instead, so a lane with no legacy record to
        // inherit from would silently come up with no effort and no compaction
        // window. Ultra maps through `claude_code_effort` because Claude Code's
        // vocabulary stops at `max` and an unknown value degrades to `high`.
        (
            "SB_LANE_CLAUDE_EFFORT",
            Some(bundle.preset.native_effort.claude_code_effort().to_string()),
        ),
        (
            "SB_LANE_CLAUDE_AUTO_COMPACT_WINDOW",
            bundle
                .preset
                .compaction_window
                .map(|window| window.to_string()),
        ),
        ("SB_LANE_CLAUDE_MODEL", aliases.default.clone()),
        ("SB_LANE_CLAUDE_OPUS_MODEL", aliases.opus.clone()),
        ("SB_LANE_CLAUDE_SONNET_MODEL", aliases.sonnet.clone()),
        ("SB_LANE_CLAUDE_HAIKU_MODEL", aliases.haiku.clone()),
        ("SB_LANE_CLAUDE_SUBAGENT_MODEL", aliases.subagent.clone()),
        (
            "SB_LANE_CLAUDE_CUSTOM_MODEL_NAME",
            bundle.preset.display_name.clone(),
        ),
        (
            "SB_LANE_CLAUDE_CUSTOM_MODEL_DESCRIPTION",
            bundle.preset.description.clone(),
        ),
    ];
    for (key, value) in optional {
        if let Some(value) = value {
            fields.push((key, value));
        }
    }
    let owned: BTreeSet<&str> = fields.iter().map(|(key, _)| *key).collect();
    let mut rendered = render_shell_record(
        "# Generated from switchback/launch-profiles@1 provider_lanes; do not hand-edit.\n",
        fields,
    );
    let preserved = preserved_provider_lane_fields(existing, &owned);
    if !preserved.is_empty() {
        rendered.push_str("\n# Preserved compatibility fields outside provider_lanes authority.\n");
        for line in preserved.into_values() {
            rendered.push_str(&line);
            rendered.push('\n');
        }
    }
    rendered
}

/// Legacy fields the authority did not emit for this lane. `owned` is the set of
/// keys actually written, not a fixed list: a field the spec now declares stops
/// being preserved (so the record cannot hold two answers for it), and a field
/// the spec has not adopted yet survives untouched.
fn preserved_provider_lane_fields(
    existing: Option<&str>,
    owned: &BTreeSet<&str>,
) -> BTreeMap<String, String> {
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
            || owned.contains(key)
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
        ("SB_LAUNCH_HARNESS", bundle.profile.harness_kind.to_string()),
        (
            "SB_LAUNCH_COMPOUND_HARNESS",
            bundle.profile.harness.to_string(),
        ),
        (
            "SB_LAUNCH_EXPECTED_EXECUTABLE",
            bundle.profile.expected_executable.to_string(),
        ),
        (
            "SB_LAUNCH_EXPECTED_VERSION",
            bundle.profile.expected_version.clone().unwrap_or_default(),
        ),
        ("SB_LAUNCH_ROUTE", bundle.profile.route.clone()),
        (
            "SB_LAUNCH_PROVIDER_LANE",
            bundle.profile.provider_lane.clone(),
        ),
        (
            "SB_LAUNCH_REQUESTED_MODEL",
            bundle.profile.requested_model.clone(),
        ),
        (
            "SB_LAUNCH_REQUEST_MODEL",
            bundle.profile.request_model.clone(),
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
            "SB_LAUNCH_CAPTURE_ENDPOINT",
            bundle.profile.capture_endpoint.clone().unwrap_or_default(),
        ),
        (
            "SB_LAUNCH_PERMISSION_POSTURE",
            bundle.profile.permission_posture.to_string(),
        ),
        (
            "SB_LAUNCH_WORKSPACE_MODE",
            bundle.profile.workspace_mode.to_string(),
        ),
        (
            "SB_LAUNCH_OUTPUT_MODE",
            bundle.profile.output_mode.to_string(),
        ),
        (
            "SB_LAUNCH_PROFILE_LABEL",
            bundle.profile.profile_label.clone(),
        ),
        (
            "SB_LAUNCH_CLIENT_PROFILE",
            bundle.profile.client_profile.clone().unwrap_or_default(),
        ),
        (
            "SB_LAUNCH_CLIENT_CREDENTIAL_ENV",
            bundle
                .profile
                .client_credential_env
                .clone()
                .unwrap_or_default(),
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
        ("ANTHROPIC_DEFAULT_FABLE_MODEL", aliases.subagent.as_deref()),
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

/// Root keys Switchback generates into a harness `settings.json`. Conformance
/// asserts over exactly these plus [`OWNED_SETTINGS_ENV_KEYS`]; everything else
/// in the document belongs to the harness or to the derived mirror below.
const OWNED_SETTINGS_ROOT_KEYS: &[&str] = &["model", "effortLevel"];

/// `env` keys Switchback generates. Keys the harness adds to `env` itself are
/// preserved and never asserted.
const OWNED_SETTINGS_ENV_KEYS: &[&str] = &[
    "ANTHROPIC_CUSTOM_MODEL_OPTION",
    "ANTHROPIC_DEFAULT_FABLE_MODEL",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "CLAUDE_CODE_AUTO_COMPACT_WINDOW",
    "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY",
    "CLAUDE_CODE_SUBAGENT_MODEL",
];

/// Permission keys mirrored from the operator's global Claude settings under
/// `inherit_allowlisted`. Switchback controls whether they are present, but the
/// VALUES come from a source outside the launch-profile authority — so hashing
/// them into `artifacts.current` makes every lane drift the moment the operator
/// edits `~/.claude/settings.json`, which Claude Code does routinely.
const DERIVED_PERMISSION_KEYS: &[&str] = &[
    "permissions",
    "skipAutoPermissionPrompt",
    "skipDangerousModePermissionPrompt",
];

/// Non-permission settings mirrored from the same source.
const DERIVED_SETTING_KEYS: &[&str] = &["autoMode", "skipWorkflowUsageWarning", "statusLine"];

/// Permission keys actually mirrored under a given mode.
fn derived_permission_keys(mode: PermissionsMode) -> &'static [&'static str] {
    match mode {
        PermissionsMode::InheritAllowlisted => DERIVED_PERMISSION_KEYS,
        PermissionsMode::Minimal | PermissionsMode::Pinned => &[],
    }
}

/// Non-permission settings actually mirrored under a given mode.
fn derived_setting_keys(mode: SettingsMode) -> &'static [&'static str] {
    match mode {
        SettingsMode::InheritAllowlisted => DERIVED_SETTING_KEYS,
        SettingsMode::Minimal | SettingsMode::Pinned => &[],
    }
}

/// Path of the global Claude settings the derived region mirrors.
fn user_claude_settings_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".claude/settings.json")
}

fn merge_allowlisted_user_settings(
    root: &mut Value,
    permissions_mode: PermissionsMode,
    settings_mode: SettingsMode,
) -> anyhow::Result<()> {
    let root_object = root
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("generated settings top level must be an object"))?;
    // Drop the mirror before refilling it, so a stale snapshot never survives a
    // mode change. `Pinned` opts out of both the drop and the refill: the lane
    // keeps the values it already has.
    if !matches!(permissions_mode, PermissionsMode::Pinned) {
        for key in DERIVED_PERMISSION_KEYS {
            root_object.remove(*key);
        }
    }
    if !matches!(settings_mode, SettingsMode::Pinned) {
        for key in DERIVED_SETTING_KEYS {
            root_object.remove(*key);
        }
    }
    if matches!(
        permissions_mode,
        PermissionsMode::Minimal | PermissionsMode::Pinned
    ) && matches!(settings_mode, SettingsMode::Minimal | SettingsMode::Pinned)
    {
        return Ok(());
    }

    let path = user_claude_settings_path();
    let Some(text) = read_optional_text(&path)? else {
        return Ok(());
    };
    let user: Value = serde_json::from_str(&text).map_err(|error| {
        anyhow::anyhow!("parse user Claude settings {}: {error}", path.display())
    })?;
    let Some(user_object) = user.as_object() else {
        anyhow::bail!("user Claude settings top level must be an object");
    };
    let permission_keys = derived_permission_keys(permissions_mode);
    let setting_keys = derived_setting_keys(settings_mode);
    for key in permission_keys.iter().chain(setting_keys.iter()) {
        if let Some(value) = user_object.get(*key) {
            root_object.insert((*key).to_string(), value.clone());
        }
    }
    Ok(())
}

/// Render the `models.json` artifact Switchback owns for a prime-agent
/// launch profile. Prime-agent reads this file under the `_providers/<label>/`
/// directory it indexes via `PRIME_AGENT_CODING_AGENT_DIR`. We declare exactly
/// one provider named `switchback`, and the `apiKey` is the `!`-resolver form
/// so the real key never lands in the artifact on disk; the wrapper exports
/// the env var prime-agent's `!printenv` invocation reads.
fn render_prime_models_json(
    existing: Option<&str>,
    bundle: &ResolvedProfileBundle,
) -> anyhow::Result<String> {
    // The lane's tap is the natural upstream for a chat-wire prime-agent
    // launch: a Switchback tap binds the local listener prime-agent points at,
    // captures wire, and forwards to the gateway. Headroom/headroom-port lanes
    // and gateway lanes use `provider.anthropic_url` or derive from
    // `cfg.server.bind`. We follow the same precedence the provider lane
    // record already encodes (tap port → headroom port → explicit URL).
    let base_url = prime_provider_base_url(&bundle.provider)?;
    let (_, credential_env_name) = bundle.provider.credential_ref.lane_fields();
    if credential_env_name.is_empty() {
        anyhow::bail!(
            "provider lane `{}` credential reference has no env var name; \
             prime-agent models.json requires the !-resolver to read a key",
            bundle.profile.provider_lane
        );
    }
    let api_key = format!("!printenv {credential_env_name}");
    let mut models = vec![bundle.provider.requested_model.clone()];
    for alias in [
        bundle.preset.model_aliases.default.as_ref(),
        bundle.preset.model_aliases.opus.as_ref(),
        bundle.preset.model_aliases.sonnet.as_ref(),
        bundle.preset.model_aliases.haiku.as_ref(),
        bundle.preset.model_aliases.subagent.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        if !models.iter().any(|existing| existing == alias) {
            models.push(alias.clone());
        }
    }
    let provider = json!({
        "baseUrl": base_url,
        "api": "openai-completions",
        "apiKey": api_key,
        "compat": {
            // Prime-agent negotiates the Anthropic developer-role header
            // against an OpenAI-compatible endpoint when this is true; the
            // Switchback gateway rejects it on chat-wire lanes, so declare
            // it off and the gateway stays in charge of role translation.
            "supportsDeveloperRole": false,
            // Reasoning-effort headers also flow through the gateway, not
            // the upstream; prime-agent emits them via `--thinking` and we
            // want them to land on the Switchback side instead.
            "supportsReasoningEffort": false,
        },
        "models": models,
    });
    let desired = json!({ "providers": { "switchback": provider } });
    let mut rendered = serde_json::to_string_pretty(&desired)?;
    rendered.push('\n');
    // Preserve any operator-owned keys outside our one-provider shape — same
    // rationale as `preserved_provider_lane_fields` for the lane record.
    if let Some(existing_text) = existing {
        let preserved = preserved_prime_provider_keys(existing_text, &desired);
        if !preserved.is_empty() {
            rendered.push('\n');
            for line in preserved {
                rendered.push_str(&line);
                rendered.push('\n');
            }
        }
    }
    Ok(rendered)
}

/// Base URL a prime-agent launch profile points its `switchback` provider at.
/// Mirrors the precedence the lane record's `SB_LANE_ANTHROPIC_URL` field
/// already encodes: tap port, then explicit anthropic_url, then the
/// lane's headroom port.
fn prime_provider_base_url(provider: &ProviderLaneSpec) -> anyhow::Result<String> {
    if let Some(port) = provider.anthropic_tap_port {
        return Ok(format!("http://127.0.0.1:{port}"));
    }
    if let Some(port) = provider.headroom_port {
        return Ok(format!("http://127.0.0.1:{port}"));
    }
    if let Some(url) = provider.anthropic_url.as_deref() {
        return Ok(url.trim_end_matches('/').to_string());
    }
    anyhow::bail!(
        "provider lane `{}` has no tap, headroom port, or anthropic_url; \
         prime-agent requires a base URL",
        "<unknown>"
    )
}

/// Keys an operator hand-added to a prime-agent `models.json` outside the
/// single-`switchback`-provider shape Switchback owns. We never overwrite an
/// unknown top-level key — only the `providers.switchback` object.
fn preserved_prime_provider_keys(existing: &str, desired: &Value) -> Vec<String> {
    let Ok(parsed) = serde_json::from_str::<Value>(existing) else {
        return Vec::new();
    };
    let Some(root) = parsed.as_object() else {
        return Vec::new();
    };
    let owned_keys: BTreeSet<String> = if let Some(obj) = desired.as_object() {
        obj.keys().cloned().collect()
    } else {
        BTreeSet::new()
    };
    let mut out = Vec::new();
    for (key, value) in root {
        if owned_keys.contains(key) {
            continue;
        }
        let line = serde_json::to_string(value).unwrap_or_else(|_| value.to_string());
        out.push(format!("{key}: {line}"));
    }
    out
}

fn render_omp_models_yaml(bundle: &ResolvedProfileBundle) -> anyhow::Result<String> {
    let endpoint = bundle
        .profile
        .capture_endpoint
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("OMP profile has no capture endpoint"))?;
    let quote = |value: &str| format!("'{}'", value.replace('\'', "''"));
    Ok(format!(
        "# switchback-owned: omp-provider-models@1\n\
providers:\n\
  switchback:\n\
    baseUrl: {}\n\
    apiKey: SB_OMP_GATEWAY_KEY\n\
    api: openai-completions\n\
    authHeader: true\n\
    headers:\n\
      x-switchback-launch-profile: {}\n\
      x-switchback-conformance-revision: {}\n\
      x-switchback-harness: omp\n\
      x-switchback-capture-policy: {}\n\
      x-switchback-requested-effort: {}\n\
    models:\n\
      - id: {}\n\
        name: {}\n\
        api: openai-completions\n\
        reasoning: true\n",
        quote(endpoint),
        quote(&bundle.profile.id),
        quote(&bundle.revision),
        quote(bundle.profile.capture.mode),
        quote(bundle.profile.requested_effort),
        quote(&bundle.profile.request_model),
        quote(&bundle.profile.request_model),
    ))
}

fn render_qwen_settings(bundle: &ResolvedProfileBundle) -> anyhow::Result<String> {
    let endpoint = bundle
        .profile
        .capture_endpoint
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("Qwen Code profile has no capture endpoint"))?;
    let value = json!({
        "security": {
            "auth": {
                "selectedType": "openai",
            },
        },
        "model": {
            "name": bundle.profile.request_model,
        },
        "modelProviders": {
            "openai": [{
                "id": bundle.profile.request_model,
                "name": bundle.profile.request_model,
                "envKey": "SB_QWEN_GATEWAY_KEY",
                "baseUrl": endpoint,
                "generationConfig": {
                    "customHeaders": {
                        "x-switchback-launch-profile": bundle.profile.id,
                        "x-switchback-conformance-revision": bundle.revision,
                        "x-switchback-harness": "qwen-code",
                        "x-switchback-capture-policy": bundle.profile.capture.mode,
                    },
                },
            }],
        },
        "tools": {
            "approvalMode": "default",
        },
        "privacy": {
            "usageStatisticsEnabled": false,
        },
    });
    let mut rendered = serde_json::to_string_pretty(&value)?;
    rendered.push('\n');
    Ok(rendered)
}

fn render_profile_wrapper(paths: &ProfilePaths, bundle: &ResolvedProfileBundle) -> String {
    let mut out = format!("#!/bin/zsh\n{PROFILE_WRAPPER_OWNER_MARKER}\nset -eu\n");
    for (key, value) in [
        ("SB_LAUNCH_PROFILE_ID", bundle.profile.id.as_str()),
        ("SB_LAUNCH_PROFILE_REVISION", bundle.revision.as_str()),
        ("SB_LAUNCH_HARNESS", bundle.profile.harness_kind),
        ("SB_LAUNCH_COMPOUND_HARNESS", bundle.profile.harness),
        (
            "SB_LAUNCH_PROVIDER_LANE",
            bundle.profile.provider_lane.as_str(),
        ),
        ("SB_LAUNCH_ROUTE", bundle.profile.route.as_str()),
        (
            "SB_LAUNCH_REQUESTED_MODEL",
            bundle.profile.requested_model.as_str(),
        ),
        (
            "SB_LAUNCH_REQUEST_MODEL",
            bundle.profile.request_model.as_str(),
        ),
        ("SB_LAUNCH_CAPTURE_POLICY", bundle.profile.capture.mode),
        (
            "SB_LAUNCH_CAPTURE_ENDPOINT",
            bundle.profile.capture_endpoint.as_deref().unwrap_or(""),
        ),
        (
            "SB_LAUNCH_PERMISSION_POSTURE",
            bundle.profile.permission_posture,
        ),
        ("SB_LAUNCH_WORKSPACE_MODE", bundle.profile.workspace_mode),
        ("SB_LAUNCH_OUTPUT_MODE", bundle.profile.output_mode),
        (
            "SB_LAUNCH_EXPECTED_EXECUTABLE",
            bundle.profile.expected_executable,
        ),
        (
            "SB_LAUNCH_EXPECTED_VERSION",
            bundle.profile.expected_version.as_deref().unwrap_or(""),
        ),
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
    if let Some(client_profile) = bundle.profile.client_profile.as_deref() {
        out.push_str("export SB_LAUNCH_CLIENT_PROFILE=");
        out.push_str(&shell_single_quote(client_profile));
        out.push('\n');
    }
    if bundle.preset.harness.is_direct_headless() {
        render_direct_headless_wrapper(paths, bundle, &mut out);
        return out;
    }
    if bundle.preset.harness == HarnessKind::PrimeAgent {
        for (key, value) in [
            (
                "SB_LANE_PRIME_MODEL",
                bundle.profile.requested_model.as_str(),
            ),
            ("SB_LANE_PRIME_EFFORT", bundle.profile.requested_effort),
        ] {
            out.push_str("export ");
            out.push_str(key);
            out.push('=');
            out.push_str(&shell_single_quote(value));
            out.push('\n');
        }
        let config_dir = prime_config_root_for_label(&bundle.profile.profile_label);
        out.push_str("export SB_LANE_PRIME_CONFIG_DIR=");
        out.push_str(&shell_single_quote(&config_dir));
        out.push('\n');
    } else {
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
            (
                "SB_LANE_CLAUDE_SUBAGENT_MODEL",
                bundle.profile.model_aliases.subagent.as_deref(),
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

fn render_direct_headless_wrapper(
    paths: &ProfilePaths,
    bundle: &ResolvedProfileBundle,
    out: &mut String,
) {
    let (_, credential_env) = bundle
        .client_credential_ref
        .as_ref()
        .expect("direct headless profile validated client_credential_ref")
        .lane_fields();
    out.push_str("export SB_LAUNCH_CREDENTIAL_ENV=");
    out.push_str(&shell_single_quote(credential_env));
    out.push('\n');
    out.push_str(
        "if [[ ! -v \"$SB_LAUNCH_CREDENTIAL_ENV\" || -z \"${(P)SB_LAUNCH_CREDENTIAL_ENV}\" ]]; then\n\
  print -u2 -- \"missing credential env $SB_LAUNCH_CREDENTIAL_ENV for $SB_LAUNCH_PROFILE_ID\"\n\
  exit 78\n\
fi\n\
if (( $# == 0 )); then\n\
  print -u2 -- \"$SB_LAUNCH_PROFILE_ID requires a headless prompt\"\n\
  exit 64\n\
fi\n\
typeset prompt=\"$*\"\n",
    );
    match bundle.preset.harness {
        HarnessKind::Omp => {
            let agent_dir = paths
                .omp_profiles_root
                .join(&bundle.profile.profile_label)
                .join("agent");
            out.push_str("export SB_OMP_GATEWAY_KEY=\"${(P)SB_LAUNCH_CREDENTIAL_ENV}\"\n");
            out.push_str("export PI_CODING_AGENT_DIR=");
            out.push_str(&shell_single_quote(&agent_dir.display().to_string()));
            out.push('\n');
            out.push_str("exec omp --cwd=\"$PWD\" --provider=switchback --model=");
            out.push_str(&shell_single_quote(&format!(
                "switchback/{}",
                bundle.profile.request_model
            )));
            out.push_str(" --mode=text --print --no-session --no-extensions --no-skills --no-rules --approval-mode=always-ask");
            if preset_omp_thinking(bundle.preset.native_effort).is_some() {
                out.push_str(" --thinking=");
                out.push_str(
                    preset_omp_thinking(bundle.preset.native_effort).expect("checked above"),
                );
            }
            out.push_str(" \"$prompt\"\n");
        }
        HarnessKind::QwenCode => {
            let qwen_home = paths.qwen_profiles_root.join(&bundle.profile.profile_label);
            out.push_str("export SB_QWEN_GATEWAY_KEY=\"${(P)SB_LAUNCH_CREDENTIAL_ENV}\"\n");
            out.push_str("export QWEN_HOME=");
            out.push_str(&shell_single_quote(&qwen_home.display().to_string()));
            out.push('\n');
            out.push_str("export OPENAI_API_KEY=\"$SB_QWEN_GATEWAY_KEY\"\n");
            out.push_str("export OPENAI_BASE_URL=\"$SB_LAUNCH_CAPTURE_ENDPOINT\"\n");
            out.push_str("export OPENAI_MODEL=\"$SB_LAUNCH_REQUEST_MODEL\"\n");
            out.push_str("exec qwen --safe-mode --approval-mode=default --model=");
            out.push_str(&shell_single_quote(&bundle.profile.request_model));
            out.push_str(" --output-format=stream-json --prompt \"$prompt\"\n");
        }
        HarnessKind::DeepseekHarness => {
            let dsh_home = paths.dsh_profiles_root.join(&bundle.profile.profile_label);
            out.push_str("export DSH_HOME=");
            out.push_str(&shell_single_quote(&dsh_home.display().to_string()));
            out.push('\n');
            out.push_str("export DEEPSEEK_API_KEY=\"${(P)SB_LAUNCH_CREDENTIAL_ENV}\"\n");
            out.push_str("export DEEPSEEK_BASE_URL=\"$SB_LAUNCH_CAPTURE_ENDPOINT\"\n");
            out.push_str("export DSH_PERMISSION_MODE=workspace-write\n");
            out.push_str("exec dsh --profile headless \"$prompt\"\n");
        }
        _ => unreachable!("direct renderer called for non-direct harness"),
    }
}

fn preset_omp_thinking(effort: NativeEffort) -> Option<&'static str> {
    match effort {
        NativeEffort::Default => None,
        NativeEffort::Ultra => None,
        _ => Some(effort.as_str()),
    }
}

/// Resolve the prime-agent config root for a profile label. This is the
/// parent of `_providers/`, exactly what `PRIME_AGENT_CODING_AGENT_DIR`
/// expects at runtime.
fn prime_config_root_for_label(_label: &str) -> String {
    let paths = RuntimePaths::from_env();
    paths
        .config_root()
        .join("prime")
        .to_string_lossy()
        .into_owned()
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
            "harness_kind": bundle.profile.harness_kind,
            "expected_executable": bundle.profile.expected_executable,
            "expected_version": bundle.profile.expected_version,
            "provider_lane": bundle.profile.provider_lane,
            "route": bundle.profile.route,
            "requested_model": bundle.profile.requested_model,
            "request_model": bundle.profile.request_model,
            "requested_effort": bundle.profile.requested_effort,
            "model_aliases": bundle.profile.model_aliases,
            "client_profile": bundle.profile.client_profile,
            "client_credential_env": bundle.profile.client_credential_env,
            "capture_policy": bundle.profile.capture,
            "capture_endpoint": bundle.profile.capture_endpoint,
            "headless": bundle.profile.headless,
            "workspace_mode": bundle.profile.workspace_mode,
            "output_mode": bundle.profile.output_mode,
            "permission_posture": bundle.profile.permission_posture,
        },
        "capture_identity": harness_capture_identity(bundle.preset.harness),
        "feature_posture": harness_feature_posture(bundle.preset.harness),
        "unsupported_floors": harness_unsupported_floors(bundle.preset.harness),
        "settings_regions": conformance_settings_regions(bundle),
    });
    let mut rendered = serde_json::to_string_pretty(&value)?;
    rendered.push('\n');
    Ok(rendered)
}

fn harness_capture_identity(kind: HarnessKind) -> Value {
    match kind {
        HarnessKind::Omp => json!({
            "profile_id_source": "wrapper_env",
            "wire_headers": "omp_models_artifact",
        }),
        HarnessKind::QwenCode => json!({
            "profile_id_source": "wrapper_env",
            "wire_headers": "qwen_settings_artifact",
        }),
        HarnessKind::DeepseekHarness => json!({
            "profile_id_source": "wrapper_env",
            "wire_headers": "native_dsh_attribution_only",
            "warning": "DSH pre-1.0 has no custom-header profile seam; adopt later without claiming wire profile identity",
        }),
        _ => json!({
            "profile_id_source": "wrapper_env",
            "wire_headers": "sb_run",
        }),
    }
}

fn harness_feature_posture(kind: HarnessKind) -> Value {
    match kind {
        HarnessKind::Omp => json!({
            "hooks": "disabled_by_no_extensions",
            "skills": "disabled_by_no_skills",
            "mcp": "unsupported_not_claimed",
        }),
        HarnessKind::QwenCode => json!({
            "hooks": "disabled_by_safe_mode",
            "skills": "disabled_by_safe_mode",
            "mcp": "disabled_by_safe_mode_no_profile_artifact",
        }),
        HarnessKind::DeepseekHarness => json!({
            "hooks": "profile_owned_not_claimed",
            "skills": "profile_owned_not_claimed",
            "mcp": "profile_owned_not_claimed",
        }),
        _ => json!({
            "hooks": "harness_managed",
            "skills": "harness_preset",
            "mcp": "harness_preset",
        }),
    }
}

fn harness_unsupported_floors(kind: HarnessKind) -> Vec<&'static str> {
    match kind {
        HarnessKind::Omp => vec![
            "interactive_mode",
            "mcp_configuration",
            "inherited_permissions",
            "ultra_effort",
        ],
        HarnessKind::QwenCode => vec![
            "interactive_mode",
            "hooks_until_installed",
            "skills_until_installed",
            "mcp_without_profile_artifact",
            "implicit_yolo",
            "effort_flag",
        ],
        HarnessKind::DeepseekHarness => vec![
            "interactive_or_web_profile",
            "model_flag",
            "structured_output_flag",
            "inherited_permissions",
            "wire_launch_profile_header_pre_1_0",
            "hooks_skills_mcp_adopt_later",
        ],
        _ => Vec::new(),
    }
}

fn conformance_settings_regions(bundle: &ResolvedProfileBundle) -> Value {
    match bundle.preset.harness {
        HarnessKind::ClaudeCode => json!({
            "owned": {
                "root_keys": OWNED_SETTINGS_ROOT_KEYS,
                "env_keys": OWNED_SETTINGS_ENV_KEYS,
            },
            "derived": {
                "source": user_claude_settings_path().display().to_string(),
                "permissions_mode": bundle.preset.permissions_mode,
                "settings_mode": bundle.preset.settings_mode,
                "permission_keys": derived_permission_keys(bundle.preset.permissions_mode),
                "setting_keys": derived_setting_keys(bundle.preset.settings_mode),
            },
        }),
        HarnessKind::Omp => json!({
            "owned": "complete_models_yml",
            "derived": Value::Null,
        }),
        HarnessKind::QwenCode => json!({
            "owned": "complete_settings_json",
            "derived": Value::Null,
        }),
        HarnessKind::DeepseekHarness => json!({
            "owned": "wrapper_only",
            "derived": "dsh_shipped_headless_profile",
        }),
        _ => json!({
            "owned": "harness_specific_artifact",
            "derived": Value::Null,
        }),
    }
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
            let compared_desired = comparable_bytes(desired, artifact.comparison);
            let compared_current = current
                .as_deref()
                .map(|bytes| comparable_bytes(bytes, artifact.comparison));
            let region_matches = compared_current.as_deref() == Some(compared_desired.as_slice());
            Ok(ProfileArtifactStatus {
                kind: artifact.kind,
                path: artifact.path.display().to_string(),
                exists: current.is_some(),
                changed: !region_matches || !mode_matches,
                current_sha256: current.as_deref().map(sha256_hex),
                desired_sha256: sha256_hex(desired),
                mode: format!("{:04o}", artifact.mode),
                actual_mode: actual_mode.map(|mode| format!("{mode:04o}")),
                mode_matches,
                comparison: artifact.comparison,
                compared_current_sha256: compared_current.as_deref().map(sha256_hex),
                compared_desired_sha256: sha256_hex(&compared_desired),
            })
        })
        .collect()
}
fn profile_capture_tap_port(bundle: &ResolvedProfileBundle) -> Option<u16> {
    if bundle.preset.harness.is_direct_headless() {
        bundle.provider.openai_tap_port
    } else if bundle.provider.claude_via_tap || bundle.provider.transport == LaneTransport::Tap {
        bundle.provider.anthropic_tap_port
    } else {
        None
    }
}

fn current_wrappers_contain(artifacts: &[PlannedProfileArtifact], required: &[String]) -> bool {
    let wrappers: Vec<&PlannedProfileArtifact> = artifacts
        .iter()
        .filter(|artifact| artifact.kind == "wrapper")
        .collect();
    !wrappers.is_empty()
        && wrappers.iter().all(|artifact| {
            std::fs::read_to_string(&artifact.path)
                .ok()
                .is_some_and(|contents| required.iter().all(|needle| contents.contains(needle)))
        })
}

fn expected_export(name: &str, value: &str) -> String {
    format!("export {name}={}", shell_single_quote(value))
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
    if bundle.profile.capture.mode == LaunchCaptureMode::SegmentedFullWire.as_str() {
        let capture_tap = profile_capture_tap_port(bundle).and_then(|port| {
            cfg.server
                .taps
                .iter()
                .find(|tap| tap_bind_port(&tap.bind) == Some(port))
        });
        push_check(
            &mut checks,
            "tap.capture_binding",
            json!(true),
            json!(capture_tap.is_some()),
        );
        push_check(
            &mut checks,
            "tap.capture_bodies",
            json!(true),
            json!(capture_tap.is_some_and(|tap| tap.capture_bodies)),
        );
    }
    push_check(
        &mut checks,
        "wrapper.ownership_marker",
        json!(true),
        json!(current_wrappers_contain(
            artifacts,
            &[PROFILE_WRAPPER_OWNER_MARKER.to_string()]
        )),
    );
    push_check(
        &mut checks,
        "wrapper.launch_profile_id",
        json!(true),
        json!(current_wrappers_contain(
            artifacts,
            &[expected_export("SB_LAUNCH_PROFILE_ID", &bundle.profile.id)]
        )),
    );
    push_check(
        &mut checks,
        "wrapper.provider_model_binding",
        json!(true),
        json!(current_wrappers_contain(
            artifacts,
            &[
                expected_export("SB_LAUNCH_PROVIDER_LANE", &bundle.profile.provider_lane),
                expected_export("SB_LAUNCH_REQUESTED_MODEL", &bundle.profile.requested_model),
                expected_export("SB_LAUNCH_REQUEST_MODEL", &bundle.profile.request_model),
                expected_export("SB_LAUNCH_ROUTE", &bundle.profile.route),
            ]
        )),
    );
    if let Some(client_credential_env) = bundle.profile.client_credential_env.as_deref() {
        push_check(
            &mut checks,
            "wrapper.gateway_client_credential",
            json!(true),
            json!(current_wrappers_contain(
                artifacts,
                &[expected_export(
                    "SB_LAUNCH_CREDENTIAL_ENV",
                    client_credential_env
                )]
            )),
        );
    }
    push_check(
        &mut checks,
        "wrapper.capture_posture",
        json!(true),
        json!(current_wrappers_contain(
            artifacts,
            &[
                expected_export("SB_LAUNCH_CAPTURE_POLICY", bundle.profile.capture.mode),
                expected_export(
                    "SB_LAUNCH_CAPTURE_ENDPOINT",
                    bundle.profile.capture_endpoint.as_deref().unwrap_or("")
                ),
                expected_export(
                    "SB_LAUNCH_PERMISSION_POSTURE",
                    bundle.profile.permission_posture
                ),
            ]
        )),
    );
    push_check(
        &mut checks,
        "wrapper.expected_harness",
        json!(true),
        json!(current_wrappers_contain(
            artifacts,
            &[
                expected_export(
                    "SB_LAUNCH_EXPECTED_EXECUTABLE",
                    bundle.profile.expected_executable
                ),
                expected_export(
                    "SB_LAUNCH_EXPECTED_VERSION",
                    bundle.profile.expected_version.as_deref().unwrap_or("")
                ),
                expected_export("SB_LAUNCH_COMPOUND_HARNESS", bundle.profile.harness),
            ]
        )),
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
    // `claude_via_tap` now implies headroom transport (enforced at resolve time),
    // so this is every Anthropic-wire lane. The previous form was
    // `transport == Headroom && (claude_via_tap || transport == Tap)`, whose second
    // disjunct the first conjunct made unreachable — it read as "also covers tap
    // lanes" while covering none of them, which is precisely the case that breaks.
    if bundle.provider.claude_via_tap {
        if let (Some(tap_port), Some(headroom_port)) = (
            bundle.provider.anthropic_tap_port,
            bundle.provider.headroom_port,
        ) {
            // Only when this config actually declares the lane's tap.
            // `tap.exists` below is what answers whether a tap is declared at
            // all; this one only answers, given that it is, where it points.
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
    // The gpt56-sol-ultra incident: the lane's own record named an
    // `anthropic_tap_port` that nothing in `cfg.server.taps` binds — 8788 was a
    // Headroom process, not a tap, so the lane's Claude traffic never passed
    // through capture at all. A port that merely listens is not a tap; the
    // check above only compares a *declared* tap's binding, so without this it
    // stays silent exactly when there is no tap to find. That silence is the
    // failure: capture is the product, and this is how it goes missing without
    // a single request ever failing.
    if bundle.provider.claude_via_tap || bundle.provider.transport == LaneTransport::Tap {
        let tap_port = bundle.provider.anthropic_tap_port.ok_or_else(|| {
            anyhow::anyhow!(
                "profile {} declares a tap transport without an anthropic_tap_port",
                bundle.profile.id
            )
        })?;
        push_check(
            &mut checks,
            "tap.exists",
            json!(true),
            json!(cfg
                .server
                .taps
                .iter()
                .any(|tap| tap_bind_port(&tap.bind) == Some(tap_port))),
        );
    }
    if bundle.preset.harness.is_direct_headless() {
        let tap_port = bundle.provider.openai_tap_port.ok_or_else(|| {
            anyhow::anyhow!(
                "profile {} direct headless harness has no openai_tap_port",
                bundle.profile.id
            )
        })?;
        push_check(
            &mut checks,
            "tap.openai_exists",
            json!(true),
            json!(cfg
                .server
                .taps
                .iter()
                .any(|tap| tap_bind_port(&tap.bind) == Some(tap_port))),
        );
    }
    if matches!(scope, ProfileDoctorScope::Live) {
        if let Some(expected_version) = bundle.profile.expected_version.as_deref() {
            match read_harness_version(bundle.preset.harness) {
                Ok(actual_version) => {
                    push_check(
                        &mut checks,
                        "harness.executable",
                        json!(bundle.profile.expected_executable),
                        json!(bundle.profile.expected_executable),
                    );
                    push_check(
                        &mut checks,
                        "harness.version",
                        json!(expected_version),
                        json!(actual_version),
                    );
                }
                Err(error) => {
                    push_check(
                        &mut checks,
                        "harness.executable",
                        json!(bundle.profile.expected_executable),
                        json!(error),
                    );
                    push_check(
                        &mut checks,
                        "harness.version",
                        json!(expected_version),
                        Value::Null,
                    );
                }
            }
        }
        if bundle.preset.harness.is_direct_headless() {
            let port = bundle.provider.openai_tap_port.ok_or_else(|| {
                anyhow::anyhow!(
                    "profile {} requires an OpenAI-compatible tap listener",
                    bundle.profile.id
                )
            })?;
            push_check(
                &mut checks,
                "listener.openai_tap",
                json!(true),
                json!(local_listener_ready(port)),
            );
        }
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
        // reaches a session. Run it only when a credential is actually
        // resolvable (see `resolve_tap_credential`): a doctor that failed
        // because it could not find a key would train everyone to ignore it,
        // and one that passed without asking the provider would be claiming
        // something it never checked.
        if bundle.provider.claude_via_tap {
            if let Some(port) = bundle.provider.anthropic_tap_port {
                if let Some(credential) =
                    resolve_tap_credential(cfg, &bundle.provider.credential_ref, port)
                {
                    if let Some(status) =
                        tap_preflight_status(port, &bundle.profile.requested_model, &credential)
                    {
                        // Two verdicts from ONE exchange. They disagree exactly when a lane
                        // is authenticated but unusable (402 out of credit, 429 plan
                        // exhausted), which is the case that used to audit clean.
                        push_check(
                            &mut checks,
                            "preflight.provider_accepts_credential",
                            json!(true),
                            json!(preflight_credential_accepted(status)),
                        );
                        // Carries the status so a red line names the wall the lane hit.
                        push_check(
                            &mut checks,
                            "preflight.upstream_healthy",
                            json!(true),
                            json!(if preflight_upstream_healthy(status) {
                                json!(true)
                            } else {
                                json!(format!("upstream returned HTTP {status}"))
                            }),
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
        derived_settings: derived_settings_status(bundle),
    })
}

fn read_harness_version(kind: HarnessKind) -> Result<String, String> {
    let executable = kind.executable();
    let output = Command::new(executable)
        .arg("--version")
        .output()
        .map_err(|error| format!("{executable}: {error}"))?;
    if !output.status.success() {
        return Err(format!("{executable} --version exited {}", output.status));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let text = if stdout.trim().is_empty() {
        stderr.trim()
    } else {
        stdout.trim()
    };
    let version = text
        .split_whitespace()
        .find_map(|token| {
            let token = token
                .strip_prefix(&format!("{executable}/"))
                .unwrap_or(token)
                .trim_matches(|ch: char| !ch.is_ascii_alphanumeric());
            token
                .chars()
                .next()
                .is_some_and(|ch| ch.is_ascii_digit())
                .then(|| token.to_string())
        })
        .ok_or_else(|| format!("{executable} --version returned no version token"))?;
    Ok(version)
}

/// Port a tap listens on, from a `host:port` bind string.
fn tap_bind_port(bind: &str) -> Option<u16> {
    bind.rsplit_once(':')
        .and_then(|(_, port)| port.parse::<u16>().ok())
}

/// True when this lane's own tap forwards to Switchback's own gateway rather
/// than to a provider. `gpt56-sol-ultra`, `neuralwatt`, and `opencode-go` are
/// all wired this way — their tap's declared `upstream` IS `server.bind`. That
/// case is different from an ordinary tap: the far end checks its own
/// `api_keys`, not a provider credential, so the right thing to preflight with
/// is a gateway key, never the provider-shaped one `credential_ref` names.
fn tap_forwards_to_gateway(cfg: &Config, port: u16) -> bool {
    let gateway = format!("http://{}", cfg.server.bind);
    cfg.server
        .taps
        .iter()
        .find(|tap| tap_bind_port(&tap.bind) == Some(port))
        .is_some_and(|tap| tap.upstream.trim_end_matches('/') == gateway)
}

/// A live gateway key for the preflight to present when a lane's tap forwards
/// to Switchback itself. `cli/sb` resolves that lane class as
/// `${SWITCHBACK_SCOUT_API_KEY:-scout-local}` — that variable names the
/// gateway's OWN token, not a provider secret, and it is deliberately absent
/// from the operator's exported environment, so the literal env lookup in
/// `resolve_tap_credential` always misses for these lanes. The doctor must not
/// be stricter than the shell it audits, so this reads the same `api_keys`
/// entries the gateway itself checks rather than assuming the shell's
/// `scout-local` literal stays correct forever — `key` is a plain
/// `Option<String>` (only its `Debug` impl redacts it), so this is a real
/// read, not a guess. `key_hash` entries are skipped: a hash cannot be turned
/// back into a credential. Prefers the least-privileged usable key (client
/// over operator over admin) — this preflight only ever needs an
/// accepted/rejected verdict, never elevated access.
fn resolve_gateway_key(cfg: &Config) -> Option<String> {
    cfg.api_keys
        .iter()
        .filter_map(|entry| {
            let value = entry
                .key
                .as_deref()
                .filter(|value| !value.trim().is_empty())?;
            Some((entry.role, value.to_string()))
        })
        .min_by_key(|(role, _)| match role {
            ApiKeyRole::Client => 0,
            ApiKeyRole::Operator => 1,
            ApiKeyRole::Admin => 2,
        })
        .map(|(_, value)| value)
}

/// The credential this lane's tap preflight should present, or `None` when
/// nothing is resolvable by any route. `None` must mean the caller SKIPS the
/// check rather than failing it — an operator with no key configured would
/// otherwise learn to ignore a doctor that cries wolf. Ordinary lanes resolve
/// through their declared `credential_ref` env var; a lane whose tap forwards
/// to the gateway falls back to a live gateway key (`resolve_gateway_key`)
/// because its declared env var names the gateway token `cli/sb` defaults
/// rather than requires.
fn resolve_tap_credential(
    cfg: &Config,
    credential_ref: &CredentialReference,
    port: u16,
) -> Option<String> {
    if let CredentialReference::Env { name } = credential_ref {
        if let Some(value) = std::env::var(name)
            .ok()
            .filter(|value| !value.trim().is_empty())
        {
            return Some(value);
        }
    }
    if tap_forwards_to_gateway(cfg, port) {
        return resolve_gateway_key(cfg);
    }
    None
}

/// One authenticated round trip through a lane's own tap, returning the upstream
/// HTTP status. Ports can listen and bindings can be right while the far end
/// still rejects every request — a wrong key, a revoked plan, or a tap wired to
/// a provider this key isn't for. Without this, the first thing that discovers
/// it is a real session.
///
/// Returns the STATUS rather than a verdict because one exchange answers two
/// different questions, and collapsing them here is what let a dead lane report
/// green. `402 Payment Required` and `429 usage limit` both mean the credential
/// WAS accepted — correct for auth, useless for usability. `neuralwatt` sat at
/// 402 (out of credit) and `opencode-go` at 429 (plan exhausted) while both
/// audited clean. The caller derives `provider_accepts_credential` and
/// `upstream_healthy` from this single value; probing twice would double every
/// doctor run's upstream traffic to answer questions one response already has.
///
/// Raw HTTP/1.1 over loopback on purpose: this runs on the doctor's synchronous
/// path, so borrowing an async client would mean standing up a runtime inside a
/// health check. `None` means the exchange never completed, which the listener
/// checks already describe and this must not restate as a provider verdict.
fn tap_preflight_status(port: u16, model: &str, credential: &str) -> Option<u16> {
    use std::io::{Read as _, Write as _};

    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_millis(400)).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .ok()?;
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .ok()?;
    // A 1-token budget is not a valid request for a reasoning model — the whole
    // budget is consumed before any visible output, and gpt-5.x-class endpoints
    // reject it outright with a 400. That is a malformed probe, not an unhealthy
    // upstream, and it made `preflight.upstream_healthy` red on lanes that serve
    // real traffic perfectly. Ask for the smallest budget these models accept.
    let body = format!(
        r#"{{"model":"{model}","max_tokens":16,"messages":[{{"role":"user","content":"ping"}}]}}"#
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
    Some(status)
}

/// Did the far end accept this lane's credential? 401/403 are the only statuses
/// that mean "no". Deliberately NOT a usability verdict — see `upstream_healthy`.
fn preflight_credential_accepted(status: u16) -> bool {
    !matches!(status, 401 | 403)
}

/// Is the lane actually usable right now? Only a 2xx says yes.
///
/// This is the check `provider_accepts_credential` cannot be: a lane that is out
/// of credit (402) or over its plan limit (429) has a perfectly valid credential
/// and cannot serve a single request. Anything non-2xx fails here and carries the
/// status, so the report says WHICH wall the lane hit instead of just "not ok".
fn preflight_upstream_healthy(status: u16) -> bool {
    (200..300).contains(&status)
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
    // Text is the mode an operator actually reads. A failure whose remedy stays
    // in the JSON is a failure that gets acted on by guesswork.
    for action in &report.next_actions {
        println!("next {action}");
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

#[cfg(test)]
mod strict_claude_profile_tests {
    use super::*;
    use sb_core::Config;

    const ROUTE: &str = "codex/gpt-5.6-sol";
    const CLIENT_PROFILE: &str = "claude-gpt-default";

    fn authority(subagent: Option<&str>) -> LaunchProfilesDocument {
        let mut aliases = json!({
            "default": ROUTE,
            "opus": ROUTE,
            "sonnet": "codex/gpt-5.6-terra",
            "haiku": "codex/gpt-5.6-luna"
        });
        if let Some(subagent) = subagent {
            aliases["subagent"] = json!(subagent);
        }

        serde_json::from_value(json!({
            "schema": LAUNCH_PROFILES_SCHEMA,
            "provider_lanes": {
                "gpt": {
                    "route": ROUTE,
                    "requested_model": ROUTE,
                    "transport": "gateway",
                    "credential_ref": { "kind": "env", "name": "TEST_GATEWAY_KEY" },
                    "min_fallbacks": 0
                }
            },
            "harness_presets": {
                "claude-gpt": {
                    "harness": "claude-code",
                    "native_effort": "xhigh",
                    "model_aliases": aliases,
                    "permissions_mode": "minimal",
                    "mcp_mode": "none",
                    "skills_mode": "disabled",
                    "settings_mode": "minimal"
                }
            },
            "capture_policies": {
                "observed": { "mode": "segmented_full_wire" }
            },
            "launch_profiles": {
                "claude-gpt": {
                    "provider_lane": "gpt",
                    "harness_preset": "claude-gpt",
                    "capture_policy": "observed",
                    "client_profile": CLIENT_PROFILE
                }
            }
        }))
        .expect("strict Claude authority parses")
    }

    fn config() -> Config {
        Config::from_yaml(
            r#"
server:
  bind: "127.0.0.1:18765"
providers:
  - id: codex-relay
    type: openai_compatible
    base_url: "https://gateway.example.invalid/v1"
    api_key_env: TEST_GATEWAY_KEY
routes:
  - name: codex-sol
    match: { model: "codex/gpt-5.6-sol" }
    targets: ["codex-relay/gpt-5.6-sol"]
client_profiles:
  - id: claude-gpt-default
    kind: claude_code
    models:
      - codex/gpt-5.6-sol
      - codex/gpt-5.6-terra
      - codex/gpt-5.6-luna
"#,
        )
        .expect("strict Claude config parses")
    }

    #[test]
    fn strict_claude_profile_requires_explicit_subagent_alias() {
        let error = resolve_launch_profile(&authority(None), &config(), "claude-gpt")
            .expect_err("strict Claude profile without a subagent alias must fail closed");

        assert!(
            error
                .to_string()
                .contains("requires an explicit subagent model alias"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn strict_claude_profile_rejects_subagent_alias_outside_allowlist() {
        let error =
            resolve_launch_profile(&authority(Some("claude-fable-5")), &config(), "claude-gpt")
                .expect_err("strict Claude profile must reject a denied subagent alias");

        assert!(
            error
                .to_string()
                .contains("subagent model alias `claude-fable-5` is denied"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn strict_claude_profile_accepts_subagent_alias_inside_allowlist() {
        let bundle = resolve_launch_profile(
            &authority(Some("codex/gpt-5.6-terra")),
            &config(),
            "claude-gpt",
        )
        .expect("strict Claude profile with an allowed subagent alias must resolve");

        assert_eq!(
            bundle.profile.model_aliases.subagent.as_deref(),
            Some("codex/gpt-5.6-terra")
        );
    }

    #[test]
    fn claude_profile_without_client_fence_still_requires_explicit_subagent_alias() {
        let mut document = authority(None);
        document
            .launch_profiles
            .get_mut("claude-gpt")
            .expect("launch profile exists")
            .client_profile = None;

        let error = resolve_launch_profile(&document, &config(), "claude-gpt")
            .expect_err("every Claude Code profile must pin the subagent model");

        assert!(
            error
                .to_string()
                .contains("requires an explicit subagent model alias"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn strict_claude_profile_requires_every_role_alias() {
        let mut document = authority(Some("codex/gpt-5.6-terra"));
        document
            .harness_presets
            .get_mut("claude-gpt")
            .expect("harness preset exists")
            .model_aliases
            .haiku = None;

        let error = resolve_launch_profile(&document, &config(), "claude-gpt")
            .expect_err("strict Claude profiles must close every model alias");

        assert!(
            error
                .to_string()
                .contains("requires an explicit haiku model alias"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn strict_claude_profile_rejects_any_role_alias_outside_allowlist() {
        let mut document = authority(Some("codex/gpt-5.6-terra"));
        document
            .harness_presets
            .get_mut("claude-gpt")
            .expect("harness preset exists")
            .model_aliases
            .opus = Some("claude-fable-5".to_string());

        let error = resolve_launch_profile(&document, &config(), "claude-gpt")
            .expect_err("strict Claude profiles must fence every model alias");

        assert!(
            error
                .to_string()
                .contains("opus model alias `claude-fable-5` is denied"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn unrestricted_client_profile_allows_explicit_aliases() {
        let mut cfg = config();
        cfg.client_profiles[0].models.clear();

        resolve_launch_profile(&authority(Some("claude-fable-5")), &cfg, "claude-gpt")
            .expect("an empty client-profile model list remains allow-all");
    }
}

#[cfg(test)]
mod tap_credential_resolution_tests {
    use super::*;

    // A name nothing else in this process sets or reads. The point of this
    // suite is proving the CONFIG fallback fires when the env route misses,
    // so tests drive resolution through `Config` values built from literal
    // YAML rather than mutating `std::env` — process env is shared global
    // state under the default multi-threaded test harness, and a test that
    // set/removed a real var would race every other test reading it.
    const UNSET_ENV_NAME: &str = "SWITCHBACK_TEST_DOES_NOT_EXIST_CREDENTIAL";

    /// A minimal config with one tap at `tap_port` and the given `api_keys:`
    /// YAML body spliced in verbatim, so each test only states the keys it
    /// cares about.
    fn cfg_with_gateway_tap(tap_port: u16, api_keys_yaml: &str) -> Config {
        let yaml = format!(
            r#"
server:
  bind: "127.0.0.1:18765"
  taps:
    - id: test-tap
      bind: "127.0.0.1:{tap_port}"
      upstream: "http://127.0.0.1:18765"
api_keys:
{api_keys_yaml}
"#
        );
        Config::from_yaml(&yaml).expect("valid test config")
    }

    fn unset_env_credential_ref() -> CredentialReference {
        CredentialReference::Env {
            name: UNSET_ENV_NAME.to_string(),
        }
    }

    /// The defect this whole change fixes: a lane's declared credential env
    /// var is absent (as it always is for `gpt56-sol-ultra` / `neuralwatt` /
    /// `opencode-go` — the operator's env never exports the gateway token),
    /// but its tap forwards to the gateway, so a gateway `api_keys` entry
    /// should resolve and the preflight should run.
    #[test]
    fn falls_back_to_gateway_key_when_declared_env_is_unset() {
        let cfg = cfg_with_gateway_tap(
            19001,
            "  - key: \"least-priv-test-key\"\n    tenant: t1\n    role: client\n",
        );
        let resolved = resolve_tap_credential(&cfg, &unset_env_credential_ref(), 19001);
        assert_eq!(resolved.as_deref(), Some("least-priv-test-key"));
    }

    /// Least-privileged-first: an admin key must not be preferred over a
    /// client key just because it sorts first in config.
    #[test]
    fn prefers_least_privileged_role_among_usable_gateway_keys() {
        let cfg = cfg_with_gateway_tap(
            19002,
            "  - key: \"admin-test-key\"\n    tenant: t2\n    role: admin\n  - key: \"client-test-key\"\n    tenant: t1\n    role: client\n",
        );
        assert_eq!(
            resolve_gateway_key(&cfg).as_deref(),
            Some("client-test-key")
        );
    }

    /// A tap that does NOT forward to the gateway (this config declares no
    /// tap at all on this port) must not fall back — there is genuinely no
    /// credential reachable by any route, so the caller must SKIP, not fail.
    #[test]
    fn skips_when_tap_does_not_forward_to_gateway() {
        let cfg = cfg_with_gateway_tap(
            19003,
            "  - key: \"admin-test-key\"\n    tenant: t2\n    role: admin\n",
        );
        // Port 9999 is not the declared tap's port in this config, so the tap
        // lookup in `tap_forwards_to_gateway` fails closed.
        let resolved = resolve_tap_credential(&cfg, &unset_env_credential_ref(), 9999);
        assert!(resolved.is_none());
    }

    /// A gateway-forwarding tap whose only configured key is a `key_hash`
    /// (unrecoverable by design) still has no resolvable credential — the
    /// doctor must skip rather than fail.
    #[test]
    fn skips_when_gateway_tap_has_no_usable_key() {
        let cfg = cfg_with_gateway_tap(
            19004,
            "  - key_hash: \"sha256:eeeef83a9f5d2081e8f9902cf422b483d41f0a202d330a272eb5e61dc2047230\"\n    tenant: t3\n    role: client\n",
        );
        let resolved = resolve_tap_credential(&cfg, &unset_env_credential_ref(), 19004);
        assert!(resolved.is_none());
    }

    /// REGRESSION: these two verdicts must DISAGREE on an authenticated-but-unusable
    /// lane. Collapsing them is what let `neuralwatt` (402, out of credit) and
    /// `opencode-go` (429, plan exhausted) audit clean while serving nothing.
    #[test]
    fn preflight_separates_auth_from_usability() {
        for status in [402u16, 429, 500, 503] {
            assert!(
                preflight_credential_accepted(status),
                "HTTP {status} means the credential WAS accepted"
            );
            assert!(
                !preflight_upstream_healthy(status),
                "HTTP {status} lane is not usable and must fail upstream_healthy"
            );
        }
    }

    #[test]
    fn preflight_rejects_only_401_and_403_as_auth_failures() {
        assert!(!preflight_credential_accepted(401));
        assert!(!preflight_credential_accepted(403));
        // 400 is a malformed request, not a rejected credential.
        assert!(preflight_credential_accepted(400));
        assert!(preflight_credential_accepted(200));
    }

    #[test]
    fn preflight_upstream_healthy_only_on_2xx() {
        for status in [200u16, 201, 299] {
            assert!(
                preflight_upstream_healthy(status),
                "HTTP {status} is healthy"
            );
        }
        for status in [199u16, 300, 301, 400, 401, 404, 500] {
            assert!(
                !preflight_upstream_healthy(status),
                "HTTP {status} is not healthy"
            );
        }
    }

    /// A fully working lane passes BOTH, or the new check is just noise.
    #[test]
    fn preflight_healthy_lane_passes_both() {
        assert!(preflight_credential_accepted(200));
        assert!(preflight_upstream_healthy(200));
    }
}

/// A harness `settings.json` has three writers: Switchback generates the model
/// and env keys, the derived mirror supplies `permissions` from the operator's
/// global settings, and Claude Code writes its own keys at runtime. Conformance
/// must assert over the first group only — hashing the whole file reports drift
/// that no `apply` can durably fix, and re-applying to "fix" it reverts the
/// permission block another owner applied.
#[cfg(test)]
mod settings_ownership_tests {
    use super::*;

    /// A settings document carrying all three regions.
    fn shared_settings() -> Value {
        json!({
            "model": "MiniMax-M3[1m]",
            "effortLevel": "xhigh",
            "env": {
                "ANTHROPIC_DEFAULT_OPUS_MODEL": "MiniMax-M3[1m]",
                "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY": "1",
                "SOME_HARNESS_WRITTEN_VAR": "written-by-claude-code"
            },
            "permissions": { "defaultMode": "auto", "deny": ["Bash(rm:*)"] },
            "hooks": { "PreToolUse": [{ "matcher": "*" }] },
            "theme": "dark"
        })
    }

    #[test]
    fn owned_region_excludes_derived_and_harness_written_keys() {
        let owned = owned_settings_region(&shared_settings());

        assert_eq!(owned["model"], json!("MiniMax-M3[1m]"));
        assert_eq!(owned["effortLevel"], json!("xhigh"));
        assert_eq!(
            owned["env"]["ANTHROPIC_DEFAULT_OPUS_MODEL"],
            json!("MiniMax-M3[1m]")
        );
        for foreign in ["permissions", "hooks", "theme"] {
            assert!(
                owned.get(foreign).is_none(),
                "`{foreign}` is not Switchback's to assert: {owned}"
            );
        }
        assert!(
            owned["env"].get("SOME_HARNESS_WRITTEN_VAR").is_none(),
            "harness-written env keys stay out of the owned region: {owned}"
        );
    }

    #[test]
    fn harness_and_permission_edits_are_not_drift() {
        let desired = serde_json::to_vec(&shared_settings()).unwrap();
        let mut mutated = shared_settings();
        // Exactly what happens in the field: Claude Code rewrites permissions,
        // compound applies its hooks, the operator flips a theme.
        mutated["permissions"] = json!({ "defaultMode": "bypassPermissions", "deny": [] });
        mutated["hooks"] = json!({ "SessionStart": [{ "matcher": "*" }] });
        mutated["theme"] = json!("light");
        mutated["env"]["SOME_HARNESS_WRITTEN_VAR"] = json!("changed");
        let current = serde_json::to_vec(&mutated).unwrap();

        assert_eq!(
            comparable_bytes(&current, ArtifactComparison::OwnedJsonRegion),
            comparable_bytes(&desired, ArtifactComparison::OwnedJsonRegion),
            "a document differing only outside the owned region is not drift"
        );
        assert_ne!(
            comparable_bytes(&current, ArtifactComparison::Bytes),
            comparable_bytes(&desired, ArtifactComparison::Bytes),
            "the old whole-file rule is what reported this as drift"
        );
    }

    #[test]
    fn owned_region_edits_are_still_drift() {
        let desired = serde_json::to_vec(&shared_settings()).unwrap();
        let mut mutated = shared_settings();
        mutated["model"] = json!("some-other-model");
        let current = serde_json::to_vec(&mutated).unwrap();

        assert_ne!(
            comparable_bytes(&current, ArtifactComparison::OwnedJsonRegion),
            comparable_bytes(&desired, ArtifactComparison::OwnedJsonRegion),
            "narrowing the assertion must not blind it to what Switchback owns"
        );
    }

    #[test]
    fn key_order_and_whitespace_are_not_drift() {
        let a = br#"{"model":"m","effortLevel":"xhigh"}"#;
        let b = b"{\n  \"effortLevel\": \"xhigh\",\n  \"model\": \"m\"\n}\n";

        assert_eq!(
            comparable_bytes(a, ArtifactComparison::CanonicalJson),
            comparable_bytes(b, ArtifactComparison::CanonicalJson),
            "a reserialized document is not a changed document"
        );
    }

    #[test]
    fn unparseable_json_falls_back_to_bytes_rather_than_erroring() {
        let mangled = b"{not json";

        assert_eq!(
            comparable_bytes(mangled, ArtifactComparison::OwnedJsonRegion),
            mangled.to_vec(),
            "a hand-mangled file surfaces as drift, not as a hard error"
        );
    }

    #[test]
    fn pinned_permissions_survive_a_regen() {
        let mut root = shared_settings();
        // Pinned on both regions means no read of the global settings file at
        // all, so this asserts without touching process-wide `HOME`.
        merge_allowlisted_user_settings(&mut root, PermissionsMode::Pinned, SettingsMode::Pinned)
            .expect("pinned merge succeeds");

        assert_eq!(
            root["permissions"],
            json!({ "defaultMode": "auto", "deny": ["Bash(rm:*)"] }),
            "pinned is the lane that must stay stricter than the operator's global default"
        );
    }

    #[test]
    fn pinned_publishes_an_empty_derived_key_set() {
        assert!(derived_permission_keys(PermissionsMode::Pinned).is_empty());
        assert!(derived_setting_keys(SettingsMode::Pinned).is_empty());
        assert_eq!(
            derived_permission_keys(PermissionsMode::InheritAllowlisted),
            DERIVED_PERMISSION_KEYS,
            "inherit_allowlisted still declares what it mirrors"
        );
    }
}

/// Falsifiers F1-F6 for the `prime-agent` harness kind. Each test asserts the
/// post-implementation behavior, so the entire suite is RED at base (variant
/// unknown, fence absent, artifacts arm absent, zsh dispatch absent) and GREEN
/// after the implementation lands.
#[cfg(test)]
mod prime_agent_harness_tests {
    use super::*;
    use sb_core::Config;

    /// Minimal launch-profiles authority declaring one prime-agent preset and a
    /// prime launch profile bound to a `minimax` provider lane.
    const PRIME_AUTHORITY: &str = r#"{
        "schema": "switchback/launch-profiles@1",
        "provider_lanes": {
            "minimax": {
                "route": "minimax/MiniMax-M3",
                "requested_model": "MiniMax-M3",
                "transport": "tap",
                "credential_ref": { "kind": "env", "name": "MINIMAX_API_KEY" },
                "anthropic_tap_port": 18790,
                "min_fallbacks": 0
            }
        },
        "harness_presets": {
            "prime-minimax": {
                "harness": "prime-agent",
                "native_effort": "xhigh",
                "permissions_mode": "minimal",
                "mcp_mode": "none",
                "skills_mode": "disabled",
                "settings_mode": "minimal",
                "launch_args": [],
                "model_aliases": {}
            }
        },
        "capture_policies": {
            "observed": { "mode": "segmented_full_wire" }
        },
        "launch_profiles": {
            "prime-minimax": {
                "provider_lane": "minimax",
                "harness_preset": "prime-minimax",
                "capture_policy": "observed",
                "profile_label": "minimax-prime",
                "wrappers": ["prime-minimax"]
            }
        }
    }"#;

    /// Same as PRIME_AUTHORITY but the launch profile declares a `client_profile`,
    /// which is not legal for prime-agent lanes in v1.
    const PRIME_AUTHORITY_WITH_CLIENT_PROFILE: &str = r#"{
        "schema": "switchback/launch-profiles@1",
        "provider_lanes": {
            "minimax": {
                "route": "minimax/MiniMax-M3",
                "requested_model": "MiniMax-M3",
                "transport": "tap",
                "credential_ref": { "kind": "env", "name": "MINIMAX_API_KEY" },
                "anthropic_tap_port": 18790,
                "min_fallbacks": 0
            }
        },
        "harness_presets": {
            "prime-minimax": {
                "harness": "prime-agent",
                "native_effort": "xhigh",
                "permissions_mode": "minimal",
                "mcp_mode": "none",
                "skills_mode": "disabled",
                "settings_mode": "minimal"
            }
        },
        "capture_policies": {
            "observed": { "mode": "segmented_full_wire" }
        },
        "launch_profiles": {
            "prime-minimax": {
                "provider_lane": "minimax",
                "harness_preset": "prime-minimax",
                "capture_policy": "observed",
                "client_profile": "minimax-profile",
                "profile_label": "minimax-prime",
                "wrappers": ["prime-minimax"]
            }
        }
    }"#;

    /// Codex authority — kept here so F2's "Codex fence stays byte-identical"
    /// half can be exercised without depending on any file outside the test
    /// module.
    const CODEX_AUTHORITY: &str = r#"{
        "schema": "switchback/launch-profiles@1",
        "provider_lanes": {
            "minimax": {
                "route": "minimax/MiniMax-M3",
                "requested_model": "MiniMax-M3",
                "transport": "tap",
                "credential_ref": { "kind": "env", "name": "MINIMAX_API_KEY" },
                "anthropic_tap_port": 18790,
                "min_fallbacks": 0
            }
        },
        "harness_presets": {
            "codex-minimax": {
                "harness": "codex",
                "native_effort": "xhigh",
                "permissions_mode": "minimal",
                "mcp_mode": "none",
                "skills_mode": "disabled",
                "settings_mode": "minimal"
            }
        },
        "capture_policies": {
            "observed": { "mode": "segmented_full_wire" }
        },
        "launch_profiles": {
            "codex-minimax": {
                "provider_lane": "minimax",
                "harness_preset": "codex-minimax",
                "capture_policy": "observed",
                "profile_label": "minimax-codex",
                "wrappers": ["codex-minimax"]
            }
        }
    }"#;

    /// Claude-code authority that mirrors PRIME_AUTHORITY's lane so F5 can
    /// compare wrapper/settings output before/after the prime-agent arm is
    /// added.
    const CLAUDE_AUTHORITY: &str = r#"{
        "schema": "switchback/launch-profiles@1",
        "provider_lanes": {
            "minimax": {
                "route": "minimax/MiniMax-M3",
                "requested_model": "MiniMax-M3",
                "transport": "tap",
                "credential_ref": { "kind": "env", "name": "MINIMAX_API_KEY" },
                "anthropic_tap_port": 18790,
                "min_fallbacks": 0
            }
        },
        "harness_presets": {
            "claude-minimax": {
                "harness": "claude-code",
                "native_effort": "xhigh",
                "permissions_mode": "minimal",
                "mcp_mode": "none",
                "skills_mode": "disabled",
                "settings_mode": "minimal",
                "model_aliases": {
                    "default": "MiniMax-M3",
                    "subagent": "MiniMax-M3"
                }
            }
        },
        "capture_policies": {
            "observed": { "mode": "segmented_full_wire" }
        },
        "launch_profiles": {
            "claude-minimax": {
                "provider_lane": "minimax",
                "harness_preset": "claude-minimax",
                "capture_policy": "observed",
                "profile_label": "minimax-claude",
                "wrappers": ["claude-minimax"]
            }
        }
    }"#;

    fn cfg_minimax() -> Config {
        Config::from_yaml(
            r#"
server:
  bind: "127.0.0.1:18765"
  taps:
    - id: minimax-tap
      bind: "127.0.0.1:18790"
      upstream: "http://127.0.0.1:18765"
providers:
  - id: minimax
    type: openai_compatible
    base_url: "http://minimax-tap.invalid:18790/v1"
    api_key_env: "MINIMAX_API_KEY"
routes:
  - name: minimax
    match: { model: "minimax/MiniMax-M3" }
    targets: ["minimax/MiniMax-M3"]
"#,
        )
        .expect("valid minimax config")
    }

    fn cfg_minimax_with_client_profile() -> Config {
        Config::from_yaml(
            r#"
server:
  bind: "127.0.0.1:18765"
  taps:
    - id: minimax-tap
      bind: "127.0.0.1:18790"
      upstream: "http://127.0.0.1:18765"
providers:
  - id: minimax
    type: openai_compatible
    base_url: "http://minimax-tap.invalid:18790/v1"
    api_key_env: "MINIMAX_API_KEY"
routes:
  - name: minimax
    match: { model: "minimax/MiniMax-M3" }
    targets: ["minimax/MiniMax-M3"]
client_profiles:
  - id: minimax-profile
    kind: claude_code
    models: ["minimax/MiniMax-M3"]
"#,
        )
        .expect("valid minimax config with client profile")
    }

    fn prime_authority_doc() -> LaunchProfilesDocument {
        serde_json::from_str(PRIME_AUTHORITY).expect("prime authority parses")
    }

    fn prime_authority_doc_with_client_profile() -> LaunchProfilesDocument {
        serde_json::from_str(PRIME_AUTHORITY_WITH_CLIENT_PROFILE)
            .expect("prime+client_profile authority parses")
    }

    fn codex_authority_doc() -> LaunchProfilesDocument {
        serde_json::from_str(CODEX_AUTHORITY).expect("codex authority parses")
    }

    fn claude_authority_doc() -> LaunchProfilesDocument {
        serde_json::from_str(CLAUDE_AUTHORITY).expect("claude authority parses")
    }

    fn fixed_paths() -> ProfilePaths {
        ProfilePaths {
            authority: PathBuf::from("/tmp/sb-prime/authority.json"),
            lane_root: PathBuf::from("/tmp/sb-prime/lanes"),
            profile_root: PathBuf::from("/tmp/sb-prime/claude/_providers"),
            prime_profiles_root: PathBuf::from("/tmp/sb-prime/prime/_providers"),
            omp_profiles_root: PathBuf::from("/tmp/sb-prime/omp/profiles"),
            qwen_profiles_root: PathBuf::from("/tmp/sb-prime/qwen/profiles"),
            dsh_profiles_root: PathBuf::from("/tmp/sb-prime/dsh/profiles"),
            wrapper_root: PathBuf::from("/tmp/sb-prime/wrappers"),
            projection_root: PathBuf::from("/tmp/sb-prime/projections"),
        }
    }

    // ---- F1: parse ----

    #[test]
    fn f1_parses_prime_agent_harness_preset() {
        let doc = prime_authority_doc();
        let preset = doc
            .harness_presets
            .get("prime-minimax")
            .expect("preset present");
        assert_eq!(
            preset.harness.as_str(),
            "prime-agent",
            "kebab-case serde tag accepted"
        );
        assert_eq!(
            preset.harness.run_token(),
            "prime",
            "wrapper run token is `prime`"
        );
    }

    // ---- F2: resolve + Codex fence ----

    #[test]
    fn f2_resolves_prime_agent_profile_to_bundle() {
        let doc = prime_authority_doc();
        let bundle = resolve_launch_profile(&doc, &cfg_minimax(), "prime-minimax")
            .expect("prime preset + client_profile-free lane resolves to a bundle");
        assert_eq!(bundle.profile.harness, "prime_agent");
        assert_eq!(bundle.profile.harness_kind, "prime-agent");
        assert_eq!(bundle.profile.profile_label, "minimax-prime");
    }

    #[test]
    fn f2_codex_preset_still_bails_with_exact_message() {
        let doc = codex_authority_doc();
        let err = resolve_launch_profile(&doc, &cfg_minimax(), "codex-minimax")
            .expect_err("Codex fence must still bail");
        assert_eq!(
            err.to_string(),
            "Codex launch-profile materialization is not yet supported; refusing to leave native_effort declarative-only",
            "Codex bail text must stay byte-identical so live lanes can't read a different fence"
        );
    }

    // ---- F3: client_profile fence ----

    #[test]
    fn f3_prime_profile_with_client_profile_fails_with_clear_error() {
        let doc = prime_authority_doc_with_client_profile();
        let err = resolve_launch_profile(&doc, &cfg_minimax_with_client_profile(), "prime-minimax")
            .expect_err("prime-agent lane declaring client_profile must fail");
        assert!(
            err.to_string()
                .contains("prime-agent lanes must not declare client_profile"),
            "the new fence must name the harness explicitly; got: {err}"
        );
    }

    // ---- F4: artifacts ----

    #[test]
    fn f4_build_profile_artifacts_for_prime_emits_models_wrapper_conformance() {
        let doc = prime_authority_doc();
        let bundle = resolve_launch_profile(&doc, &cfg_minimax(), "prime-minimax")
            .expect("prime bundle resolves");
        let artifacts =
            build_profile_artifacts(&fixed_paths(), &bundle).expect("prime artifacts build");

        // (a) models.json artifact
        let models_artifact = artifacts
            .iter()
            .find(|a| a.kind == "prime_provider_models")
            .expect("prime_provider_models artifact present");
        assert!(
            models_artifact.path.ends_with("models.json"),
            "artifact lives at the lane's models.json; got path {}",
            models_artifact.path.display()
        );
        let v: Value =
            serde_json::from_str(&models_artifact.contents).expect("models.json is parseable JSON");
        let provider = &v["providers"]["switchback"];
        assert_eq!(
            provider["api"].as_str(),
            Some("openai-completions"),
            "prime uses openai-completions wire API"
        );
        let api_key = provider["apiKey"]
            .as_str()
            .expect("apiKey present")
            .to_string();
        assert!(
            api_key.starts_with('!'),
            "apiKey uses the `!`-resolver form so the real key never lands in the artifact; got {api_key:?}"
        );
        assert!(
            !api_key.contains("sk-"),
            "apiKey must not contain a literal secret prefix; got {api_key:?}"
        );
        assert_eq!(
            provider["compat"]["supportsDeveloperRole"],
            json!(false),
            "compat.supportsDeveloperRole must be false"
        );
        assert_eq!(
            provider["compat"]["supportsReasoningEffort"],
            json!(false),
            "compat.supportsReasoningEffort must be false"
        );
        let models = provider["models"].as_array().expect("models is an array");
        assert!(
            models.iter().any(|m| m.as_str() == Some("MiniMax-M3")),
            "preset primary model present in the artifact's models list"
        );

        // (b) wrapper contains marker + sb run prime --with + the three SB_LANE_PRIME_* vars
        let wrapper_artifact = artifacts
            .iter()
            .find(|a| a.kind == "wrapper")
            .expect("wrapper artifact present");
        let wrapper = &wrapper_artifact.contents;
        assert!(
            wrapper.contains(PROFILE_WRAPPER_OWNER_MARKER),
            "wrapper carries the owner marker"
        );
        assert!(
            wrapper.contains("sb run prime --with minimax"),
            "wrapper execs `sb run prime --with <lane>`"
        );
        assert!(
            wrapper.contains("SB_LANE_PRIME_MODEL="),
            "wrapper exports SB_LANE_PRIME_MODEL"
        );
        assert!(
            wrapper.contains("SB_LANE_PRIME_EFFORT="),
            "wrapper exports SB_LANE_PRIME_EFFORT"
        );
        assert!(
            wrapper.contains("SB_LANE_PRIME_CONFIG_DIR="),
            "wrapper exports SB_LANE_PRIME_CONFIG_DIR"
        );
        // The CONFIG_DIR export must point at the prime-agent config root
        // (parent of `_providers/`); `cli/sb` forwards it verbatim into
        // `PRIME_AGENT_CODING_AGENT_DIR`, and prime-agent looks for the
        // generated `models.json` at `<root>/_providers/<label>/models.json`.
        let config_dir_line = wrapper
            .lines()
            .find(|line| line.starts_with("export SB_LANE_PRIME_CONFIG_DIR="))
            .expect("SB_LANE_PRIME_CONFIG_DIR export present");
        assert!(
            config_dir_line.contains("prime"),
            "SB_LANE_PRIME_CONFIG_DIR points at the prime config root; got `{config_dir_line}`"
        );
        assert!(
            !config_dir_line.contains("_providers"),
            "SB_LANE_PRIME_CONFIG_DIR must NOT include `_providers/` — that is exactly the layout prime-agent resolves under it; got `{config_dir_line}`"
        );

        // (c) conformance projection carries `"harness": "prime-agent"`
        let conformance = artifacts
            .iter()
            .find(|a| a.kind == "conformance_projection")
            .expect("conformance_projection artifact present");
        let cv: Value = serde_json::from_str(&conformance.contents)
            .expect("conformance projection is parseable JSON");
        assert_eq!(
            cv["profile"]["harness"], "prime_agent",
            "conformance projection publishes the canonical Compound harness"
        );
        assert_eq!(cv["profile"]["harness_kind"], "prime-agent");
    }

    // ---- F5: no-regression for Claude wrapper + settings ----

    #[test]
    fn f5_claude_wrapper_byte_identical_to_pre_prime_snapshot() {
        let doc = claude_authority_doc();
        let bundle = resolve_launch_profile(&doc, &cfg_minimax(), "claude-minimax")
            .expect("claude bundle resolves");
        let wrapper = render_profile_wrapper(&fixed_paths(), &bundle);
        // The Claude wrapper must keep its pre-prime shape: the marker, the
        // Claude run token, the SB_LANE_CLAUDE_* exports, and no prime-agent
        // leakage. This is the regression fence for F5 — the prime-agent arm
        // must not bleed into the Claude branch.
        assert!(wrapper.starts_with("#!/bin/zsh\n# switchback-owned: launch-profile-wrapper@1\n"));
        assert!(wrapper.contains("exec sb run claude --with minimax"));
        assert!(wrapper.contains("export SB_LANE_CLAUDE_MODEL='MiniMax-M3'"));
        assert!(wrapper.contains("export SB_LANE_CLAUDE_SUBAGENT_MODEL='MiniMax-M3'"));
        assert!(wrapper.contains("export SB_LAUNCH_HARNESS='claude-code'"));
        // Negative: the Claude wrapper must NOT carry the prime-specific
        // exports or the prime run token.
        assert!(
            !wrapper.contains("SB_LANE_PRIME_"),
            "Claude wrapper leaked prime-agent exports: {wrapper}"
        );
        assert!(
            !wrapper.contains("sb run prime"),
            "Claude wrapper leaked `sb run prime`: {wrapper}"
        );
    }

    #[test]
    fn f5_claude_settings_owned_shape_remains_claude_only() {
        let doc = claude_authority_doc();
        let bundle = resolve_launch_profile(&doc, &cfg_minimax(), "claude-minimax")
            .expect("claude bundle resolves");
        let settings =
            render_launch_profile_settings(None, &bundle).expect("claude settings render");
        let v: Value = serde_json::from_str(&settings).expect("parseable JSON");
        // Snapshot the structural shape Switchback owns in a Claude settings
        // doc. Drift here means the prime-agent arm leaked into the Claude
        // branch — the whole point of F5.
        let owned = owned_settings_region(&v);
        let expected = json!({
            "model": "MiniMax-M3",
            "effortLevel": "xhigh",
            "env": {
                "ANTHROPIC_CUSTOM_MODEL_OPTION": "MiniMax-M3",
                "ANTHROPIC_DEFAULT_FABLE_MODEL": "MiniMax-M3",
                "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY": "1",
                "CLAUDE_CODE_SUBAGENT_MODEL": "MiniMax-M3"
            }
        });
        assert_eq!(owned, expected, "owned region must remain Claude-only");
    }

    // ---- F6: zsh dispatch ----

    #[test]
    fn f6_cli_sb_dispatch_handles_prime_lane() {
        // The repository carries no zsh test precedent; the packet allows a
        // Rust-side content test on the wrapper plus this file-content check
        // on cli/sb. We cover the wrapper content in F4; this assertion keeps
        // the dispatch shape honest so a future refactor cannot silently drop
        // the prime arm without a failing test.
        let cli_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .expect("workspace root resolvable from CARGO_MANIFEST_DIR")
            .join("cli/sb");
        let text = std::fs::read_to_string(&cli_path).expect("cli/sb readable");
        assert!(
            text.contains("prime:") || text.contains("prime:*"),
            "cli/sb dispatch must carry a prime-prefixed case arm; checked string not found"
        );
        assert!(
            text.contains("_run_prime_lane"),
            "cli/sb must define `_run_prime_lane`"
        );
        assert!(
            text.contains("PRIME_AGENT_CODING_AGENT_DIR"),
            "cli/sb must wire the PRIME_AGENT_CODING_AGENT_DIR env var"
        );
    }
}

#[cfg(test)]
mod direct_headless_harness_tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    const AUTHORITY: &str = r#"{
      "schema": "switchback/launch-profiles@1",
      "provider_lanes": {
        "omp-lane": {
          "route": "test/omp-model",
          "requested_model": "omp-model",
          "transport": "tap",
          "credential_ref": {"kind": "env", "name": "OMP_TEST_KEY"},
          "anthropic_tap_port": 18801,
          "openai_tap_port": 18801,
          "min_fallbacks": 0
        },
        "qwen-lane": {
          "route": "test/qwen-model",
          "requested_model": "qwen-model",
          "transport": "tap",
          "credential_ref": {"kind": "env", "name": "QWEN_TEST_KEY"},
          "anthropic_tap_port": 18802,
          "openai_tap_port": 18802,
          "min_fallbacks": 0
        },
        "dsh-lane": {
          "route": "deepseek-v4-flash",
          "requested_model": "deepseek-v4-flash",
          "transport": "tap",
          "credential_ref": {"kind": "env", "name": "DSH_TEST_KEY"},
          "anthropic_tap_port": 18803,
          "openai_tap_port": 18803,
          "min_fallbacks": 0
        }
      },
      "harness_presets": {
        "omp-headless": {
          "harness": "omp",
          "native_effort": "high",
          "permissions_mode": "minimal",
          "mcp_mode": "none",
          "skills_mode": "disabled",
          "settings_mode": "minimal",
          "expected_version": "18.0.0"
        },
        "qwen-headless": {
          "harness": "qwen-code",
          "native_effort": "default",
          "permissions_mode": "minimal",
          "mcp_mode": "none",
          "skills_mode": "disabled",
          "settings_mode": "minimal",
          "expected_version": "0.21.15"
        },
        "dsh-headless": {
          "harness": "deepseek-harness",
          "native_effort": "default",
          "permissions_mode": "minimal",
          "mcp_mode": "none",
          "skills_mode": "disabled",
          "settings_mode": "minimal",
          "expected_version": "0.1.0-rc.7"
        }
      },
      "capture_policies": {
        "observed": {"mode": "segmented_full_wire"}
      },
      "launch_profiles": {
        "omp-test": {
          "provider_lane": "omp-lane",
          "harness_preset": "omp-headless",
          "capture_policy": "observed",
          "client_credential_ref": {"kind": "env", "name": "SWITCHBACK_TEST_GATEWAY_KEY"}
        },
        "qwen-test": {
          "provider_lane": "qwen-lane",
          "harness_preset": "qwen-headless",
          "capture_policy": "observed",
          "client_credential_ref": {"kind": "env", "name": "SWITCHBACK_TEST_GATEWAY_KEY"}
        },
        "dsh-test": {
          "provider_lane": "dsh-lane",
          "harness_preset": "dsh-headless",
          "capture_policy": "observed",
          "client_credential_ref": {"kind": "env", "name": "SWITCHBACK_TEST_GATEWAY_KEY"}
        }
      }
    }"#;

    fn authority() -> LaunchProfilesDocument {
        serde_json::from_str(AUTHORITY).expect("direct harness authority parses")
    }

    fn config() -> Config {
        Config::from_yaml(
            r#"
server:
  bind: "127.0.0.1:18765"
providers:
  - id: test
    type: openai_compatible
    base_url: "http://provider.invalid/v1"
    api_key_env: "TEST_KEY"
routes:
  - name: omp
    match: { model: "test/omp-model" }
    targets: ["test/omp-model"]
  - name: qwen
    match: { model: "test/qwen-model" }
    targets: ["test/qwen-model"]
  - name: dsh
    match: { model: "deepseek-v4-flash" }
    targets: ["test/deepseek-v4-flash"]
"#,
        )
        .expect("direct harness config parses")
    }

    fn paths(root: &Path) -> ProfilePaths {
        ProfilePaths {
            authority: root.join("authority.json"),
            lane_root: root.join("lanes"),
            profile_root: root.join("claude/_providers"),
            prime_profiles_root: root.join("prime/_providers"),
            omp_profiles_root: root.join("omp/profiles"),
            qwen_profiles_root: root.join("qwen/profiles"),
            dsh_profiles_root: root.join("dsh/profiles"),
            wrapper_root: root.join("bin"),
            projection_root: root.join("conformance"),
        }
    }

    fn bundle(name: &str) -> ResolvedProfileBundle {
        resolve_launch_profile(&authority(), &config(), name).expect("profile resolves")
    }

    fn artifact<'a>(
        artifacts: &'a [PlannedProfileArtifact],
        kind: &str,
    ) -> &'a PlannedProfileArtifact {
        artifacts
            .iter()
            .find(|artifact| artifact.kind == kind)
            .unwrap_or_else(|| panic!("missing {kind} artifact"))
    }

    #[test]
    fn parses_distinct_kinds_and_rejects_unknown_kind() {
        let doc = authority();
        assert_eq!(doc.harness_presets["omp-headless"].harness.as_str(), "omp");
        assert_eq!(
            doc.harness_presets["qwen-headless"].harness.as_str(),
            "qwen-code"
        );
        assert_eq!(
            doc.harness_presets["dsh-headless"].harness.as_str(),
            "deepseek-harness"
        );
        let invalid = AUTHORITY.replacen("\"harness\": \"omp\"", "\"harness\": \"other\"", 1);
        assert!(
            serde_json::from_str::<LaunchProfilesDocument>(&invalid).is_err(),
            "unknown harness kinds fail closed"
        );
    }

    #[test]
    fn omp_renderer_uses_only_current_omp_contract() {
        let bundle = bundle("omp-test");
        let artifacts =
            build_profile_artifacts(&paths(Path::new("/tmp/sb-direct")), &bundle).unwrap();
        let wrapper = &artifact(&artifacts, "wrapper").contents;
        assert!(wrapper.contains(PROFILE_WRAPPER_OWNER_MARKER));
        assert!(wrapper.contains("export SB_LAUNCH_PROFILE_ID='omp-test'"));
        assert!(wrapper.contains("export SB_LAUNCH_PROVIDER_LANE='omp-lane'"));
        assert!(wrapper.contains("export SB_LAUNCH_REQUESTED_MODEL='omp-model'"));
        assert!(wrapper.contains("export SB_LAUNCH_PERMISSION_POSTURE='always_ask'"));
        assert!(wrapper.contains("export SB_LAUNCH_CREDENTIAL_ENV='SWITCHBACK_TEST_GATEWAY_KEY'"));
        assert!(!wrapper.contains("OMP_TEST_KEY"));
        assert!(wrapper.contains("exec omp --cwd=\"$PWD\" --provider=switchback"));
        assert!(wrapper.contains("--model='switchback/test/omp-model'"));
        assert!(wrapper.contains("--print"));
        assert!(wrapper.contains("--approval-mode=always-ask"));
        assert!(wrapper.contains("--thinking=high"));
        assert!(!wrapper.contains("--mcp"));
        assert!(!wrapper.contains("SB_LANE_PRIME_"));
        assert!(!wrapper.contains(":18765"));
        let models = &artifact(&artifacts, "omp_provider_models").contents;
        assert!(models.contains("baseUrl: 'http://127.0.0.1:18801/v1'"));
        assert!(models.contains("x-switchback-launch-profile: 'omp-test'"));
        assert!(models.contains("id: 'test/omp-model'"));
    }

    #[test]
    fn qwen_renderer_is_process_cwd_structured_and_non_yolo() {
        let bundle = bundle("qwen-test");
        let artifacts =
            build_profile_artifacts(&paths(Path::new("/tmp/sb-direct")), &bundle).unwrap();
        let wrapper = &artifact(&artifacts, "wrapper").contents;
        assert!(wrapper.contains("export SB_LAUNCH_WORKSPACE_MODE='process_cwd'"));
        assert!(wrapper.contains("export SB_LAUNCH_CREDENTIAL_ENV='SWITCHBACK_TEST_GATEWAY_KEY'"));
        assert!(!wrapper.contains("QWEN_TEST_KEY"));
        assert!(wrapper.contains("export OPENAI_BASE_URL=\"$SB_LAUNCH_CAPTURE_ENDPOINT\""));
        assert!(wrapper.contains("exec qwen --safe-mode --approval-mode=default"));
        assert!(wrapper.contains("--output-format=stream-json --prompt \"$prompt\""));
        assert!(!wrapper.contains("--yolo"));
        assert!(!wrapper.contains("cd "));
        assert!(!wrapper.contains(":18765"));
        let settings: Value =
            serde_json::from_str(&artifact(&artifacts, "qwen_profile_settings").contents)
                .expect("Qwen settings JSON");
        assert_eq!(
            settings["modelProviders"]["openai"][0]["baseUrl"],
            "http://127.0.0.1:18802/v1"
        );
        assert_eq!(
            settings["modelProviders"]["openai"][0]["id"],
            "test/qwen-model"
        );
        assert_eq!(
            settings["modelProviders"]["openai"][0]["generationConfig"]["customHeaders"]
                ["x-switchback-launch-profile"],
            "qwen-test"
        );
        assert!(settings.get("mcpServers").is_none());
        assert!(settings.get("hooks").is_none());
        assert!(settings.get("skills").is_none());
    }

    #[test]
    fn dsh_renderer_exposes_pre_1_0_limits_without_fake_flags() {
        let bundle = bundle("dsh-test");
        let artifacts =
            build_profile_artifacts(&paths(Path::new("/tmp/sb-direct")), &bundle).unwrap();
        let wrapper = &artifact(&artifacts, "wrapper").contents;
        assert!(wrapper.contains("export SB_LAUNCH_CREDENTIAL_ENV='SWITCHBACK_TEST_GATEWAY_KEY'"));
        assert!(!wrapper.contains("DSH_TEST_KEY"));
        assert!(wrapper.contains("export DEEPSEEK_BASE_URL=\"$SB_LAUNCH_CAPTURE_ENDPOINT\""));
        assert!(wrapper.contains("export DSH_PERMISSION_MODE=workspace-write"));
        assert!(wrapper.contains("exec dsh --profile headless \"$prompt\""));
        assert!(!wrapper.contains("dsh --model"));
        assert!(!wrapper.contains("--output-format"));
        assert!(!wrapper.contains("--yolo"));
        assert!(!wrapper.contains(":18765"));
        let projection: Value =
            serde_json::from_str(&artifact(&artifacts, "conformance_projection").contents)
                .expect("conformance JSON");
        assert_eq!(projection["profile"]["harness"], "deepseek_harness");
        assert_eq!(projection["profile"]["harness_kind"], "deepseek-harness");
        assert_eq!(
            projection["capture_identity"]["wire_headers"],
            "native_dsh_attribution_only"
        );
        assert!(projection["unsupported_floors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "model_flag"));
        assert!(projection["capture_identity"]["warning"]
            .as_str()
            .unwrap()
            .contains("pre-1.0"));
    }

    #[test]
    fn unsupported_combinations_refuse_with_harness_reason() {
        let qwen_mcp = AUTHORITY.replacen(
            "\"mcp_mode\": \"none\"",
            "\"mcp_mode\": \"selected\", \"mcp_servers\": [\"browser\"]",
            2,
        );
        let doc: LaunchProfilesDocument = serde_json::from_str(&qwen_mcp).unwrap();
        let error = resolve_launch_profile(&doc, &config(), "qwen-test").unwrap_err();
        assert!(error.to_string().contains("MCP is not adopted"));
        let qwen_yolo = AUTHORITY.replace(
            "\"expected_version\": \"0.21.15\"",
            "\"expected_version\": \"0.21.15\", \"launch_args\": [\"--yolo\"]",
        );
        let doc: LaunchProfilesDocument = serde_json::from_str(&qwen_yolo).unwrap();
        let error = resolve_launch_profile(&doc, &config(), "qwen-test").unwrap_err();
        assert!(error
            .to_string()
            .contains("renderer owns the complete supported headless flag contract"));

        let dsh_model = AUTHORITY.replace(
            "\"requested_model\": \"deepseek-v4-flash\"",
            "\"requested_model\": \"deepseek-v4-pro\"",
        );
        let doc: LaunchProfilesDocument = serde_json::from_str(&dsh_model).unwrap();
        let error = resolve_launch_profile(&doc, &config(), "dsh-test").unwrap_err();
        assert!(error.to_string().contains("DSH has no model flag"));
        let dsh_prefixed_route = AUTHORITY.replace(
            "\"route\": \"deepseek-v4-flash\"",
            "\"route\": \"test/deepseek-v4-flash\"",
        );
        let doc: LaunchProfilesDocument = serde_json::from_str(&dsh_prefixed_route).unwrap();
        let error = resolve_launch_profile(&doc, &config(), "dsh-test").unwrap_err();
        assert!(error.to_string().contains("cannot exact-match route"));
        let dsh_output = AUTHORITY.replace(
            "\"expected_version\": \"0.1.0-rc.7\"",
            "\"expected_version\": \"0.1.0-rc.7\", \"launch_args\": [\"--output-format=json\"]",
        );
        let doc: LaunchProfilesDocument = serde_json::from_str(&dsh_output).unwrap();
        let error = resolve_launch_profile(&doc, &config(), "dsh-test").unwrap_err();
        assert!(error
            .to_string()
            .contains("renderer owns the complete supported headless flag contract"));
        let upstream_credential = AUTHORITY.replacen(
            "\"name\": \"SWITCHBACK_TEST_GATEWAY_KEY\"",
            "\"name\": \"OMP_TEST_KEY\"",
            1,
        );
        let doc: LaunchProfilesDocument = serde_json::from_str(&upstream_credential).unwrap();
        let error = resolve_launch_profile(&doc, &config(), "omp-test").unwrap_err();
        assert!(error
            .to_string()
            .contains("client_credential_ref must be distinct"));

        let bare = AUTHORITY.replacen(
            "\"openai_tap_port\": 18801",
            "\"openai_tap_port\": 18765",
            1,
        );
        let doc: LaunchProfilesDocument = serde_json::from_str(&bare).unwrap();
        let error = resolve_launch_profile(&doc, &config(), "omp-test").unwrap_err();
        assert!(error.to_string().contains("refuses bare gateway :18765"));
    }

    #[test]
    fn wrapper_drift_is_detected() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "switchback-direct-profile-drift-{}-{nanos}",
            std::process::id()
        ));
        let bundle = bundle("omp-test");
        let artifacts = build_profile_artifacts(&paths(&root), &bundle).unwrap();
        let wrapper = artifact(&artifacts, "wrapper").clone();
        fs::create_dir_all(wrapper.path.parent().unwrap()).unwrap();
        fs::write(&wrapper.path, &wrapper.contents).unwrap();
        set_mode(&wrapper.path, wrapper.mode).unwrap();
        assert!(!artifact_statuses(std::slice::from_ref(&wrapper)).unwrap()[0].changed);
        fs::write(&wrapper.path, "# hand-edited\n").unwrap();
        assert!(artifact_statuses(std::slice::from_ref(&wrapper)).unwrap()[0].changed);
        let _ = fs::remove_dir_all(root);
    }
}
