use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use clap::{Subcommand, ValueEnum};
use sb_core::{AuthConfig, ClientProfileKind, Config};
use sb_runtime::Engine;
use serde::Serialize;

use crate::config_cli::{init_config_file, write_file_atomic, InitTemplate};
use crate::print_json;

#[derive(Subcommand)]
pub(crate) enum SetupCmd {
    /// Create/inspect the native Codex + Claude Code setup path.
    Native {
        #[arg(long, default_value = "switchback.yaml")]
        config: PathBuf,
        /// Replace the config file with the native-client starter template.
        #[arg(long)]
        force: bool,
        /// Limit reporting to one native client.
        #[arg(long, value_enum, default_value_t = NativeClientTarget::All)]
        client: NativeClientTarget,
    },
    /// Inspect first-party native relay readiness without enabling relay.
    NativeRelay {
        #[command(subcommand)]
        action: NativeRelayCmd,
    },
    /// List or install low-friction setup packs.
    Pack {
        #[command(subcommand)]
        action: SetupPackCmd,
        #[arg(long, global = true, default_value = "switchback.yaml")]
        config: PathBuf,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum NativeClientTarget {
    All,
    Codex,
    ClaudeCode,
}

#[derive(Subcommand)]
pub(crate) enum NativeRelayCmd {
    /// Print the tracked relay implementation plan.
    Plan,
    /// Audit local native-client installation/auth-store shape.
    Audit {
        /// Limit reporting to one native client.
        #[arg(long, value_enum, default_value_t = NativeClientTarget::All)]
        client: NativeClientTarget,
    },
}

#[derive(Subcommand)]
pub(crate) enum SetupPackCmd {
    /// List built-in setup packs.
    List,
    /// Install a setup pack into the config.
    Install {
        /// Pack id. Currently: native-token-adapter.
        pack: String,
        /// Replace existing pack-owned entries when ids already exist.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Serialize)]
struct NativeSetupReport {
    schema: &'static str,
    ok: bool,
    config: PathBuf,
    template: &'static str,
    created_config: bool,
    replaced_config: bool,
    validation: ValidationReport,
    clients: Vec<NativeClientReport>,
    next_commands: Vec<String>,
    warnings: Vec<String>,
}

#[derive(Serialize)]
struct ValidationReport {
    ok: bool,
    problems: Vec<String>,
}

#[derive(Serialize)]
struct NativeClientReport {
    id: &'static str,
    kind: &'static str,
    protocol: &'static str,
    profile_ids: Vec<String>,
    native_account_refs: Vec<String>,
    native_account_configured: bool,
    token_available: bool,
    token_sources: Vec<TokenSourceReport>,
    smoke_command: String,
    setup_pack: &'static str,
}

#[derive(Serialize)]
struct TokenSourceReport {
    kind: &'static str,
    label: String,
    configured: bool,
    available: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum SourceSpec {
    Env { name: String },
    File { path: String, pointer: String },
    Vault { name: String },
}

#[derive(Serialize)]
struct SetupPackListReport {
    schema: &'static str,
    packs: Vec<SetupPackInfo>,
}

#[derive(Serialize)]
struct SetupPackInfo {
    id: &'static str,
    title: &'static str,
    description: &'static str,
    adds: Vec<&'static str>,
    install: &'static str,
}

#[derive(Serialize)]
struct SetupPackInstallReport {
    schema: &'static str,
    ok: bool,
    pack: &'static str,
    config: PathBuf,
    initialized_config: bool,
    wrote_config: bool,
    changes: Vec<PackChange>,
    next_commands: Vec<String>,
    warnings: Vec<String>,
}

#[derive(Serialize)]
struct PackChange {
    kind: &'static str,
    id: String,
    action: &'static str,
}

#[derive(Serialize)]
struct NativeRelayPlanReport {
    schema: &'static str,
    status: &'static str,
    plan_path: &'static str,
    provider_kinds: Vec<&'static str>,
    invariant: &'static str,
    phases: Vec<&'static str>,
}

#[derive(Serialize)]
struct NativeRelayAuditReport {
    schema: &'static str,
    status: &'static str,
    relay_implemented: bool,
    adapter_gate: &'static str,
    clients: Vec<NativeRelayClientAudit>,
    required_fixtures: Vec<&'static str>,
    boundaries: Vec<&'static str>,
    next_commands: Vec<&'static str>,
}

#[derive(Serialize)]
struct NativeRelayClientAudit {
    id: &'static str,
    command: &'static str,
    installed: bool,
    version: Option<String>,
    version_error: Option<String>,
    auth_store: AuthStoreAudit,
    planned_provider_kind: &'static str,
}

#[derive(Serialize)]
struct AuthStoreAudit {
    path: &'static str,
    exists: bool,
    access_token_pointer: &'static str,
    access_token_present: bool,
    inspected_shape_only: bool,
}

pub(crate) fn run_setup_cmd(action: SetupCmd, json: bool) -> anyhow::Result<()> {
    match action {
        SetupCmd::Native {
            config,
            force,
            client,
        } => {
            let report = native_setup_report(&config, force, client)?;
            if json {
                print_json(&report)?;
            } else {
                print_native_setup_text(&report);
            }
            if !report.ok {
                std::process::exit(1);
            }
        }
        SetupCmd::NativeRelay { action } => match action {
            NativeRelayCmd::Plan => {
                let report = native_relay_plan_report();
                if json {
                    print_json(&report)?;
                } else {
                    print_native_relay_plan_text(&report);
                }
            }
            NativeRelayCmd::Audit { client } => {
                let report = native_relay_audit_report(client);
                if json {
                    print_json(&report)?;
                } else {
                    print_native_relay_audit_text(&report);
                }
            }
        },
        SetupCmd::Pack { action, config } => match action {
            SetupPackCmd::List => {
                let report = setup_pack_list_report();
                if json {
                    print_json(&report)?;
                } else {
                    for pack in report.packs {
                        println!("{} - {}", pack.id, pack.title);
                        println!("  {}", pack.description);
                        println!("  install: {}", pack.install);
                    }
                }
            }
            SetupPackCmd::Install { pack, force } => {
                let report = setup_pack_install_report(&config, &pack, force)?;
                if json {
                    print_json(&report)?;
                } else {
                    println!("installed pack `{}` into {}", report.pack, config.display());
                    for change in &report.changes {
                        println!("{} {}: {}", change.action, change.kind, change.id);
                    }
                    for command in &report.next_commands {
                        println!("next: {command}");
                    }
                }
            }
        },
    }
    Ok(())
}

fn native_setup_report(
    config: &Path,
    force: bool,
    target: NativeClientTarget,
) -> anyhow::Result<NativeSetupReport> {
    let existed = config.exists();
    let mut created_config = false;
    let mut replaced_config = false;
    if !existed || force {
        init_config_file(config, true, InitTemplate::NativeClients)?;
        created_config = !existed;
        replaced_config = existed;
    }

    let (cfg, validation) = load_and_validate_config(config);
    let clients = match cfg.as_ref() {
        Some(cfg) => native_client_reports(cfg, target),
        None => native_client_reports_from_defaults(target),
    };
    let mut warnings = Vec::new();
    if clients
        .iter()
        .any(|client| !client.native_account_configured)
    {
        warnings.push(
            "native token sources were inspected, but at least one native token adapter account is not active in the config; this is not the same as first-party Codex/Claude Code subscription relay".to_string(),
        );
    }
    if clients
        .iter()
        .any(|client| client.native_account_configured && !client.token_available)
    {
        warnings.push(
            "a native OAuth provider account is configured but no readable token source was detected"
                .to_string(),
        );
    }

    Ok(NativeSetupReport {
        schema: "switchback/setup-native@1",
        ok: validation.ok,
        config: config.to_path_buf(),
        template: InitTemplate::NativeClients.id(),
        created_config,
        replaced_config,
        validation,
        clients,
        next_commands: native_next_commands(config),
        warnings,
    })
}

fn load_and_validate_config(path: &Path) -> (Option<Config>, ValidationReport) {
    match Config::from_path(path) {
        Ok(cfg) => match Engine::validate_config(&cfg) {
            Ok(()) => (
                Some(cfg),
                ValidationReport {
                    ok: true,
                    problems: Vec::new(),
                },
            ),
            Err(e) => (
                Some(cfg),
                ValidationReport {
                    ok: false,
                    problems: e.split("; ").map(str::to_string).collect(),
                },
            ),
        },
        Err(e) => (
            None,
            ValidationReport {
                ok: false,
                problems: vec![e.to_string()],
            },
        ),
    }
}

fn native_client_reports(cfg: &Config, target: NativeClientTarget) -> Vec<NativeClientReport> {
    native_client_kinds(target)
        .into_iter()
        .map(|kind| native_client_report(cfg, kind))
        .collect()
}

fn native_client_reports_from_defaults(target: NativeClientTarget) -> Vec<NativeClientReport> {
    native_client_kinds(target)
        .into_iter()
        .map(|kind| {
            let specs = default_source_specs(kind);
            let token_sources = detect_token_sources(specs);
            NativeClientReport {
                id: native_client_id(kind),
                kind: native_auth_kind(kind),
                protocol: native_protocol(kind),
                profile_ids: Vec::new(),
                native_account_refs: Vec::new(),
                native_account_configured: false,
                token_available: token_sources
                    .iter()
                    .any(|source| source.available == Some(true)),
                token_sources,
                smoke_command: native_smoke_command(kind),
                setup_pack: "native-token-adapter",
            }
        })
        .collect()
}

fn native_client_report(cfg: &Config, kind: ClientProfileKind) -> NativeClientReport {
    let profile_ids = cfg
        .client_profiles
        .iter()
        .filter(|profile| profile.kind == kind)
        .map(|profile| profile.id.clone())
        .collect::<Vec<_>>();
    let native_account_refs = native_account_refs(cfg, kind);
    let specs = source_specs_for_config(cfg, kind);
    let token_sources = detect_token_sources(specs);
    NativeClientReport {
        id: native_client_id(kind),
        kind: native_auth_kind(kind),
        protocol: native_protocol(kind),
        profile_ids,
        native_account_configured: !native_account_refs.is_empty(),
        native_account_refs,
        token_available: token_sources
            .iter()
            .any(|source| source.available == Some(true)),
        token_sources,
        smoke_command: native_smoke_command(kind),
        setup_pack: "native-token-adapter",
    }
}

fn native_client_kinds(target: NativeClientTarget) -> Vec<ClientProfileKind> {
    match target {
        NativeClientTarget::All => vec![ClientProfileKind::Codex, ClientProfileKind::ClaudeCode],
        NativeClientTarget::Codex => vec![ClientProfileKind::Codex],
        NativeClientTarget::ClaudeCode => vec![ClientProfileKind::ClaudeCode],
    }
}

fn native_account_refs(cfg: &Config, kind: ClientProfileKind) -> Vec<String> {
    cfg.providers
        .iter()
        .flat_map(|provider| {
            provider.accounts.iter().filter_map(move |account| {
                auth_matches_kind(&account.auth, kind)
                    .then(|| format!("{}/{}", provider.id, account.id))
            })
        })
        .collect()
}

fn source_specs_for_config(cfg: &Config, kind: ClientProfileKind) -> Vec<SourceSpec> {
    let mut specs = BTreeSet::new();
    for provider in &cfg.providers {
        for account in &provider.accounts {
            add_auth_source_specs(&mut specs, &account.auth, kind);
        }
    }
    if specs.is_empty() {
        specs.extend(default_source_specs(kind));
    }
    specs.into_iter().collect()
}

fn add_auth_source_specs(
    specs: &mut BTreeSet<SourceSpec>,
    auth: &AuthConfig,
    kind: ClientProfileKind,
) {
    match (kind, auth) {
        (
            ClientProfileKind::Codex,
            AuthConfig::CodexOauth {
                token_env,
                token_vault,
                token_file,
                access_token_pointer,
            },
        )
        | (
            ClientProfileKind::ClaudeCode,
            AuthConfig::ClaudeCodeOauth {
                token_env,
                token_vault,
                token_file,
                access_token_pointer,
            },
        ) => {
            if let Some(name) = token_env
                .as_deref()
                .filter(|value| !value.trim().is_empty())
            {
                specs.insert(SourceSpec::Env {
                    name: name.to_string(),
                });
            }
            if let Some(name) = token_vault
                .as_deref()
                .filter(|value| !value.trim().is_empty())
            {
                specs.insert(SourceSpec::Vault {
                    name: name.to_string(),
                });
            }
            if let Some(path) = token_file
                .as_deref()
                .filter(|value| !value.trim().is_empty())
            {
                specs.insert(SourceSpec::File {
                    path: path.to_string(),
                    pointer: access_token_pointer.clone(),
                });
            }
        }
        _ => {}
    }
}

fn default_source_specs(kind: ClientProfileKind) -> Vec<SourceSpec> {
    match kind {
        ClientProfileKind::Codex => vec![
            SourceSpec::Env {
                name: "CODEX_ACCESS_TOKEN".to_string(),
            },
            SourceSpec::File {
                path: "${HOME}/.codex/auth.json".to_string(),
                pointer: "/tokens/access_token".to_string(),
            },
        ],
        ClientProfileKind::ClaudeCode => vec![
            SourceSpec::Env {
                name: "CLAUDE_CODE_OAUTH_TOKEN".to_string(),
            },
            SourceSpec::File {
                path: "${HOME}/.claude/.credentials.json".to_string(),
                pointer: "/claudeAiOauth/accessToken".to_string(),
            },
        ],
    }
}

fn detect_token_sources(specs: Vec<SourceSpec>) -> Vec<TokenSourceReport> {
    specs
        .into_iter()
        .map(|spec| match spec {
            SourceSpec::Env { name } => {
                let available = std::env::var(&name)
                    .ok()
                    .map(|value| !value.trim().is_empty())
                    .unwrap_or(false);
                TokenSourceReport {
                    kind: "env",
                    label: name,
                    configured: true,
                    available: Some(available),
                }
            }
            SourceSpec::File { path, pointer } => {
                let expanded = expand_home(&path);
                let available = json_pointer_has_nonempty_string(&expanded, &pointer);
                TokenSourceReport {
                    kind: "native_token_file",
                    label: format!("{} {}", path, pointer),
                    configured: true,
                    available: Some(available),
                }
            }
            SourceSpec::Vault { name } => TokenSourceReport {
                kind: "vault",
                label: name,
                configured: true,
                available: None,
            },
        })
        .collect()
}

fn expand_home(path: &str) -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if path == "${HOME}" || path == "~" {
        return home.unwrap_or_else(|| PathBuf::from(path));
    }
    if let Some(rest) = path.strip_prefix("${HOME}/") {
        if let Some(home) = home {
            return home.join(rest);
        }
    }
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = home {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}

fn json_pointer_has_nonempty_string(path: &Path, pointer: &str) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };
    value
        .pointer(pointer)
        .and_then(serde_json::Value::as_str)
        .map(|token| !token.trim().is_empty())
        .unwrap_or(false)
}

fn auth_matches_kind(auth: &AuthConfig, kind: ClientProfileKind) -> bool {
    matches!(
        (kind, auth),
        (ClientProfileKind::Codex, AuthConfig::CodexOauth { .. })
            | (
                ClientProfileKind::ClaudeCode,
                AuthConfig::ClaudeCodeOauth { .. }
            )
    )
}

fn native_client_id(kind: ClientProfileKind) -> &'static str {
    match kind {
        ClientProfileKind::Codex => "codex",
        ClientProfileKind::ClaudeCode => "claude-code",
    }
}

fn native_auth_kind(kind: ClientProfileKind) -> &'static str {
    match kind {
        ClientProfileKind::Codex => "codex_oauth",
        ClientProfileKind::ClaudeCode => "claude_code_oauth",
    }
}

fn native_protocol(kind: ClientProfileKind) -> &'static str {
    match kind {
        ClientProfileKind::Codex => "openai_responses",
        ClientProfileKind::ClaudeCode => "anthropic_messages",
    }
}

fn native_smoke_command(kind: ClientProfileKind) -> String {
    match kind {
        ClientProfileKind::Codex => "OPENAI_BASE_URL=http://127.0.0.1:8765/v1 OPENAI_API_KEY=$SWITCHBACK_API_KEY codex exec --model coding \"ping through Switchback\"".to_string(),
        ClientProfileKind::ClaudeCode => "ANTHROPIC_BASE_URL=http://127.0.0.1:8765 ANTHROPIC_AUTH_TOKEN=$SWITCHBACK_API_KEY claude -p \"ping through Switchback\"".to_string(),
    }
}

fn native_next_commands(config: &Path) -> Vec<String> {
    let mut commands = InitTemplate::NativeClients.next_commands(config);
    commands.push("switchback setup pack list".to_string());
    commands.push(format!(
        "switchback setup pack install native-token-adapter --config {}",
        config.display()
    ));
    commands
}

fn print_native_setup_text(report: &NativeSetupReport) {
    if report.created_config {
        println!("created {}", report.config.display());
    } else if report.replaced_config {
        println!("replaced {}", report.config.display());
    } else {
        println!("inspected {}", report.config.display());
    }
    println!(
        "validation: {}",
        if report.validation.ok { "ok" } else { "failed" }
    );
    for problem in &report.validation.problems {
        println!("problem: {problem}");
    }
    for client in &report.clients {
        println!(
            "{}: profile(s)={}, native_account={}, token_source={}",
            client.id,
            if client.profile_ids.is_empty() {
                "-".to_string()
            } else {
                client.profile_ids.join(",")
            },
            if client.native_account_configured {
                client.native_account_refs.join(",")
            } else {
                "not active".to_string()
            },
            if client.token_available {
                "available"
            } else {
                "not detected"
            }
        );
    }
    for warning in &report.warnings {
        println!("warning: {warning}");
    }
    for command in &report.next_commands {
        println!("next: {command}");
    }
}

fn native_relay_plan_report() -> NativeRelayPlanReport {
    NativeRelayPlanReport {
        schema: "switchback/native-relay-plan@1",
        status: "planned_not_implemented",
        plan_path: "NATIVE_RELAY.md",
        provider_kinds: vec!["codex_native_relay", "claude_code_native_relay"],
        invariant: "relay provider kinds parse as intent but fail validation until audited wire fixtures and adapters exist",
        phases: vec![
            "protocol audit",
            "auth-store contract",
            "typed relay providers",
            "relay adapters",
            "conformance suite",
            "dashboard and setup UX",
        ],
    }
}

fn print_native_relay_plan_text(report: &NativeRelayPlanReport) {
    println!("status: {}", report.status);
    println!("plan: {}", report.plan_path);
    println!("provider kinds: {}", report.provider_kinds.join(", "));
    println!("invariant: {}", report.invariant);
    for phase in &report.phases {
        println!("phase: {phase}");
    }
}

fn native_relay_audit_report(target: NativeClientTarget) -> NativeRelayAuditReport {
    NativeRelayAuditReport {
        schema: "switchback/native-relay-audit@1",
        status: "planned_not_implemented",
        relay_implemented: false,
        adapter_gate: "AdapterRegistry rejects codex_native_relay and claude_code_native_relay until relay codecs/transports are implemented from sanitized fixtures",
        clients: native_client_kinds(target)
            .into_iter()
            .map(native_relay_client_audit)
            .collect(),
        required_fixtures: vec![
            "model_list",
            "non_stream_request_response",
            "stream_request_first_byte_and_finish",
            "tool_call_and_tool_result",
            "token_count",
            "expired_token_or_refresh_failure",
            "client_abort_before_first_byte",
            "client_abort_after_first_byte",
        ],
        boundaries: vec![
            "no token values printed",
            "no browser-cookie or MITM relay",
            "no public API bearer adapter labeled as subscription relay",
            "provider wire JSON stays out of sb-core",
        ],
        next_commands: vec![
            "switchback setup native-relay plan",
            "switchback schema commands",
            "switchback config validate --config switchback.yaml",
        ],
    }
}

fn print_native_relay_audit_text(report: &NativeRelayAuditReport) {
    println!("status: {}", report.status);
    println!("relay implemented: {}", report.relay_implemented);
    println!("adapter gate: {}", report.adapter_gate);
    for client in &report.clients {
        println!(
            "{}: installed={}, version={}, auth_store_exists={}, token_present={}",
            client.id,
            client.installed,
            client.version.as_deref().unwrap_or("-"),
            client.auth_store.exists,
            client.auth_store.access_token_present,
        );
    }
    for fixture in &report.required_fixtures {
        println!("required fixture: {fixture}");
    }
}

fn native_relay_client_audit(kind: ClientProfileKind) -> NativeRelayClientAudit {
    let (id, command, auth_path, pointer, provider_kind) = match kind {
        ClientProfileKind::Codex => (
            "codex",
            "codex",
            "${HOME}/.codex/auth.json",
            "/tokens/access_token",
            "codex_native_relay",
        ),
        ClientProfileKind::ClaudeCode => (
            "claude-code",
            "claude",
            "${HOME}/.claude/.credentials.json",
            "/claudeAiOauth/accessToken",
            "claude_code_native_relay",
        ),
    };
    let installed = command_exists(command);
    let (version, version_error) = if installed {
        command_version(command)
    } else {
        (None, Some("command not found on PATH".to_string()))
    };
    let expanded_auth_path = expand_home(auth_path);
    NativeRelayClientAudit {
        id,
        command,
        installed,
        version,
        version_error,
        auth_store: AuthStoreAudit {
            path: auth_path,
            exists: expanded_auth_path.exists(),
            access_token_pointer: pointer,
            access_token_present: json_pointer_has_nonempty_string(&expanded_auth_path, pointer),
            inspected_shape_only: true,
        },
        planned_provider_kind: provider_kind,
    }
}

fn command_exists(command: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let candidate = dir.join(command);
        candidate.is_file()
    })
}

fn command_version(command: &str) -> (Option<String>, Option<String>) {
    match Command::new(command).arg("--version").output() {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let version = if stdout.is_empty() { stderr } else { stdout };
            if version.is_empty() {
                (None, Some("version command returned no output".to_string()))
            } else {
                (Some(version), None)
            }
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let message = if stderr.is_empty() {
                format!("version command exited with {}", output.status)
            } else {
                stderr
            };
            (None, Some(message))
        }
        Err(e) => (None, Some(e.to_string())),
    }
}

fn setup_pack_list_report() -> SetupPackListReport {
    SetupPackListReport {
        schema: "switchback/setup-packs@1",
        packs: vec![SetupPackInfo {
            id: "native-token-adapter",
            title: "Native token-source adapter accounts",
            description: "Adds separate Codex/Claude Code token-source provider accounts and client profiles. This is a direct bearer-token adapter, not first-party subscription relay.",
            adds: vec![
                "provider openai-native/codex-native",
                "provider anthropic-claude-code-native/claude-code-native",
                "client profile codex-native",
                "client profile claude-code-native",
                "routes codex-native and claude-code-native",
            ],
            install: "switchback setup pack install native-token-adapter --config switchback.yaml",
        }],
    }
}

fn setup_pack_install_report(
    config: &Path,
    pack: &str,
    force: bool,
) -> anyhow::Result<SetupPackInstallReport> {
    match pack {
        "native-token-adapter" => install_native_token_adapter_pack(config, force),
        other => anyhow::bail!("unknown setup pack `{other}`; run `switchback setup pack list`"),
    }
}

fn install_native_token_adapter_pack(
    config: &Path,
    force: bool,
) -> anyhow::Result<SetupPackInstallReport> {
    let initialized_config = if config.exists() {
        false
    } else {
        init_config_file(config, true, InitTemplate::NativeClients)?;
        true
    };
    let text = std::fs::read_to_string(config)
        .map_err(|e| anyhow::anyhow!("read {}: {e}", config.display()))?;
    let mut root: serde_yaml::Value = serde_yaml::from_str(&text)
        .map_err(|e| anyhow::anyhow!("parse {} as YAML: {e}", config.display()))?;
    let Some(mapping) = root.as_mapping_mut() else {
        anyhow::bail!("{} must be a YAML mapping", config.display());
    };

    let mut changes = Vec::new();
    upsert_top_level_sequence_item(
        mapping,
        "providers",
        "openai-native",
        yaml_value(
            r#"
id: openai-native
type: openai_compatible
base_url: "https://api.openai.com/v1"
selection: fill_first
accounts:
  - id: codex-native
    auth: { kind: codex_oauth }
"#,
        )?,
        force,
        &mut changes,
        "provider",
    )?;
    upsert_top_level_sequence_item(
        mapping,
        "providers",
        "anthropic-claude-code-native",
        yaml_value(
            r#"
id: anthropic-claude-code-native
type: anthropic
auth_scheme: { kind: bearer }
selection: fill_first
accounts:
  - id: claude-code-native
    auth: { kind: claude_code_oauth }
"#,
        )?,
        force,
        &mut changes,
        "provider",
    )?;
    upsert_top_level_sequence_item(
        mapping,
        "client_profiles",
        "codex-native",
        yaml_value(
            r#"
id: codex-native
kind: codex
models: ["codex-native"]
accounts: ["openai-native/codex-native"]
description: "Codex profile backed by the native Codex OAuth token source."
"#,
        )?,
        force,
        &mut changes,
        "client_profile",
    )?;
    upsert_top_level_sequence_item(
        mapping,
        "client_profiles",
        "claude-code-native",
        yaml_value(
            r#"
id: claude-code-native
kind: claude_code
models: ["claude-code-native"]
accounts: ["anthropic-claude-code-native/claude-code-native"]
description: "Claude Code profile backed by the native Claude Code OAuth token source."
"#,
        )?,
        force,
        &mut changes,
        "client_profile",
    )?;
    upsert_top_level_sequence_item(
        mapping,
        "routes",
        "codex-native",
        yaml_value(
            r#"
name: codex-native
match:
  model: "codex-native"
targets:
  - "openai-native/gpt-4.1-mini"
"#,
        )?,
        force,
        &mut changes,
        "route",
    )?;
    upsert_top_level_sequence_item(
        mapping,
        "routes",
        "claude-code-native",
        yaml_value(
            r#"
name: claude-code-native
match:
  model: "claude-code-native"
targets:
  - "anthropic-claude-code-native/claude-3-5-sonnet-latest"
"#,
        )?,
        force,
        &mut changes,
        "route",
    )?;

    let rendered = serde_yaml::to_string(&root)?;
    let cfg = Config::from_yaml(&rendered)
        .map_err(|e| anyhow::anyhow!("native-token-adapter pack would make config invalid: {e}"))?;
    Engine::validate_config(&cfg)
        .map_err(|e| anyhow::anyhow!("native-token-adapter pack would make config invalid: {e}"))?;
    write_file_atomic(config, &rendered)?;
    let wrote_config = changes
        .iter()
        .any(|change| change.action == "added" || change.action == "replaced");

    Ok(SetupPackInstallReport {
        schema: "switchback/setup-pack-install@1",
        ok: true,
        pack: "native-token-adapter",
        config: config.to_path_buf(),
        initialized_config,
        wrote_config,
        changes,
        next_commands: vec![
            format!("switchback setup native --config {}", config.display()),
            format!("switchback serve --config {}", config.display()),
            "OPENAI_BASE_URL=http://127.0.0.1:8765/v1 OPENAI_API_KEY=$SWITCHBACK_API_KEY codex exec --model codex-native \"ping through Switchback\"".to_string(),
            "ANTHROPIC_BASE_URL=http://127.0.0.1:8765 ANTHROPIC_AUTH_TOKEN=$SWITCHBACK_API_KEY claude -p \"ping through Switchback\"".to_string(),
        ],
        warnings: vec![
            "native token adapter accounts lease tokens from local Codex/Claude Code stores; no token values were written to config".to_string(),
            "this pack is not first-party Codex/Claude Code subscription relay; use it only where the upstream accepts these bearer tokens".to_string(),
        ],
    })
}

fn yaml_value(text: &str) -> anyhow::Result<serde_yaml::Value> {
    serde_yaml::from_str(text).map_err(Into::into)
}

fn upsert_top_level_sequence_item(
    root: &mut serde_yaml::Mapping,
    key: &str,
    id: &str,
    item: serde_yaml::Value,
    force: bool,
    changes: &mut Vec<PackChange>,
    kind: &'static str,
) -> anyhow::Result<()> {
    let key_value = serde_yaml::Value::String(key.to_string());
    if !root.contains_key(&key_value) {
        root.insert(key_value.clone(), serde_yaml::Value::Sequence(Vec::new()));
    }
    let sequence = root
        .get_mut(&key_value)
        .and_then(serde_yaml::Value::as_sequence_mut)
        .ok_or_else(|| anyhow::anyhow!("top-level `{key}` must be a YAML sequence"))?;

    if let Some(index) = sequence
        .iter()
        .position(|value| mapping_id(value) == Some(id))
    {
        if force {
            sequence[index] = item;
            changes.push(PackChange {
                kind,
                id: id.to_string(),
                action: "replaced",
            });
        } else {
            changes.push(PackChange {
                kind,
                id: id.to_string(),
                action: "kept",
            });
        }
    } else {
        sequence.push(item);
        changes.push(PackChange {
            kind,
            id: id.to_string(),
            action: "added",
        });
    }
    Ok(())
}

fn mapping_id(value: &serde_yaml::Value) -> Option<&str> {
    value
        .as_mapping()?
        .get(serde_yaml::Value::String("id".to_string()))
        .or_else(|| {
            value
                .as_mapping()?
                .get(serde_yaml::Value::String("name".to_string()))
        })?
        .as_str()
}
