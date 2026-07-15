use std::collections::HashSet;
use std::time::Duration;

use sb_core::{Config, ProviderConfig, ProviderKind};
use sb_credentials::{CredentialResolver, ResolveOutcome};
use serde::Serialize;

const FAL_BILLING_PATH: &str = "v1/account/billing";
const FAL_BALANCE_MODEL_SCOPE: &str = "__account_billing__";

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FalProbeLevel {
    Ok,
    Warn,
    Fail,
    Skip,
}

impl FalProbeLevel {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Fail => "fail",
            Self::Skip => "skip",
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct FalBalanceCheck {
    pub provider_id: String,
    pub level: FalProbeLevel,
    pub endpoint: String,
    pub low_balance_threshold_usd: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub balance_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub currency: Option<String>,
    pub detail: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct FalBalanceReport {
    pub schema: &'static str,
    pub level: FalProbeLevel,
    pub checks: Vec<FalBalanceCheck>,
}

impl FalBalanceReport {
    fn new(checks: Vec<FalBalanceCheck>) -> Self {
        let level = aggregate_level(&checks);
        Self {
            schema: "switchback/fal-balance-doctor@1",
            level,
            checks,
        }
    }

    fn skipped(detail: impl Into<String>) -> Self {
        Self::new(vec![FalBalanceCheck {
            provider_id: "fal".to_string(),
            level: FalProbeLevel::Skip,
            endpoint: default_billing_endpoint(),
            low_balance_threshold_usd: 5.0,
            balance_usd: None,
            currency: None,
            detail: detail.into(),
        }])
    }
}

pub(crate) async fn fal_balance_report(cfg: &Config, timeout_ms: u64) -> FalBalanceReport {
    let providers: Vec<_> = cfg
        .providers
        .iter()
        .filter(|provider| matches!(provider.kind, ProviderKind::Fal { .. }))
        .collect();
    if providers.is_empty() {
        return FalBalanceReport::skipped("no fal provider configured");
    }

    let resolver = match CredentialResolver::from_config(cfg) {
        Ok(resolver) => resolver,
        Err(error) => {
            return FalBalanceReport::new(
                providers
                    .into_iter()
                    .map(|provider| failed_check(provider, format!("credential resolver: {error}")))
                    .collect(),
            )
        }
    };
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_millis(timeout_ms.max(1)))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            return FalBalanceReport::new(
                providers
                    .into_iter()
                    .map(|provider| failed_check(provider, format!("build fal client: {error}")))
                    .collect(),
            )
        }
    };

    let mut checks = Vec::with_capacity(providers.len());
    for provider in providers {
        checks.push(probe_provider(cfg, &resolver, &client, provider).await);
    }
    FalBalanceReport::new(checks)
}

pub(crate) fn print_fal_balance_text(report: &FalBalanceReport) {
    println!("fal balance doctor: {}", report.level.as_str());
    for check in &report.checks {
        println!(
            "{} {}: {}",
            check.level.as_str(),
            check.provider_id,
            check.detail
        );
    }
}

async fn probe_provider(
    cfg: &Config,
    resolver: &CredentialResolver,
    client: &reqwest::Client,
    provider: &ProviderConfig,
) -> FalBalanceCheck {
    let ProviderKind::Fal {
        platform_base_url,
        low_balance_threshold_usd,
        ..
    } = &provider.kind
    else {
        return failed_check(provider, "provider is not configured as fal".to_string());
    };
    let endpoint = match billing_endpoint(platform_base_url) {
        Ok(endpoint) => endpoint,
        Err(error) => return failed_check(provider, error),
    };
    if !low_balance_threshold_usd.is_finite() || *low_balance_threshold_usd < 0.0 {
        return failed_check(
            provider,
            "low_balance_threshold_usd must be a finite non-negative number".to_string(),
        );
    }

    let (account_id, lease) =
        match resolver.resolve(&provider.id, FAL_BALANCE_MODEL_SCOPE, &HashSet::new()) {
            ResolveOutcome::Selected { account_id, lease } if !lease.secret.is_empty() => {
                (account_id, lease)
            }
            ResolveOutcome::Selected { .. } | ResolveOutcome::NoAccounts => {
                return FalBalanceCheck {
                    provider_id: provider.id.clone(),
                    level: FalProbeLevel::Skip,
                    endpoint: endpoint.to_string(),
                    low_balance_threshold_usd: *low_balance_threshold_usd,
                    balance_usd: None,
                    currency: None,
                    detail: "fal credential is not configured".to_string(),
                }
            }
            ResolveOutcome::AllUnavailable { .. } => {
                return failed_check(
                    provider,
                    "all fal credential accounts are unavailable".to_string(),
                )
            }
        };
    let lease = match resolver.fresh_lease(&provider.id, &account_id, lease).await {
        Ok(lease) if !lease.secret.is_empty() => lease,
        Ok(_) => {
            return FalBalanceCheck {
                provider_id: provider.id.clone(),
                level: FalProbeLevel::Skip,
                endpoint: endpoint.to_string(),
                low_balance_threshold_usd: *low_balance_threshold_usd,
                balance_usd: None,
                currency: None,
                detail: "fal credential is not configured".to_string(),
            }
        }
        Err(error) => return failed_check(provider, format!("resolve fal credential: {error}")),
    };

    if let Err(error) = sb_net::guard_url(
        endpoint.as_str(),
        sb_net::NetworkUrlKind::ProviderUpstream,
        cfg.server.block_private_networks,
    )
    .await
    {
        return failed_check(provider, format!("fal billing endpoint rejected: {error}"));
    }

    let response = match client
        .get(endpoint.clone())
        .header(
            reqwest::header::AUTHORIZATION,
            format!("Key {}", lease.secret.expose()),
        )
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => return failed_check(provider, format!("fal balance request: {error}")),
    };
    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return failed_check(
            provider,
            format!("fal authentication failed with status {status}"),
        );
    }
    if !status.is_success() {
        return failed_check(
            provider,
            format!("fal balance request failed with status {status}"),
        );
    }
    let body: serde_json::Value = match response.json().await {
        Ok(body) => body,
        Err(error) => {
            return failed_check(
                provider,
                format!("fal balance response is not JSON: {error}"),
            )
        }
    };
    let Some(balance_usd) = parse_balance_usd(&body) else {
        return failed_check(
            provider,
            "fal balance response did not contain a numeric current balance".to_string(),
        );
    };
    let currency = parse_currency(&body);
    let level = classify_balance(balance_usd, *low_balance_threshold_usd);
    let detail = match level {
        FalProbeLevel::Warn => format!(
            "fal balance ${balance_usd:.2} is below the ${low_balance_threshold_usd:.2} threshold"
        ),
        FalProbeLevel::Ok => format!(
            "fal balance ${balance_usd:.2} is at or above the ${low_balance_threshold_usd:.2} threshold"
        ),
        FalProbeLevel::Fail | FalProbeLevel::Skip => unreachable!("balance classification"),
    };
    FalBalanceCheck {
        provider_id: provider.id.clone(),
        level,
        endpoint: endpoint.to_string(),
        low_balance_threshold_usd: *low_balance_threshold_usd,
        balance_usd: Some(balance_usd),
        currency,
        detail,
    }
}

fn failed_check(provider: &ProviderConfig, detail: String) -> FalBalanceCheck {
    let (platform_base_url, threshold) = match &provider.kind {
        ProviderKind::Fal {
            platform_base_url,
            low_balance_threshold_usd,
            ..
        } => (platform_base_url.as_str(), *low_balance_threshold_usd),
        _ => ("https://api.fal.ai", 5.0),
    };
    FalBalanceCheck {
        provider_id: provider.id.clone(),
        level: FalProbeLevel::Fail,
        endpoint: billing_endpoint(platform_base_url)
            .map(|endpoint| endpoint.to_string())
            .unwrap_or_else(|_| default_billing_endpoint()),
        low_balance_threshold_usd: threshold,
        balance_usd: None,
        currency: None,
        detail,
    }
}

fn aggregate_level(checks: &[FalBalanceCheck]) -> FalProbeLevel {
    if checks
        .iter()
        .any(|check| check.level == FalProbeLevel::Fail)
    {
        FalProbeLevel::Fail
    } else if checks
        .iter()
        .any(|check| check.level == FalProbeLevel::Warn)
    {
        FalProbeLevel::Warn
    } else if checks.iter().any(|check| check.level == FalProbeLevel::Ok) {
        FalProbeLevel::Ok
    } else {
        FalProbeLevel::Skip
    }
}

fn classify_balance(balance_usd: f64, threshold_usd: f64) -> FalProbeLevel {
    if balance_usd < threshold_usd {
        FalProbeLevel::Warn
    } else {
        FalProbeLevel::Ok
    }
}

fn parse_balance_usd(value: &serde_json::Value) -> Option<f64> {
    [
        value.pointer("/credits/current_balance"),
        value.pointer("/credits/balance"),
        value.pointer("/current_balance"),
        value.pointer("/balance"),
    ]
    .into_iter()
    .flatten()
    .find_map(parse_finite_number)
}

fn parse_currency(value: &serde_json::Value) -> Option<String> {
    value
        .pointer("/credits/currency")
        .or_else(|| value.pointer("/currency"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

fn parse_finite_number(value: &serde_json::Value) -> Option<f64> {
    let number = value
        .as_f64()
        .or_else(|| value.as_str().and_then(|text| text.parse().ok()))?;
    number.is_finite().then_some(number)
}

fn billing_endpoint(platform_base_url: &str) -> Result<reqwest::Url, String> {
    let normalized = format!("{}/", platform_base_url.trim_end_matches('/'));
    let mut endpoint = reqwest::Url::parse(&normalized)
        .and_then(|base| base.join(FAL_BILLING_PATH))
        .map_err(|error| format!("invalid fal platform_base_url: {error}"))?;
    endpoint.query_pairs_mut().append_pair("expand", "credits");
    Ok(endpoint)
}

fn default_billing_endpoint() -> String {
    "https://api.fal.ai/v1/account/billing?expand=credits".to_string()
}

#[cfg(test)]
mod tests {
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::get;
    use axum::{Json, Router};
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Clone)]
    struct FakeBilling {
        status: StatusCode,
        body: serde_json::Value,
        authorization: Arc<Mutex<Option<String>>>,
    }

    async fn billing_handler(
        State(state): State<FakeBilling>,
        headers: HeaderMap,
    ) -> impl IntoResponse {
        *state.authorization.lock().unwrap() = headers
            .get(reqwest::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        (state.status, Json(state.body))
    }

    async fn fake_billing(
        status: StatusCode,
        body: serde_json::Value,
    ) -> (String, Arc<Mutex<Option<String>>>) {
        let authorization = Arc::new(Mutex::new(None));
        let app = Router::new()
            .route("/v1/account/billing", get(billing_handler))
            .with_state(FakeBilling {
                status,
                body,
                authorization: authorization.clone(),
            });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{address}"), authorization)
    }

    fn fal_config(platform_base_url: &str, auth: &str, threshold: f64) -> Config {
        Config::from_yaml(&format!(
            r#"
server:
  bind: "127.0.0.1:8765"
  block_private_networks: false
providers:
  - id: fal
    type: fal
    platform_base_url: "{platform_base_url}"
    low_balance_threshold_usd: {threshold}
    accounts:
      - id: test
        auth: {auth}
"#
        ))
        .unwrap()
    }

    #[test]
    fn parses_documented_and_defensive_balance_shapes() {
        assert_eq!(
            parse_balance_usd(&json!({"credits": {"current_balance": 12.5}})),
            Some(12.5)
        );
        assert_eq!(
            parse_balance_usd(&json!({"credits": {"balance": "4.25"}})),
            Some(4.25)
        );
        assert_eq!(parse_balance_usd(&json!({"balance": 3})), Some(3.0));
        assert_eq!(parse_balance_usd(&json!({"credits": {}})), None);
    }

    #[test]
    fn classifies_balance_against_configured_threshold() {
        assert_eq!(classify_balance(5.0, 5.0), FalProbeLevel::Ok);
        assert_eq!(classify_balance(4.99, 5.0), FalProbeLevel::Warn);
    }

    #[tokio::test]
    async fn skips_cleanly_when_fal_credential_is_unconfigured() {
        let cfg = fal_config("https://api.fal.ai", "{ kind: none }", 5.0);
        let report = fal_balance_report(&cfg, 100).await;
        assert_eq!(report.level, FalProbeLevel::Skip);
        assert_eq!(report.checks[0].level, FalProbeLevel::Skip);
    }

    #[tokio::test]
    async fn probes_balance_with_resolved_credential_and_warns_below_threshold() {
        let (base_url, authorization) = fake_billing(
            StatusCode::OK,
            json!({"credits": {"current_balance": "4.25", "currency": "USD"}}),
        )
        .await;
        let cfg = fal_config(
            &base_url,
            "{ kind: api_key, inline: \"fal-test-secret\" }",
            5.0,
        );
        let report = fal_balance_report(&cfg, 1_000).await;
        assert_eq!(report.level, FalProbeLevel::Warn);
        assert_eq!(report.checks[0].balance_usd, Some(4.25));
        assert_eq!(report.checks[0].currency.as_deref(), Some("USD"));
        assert_eq!(
            authorization.lock().unwrap().as_deref(),
            Some("Key fal-test-secret")
        );
        let serialized = serde_json::to_string(&report).unwrap();
        assert!(!serialized.contains("fal-test-secret"));
    }

    #[tokio::test]
    async fn authentication_error_is_a_failed_probe_without_response_body_leakage() {
        let (base_url, _) = fake_billing(
            StatusCode::UNAUTHORIZED,
            json!({"error": "secret provider response must not appear"}),
        )
        .await;
        let cfg = fal_config(
            &base_url,
            "{ kind: api_key, inline: \"fal-test-secret\" }",
            5.0,
        );
        let report = fal_balance_report(&cfg, 1_000).await;
        assert_eq!(report.level, FalProbeLevel::Fail);
        let serialized = serde_json::to_string(&report).unwrap();
        assert!(serialized.contains("authentication failed"));
        assert!(!serialized.contains("secret provider response"));
        assert!(!serialized.contains("fal-test-secret"));
    }
}
