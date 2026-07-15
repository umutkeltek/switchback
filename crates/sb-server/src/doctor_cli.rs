use sb_core::{Config, ProviderConfig, ProviderKind};
use sb_runtime::Engine;
use serde::Serialize;

use crate::controlplane;
use crate::provider_cli::{provider_auth_env_names, provider_missing_envs};

#[derive(Debug, Serialize)]
struct DoctorValidationReport {
    ok: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    problems: Vec<String>,
}

#[derive(Debug, Serialize)]
struct DoctorEnvReport {
    name: String,
    present: bool,
}

#[derive(Debug, Serialize)]
struct DoctorProviderReport {
    id: String,
    #[serde(rename = "type")]
    provider_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    project: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    region: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model_hint: Option<String>,
    account_count: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    auth_envs: Vec<DoctorEnvReport>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    missing_env: Vec<String>,
}

#[derive(Debug, Serialize)]
struct DoctorRouteReport {
    name: String,
    targets: Vec<String>,
}

#[derive(Debug, Serialize)]
struct DoctorEgressReport {
    id: String,
    kind: String,
    enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reachable: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    problem: Option<String>,
}

#[derive(Debug, Serialize)]
struct DoctorCatalogReport {
    providers: usize,
    models: usize,
    accounts: usize,
    credentials: usize,
    prices: usize,
    ok: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    problems: Vec<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct DoctorReport {
    ok: bool,
    validation: DoctorValidationReport,
    providers: Vec<DoctorProviderReport>,
    routes: Vec<DoctorRouteReport>,
    egress_master_switch: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    egress: Vec<DoctorEgressReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    catalog: Option<DoctorCatalogReport>,
}

fn doctor_provider_report(provider: &ProviderConfig) -> DoctorProviderReport {
    let (base_url, project, region) = match &provider.kind {
        ProviderKind::OpenaiCompatible { base_url, .. }
        | ProviderKind::Anthropic { base_url, .. }
        | ProviderKind::Gemini { base_url, .. } => (Some(base_url.clone()), None, None),
        ProviderKind::Vertex {
            base_url,
            project,
            region,
            ..
        } => (
            base_url.clone(),
            Some(project.clone()),
            Some(region.clone()),
        ),
        ProviderKind::Bedrock {
            base_url, region, ..
        } => (base_url.clone(), None, Some(region.clone())),
        ProviderKind::ComfyUi { base_url, .. } => (Some(base_url.clone()), None, None),
        ProviderKind::Fal { base_url, .. } => (Some(base_url.clone()), None, None),
        ProviderKind::CodexNativeRelay { base_url }
        | ProviderKind::ClaudeCodeNativeRelay { base_url } => (base_url.clone(), None, None),
        ProviderKind::Mock => (None, None, None),
    };
    let auth_envs = provider_auth_env_names(provider)
        .into_iter()
        .map(|name| DoctorEnvReport {
            present: std::env::var(&name).is_ok(),
            name,
        })
        .collect();
    DoctorProviderReport {
        id: provider.id.clone(),
        provider_type: controlplane::provider_type_name(&provider.kind).to_string(),
        base_url,
        project,
        region,
        model_hint: provider.model_hint.clone(),
        account_count: provider.accounts.len(),
        auth_envs,
        missing_env: provider_missing_envs(provider),
    }
}

pub(crate) async fn doctor_report(cfg: &Config) -> DoctorReport {
    let validation = match Engine::validate_config(cfg) {
        Ok(()) => DoctorValidationReport {
            ok: true,
            problems: Vec::new(),
        },
        Err(e) => DoctorValidationReport {
            ok: false,
            problems: e.split("; ").map(ToString::to_string).collect(),
        },
    };
    let providers = cfg.providers.iter().map(doctor_provider_report).collect();
    let routes = cfg
        .routes
        .iter()
        .map(|route| DoctorRouteReport {
            name: route.name.clone(),
            targets: route.targets.clone(),
        })
        .collect();

    let mut egress = Vec::new();
    for egress_config in &cfg.egress {
        match &egress_config.kind {
            sb_core::EgressKind::Direct => egress.push(DoctorEgressReport {
                id: egress_config.id.clone(),
                kind: "direct".to_string(),
                enabled: egress_config.enabled,
                target: None,
                reachable: None,
                problem: None,
            }),
            sb_core::EgressKind::Proxy { url, url_env } => {
                let resolved = url_env
                    .as_deref()
                    .and_then(|name| std::env::var(name).ok())
                    .or_else(|| url.clone());
                match resolved.as_deref().and_then(proxy_host_port) {
                    Some(host_port) => {
                        let reachable = if egress_config.enabled {
                            probe_tcp(&host_port).await
                        } else {
                            false
                        };
                        egress.push(DoctorEgressReport {
                            id: egress_config.id.clone(),
                            kind: "proxy".to_string(),
                            enabled: egress_config.enabled,
                            target: Some(host_port),
                            reachable: Some(reachable),
                            problem: None,
                        });
                    }
                    None => egress.push(DoctorEgressReport {
                        id: egress_config.id.clone(),
                        kind: "proxy".to_string(),
                        enabled: egress_config.enabled,
                        target: None,
                        reachable: None,
                        problem: Some("no reachable url/url_env".to_string()),
                    }),
                }
            }
        }
    }

    let catalog = cfg.catalog.as_ref().map(|catalog| {
        let problems = catalog.validate();
        DoctorCatalogReport {
            providers: catalog.providers.len(),
            models: catalog.models.len(),
            accounts: catalog.accounts.len(),
            credentials: catalog.credentials.len(),
            prices: catalog.prices.len(),
            ok: problems.is_empty(),
            problems,
        }
    });
    let ok = validation.ok && catalog.as_ref().map(|c| c.ok).unwrap_or(true);

    DoctorReport {
        ok,
        validation,
        providers,
        routes,
        egress_master_switch: cfg.server.egress_enabled,
        egress,
        catalog,
    }
}

pub(crate) async fn doctor_report_json(cfg: &Config) -> anyhow::Result<serde_json::Value> {
    Ok(serde_json::to_value(doctor_report(cfg).await)?)
}

pub(crate) fn print_doctor_text(report: &DoctorReport) {
    for provider in &report.providers {
        match (
            provider.base_url.as_deref(),
            provider.project.as_deref(),
            provider.region.as_deref(),
        ) {
            (Some(base_url), _, _) => {
                println!(
                    "provider {} {} base_url={}",
                    provider.id, provider.provider_type, base_url
                );
            }
            (None, Some(project), Some(region)) => {
                println!(
                    "provider {} {} project={} region={}",
                    provider.id, provider.provider_type, project, region
                );
            }
            (None, _, Some(region)) => {
                println!(
                    "provider {} {} region={}",
                    provider.id, provider.provider_type, region
                );
            }
            _ => println!("provider {} {}", provider.id, provider.provider_type),
        }
        for env in &provider.auth_envs {
            println!(
                "provider {} api_key_env={} present={}",
                provider.id, env.name, env.present
            );
        }
    }

    for route in &report.routes {
        println!("route {} targets={}", route.name, route.targets.join(","));
    }

    if !report.egress.is_empty() {
        println!("egress: master_switch={}", report.egress_master_switch);
    }
    for egress in &report.egress {
        match egress.kind.as_str() {
            "direct" => println!("egress {} direct enabled={}", egress.id, egress.enabled),
            "proxy" => match (egress.target.as_deref(), egress.reachable) {
                (Some(target), Some(reachable)) => println!(
                    "egress {} proxy enabled={} target={} reachable={}",
                    egress.id, egress.enabled, target, reachable
                ),
                _ => println!(
                    "egress {} proxy PROBLEM: {}",
                    egress.id,
                    egress.problem.as_deref().unwrap_or("unreachable")
                ),
            },
            _ => {}
        }
    }

    if let Some(catalog) = &report.catalog {
        println!(
            "catalog: {} providers, {} models, {} accounts, {} credentials, {} prices",
            catalog.providers,
            catalog.models,
            catalog.accounts,
            catalog.credentials,
            catalog.prices
        );
        if catalog.ok {
            println!("catalog: referential integrity OK");
        } else {
            for problem in &catalog.problems {
                println!("catalog PROBLEM: {problem}");
            }
        }
    }
}

/// Extract `host:port` from a proxy URL (`scheme://[user:pass@]host:port[/...]`).
fn proxy_host_port(url: &str) -> Option<String> {
    let after_scheme = url.split("://").nth(1).unwrap_or(url);
    let after_auth = after_scheme.rsplit('@').next()?;
    let host_port = after_auth.split(['/', '?']).next()?;
    (!host_port.is_empty()).then(|| host_port.to_string())
}

/// Best-effort TCP reachability probe with a short timeout (for `doctor`).
async fn probe_tcp(host_port: &str) -> bool {
    matches!(
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            tokio::net::TcpStream::connect(host_port),
        )
        .await,
        Ok(Ok(_))
    )
}
