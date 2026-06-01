use std::path::Path;
use std::time::Instant;

use sb_core::Config;
use sb_runtime::{EmbeddingsOutcome, Engine};

use crate::engine_from_config;

use super::{
    provider_missing_envs, provider_model_hint, provider_models_config, provider_scoped_config,
    provider_test_config, ProviderCertificationCounts, ProviderCertificationSummary,
    ProviderCertifyAllSummary, ProviderDoctorCheck, ProviderDoctorSummary,
    ProviderMatrixProviderSummary, ProviderMatrixSummary,
};

fn provider_doctor_check(
    name: &str,
    ok: bool,
    required: bool,
    status: &str,
    detail: Option<String>,
) -> ProviderDoctorCheck {
    ProviderDoctorCheck {
        name: name.to_string(),
        ok,
        required,
        status: status.to_string(),
        detail,
    }
}

fn provider_doctor_ok(
    name: &str,
    required: bool,
    detail: impl Into<Option<String>>,
) -> ProviderDoctorCheck {
    provider_doctor_check(name, true, required, "ok", detail.into())
}

fn provider_doctor_failed(
    name: &str,
    required: bool,
    detail: impl Into<String>,
) -> ProviderDoctorCheck {
    provider_doctor_check(name, false, required, "failed", Some(detail.into()))
}

fn provider_doctor_unsupported(name: &str, detail: impl Into<String>) -> ProviderDoctorCheck {
    provider_doctor_check(name, false, false, "unsupported", Some(detail.into()))
}

async fn provider_doctor_embeddings_check(
    engine: &Engine,
    target_model: &str,
) -> ProviderDoctorCheck {
    let body = serde_json::json!({
        "model": target_model,
        "input": "Switchback provider doctor"
    });
    let (_revision, outcome) = engine
        .execute_embeddings(body, None, None, None, Instant::now())
        .await;
    match outcome {
        EmbeddingsOutcome::Json { value, summary, .. } => {
            let rows = value
                .get("data")
                .and_then(serde_json::Value::as_array)
                .map(Vec::len)
                .unwrap_or_default();
            provider_doctor_ok(
                "embeddings",
                false,
                Some(format!("{summary}; embeddings={rows}")),
            )
        }
        EmbeddingsOutcome::Error { error, .. }
            if error.status == 422
                || error
                    .message
                    .to_ascii_lowercase()
                    .contains("embeddings not supported") =>
        {
            provider_doctor_unsupported("embeddings", error.message)
        }
        EmbeddingsOutcome::Error { error, .. } => provider_doctor_failed(
            "embeddings",
            false,
            format!("{}: {}", error.error_type, error.message),
        ),
    }
}

pub(crate) async fn provider_doctor_config_file(
    path: &Path,
    provider_id: &str,
    model: Option<&str>,
) -> anyhow::Result<ProviderDoctorSummary> {
    let cfg = Config::from_path(path)?;
    let cfg = provider_scoped_config(&cfg, provider_id)?;
    provider_doctor_config(cfg, provider_id, model).await
}

async fn provider_doctor_config(
    cfg: Config,
    provider_id: &str,
    model: Option<&str>,
) -> anyhow::Result<ProviderDoctorSummary> {
    let engine = engine_from_config(cfg)?;
    let revision = engine.revision();
    let mut checks = Vec::new();
    checks.push(provider_doctor_ok(
        "config",
        true,
        Some(format!("revision {revision}")),
    ));

    let explicit_model = model
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string);
    let model_hint = provider_model_hint(engine.snapshot().config.as_ref(), provider_id);
    let models_required = explicit_model.is_none() && model_hint.is_none();
    let discovered =
        provider_models_config(engine.snapshot().config.as_ref().clone(), provider_id).await;
    if explicit_model.is_none() {
        if let Some(model) = model_hint.as_deref() {
            checks.push(provider_doctor_ok(
                "model_hint",
                true,
                Some(format!("using configured model hint `{model}`")),
            ));
        }
    }
    match &discovered {
        Ok(summary) => {
            checks.push(provider_doctor_ok(
                "models",
                models_required,
                Some(format!("{} model(s) discoverable", summary.models.len())),
            ));
        }
        Err(e) => {
            checks.push(provider_doctor_failed(
                "models",
                models_required,
                e.to_string(),
            ));
            if models_required {
                anyhow::bail!("model discovery failed for `{provider_id}`; pass --model: {e}");
            }
        }
    };
    let resolved_model = if let Some(model) = explicit_model {
        model
    } else if let Some(model) = model_hint {
        model
    } else {
        match &discovered {
            Ok(summary) => summary
                .models
                .first()
                .map(|model| model.id.clone())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "provider `{provider_id}` did not report any models; pass --model"
                    )
                })?,
            Err(e) => {
                anyhow::bail!("model discovery failed for `{provider_id}`; pass --model: {e}")
            }
        }
    };
    let target_model = format!("{provider_id}/{resolved_model}");

    let mut req = sb_core::AiRequest::new(
        target_model.clone(),
        vec![sb_core::Message::user("Switchback provider doctor")],
    );
    req.max_output_tokens = Some(32);
    req.temperature = Some(0.0);
    match engine.preview_route(&req) {
        Ok((_preview_revision, plan)) => {
            let selected = plan
                .decision
                .selected
                .as_ref()
                .map(|target| target.target_id.as_str());
            if selected == Some(target_model.as_str()) {
                checks.push(provider_doctor_ok(
                    "route_preview",
                    true,
                    Some(plan.decision.summary()),
                ));
            } else {
                checks.push(provider_doctor_failed(
                    "route_preview",
                    true,
                    format!(
                        "selected `{}`, expected `{target_model}`",
                        selected.unwrap_or("<none>")
                    ),
                ));
            }
        }
        Err(e) => checks.push(provider_doctor_failed("route_preview", true, e.message)),
    }

    match provider_test_config(
        engine.snapshot().config.as_ref().clone(),
        provider_id,
        Some(&resolved_model),
        false,
    )
    .await
    {
        Ok(summary) => checks.push(provider_doctor_ok(
            "chat_non_stream",
            true,
            Some(format!(
                "{}; output_chars={}",
                summary.summary, summary.output_chars
            )),
        )),
        Err(e) => checks.push(provider_doctor_failed(
            "chat_non_stream",
            true,
            e.to_string(),
        )),
    }

    match provider_test_config(
        engine.snapshot().config.as_ref().clone(),
        provider_id,
        Some(&resolved_model),
        true,
    )
    .await
    {
        Ok(summary) => checks.push(provider_doctor_ok(
            "chat_stream",
            true,
            Some(format!(
                "{}; events={}; output_chars={}",
                summary.summary, summary.event_count, summary.output_chars
            )),
        )),
        Err(e) => checks.push(provider_doctor_failed("chat_stream", true, e.to_string())),
    }

    checks.push(provider_doctor_embeddings_check(&engine, &target_model).await);
    let ok = checks
        .iter()
        .filter(|check| check.required)
        .all(|check| check.ok);

    Ok(ProviderDoctorSummary {
        ok,
        revision,
        provider_id: provider_id.to_string(),
        model: resolved_model,
        target: target_model,
        checks,
    })
}

fn provider_certification_counts(checks: &[ProviderDoctorCheck]) -> ProviderCertificationCounts {
    ProviderCertificationCounts {
        required_passed: checks
            .iter()
            .filter(|check| check.required && check.ok)
            .count(),
        required_failed: checks
            .iter()
            .filter(|check| check.required && !check.ok)
            .count(),
        optional_passed: checks
            .iter()
            .filter(|check| !check.required && check.ok)
            .count(),
        optional_failed: checks
            .iter()
            .filter(|check| !check.required && !check.ok && check.status != "unsupported")
            .count(),
        optional_unsupported: checks
            .iter()
            .filter(|check| !check.required && check.status == "unsupported")
            .count(),
    }
}

fn provider_verified_capabilities(checks: &[ProviderDoctorCheck]) -> Vec<String> {
    let check_ok = |name: &str| checks.iter().any(|check| check.name == name && check.ok);
    let mut capabilities = Vec::new();
    if check_ok("models") {
        capabilities.push("model_discovery".to_string());
    }
    if check_ok("route_preview") {
        capabilities.push("route_preview".to_string());
    }
    if check_ok("chat_non_stream") {
        capabilities.push("chat_non_stream".to_string());
    }
    if check_ok("chat_stream") {
        capabilities.push("chat_stream".to_string());
    }
    if check_ok("embeddings") {
        capabilities.push("embeddings".to_string());
    }
    capabilities
}

fn provider_certification_next_commands(provider_id: &str, model: Option<&str>) -> Vec<String> {
    let routed_model = model
        .map(|model| format!("{provider_id}/{model}"))
        .unwrap_or_else(|| format!("{provider_id}/<model>"));
    vec![
        format!("switchback provider doctor {provider_id} --config switchback.yaml"),
        format!("switchback route-preview --config switchback.yaml --model {routed_model}"),
        "switchback serve --config switchback.yaml".to_string(),
    ]
}

fn provider_certification_env_commands(missing_env: &[String]) -> Vec<String> {
    missing_env
        .iter()
        .map(|name| format!("export {name}=<redacted>"))
        .collect()
}

fn provider_certification_from_doctor(
    doctor: ProviderDoctorSummary,
) -> ProviderCertificationSummary {
    let counts = provider_certification_counts(&doctor.checks);
    let status = if counts.required_failed == 0 {
        "certified"
    } else {
        "failed"
    };
    let mut recommendations = Vec::new();
    if status == "certified" {
        recommendations.push("Provider is ready for chat and streaming traffic.".to_string());
    } else {
        recommendations
            .push("Fix failed required checks, then rerun provider certification.".to_string());
    }
    if counts.optional_unsupported > 0 {
        recommendations.push(
            "Optional embeddings are unsupported; chat/stream certification is still valid."
                .to_string(),
        );
    }
    if counts.optional_failed > 0 {
        recommendations
            .push("Review failed optional checks before enabling those features.".to_string());
    }

    ProviderCertificationSummary {
        schema: "switchback/provider-certification@1",
        ok: status == "certified",
        status: status.to_string(),
        provider_id: doctor.provider_id.clone(),
        revision: Some(doctor.revision),
        model: Some(doctor.model.clone()),
        target: Some(doctor.target.clone()),
        summary: counts,
        verified_capabilities: provider_verified_capabilities(&doctor.checks),
        checks: doctor.checks,
        missing_env: Vec::new(),
        recommendations,
        next_commands: provider_certification_next_commands(
            &doctor.provider_id,
            Some(&doctor.model),
        ),
    }
}

pub(crate) async fn provider_certify_config_file(
    path: &Path,
    provider_id: &str,
    model: Option<&str>,
) -> anyhow::Result<ProviderCertificationSummary> {
    let cfg = Config::from_path(path)?;
    provider_certify_config(&cfg, provider_id, model).await
}

async fn provider_certify_config(
    cfg: &Config,
    provider_id: &str,
    model: Option<&str>,
) -> anyhow::Result<ProviderCertificationSummary> {
    let provider = cfg
        .providers
        .iter()
        .find(|provider| provider.id == provider_id)
        .ok_or_else(|| anyhow::anyhow!("provider `{provider_id}` is not configured"))?;
    let missing_env = provider_missing_envs(provider);
    if !missing_env.is_empty() {
        return Ok(ProviderCertificationSummary {
            schema: "switchback/provider-certification@1",
            ok: false,
            status: "blocked".to_string(),
            provider_id: provider_id.to_string(),
            revision: None,
            model: model
                .map(ToString::to_string)
                .or_else(|| provider.model_hint.clone()),
            target: None,
            summary: ProviderCertificationCounts {
                required_passed: 0,
                required_failed: 1,
                optional_passed: 0,
                optional_failed: 0,
                optional_unsupported: 0,
            },
            verified_capabilities: Vec::new(),
            checks: vec![provider_doctor_failed(
                "credentials",
                true,
                format!("missing env: {}", missing_env.join(", ")),
            )],
            missing_env: missing_env.clone(),
            recommendations: vec![format!(
                "Set missing environment variables, then rerun: {}",
                missing_env.join(", ")
            )],
            next_commands: provider_certification_env_commands(&missing_env)
                .into_iter()
                .chain(provider_certification_next_commands(provider_id, model))
                .collect(),
        });
    }

    match provider_doctor_config(
        provider_scoped_config(cfg, provider_id)?,
        provider_id,
        model,
    )
    .await
    {
        Ok(doctor) => Ok(provider_certification_from_doctor(doctor)),
        Err(e) => Ok(ProviderCertificationSummary {
            schema: "switchback/provider-certification@1",
            ok: false,
            status: "blocked".to_string(),
            provider_id: provider_id.to_string(),
            revision: None,
            model: model
                .map(ToString::to_string)
                .or_else(|| provider.model_hint.clone()),
            target: None,
            summary: ProviderCertificationCounts {
                required_passed: 0,
                required_failed: 1,
                optional_passed: 0,
                optional_failed: 0,
                optional_unsupported: 0,
            },
            verified_capabilities: Vec::new(),
            checks: vec![provider_doctor_failed("certification", true, e.to_string())],
            missing_env: Vec::new(),
            recommendations: vec![
                "Resolve the certification blocker and rerun provider certify.".to_string(),
            ],
            next_commands: provider_certification_next_commands(provider_id, model),
        }),
    }
}

fn provider_certification_skipped_missing_env(
    provider_id: &str,
    model: Option<String>,
    missing_env: Vec<String>,
) -> ProviderCertificationSummary {
    let next_commands = provider_certification_env_commands(&missing_env)
        .into_iter()
        .chain(provider_certification_next_commands(
            provider_id,
            model.as_deref(),
        ))
        .collect();
    ProviderCertificationSummary {
        schema: "switchback/provider-certification@1",
        ok: false,
        status: "skipped".to_string(),
        provider_id: provider_id.to_string(),
        revision: None,
        model,
        target: None,
        summary: ProviderCertificationCounts {
            required_passed: 0,
            required_failed: 0,
            optional_passed: 0,
            optional_failed: 0,
            optional_unsupported: 0,
        },
        verified_capabilities: Vec::new(),
        checks: vec![provider_doctor_check(
            "credentials",
            false,
            true,
            "skipped",
            Some(format!("missing env: {}", missing_env.join(", "))),
        )],
        missing_env: missing_env.clone(),
        recommendations: vec![
            "Skipped because --skip-missing-env was set and credentials are absent.".to_string(),
            format!(
                "Set missing environment variables to include this provider: {}",
                missing_env.join(", ")
            ),
        ],
        next_commands,
    }
}

pub(crate) async fn provider_certify_all_config_file(
    path: &Path,
    skip_missing_env: bool,
) -> anyhow::Result<ProviderCertifyAllSummary> {
    let cfg = Config::from_path(path)?;
    let mut providers = Vec::new();
    for provider in &cfg.providers {
        let missing_env = provider_missing_envs(provider);
        if skip_missing_env && !missing_env.is_empty() {
            providers.push(provider_certification_skipped_missing_env(
                &provider.id,
                provider.model_hint.clone(),
                missing_env,
            ));
            continue;
        }
        providers.push(provider_certify_config(&cfg, &provider.id, None).await?);
    }

    let certified = providers
        .iter()
        .filter(|provider| provider.status == "certified")
        .count();
    let blocked = providers
        .iter()
        .filter(|provider| provider.status == "blocked")
        .count();
    let skipped = providers
        .iter()
        .filter(|provider| provider.status == "skipped")
        .count();
    let failed = providers
        .iter()
        .filter(|provider| provider.status == "failed")
        .count();

    Ok(ProviderCertifyAllSummary {
        schema: "switchback/provider-certifications@1",
        ok: blocked == 0 && failed == 0,
        total: providers.len(),
        certified,
        skipped,
        blocked,
        failed,
        providers,
    })
}

pub(crate) async fn provider_matrix_config_file(
    path: &Path,
) -> anyhow::Result<ProviderMatrixSummary> {
    let cfg = Config::from_path(path)?;
    let mut providers = Vec::new();
    let mut checked = 0usize;
    let mut skipped = 0usize;
    let mut failed = 0usize;

    for provider in &cfg.providers {
        let missing_env = provider_missing_envs(provider);
        if !missing_env.is_empty() {
            skipped += 1;
            providers.push(ProviderMatrixProviderSummary {
                provider_id: provider.id.clone(),
                status: "skipped".to_string(),
                ok: false,
                missing_env,
                reason: Some("required credential environment variable is not set".to_string()),
                doctor: None,
            });
            continue;
        }

        checked += 1;
        let scoped = provider_scoped_config(&cfg, &provider.id)?;
        match provider_doctor_config(scoped, &provider.id, None).await {
            Ok(doctor) if doctor.ok => providers.push(ProviderMatrixProviderSummary {
                provider_id: provider.id.clone(),
                status: "ok".to_string(),
                ok: true,
                missing_env: Vec::new(),
                reason: None,
                doctor: Some(doctor),
            }),
            Ok(doctor) => {
                failed += 1;
                providers.push(ProviderMatrixProviderSummary {
                    provider_id: provider.id.clone(),
                    status: "failed".to_string(),
                    ok: false,
                    missing_env: Vec::new(),
                    reason: Some("one or more required provider checks failed".to_string()),
                    doctor: Some(doctor),
                });
            }
            Err(e) => {
                failed += 1;
                providers.push(ProviderMatrixProviderSummary {
                    provider_id: provider.id.clone(),
                    status: "failed".to_string(),
                    ok: false,
                    missing_env: Vec::new(),
                    reason: Some(e.to_string()),
                    doctor: None,
                });
            }
        }
    }

    Ok(ProviderMatrixSummary {
        schema: "switchback/provider-matrix@1",
        ok: failed == 0,
        total: cfg.providers.len(),
        checked,
        skipped,
        failed,
        providers,
    })
}
