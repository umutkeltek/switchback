use sb_core::{AiRequest, Config, ErrorClass};

/// The outbound egress for an attempt: account override → provider default →
/// `server.default_egress`. `None` means the default (direct) path. The pool
/// turns an unknown/disabled id back into direct.
pub(crate) fn resolve_egress(
    config: &Config,
    provider_id: &str,
    account_id: &str,
) -> Option<String> {
    if let Some(provider) = config.providers.iter().find(|p| p.id == provider_id) {
        if let Some(account) = provider.accounts.iter().find(|a| a.id == account_id) {
            if account.egress.is_some() {
                return account.egress.clone();
            }
        }
        if provider.egress.is_some() {
            return provider.egress.clone();
        }
    }
    config.server.default_egress.clone()
}

/// Transient errors an immediate same-account retry might fix. Rate-limit /
/// overload / auth deliberately fall over to a different account instead.
pub(crate) fn retryable(class: ErrorClass) -> bool {
    matches!(
        class,
        ErrorClass::Timeout | ErrorClass::Network | ErrorClass::ServerError
    )
}

/// Capped exponential backoff for retry attempt `n` (1-based). Deterministic.
pub(crate) fn retry_backoff(retry: &sb_core::RetryConfig, attempt: u32) -> std::time::Duration {
    let factor = 2u64.saturating_pow(attempt.saturating_sub(1));
    let ms = retry
        .base_delay_ms
        .saturating_mul(factor)
        .min(retry.max_delay_ms);
    std::time::Duration::from_millis(ms)
}

pub(crate) fn session_affinity_key(req: &AiRequest) -> Option<&str> {
    for key in ["session_id", "switchback_session_id", "codex_session_id"] {
        if let Some(value) = req.metadata.get(key).filter(|v| !v.is_empty()) {
            return Some(value.as_str());
        }
    }
    let metadata = req
        .passthrough
        .get("metadata")
        .and_then(|v| v.as_object())?;
    for key in ["session_id", "switchback_session_id", "codex_session_id"] {
        if let Some(value) = metadata.get(key).and_then(|v| v.as_str()) {
            if !value.is_empty() {
                return Some(value);
            }
        }
    }
    None
}
