use std::path::Path;

use clap::Subcommand;
use sb_core::{ClientProfileKind, Config, ProviderKind, RouteConfig};
use serde::Serialize;

#[derive(Subcommand)]
pub(crate) enum LaneCmd {
    /// Inspect local lane identity, defaults, and fail-closed native state.
    Doctor,
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
    id: &'static str,
    state: LaneState,
    surface: &'static str,
    execution_class: &'static str,
    cost_policy: &'static str,
    resume_scope: &'static str,
    source: LaneSource,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    aliases: Vec<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    primary_target: Option<String>,
    fallback_count: usize,
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
    }
    Ok(())
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
        LaneReport {
            id: "pro/manual",
            state: LaneState::Manual,
            surface: "manual_pro",
            execution_class: "external_manual",
            cost_policy: "subscription_native",
            resume_scope: "not_applicable",
            source: LaneSource::ManualHandoff,
            aliases: vec!["oracle", "chatgpt-pro"],
            primary_target: None,
            fallback_count: 0,
            problems: Vec::new(),
            warnings: vec![
                "ChatGPT Pro is a creative handoff lane, not an automatic router provider"
                    .to_string(),
            ],
        },
    ];

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
    if lanes.iter().any(|lane| {
        matches!(
            lane.source,
            LaneSource::LegacyCombo {
                canonical_route: _,
                ..
            }
        )
    }) {
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

    lanes.sort_by_key(|lane| match lane.id {
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
        return lane_from_targets(
            spec,
            LaneState::Green,
            LaneSource::ExactRoute {
                name: route.name.clone(),
            },
            &route.targets,
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
            return lane_from_targets(
                spec,
                LaneState::Yellow,
                source,
                &combo.models,
                Vec::new(),
                vec![warning],
            );
        }
    }

    LaneReport {
        id: spec.id,
        state: LaneState::Red,
        surface: spec.surface,
        execution_class: spec.execution_class,
        cost_policy: spec.cost_policy,
        resume_scope: spec.resume_scope,
        source: LaneSource::Missing {
            expected: spec
                .legacy_combo
                .map(|combo| vec![spec.exact_route, combo])
                .unwrap_or_else(|| vec![spec.exact_route]),
        },
        aliases: spec.aliases,
        primary_target: None,
        fallback_count: 0,
        problems: vec![format!(
            "missing exact route `{}`{}",
            spec.exact_route,
            spec.legacy_combo
                .map(|combo| format!(" or legacy combo `{combo}`"))
                .unwrap_or_default()
        )],
        warnings: Vec::new(),
    }
}

fn lane_from_targets(
    spec: LaneSpec,
    state: LaneState,
    source: LaneSource,
    targets: &[String],
    problems: Vec<String>,
    warnings: Vec<String>,
) -> LaneReport {
    LaneReport {
        id: spec.id,
        state,
        surface: spec.surface,
        execution_class: spec.execution_class,
        cost_policy: spec.cost_policy,
        resume_scope: spec.resume_scope,
        source,
        aliases: spec.aliases,
        primary_target: targets.first().cloned(),
        fallback_count: targets.len().saturating_sub(1),
        problems,
        warnings,
    }
}

fn codex_api_lane(cfg: &Config) -> LaneReport {
    let spec = LaneSpec {
        id: "codex/api",
        surface: "openai_responses",
        execution_class: "paid_api_or_scout_pool",
        cost_policy: "cheap_first",
        resume_scope: "codex_profile:api",
        exact_route: "codex/api",
        legacy_combo: Some("nonstop-code"),
        aliases: vec!["codex"],
    };
    let mut lane = lane_from_route_or_combo(cfg, spec);
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

    LaneReport {
        id: "codex-native",
        state,
        surface: "openai_responses",
        execution_class: "native_relay",
        cost_policy: "subscription_native",
        resume_scope: "codex_profile:native",
        source,
        aliases: vec!["codex-native"],
        primary_target: targets.first().cloned(),
        fallback_count: targets.len().saturating_sub(1),
        problems,
        warnings,
    }
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
