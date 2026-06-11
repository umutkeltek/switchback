use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use clap::Subcommand;
use sb_core::{
    AuthConfig, ClientProfileConfig, ClientProfileKind, ClientProfileMode, Config, ProviderKind,
    TapConfig,
};
use sb_runtime::Engine;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::native_history_cli::{
    native_history_import, print_native_import_history_text, NativeImportHistoryArgs,
};
use crate::setup_cli::NativeClientTarget;

#[derive(Subcommand)]
pub(crate) enum NativeCmd {
    /// Read-only status for Codex/Claude Code native-client setup.
    Status {
        /// Limit reporting to one native client.
        #[arg(long, value_enum, default_value_t = NativeClientTarget::All)]
        client: NativeClientTarget,
        /// Include exact local helper labels. Defaults to redacted hashes.
        #[arg(long)]
        show_local_ids: bool,
    },
    /// Inspect and use named native client profiles.
    Profiles {
        #[command(subcommand)]
        action: NativeProfilesCmd,
    },
    /// Preview metadata-only import from native client history stores.
    ImportHistory(NativeImportHistoryArgs),
}

#[derive(Subcommand)]
pub(crate) enum NativeProfilesCmd {
    /// List configured native client profiles.
    List,
    /// Diagnose one configured profile.
    Doctor {
        /// Profile id from `client_profiles`.
        profile: String,
    },
    /// Print safe environment/header hints for one profile.
    Env {
        /// Profile id from `client_profiles`.
        profile: String,
    },
}

#[derive(Debug, Serialize)]
struct NativeStatusReport {
    schema: &'static str,
    ok: bool,
    read_only: bool,
    config: String,
    validation: NativeValidationStatus,
    server: NativeServerStatus,
    clients: Vec<NativeClientStatus>,
    lane_separation: LaneSeparationStatus,
    local_runtime: LocalRuntimeStatus,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    next_actions: Vec<String>,
}

#[derive(Debug, Serialize)]
struct NativeValidationStatus {
    ok: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    problems: Vec<String>,
}

#[derive(Debug, Serialize)]
struct NativeServerStatus {
    bind: String,
    health: ServerHealthStatus,
}

#[derive(Debug, Serialize)]
struct ServerHealthStatus {
    status: &'static str,
    reachable: Option<bool>,
    detail: String,
}

#[derive(Debug, Serialize)]
struct NativeClientStatus {
    id: &'static str,
    command: &'static str,
    installed: bool,
    version: Option<String>,
    version_error: Option<String>,
    protocol: &'static str,
    required_endpoints: Vec<&'static str>,
    session_headers: Vec<&'static str>,
    profiles: Vec<ClientProfileStatus>,
    native_accounts: Vec<NativeAccountStatus>,
    native_account_configured: bool,
    token_available: bool,
    token_sources: Vec<TokenSourceStatus>,
    modes: NativeClientModes,
    fidelity: NativeFidelityStatus,
}

#[derive(Debug, Serialize)]
struct ClientProfileStatus {
    id: String,
    enabled: bool,
    mode: ClientProfileMode,
    models: Vec<String>,
    accounts: Vec<String>,
}

#[derive(Debug, Serialize)]
struct NativeProfilesReport {
    schema: &'static str,
    ok: bool,
    config: String,
    profiles: Vec<NativeProfileRow>,
}

#[derive(Debug, Serialize)]
struct NativeProfileDoctorReport {
    schema: &'static str,
    ok: bool,
    config: String,
    profile: NativeProfileRow,
}

#[derive(Debug, Serialize)]
struct NativeProfileEnvReport {
    schema: &'static str,
    ok: bool,
    profile: String,
    mode: ClientProfileMode,
    protocol: &'static str,
    fidelity: NativeFidelityStatus,
    base_url: String,
    model: Option<String>,
    headers: Vec<NativeProfileHeaderHint>,
    env: Vec<NativeProfileEnvVar>,
    command_hint: String,
    warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
struct NativeProfileHeaderHint {
    name: &'static str,
    value: String,
}

#[derive(Debug, Serialize)]
struct NativeProfileEnvVar {
    name: &'static str,
    value: String,
}

#[derive(Debug, Serialize)]
struct NativeProfileRow {
    id: String,
    kind: ClientProfileKind,
    mode: ClientProfileMode,
    enabled: bool,
    protocol: &'static str,
    fidelity: NativeFidelityStatus,
    models: Vec<NativeProfileModelStatus>,
    accounts: Vec<NativeProfileAccountStatus>,
    ready: bool,
    command_hint: String,
    problems: Vec<String>,
}

#[derive(Debug, Serialize)]
struct NativeProfileModelStatus {
    id: String,
    resolvable: bool,
    resolution: &'static str,
}

#[derive(Debug, Serialize)]
struct NativeProfileAccountStatus {
    reference: String,
    provider: String,
    account: String,
    provider_kind: Option<&'static str>,
    auth_kind: Option<&'static str>,
    exists: bool,
    native_relay_compatible: bool,
}

#[derive(Debug, Serialize)]
struct NativeAccountStatus {
    provider: String,
    account: String,
    provider_kind: &'static str,
    auth_kind: &'static str,
}

#[derive(Debug, Serialize)]
struct TokenSourceStatus {
    kind: &'static str,
    label: String,
    configured: bool,
    available: Option<bool>,
}

#[derive(Debug, Serialize)]
struct NativeClientModes {
    direct_native: ModeStatus,
    native_tap: NativeTapModeStatus,
    switchback_ingress: ModeStatus,
    native_relay: ModeStatus,
}

#[derive(Debug, Serialize)]
struct ModeStatus {
    state: &'static str,
    ready: bool,
    reasons: Vec<String>,
}

#[derive(Debug, Serialize)]
struct NativeTapModeStatus {
    state: &'static str,
    ready: bool,
    reasons: Vec<String>,
    listener: Option<NativeTapListenerStatus>,
}

#[derive(Debug, Serialize)]
struct NativeTapListenerStatus {
    id: String,
    bind: String,
    upstream: String,
    capture_bodies: bool,
}

#[derive(Debug, Serialize)]
struct NativeFidelityStatus {
    best_mode: &'static str,
    guarantee: &'static str,
    native_wire_verbatim: bool,
    switchback_rewrites_request: bool,
    switchback_reissues_auth: bool,
    reasons: Vec<&'static str>,
}

#[derive(Debug, Serialize)]
struct LaneSeparationStatus {
    ok: bool,
    scout_code: LanePresence,
    native_routes: Vec<NativeRouteStatus>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
struct LanePresence {
    configured: bool,
    source: &'static str,
    targets: Vec<String>,
}

#[derive(Debug, Serialize)]
struct NativeRouteStatus {
    client: &'static str,
    route: &'static str,
    configured: bool,
    fail_closed: bool,
    targets: Vec<String>,
    provider_kind_ok: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    problems: Vec<String>,
}

#[derive(Debug, Serialize)]
struct LocalRuntimeStatus {
    inspected: bool,
    source: &'static str,
    possible_conflicts: Vec<LocalRuntimeConflict>,
    detail: String,
}

#[derive(Debug, Serialize)]
struct LocalRuntimeConflict {
    source: &'static str,
    id: String,
    id_redacted: bool,
    detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum SourceSpec {
    Env {
        name: String,
    },
    File {
        path: String,
        pointer: String,
    },
    JsonPresence {
        kind: &'static str,
        path: String,
        pointer: String,
    },
    Vault {
        name: String,
    },
}

pub(crate) fn run_native_cmd(action: NativeCmd, config: &Path, json: bool) -> anyhow::Result<()> {
    match action {
        NativeCmd::Status {
            client,
            show_local_ids,
        } => {
            let report = native_status_report(config, client, show_local_ids);
            if json {
                crate::print_json(&report)?;
            } else {
                print_native_status_text(&report);
            }
        }
        NativeCmd::Profiles { action } => match action {
            NativeProfilesCmd::List => {
                let report = native_profiles_report(config)?;
                if json {
                    crate::print_json(&report)?;
                } else {
                    print_native_profiles_text(&report);
                }
            }
            NativeProfilesCmd::Doctor { profile } => {
                let report = native_profile_doctor_report(config, &profile)?;
                if json {
                    crate::print_json(&report)?;
                } else {
                    print_native_profile_doctor_text(&report);
                }
                if !report.ok {
                    std::process::exit(1);
                }
            }
            NativeProfilesCmd::Env { profile } => {
                let report = native_profile_env_report(config, &profile)?;
                if json {
                    crate::print_json(&report)?;
                } else {
                    print_native_profile_env_text(&report);
                }
                if !report.ok {
                    std::process::exit(1);
                }
            }
        },
        NativeCmd::ImportHistory(args) => {
            let report = native_history_import(args, config)?;
            if json {
                crate::print_json(&report)?;
            } else {
                print_native_import_history_text(&report);
            }
        }
    }
    Ok(())
}

fn native_profiles_report(config: &Path) -> anyhow::Result<NativeProfilesReport> {
    let cfg = Config::from_path(config)?;
    let profiles = cfg
        .client_profiles
        .iter()
        .map(|profile| native_profile_row(&cfg, profile))
        .collect::<Vec<_>>();
    let ok = !profiles.is_empty() && profiles.iter().all(|profile| profile.ready);
    Ok(NativeProfilesReport {
        schema: "switchback/native-profiles@1",
        ok,
        config: config.display().to_string(),
        profiles,
    })
}

fn native_profile_doctor_report(
    config: &Path,
    profile_id: &str,
) -> anyhow::Result<NativeProfileDoctorReport> {
    let cfg = Config::from_path(config)?;
    let Some(profile) = cfg
        .client_profiles
        .iter()
        .find(|profile| profile.id == profile_id)
    else {
        anyhow::bail!("client profile `{profile_id}` is not configured");
    };
    let row = native_profile_row(&cfg, profile);
    Ok(NativeProfileDoctorReport {
        schema: "switchback/native-profile-doctor@1",
        ok: row.ready,
        config: config.display().to_string(),
        profile: row,
    })
}

fn native_profile_env_report(
    config: &Path,
    profile_id: &str,
) -> anyhow::Result<NativeProfileEnvReport> {
    let cfg = Config::from_path(config)?;
    let Some(profile) = cfg
        .client_profiles
        .iter()
        .find(|profile| profile.id == profile_id)
    else {
        anyhow::bail!("client profile `{profile_id}` is not configured");
    };
    let row = native_profile_row(&cfg, profile);
    let model = profile.models.first().cloned();
    let base_url = profile_base_url(&cfg, profile);
    let mut env = vec![
        NativeProfileEnvVar {
            name: "SWITCHBACK_CLIENT_PROFILE",
            value: profile.id.clone(),
        },
        NativeProfileEnvVar {
            name: "SWITCHBACK_CLIENT_PROFILE_HEADER",
            value: format!("x-switchback-client-profile: {}", profile.id),
        },
    ];
    match profile.kind {
        ClientProfileKind::Codex => {
            let openai_base_url = if profile.mode == ClientProfileMode::Tap {
                base_url.clone()
            } else {
                format!("{base_url}/v1")
            };
            env.push(NativeProfileEnvVar {
                name: "OPENAI_BASE_URL",
                value: openai_base_url,
            });
            env.push(NativeProfileEnvVar {
                name: "OPENAI_API_KEY",
                value: "${SWITCHBACK_API_KEY:-switchback-local}".to_string(),
            });
        }
        ClientProfileKind::ClaudeCode => {
            env.push(NativeProfileEnvVar {
                name: "ANTHROPIC_BASE_URL",
                value: base_url.clone(),
            });
            env.push(NativeProfileEnvVar {
                name: "ANTHROPIC_AUTH_TOKEN",
                value: "${SWITCHBACK_API_KEY:-switchback-local}".to_string(),
            });
        }
    }
    let mut warnings = Vec::new();
    if !row.ready {
        warnings.extend(row.problems.clone());
    }
    warnings.push(
        "native clients that cannot set custom headers should use a unique model alias from this profile; Switchback infers the profile from that model"
            .to_string(),
    );
    Ok(NativeProfileEnvReport {
        schema: "switchback/native-profile-env@1",
        ok: row.ready,
        profile: profile.id.clone(),
        mode: profile.mode,
        protocol: profile.kind.protocol(),
        fidelity: profile_fidelity_status(profile.mode),
        base_url,
        model: model.clone(),
        headers: vec![NativeProfileHeaderHint {
            name: "x-switchback-client-profile",
            value: profile.id.clone(),
        }],
        env,
        command_hint: profile_command_hint(&cfg, profile),
        warnings,
    })
}

fn native_profile_row(cfg: &Config, profile: &ClientProfileConfig) -> NativeProfileRow {
    let models = if profile.models.is_empty() {
        Vec::new()
    } else {
        profile
            .models
            .iter()
            .map(|model| native_profile_model_status(cfg, model))
            .collect()
    };
    let accounts = profile
        .accounts
        .iter()
        .map(|account_ref| {
            native_profile_account_status(cfg, profile.kind, profile.mode, account_ref)
        })
        .collect::<Vec<_>>();
    let mut problems = Vec::new();
    if !profile.enabled {
        problems.push("profile is disabled".to_string());
    }
    if models.is_empty() && profile.mode != ClientProfileMode::Tap {
        problems.push("profile does not pin a model alias".to_string());
    }
    for model in &models {
        if !model.resolvable && profile.mode != ClientProfileMode::Tap {
            problems.push(format!("model `{}` does not resolve", model.id));
        }
    }
    if accounts.is_empty() && profile.mode != ClientProfileMode::Tap {
        problems.push("profile does not pin an account".to_string());
    }
    if profile.mode == ClientProfileMode::Tap && native_tap_listener(cfg, profile.kind).is_none() {
        problems.push("profile mode tap has no matching transparent tap listener".to_string());
    }
    for account in &accounts {
        if !account.exists {
            problems.push(format!("account `{}` does not exist", account.reference));
        }
        if profile.mode == ClientProfileMode::NativeRelay && !account.native_relay_compatible {
            problems.push(format!(
                "account `{}` is not compatible with native relay mode",
                account.reference
            ));
        }
    }
    NativeProfileRow {
        id: profile.id.clone(),
        kind: profile.kind,
        mode: profile.mode,
        enabled: profile.enabled,
        protocol: profile.kind.protocol(),
        fidelity: profile_fidelity_status(profile.mode),
        models,
        accounts,
        ready: problems.is_empty(),
        command_hint: profile_command_hint(cfg, profile),
        problems,
    }
}

fn native_profile_model_status(cfg: &Config, model: &str) -> NativeProfileModelStatus {
    let resolution = if cfg.exact_route_for(model).is_some() {
        "route"
    } else if cfg.combo_for(model).is_some() {
        "combo"
    } else if provider_model_ref_resolves(cfg, model) {
        "provider_model"
    } else {
        "missing"
    };
    NativeProfileModelStatus {
        id: model.to_string(),
        resolvable: resolution != "missing",
        resolution,
    }
}

fn native_profile_account_status(
    cfg: &Config,
    kind: ClientProfileKind,
    mode: ClientProfileMode,
    account_ref: &str,
) -> NativeProfileAccountStatus {
    let Some((provider_id, account_id)) = account_ref.split_once('/') else {
        return NativeProfileAccountStatus {
            reference: account_ref.to_string(),
            provider: String::new(),
            account: String::new(),
            provider_kind: None,
            auth_kind: None,
            exists: false,
            native_relay_compatible: false,
        };
    };
    let provider = cfg
        .providers
        .iter()
        .find(|provider| provider.id == provider_id);
    let account = provider.and_then(|provider| {
        provider
            .accounts
            .iter()
            .find(|account| account.id == account_id)
    });
    let provider_kind = provider.map(|provider| provider_kind_name(&provider.kind));
    let auth_kind = account.map(|account| auth_kind_name(&account.auth));
    let native_relay_compatible = match (mode, provider, account) {
        (ClientProfileMode::NativeRelay, Some(provider), Some(account)) => {
            provider_matches_native_relay(&provider.kind, kind)
                && auth_matches_kind(&account.auth, kind)
        }
        (ClientProfileMode::NativeRelay, _, _) => false,
        _ => true,
    };
    NativeProfileAccountStatus {
        reference: account_ref.to_string(),
        provider: provider_id.to_string(),
        account: account_id.to_string(),
        provider_kind,
        auth_kind,
        exists: account.is_some(),
        native_relay_compatible,
    }
}

fn provider_model_ref_resolves(cfg: &Config, model: &str) -> bool {
    let Some((provider_id, model_id)) = model.split_once('/') else {
        return false;
    };
    !provider_id.is_empty()
        && !model_id.is_empty()
        && cfg
            .providers
            .iter()
            .any(|provider| provider.id == provider_id)
}

fn profile_base_url(cfg: &Config, profile: &ClientProfileConfig) -> String {
    if profile.mode == ClientProfileMode::Tap {
        if let Some(listener) = native_tap_listener(cfg, profile.kind) {
            return format!("http://{}", listener.bind);
        }
    }
    format!("http://{}", cfg.server.bind)
}

fn profile_command_hint(cfg: &Config, profile: &ClientProfileConfig) -> String {
    let model = profile
        .models
        .first()
        .cloned()
        .unwrap_or_else(|| "<model>".to_string());
    let base_url = profile_base_url(cfg, profile);
    match profile.kind {
        ClientProfileKind::Codex => {
            let openai_base_url = if profile.mode == ClientProfileMode::Tap {
                base_url
            } else {
                format!("{base_url}/v1")
            };
            format!(
                "SWITCHBACK_CLIENT_PROFILE={} OPENAI_BASE_URL={} OPENAI_API_KEY=$SWITCHBACK_API_KEY codex exec --model {}",
                profile.id, openai_base_url, model
            )
        }
        ClientProfileKind::ClaudeCode => format!(
            "SWITCHBACK_CLIENT_PROFILE={} ANTHROPIC_BASE_URL={} ANTHROPIC_AUTH_TOKEN=$SWITCHBACK_API_KEY claude -p --model {}",
            profile.id, base_url, model
        ),
    }
}

fn native_status_report(
    config: &Path,
    target: NativeClientTarget,
    show_local_ids: bool,
) -> NativeStatusReport {
    let (cfg, validation) = load_config_for_status(config);
    let server = cfg
        .as_ref()
        .map(|cfg| NativeServerStatus {
            bind: cfg.server.bind.clone(),
            health: probe_server_health(&cfg.server.bind),
        })
        .unwrap_or_else(|| NativeServerStatus {
            bind: "-".to_string(),
            health: ServerHealthStatus {
                status: "skipped",
                reachable: None,
                detail: "config did not load".to_string(),
            },
        });
    let clients = native_client_kinds(target)
        .into_iter()
        .map(|kind| native_client_status(cfg.as_ref(), kind))
        .collect::<Vec<_>>();
    let lane_separation = lane_separation_status(cfg.as_ref(), target);
    let local_runtime = inspect_local_runtime(show_local_ids);

    let mut warnings = Vec::new();
    if clients.iter().any(|client| !client.installed) {
        warnings.push("at least one native client command was not found on PATH".to_string());
    }
    if clients
        .iter()
        .any(|client| client.native_account_configured && !client.token_available)
    {
        warnings.push(
            "a native OAuth account is configured but no readable token source was detected"
                .to_string(),
        );
    }
    if !local_runtime.possible_conflicts.is_empty() {
        warnings.push(
            "possible local runtime proxy or watchdog helpers are present; audit before relying on native routing"
                .to_string(),
        );
    }
    warnings.extend(lane_separation.warnings.iter().cloned());

    let mut next_actions = Vec::new();
    if cfg.is_none() {
        next_actions.push(format!(
            "switchback init --native-clients --config {}",
            config.display()
        ));
    }
    if clients.iter().any(needs_native_token_adapter_action) {
        next_actions.push(format!(
            "switchback setup pack install native-token-adapter --config {}",
            config.display()
        ));
    }
    if clients.iter().any(|client| !client.token_available) {
        next_actions.push("run the native client login/setup flow, then re-run status".to_string());
    }

    let ok = validation.ok
        && clients
            .iter()
            .all(|client| client.installed || !client.native_account_configured)
        && lane_separation.ok;

    NativeStatusReport {
        schema: "switchback/native-status@1",
        ok,
        read_only: true,
        config: config.display().to_string(),
        validation,
        server,
        clients,
        lane_separation,
        local_runtime,
        warnings,
        next_actions,
    }
}

fn load_config_for_status(path: &Path) -> (Option<Config>, NativeValidationStatus) {
    match Config::from_path(path) {
        Ok(cfg) => match Engine::validate_config(&cfg) {
            Ok(()) => (
                Some(cfg),
                NativeValidationStatus {
                    ok: true,
                    problems: Vec::new(),
                },
            ),
            Err(e) => (
                Some(cfg),
                NativeValidationStatus {
                    ok: false,
                    problems: e.split("; ").map(str::to_string).collect(),
                },
            ),
        },
        Err(e) => (
            None,
            NativeValidationStatus {
                ok: false,
                problems: vec![e.to_string()],
            },
        ),
    }
}

fn native_client_status(cfg: Option<&Config>, kind: ClientProfileKind) -> NativeClientStatus {
    let command = native_command(kind);
    let installed = command_exists(command);
    let (version, version_error) = if installed {
        command_version(command)
    } else {
        (None, Some("command not found on PATH".to_string()))
    };
    let profiles = cfg
        .map(|cfg| {
            cfg.client_profiles
                .iter()
                .filter(|profile| profile.kind == kind)
                .map(client_profile_status)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let native_accounts = cfg
        .map(|cfg| native_account_statuses(cfg, kind))
        .unwrap_or_default();
    let token_sources = detect_token_sources(
        cfg.map(|cfg| source_specs_for_config(cfg, kind))
            .unwrap_or_else(|| default_source_specs(kind)),
    );
    let token_available = token_sources
        .iter()
        .any(|source| source.available == Some(true));
    let native_account_configured = !native_accounts.is_empty();
    let modes = native_client_modes(
        kind,
        installed,
        token_available,
        !profiles.is_empty(),
        native_account_configured,
        cfg,
    );
    let fidelity = native_fidelity_status(&modes);

    NativeClientStatus {
        id: kind.default_id(),
        command,
        installed,
        version,
        version_error,
        protocol: kind.protocol(),
        required_endpoints: kind.required_endpoints().to_vec(),
        session_headers: kind.session_headers().to_vec(),
        profiles,
        native_accounts,
        native_account_configured,
        token_available,
        token_sources,
        modes,
        fidelity,
    }
}

fn needs_native_token_adapter_action(client: &NativeClientStatus) -> bool {
    !client.native_account_configured
        && !client.modes.native_tap.ready
        && !client.modes.direct_native.ready
}

fn client_profile_status(profile: &ClientProfileConfig) -> ClientProfileStatus {
    ClientProfileStatus {
        id: profile.id.clone(),
        enabled: profile.enabled,
        mode: profile.mode,
        models: profile.models.clone(),
        accounts: profile.accounts.clone(),
    }
}

fn native_account_statuses(cfg: &Config, kind: ClientProfileKind) -> Vec<NativeAccountStatus> {
    cfg.providers
        .iter()
        .flat_map(|provider| {
            provider
                .accounts
                .iter()
                .filter(move |account| auth_matches_kind(&account.auth, kind))
                .map(move |account| NativeAccountStatus {
                    provider: provider.id.clone(),
                    account: account.id.clone(),
                    provider_kind: provider_kind_name(&provider.kind),
                    auth_kind: native_auth_kind(kind),
                })
        })
        .collect()
}

fn native_client_modes(
    kind: ClientProfileKind,
    installed: bool,
    token_available: bool,
    profile_configured: bool,
    native_account_configured: bool,
    cfg: Option<&Config>,
) -> NativeClientModes {
    let direct_ready = installed && token_available;
    let tap_listener = cfg.and_then(|cfg| native_tap_listener(cfg, kind));
    let tap_ready = installed && token_available && tap_listener.is_some();
    let switchback_ready = profile_configured;
    let relay_status = native_route_status(cfg, kind);
    NativeClientModes {
        direct_native: ModeStatus {
            state: if direct_ready { "ready" } else { "not_ready" },
            ready: direct_ready,
            reasons: mode_reasons(vec![
                (installed, format!("{} command found", native_command(kind))),
                (token_available, "native auth source available".to_string()),
            ]),
        },
        native_tap: NativeTapModeStatus {
            state: if tap_ready {
                "ready"
            } else if tap_listener.is_some() {
                "not_ready"
            } else {
                "not_configured"
            },
            ready: tap_ready,
            reasons: mode_reasons(vec![
                (installed, format!("{} command found", native_command(kind))),
                (token_available, "native auth source available".to_string()),
                (
                    tap_listener.is_some(),
                    "transparent tap listener configured".to_string(),
                ),
            ]),
            listener: tap_listener,
        },
        switchback_ingress: ModeStatus {
            state: if switchback_ready {
                "configured"
            } else {
                "not_configured"
            },
            ready: switchback_ready,
            reasons: mode_reasons(vec![
                (profile_configured, "client profile declared".to_string()),
                (
                    native_account_configured,
                    "native token-source account configured".to_string(),
                ),
            ]),
        },
        native_relay: ModeStatus {
            state: if relay_status.configured && relay_status.provider_kind_ok {
                "configured"
            } else if relay_status.fail_closed {
                "fail_closed"
            } else {
                "blocked"
            },
            ready: relay_status.configured && relay_status.provider_kind_ok,
            reasons: if relay_status.problems.is_empty() {
                vec![format!("route `{}` is fail-closed", relay_status.route)]
            } else {
                relay_status.problems.clone()
            },
        },
    }
}

fn native_fidelity_status(modes: &NativeClientModes) -> NativeFidelityStatus {
    if modes.native_tap.ready {
        return NativeFidelityStatus {
            best_mode: "native_tap",
            guarantee: "observed_native_verbatim",
            native_wire_verbatim: true,
            switchback_rewrites_request: false,
            switchback_reissues_auth: false,
            reasons: vec![
                "transparent tap forwards the native client's headers, auth, and body",
                "Switchback observes locally but does not canonicalize or lease credentials",
            ],
        };
    }
    if modes.direct_native.ready {
        return NativeFidelityStatus {
            best_mode: "direct_native",
            guarantee: "native_direct_unobserved",
            native_wire_verbatim: true,
            switchback_rewrites_request: false,
            switchback_reissues_auth: false,
            reasons: vec![
                "native client can reach its first-party backend directly",
                "Switchback is not in the request path",
            ],
        };
    }
    if modes.native_relay.ready {
        return NativeFidelityStatus {
            best_mode: "native_relay",
            guarantee: "native_auth_reissued",
            native_wire_verbatim: false,
            switchback_rewrites_request: true,
            switchback_reissues_auth: true,
            reasons: vec![
                "native relay leases local native auth but reissues the request through Switchback",
                "relay mode must pass conformance before it can claim full native compatibility",
            ],
        };
    }
    if modes.switchback_ingress.ready {
        return NativeFidelityStatus {
            best_mode: "switchback_ingress",
            guarantee: "api_compatible_routed",
            native_wire_verbatim: false,
            switchback_rewrites_request: true,
            switchback_reissues_auth: true,
            reasons: vec![
                "client speaks an API-compatible surface to Switchback",
                "Switchback selects provider credentials and renders provider wire at the edge",
            ],
        };
    }
    NativeFidelityStatus {
        best_mode: "none",
        guarantee: "not_ready",
        native_wire_verbatim: false,
        switchback_rewrites_request: false,
        switchback_reissues_auth: false,
        reasons: vec!["no executable native-client mode is currently ready"],
    }
}

fn profile_fidelity_status(mode: ClientProfileMode) -> NativeFidelityStatus {
    match mode {
        ClientProfileMode::Tap => NativeFidelityStatus {
            best_mode: "native_tap",
            guarantee: "observed_native_verbatim",
            native_wire_verbatim: true,
            switchback_rewrites_request: false,
            switchback_reissues_auth: false,
            reasons: vec![
                "transparent tap forwards the native client's headers, auth, and body",
                "profile account pins are not required because native auth stays with the client",
            ],
        },
        ClientProfileMode::NativeRelay => NativeFidelityStatus {
            best_mode: "native_relay",
            guarantee: "native_auth_reissued",
            native_wire_verbatim: false,
            switchback_rewrites_request: true,
            switchback_reissues_auth: true,
            reasons: vec![
                "native relay leases local native auth but reissues the request through Switchback",
                "relay mode must pass conformance before it can claim full native compatibility",
            ],
        },
        ClientProfileMode::SwitchbackIngress => NativeFidelityStatus {
            best_mode: "switchback_ingress",
            guarantee: "api_compatible_routed",
            native_wire_verbatim: false,
            switchback_rewrites_request: true,
            switchback_reissues_auth: true,
            reasons: vec![
                "client speaks an API-compatible surface to Switchback",
                "Switchback selects provider credentials and renders provider wire at the edge",
            ],
        },
        ClientProfileMode::ScoutApi => NativeFidelityStatus {
            best_mode: "scout_api",
            guarantee: "api_compatible_scout",
            native_wire_verbatim: false,
            switchback_rewrites_request: true,
            switchback_reissues_auth: true,
            reasons: vec![
                "scout profiles intentionally use routed API-compatible provider pools",
                "this mode is not first-party native subscription traffic",
            ],
        },
    }
}

fn mode_reasons(items: Vec<(bool, String)>) -> Vec<String> {
    items
        .into_iter()
        .map(|(ok, label)| format!("{}: {label}", if ok { "ok" } else { "missing" }))
        .collect()
}

fn native_tap_listener(cfg: &Config, kind: ClientProfileKind) -> Option<NativeTapListenerStatus> {
    cfg.server
        .taps
        .iter()
        .find(|tap| tap_matches_kind(tap, kind))
        .map(|tap| NativeTapListenerStatus {
            id: tap.id.clone(),
            bind: tap.bind.clone(),
            upstream: tap.upstream.clone(),
            capture_bodies: tap.capture_bodies,
        })
}

fn tap_matches_kind(tap: &TapConfig, kind: ClientProfileKind) -> bool {
    let id = tap.id.to_ascii_lowercase();
    let upstream = tap.upstream.to_ascii_lowercase();
    match kind {
        ClientProfileKind::Codex => {
            id.contains("codex")
                || upstream.contains("chatgpt.com")
                || upstream.contains("api.openai.com")
        }
        ClientProfileKind::ClaudeCode => {
            id.contains("claude") || upstream.contains("anthropic.com")
        }
    }
}

fn lane_separation_status(
    cfg: Option<&Config>,
    target: NativeClientTarget,
) -> LaneSeparationStatus {
    let scout_code = cfg
        .map(scout_code_presence)
        .unwrap_or_else(|| LanePresence {
            configured: false,
            source: "config_unavailable",
            targets: Vec::new(),
        });
    let native_routes = native_client_kinds(target)
        .into_iter()
        .map(|kind| native_route_status(cfg, kind))
        .collect::<Vec<_>>();
    let mut warnings = Vec::new();
    if !scout_code.configured {
        warnings.push("scout/code lane is not declared in this config".to_string());
    }
    for route in &native_routes {
        warnings.extend(route.problems.iter().cloned());
    }
    if scout_code.configured {
        let scout_targets = scout_code.targets.iter().collect::<BTreeSet<_>>();
        for route in &native_routes {
            let overlap = route
                .targets
                .iter()
                .filter(|target| scout_targets.contains(target))
                .cloned()
                .collect::<Vec<_>>();
            if !overlap.is_empty() {
                warnings.push(format!(
                    "{} route shares target(s) with scout/code: {}",
                    route.route,
                    overlap.join(", ")
                ));
            }
        }
    }
    let ok = native_routes
        .iter()
        .all(|route| route.fail_closed || route.provider_kind_ok)
        && !warnings
            .iter()
            .any(|warning| warning.contains("shares target"));
    LaneSeparationStatus {
        ok,
        scout_code,
        native_routes,
        warnings,
    }
}

fn scout_code_presence(cfg: &Config) -> LanePresence {
    if let Some(route) = cfg.exact_route_for("scout/code") {
        return LanePresence {
            configured: true,
            source: "exact_route",
            targets: route.targets.clone(),
        };
    }
    if let Some(combo) = cfg.combo_for("nonstop-code") {
        return LanePresence {
            configured: true,
            source: "legacy_combo",
            targets: combo.models.clone(),
        };
    }
    LanePresence {
        configured: false,
        source: "missing",
        targets: Vec::new(),
    }
}

fn native_route_status(cfg: Option<&Config>, kind: ClientProfileKind) -> NativeRouteStatus {
    let Some(cfg) = cfg else {
        return NativeRouteStatus {
            client: kind.default_id(),
            route: native_route_name(kind),
            configured: false,
            fail_closed: true,
            targets: Vec::new(),
            provider_kind_ok: false,
            problems: vec!["config did not load".to_string()],
        };
    };
    let route_name = native_route_name(kind);
    let Some(route) = cfg.exact_route_for(route_name) else {
        return NativeRouteStatus {
            client: kind.default_id(),
            route: route_name,
            configured: false,
            fail_closed: true,
            targets: Vec::new(),
            provider_kind_ok: false,
            problems: Vec::new(),
        };
    };
    let provider_kinds = cfg
        .providers
        .iter()
        .map(|provider| (provider.id.as_str(), &provider.kind))
        .collect::<BTreeMap<_, _>>();
    let mut problems = Vec::new();
    for target in &route.targets {
        let Some((provider_id, _model)) = target.split_once('/') else {
            problems.push(format!("target `{target}` is not a provider/model ref"));
            continue;
        };
        match provider_kinds.get(provider_id) {
            Some(provider_kind) if provider_matches_native_relay(provider_kind, kind) => {}
            Some(provider_kind) => problems.push(format!(
                "target `{target}` uses provider kind `{}` instead of `{}`",
                provider_kind_name(provider_kind),
                native_relay_provider_kind(kind)
            )),
            None => problems.push(format!("target `{target}` references an unknown provider")),
        }
    }
    NativeRouteStatus {
        client: kind.default_id(),
        route: route_name,
        configured: true,
        fail_closed: false,
        targets: route.targets.clone(),
        provider_kind_ok: problems.is_empty(),
        problems,
    }
}

fn inspect_local_runtime(show_local_ids: bool) -> LocalRuntimeStatus {
    let Some(output) = run_command_output("launchctl", &["list"]) else {
        return LocalRuntimeStatus {
            inspected: false,
            source: "launchctl",
            possible_conflicts: Vec::new(),
            detail: "launchctl not available or returned no output".to_string(),
        };
    };
    let conflicts = output
        .lines()
        .filter_map(parse_launchctl_label)
        .filter(|label| looks_like_runtime_proxy(label))
        .map(|label| {
            let (id, id_redacted) = runtime_label_id(label, show_local_ids);
            LocalRuntimeConflict {
                source: "launchctl",
                id,
                id_redacted,
                detail: "label looks like a coding-client runtime proxy, router, rotation, or watchdog helper".to_string(),
            }
        })
        .collect::<Vec<_>>();
    LocalRuntimeStatus {
        inspected: true,
        source: "launchctl",
        detail: if conflicts.is_empty() {
            "no possible runtime proxy helpers detected".to_string()
        } else {
            format!("{} possible helper(s) detected", conflicts.len())
        },
        possible_conflicts: conflicts,
    }
}

fn runtime_label_id(label: &str, show_local_ids: bool) -> (String, bool) {
    if show_local_ids {
        return (label.to_string(), false);
    }
    let mut hasher = Sha256::new();
    hasher.update(label.as_bytes());
    let digest = hasher.finalize();
    let short = digest[..6]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    (format!("runtime-helper-{short}"), true)
}

fn parse_launchctl_label(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with("PID") {
        return None;
    }
    trimmed.split_whitespace().last()
}

fn looks_like_runtime_proxy(label: &str) -> bool {
    let lower = label.to_ascii_lowercase();
    let client = lower.contains("codex") || lower.contains("claude");
    let runtime = [
        "proxy", "router", "rotation", "watchdog", "app-bind", "runtime",
    ]
    .iter()
    .any(|needle| lower.contains(needle));
    client && runtime
}

fn probe_server_health(bind: &str) -> ServerHealthStatus {
    if bind.ends_with(":0") {
        return ServerHealthStatus {
            status: "skipped_ephemeral_bind",
            reachable: None,
            detail: "server bind uses port 0".to_string(),
        };
    }
    let Ok(addrs) = bind.to_socket_addrs() else {
        return ServerHealthStatus {
            status: "invalid_bind",
            reachable: None,
            detail: format!("could not resolve bind `{bind}`"),
        };
    };
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, Duration::from_millis(150)) {
            Ok(mut stream) => {
                let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
                let _ = stream.set_write_timeout(Some(Duration::from_millis(200)));
                let request =
                    format!("GET /health HTTP/1.1\r\nHost: {bind}\r\nConnection: close\r\n\r\n");
                if stream.write_all(request.as_bytes()).is_err() {
                    return ServerHealthStatus {
                        status: "tcp_only",
                        reachable: Some(true),
                        detail: format!("connected to {addr}, health request write failed"),
                    };
                }
                let mut buf = [0u8; 256];
                let read = stream.read(&mut buf).unwrap_or(0);
                let text = String::from_utf8_lossy(&buf[..read]);
                let ok = text.starts_with("HTTP/1.1 200") || text.starts_with("HTTP/1.0 200");
                return ServerHealthStatus {
                    status: if ok { "healthy" } else { "reachable_non_200" },
                    reachable: Some(true),
                    detail: if read == 0 {
                        format!("connected to {addr}, but no health response was read")
                    } else {
                        text.lines()
                            .next()
                            .unwrap_or("health response read")
                            .to_string()
                    },
                };
            }
            Err(_) => continue,
        }
    }
    ServerHealthStatus {
        status: "not_reachable",
        reachable: Some(false),
        detail: format!("no TCP listener reachable at {bind}"),
    }
}

fn source_specs_for_config(cfg: &Config, kind: ClientProfileKind) -> Vec<SourceSpec> {
    let mut specs = BTreeSet::new();
    for provider in &cfg.providers {
        for account in &provider.accounts {
            add_auth_source_specs(&mut specs, &account.auth, kind);
        }
    }
    if specs.is_empty() {
        specs.extend(default_secret_source_specs(kind));
    }
    specs.extend(default_login_source_specs(kind));
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
    let mut specs = default_secret_source_specs(kind);
    specs.extend(default_login_source_specs(kind));
    specs
}

fn default_secret_source_specs(kind: ClientProfileKind) -> Vec<SourceSpec> {
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

fn default_login_source_specs(kind: ClientProfileKind) -> Vec<SourceSpec> {
    match kind {
        ClientProfileKind::Codex => Vec::new(),
        ClientProfileKind::ClaudeCode => vec![SourceSpec::JsonPresence {
            kind: "native_login_file",
            path: "${HOME}/.claude.json".to_string(),
            pointer: "/oauthAccount".to_string(),
        }],
    }
}

fn detect_token_sources(specs: Vec<SourceSpec>) -> Vec<TokenSourceStatus> {
    specs
        .into_iter()
        .map(|spec| match spec {
            SourceSpec::Env { name } => {
                let available = std::env::var(&name)
                    .ok()
                    .map(|value| !value.trim().is_empty())
                    .unwrap_or(false);
                TokenSourceStatus {
                    kind: "env",
                    label: name,
                    configured: true,
                    available: Some(available),
                }
            }
            SourceSpec::File { path, pointer } => {
                let expanded = expand_home(&path);
                let available = json_pointer_has_nonempty_string(&expanded, &pointer);
                TokenSourceStatus {
                    kind: "native_token_file",
                    label: format!("{} {}", path, pointer),
                    configured: true,
                    available: Some(available),
                }
            }
            SourceSpec::JsonPresence {
                kind,
                path,
                pointer,
            } => {
                let expanded = expand_home(&path);
                let available = json_pointer_has_nonempty_value(&expanded, &pointer);
                TokenSourceStatus {
                    kind,
                    label: format!("{} {}", path, pointer),
                    configured: true,
                    available: Some(available),
                }
            }
            SourceSpec::Vault { name } => TokenSourceStatus {
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

fn json_pointer_has_nonempty_value(path: &Path, pointer: &str) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };
    match value.pointer(pointer) {
        Some(serde_json::Value::String(value)) => !value.trim().is_empty(),
        Some(serde_json::Value::Array(value)) => !value.is_empty(),
        Some(serde_json::Value::Object(value)) => !value.is_empty(),
        Some(serde_json::Value::Null) | None => false,
        Some(_) => true,
    }
}

fn command_exists(command: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(command).is_file())
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

fn run_command_output(command: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(command).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if stdout.is_empty() {
        None
    } else {
        Some(stdout)
    }
}

fn native_client_kinds(target: NativeClientTarget) -> Vec<ClientProfileKind> {
    match target {
        NativeClientTarget::All => vec![ClientProfileKind::Codex, ClientProfileKind::ClaudeCode],
        NativeClientTarget::Codex => vec![ClientProfileKind::Codex],
        NativeClientTarget::ClaudeCode => vec![ClientProfileKind::ClaudeCode],
    }
}

fn native_command(kind: ClientProfileKind) -> &'static str {
    match kind {
        ClientProfileKind::Codex => "codex",
        ClientProfileKind::ClaudeCode => "claude",
    }
}

fn native_auth_kind(kind: ClientProfileKind) -> &'static str {
    match kind {
        ClientProfileKind::Codex => "codex_oauth",
        ClientProfileKind::ClaudeCode => "claude_code_oauth",
    }
}

fn native_route_name(kind: ClientProfileKind) -> &'static str {
    match kind {
        ClientProfileKind::Codex => "codex-native",
        ClientProfileKind::ClaudeCode => "claude-code-native",
    }
}

fn native_relay_provider_kind(kind: ClientProfileKind) -> &'static str {
    match kind {
        ClientProfileKind::Codex => "codex_native_relay",
        ClientProfileKind::ClaudeCode => "claude_code_native_relay",
    }
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

fn auth_kind_name(auth: &AuthConfig) -> &'static str {
    match auth {
        AuthConfig::None => "none",
        AuthConfig::ApiKey { .. } => "api_key",
        AuthConfig::Oauth { .. } => "oauth",
        AuthConfig::CodexOauth { .. } => "codex_oauth",
        AuthConfig::ClaudeCodeOauth { .. } => "claude_code_oauth",
        AuthConfig::ServiceAccount { .. } => "service_account",
        AuthConfig::AwsSigV4 { .. } => "aws_sigv4",
    }
}

fn provider_matches_native_relay(kind: &ProviderKind, client: ClientProfileKind) -> bool {
    matches!(
        (client, kind),
        (
            ClientProfileKind::Codex,
            ProviderKind::CodexNativeRelay { .. }
        ) | (
            ClientProfileKind::ClaudeCode,
            ProviderKind::ClaudeCodeNativeRelay { .. }
        )
    )
}

fn provider_kind_name(kind: &ProviderKind) -> &'static str {
    match kind {
        ProviderKind::Mock => "mock",
        ProviderKind::OpenaiCompatible { .. } => "openai_compatible",
        ProviderKind::Anthropic { .. } => "anthropic",
        ProviderKind::Gemini { .. } => "gemini",
        ProviderKind::Vertex { .. } => "vertex",
        ProviderKind::Bedrock { .. } => "bedrock",
        ProviderKind::CodexNativeRelay { .. } => "codex_native_relay",
        ProviderKind::ClaudeCodeNativeRelay { .. } => "claude_code_native_relay",
    }
}

fn print_native_profiles_text(report: &NativeProfilesReport) {
    println!(
        "native profiles {}",
        if report.ok { "ok" } else { "not-ok" }
    );
    println!("config {}", report.config);
    for profile in &report.profiles {
        println!(
            "{} kind={:?} mode={} ready={} fidelity={} models={} accounts={}",
            profile.id,
            profile.kind,
            profile.mode.as_str(),
            profile.ready,
            profile.fidelity.guarantee,
            profile
                .models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>()
                .join(","),
            profile
                .accounts
                .iter()
                .map(|account| account.reference.as_str())
                .collect::<Vec<_>>()
                .join(",")
        );
        for problem in &profile.problems {
            println!("  problem: {problem}");
        }
    }
}

fn print_native_profile_doctor_text(report: &NativeProfileDoctorReport) {
    let profile = &report.profile;
    println!(
        "native profile {} {}",
        profile.id,
        if report.ok { "ok" } else { "not-ok" }
    );
    println!(
        "kind={:?} protocol={} mode={} enabled={}",
        profile.kind,
        profile.protocol,
        profile.mode.as_str(),
        profile.enabled
    );
    for model in &profile.models {
        println!(
            "model {} resolvable={} via={}",
            model.id, model.resolvable, model.resolution
        );
    }
    for account in &profile.accounts {
        println!(
            "account {} exists={} provider_kind={} auth_kind={} native_relay_compatible={}",
            account.reference,
            account.exists,
            account.provider_kind.unwrap_or("-"),
            account.auth_kind.unwrap_or("-"),
            account.native_relay_compatible
        );
    }
    for problem in &profile.problems {
        println!("problem: {problem}");
    }
    println!("hint: {}", profile.command_hint);
}

fn print_native_profile_env_text(report: &NativeProfileEnvReport) {
    for var in &report.env {
        println!("export {}={:?}", var.name, var.value);
    }
    for header in &report.headers {
        println!("# header: {}: {}", header.name, header.value);
    }
    println!("# command: {}", report.command_hint);
    for warning in &report.warnings {
        println!("# warning: {warning}");
    }
}

fn print_native_status_text(report: &NativeStatusReport) {
    println!("native status {}", if report.ok { "ok" } else { "not-ok" });
    println!("config {}", report.config);
    println!(
        "validation {}",
        if report.validation.ok { "ok" } else { "failed" }
    );
    println!(
        "server {} health={} reachable={}",
        report.server.bind,
        report.server.health.status,
        report
            .server
            .health
            .reachable
            .map(|value| value.to_string())
            .unwrap_or_else(|| "-".to_string())
    );
    for client in &report.clients {
        println!(
            "{} installed={} token={} profiles={} native_accounts={}",
            client.id,
            client.installed,
            client.token_available,
            client.profiles.len(),
            client.native_accounts.len()
        );
        println!(
            "{} direct={} tap={} ingress={} relay={} fidelity={}",
            client.id,
            client.modes.direct_native.state,
            client.modes.native_tap.state,
            client.modes.switchback_ingress.state,
            client.modes.native_relay.state,
            client.fidelity.guarantee
        );
    }
    for conflict in &report.local_runtime.possible_conflicts {
        println!(
            "possible-runtime-conflict {} {}",
            conflict.source, conflict.id
        );
    }
    for warning in &report.warnings {
        println!("warning: {warning}");
    }
    for action in &report.next_actions {
        println!("next: {action}");
    }
}
