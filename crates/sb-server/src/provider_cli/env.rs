use sb_core::{AuthConfig, ProviderConfig, ProviderKind};

pub(super) fn env_missing(name: &str) -> bool {
    std::env::var(name)
        .map(|value| value.trim().is_empty())
        .unwrap_or(true)
}

pub(super) fn non_empty(value: Option<&String>) -> bool {
    value.is_some_and(|value| !value.trim().is_empty())
}

pub(super) fn auth_missing_envs(auth: &AuthConfig) -> Vec<String> {
    match auth {
        AuthConfig::None => Vec::new(),
        AuthConfig::ApiKey { env, inline, vault } => {
            if non_empty(vault.as_ref()) || non_empty(inline.as_ref()) {
                Vec::new()
            } else {
                env.iter()
                    .filter(|name| env_missing(name))
                    .cloned()
                    .collect()
            }
        }
        AuthConfig::Oauth {
            token_env,
            token,
            token_vault,
            refresh_env,
            refresh,
            refresh_vault,
            client_secret_env,
            client_secret,
            client_secret_vault,
            ..
        } => {
            let mut missing = Vec::new();
            if !non_empty(token.as_ref()) && !non_empty(token_vault.as_ref()) {
                if let Some(name) = token_env {
                    if env_missing(name) {
                        missing.push(name.clone());
                    }
                }
            }
            if !non_empty(refresh.as_ref()) && !non_empty(refresh_vault.as_ref()) {
                if let Some(name) = refresh_env {
                    if env_missing(name) {
                        missing.push(name.clone());
                    }
                }
            }
            if !non_empty(client_secret.as_ref()) && !non_empty(client_secret_vault.as_ref()) {
                if let Some(name) = client_secret_env {
                    if env_missing(name) {
                        missing.push(name.clone());
                    }
                }
            }
            missing
        }
        AuthConfig::CodexOauth {
            token_env,
            token_vault,
            token_file,
            ..
        }
        | AuthConfig::ClaudeCodeOauth {
            token_env,
            token_vault,
            token_file,
            ..
        } => {
            if non_empty(token_vault.as_ref()) || non_empty(token_file.as_ref()) {
                Vec::new()
            } else {
                token_env
                    .iter()
                    .filter(|name| env_missing(name))
                    .cloned()
                    .collect()
            }
        }
        AuthConfig::ServiceAccount {
            key_file, key_env, ..
        } => {
            if non_empty(key_file.as_ref()) {
                Vec::new()
            } else {
                key_env
                    .iter()
                    .filter(|name| env_missing(name))
                    .cloned()
                    .collect()
            }
        }
        AuthConfig::AwsSigV4 {
            access_key_env,
            access_key,
            secret_key_env,
            secret_key,
            session_token_env,
            ..
        } => {
            let mut missing = Vec::new();
            if !non_empty(access_key.as_ref()) && env_missing(access_key_env) {
                missing.push(access_key_env.clone());
            }
            if !non_empty(secret_key.as_ref()) && env_missing(secret_key_env) {
                missing.push(secret_key_env.clone());
            }
            if let Some(name) = session_token_env {
                if env_missing(name) {
                    missing.push(name.clone());
                }
            }
            missing
        }
    }
}

pub(crate) fn provider_missing_envs(provider: &ProviderConfig) -> Vec<String> {
    let mut missing = Vec::new();
    if provider.accounts.is_empty() {
        match &provider.kind {
            ProviderKind::OpenaiCompatible {
                api_key_env,
                api_key,
                ..
            }
            | ProviderKind::Anthropic {
                api_key_env,
                api_key,
                ..
            }
            | ProviderKind::Gemini {
                api_key_env,
                api_key,
                ..
            }
            | ProviderKind::Vertex {
                api_key_env,
                api_key,
                ..
            } => {
                if !non_empty(api_key.as_ref()) {
                    missing.extend(api_key_env.iter().filter(|name| env_missing(name)).cloned());
                }
            }
            ProviderKind::Bedrock {
                access_key_env,
                secret_key_env,
                ..
            } => {
                if env_missing(access_key_env) {
                    missing.push(access_key_env.clone());
                }
                if env_missing(secret_key_env) {
                    missing.push(secret_key_env.clone());
                }
            }
            ProviderKind::CodexNativeRelay { .. } => {
                if env_missing("CODEX_ACCESS_TOKEN") {
                    missing.push("CODEX_ACCESS_TOKEN".to_string());
                }
            }
            ProviderKind::ClaudeCodeNativeRelay { .. } => {
                if env_missing("CLAUDE_CODE_OAUTH_TOKEN") {
                    missing.push("CLAUDE_CODE_OAUTH_TOKEN".to_string());
                }
            }
            ProviderKind::Mock => {}
        }
    } else {
        for account in &provider.accounts {
            missing.extend(auth_missing_envs(&account.auth));
        }
    }
    missing.sort();
    missing.dedup();
    missing
}

pub(super) fn auth_env_names(auth: &AuthConfig) -> Vec<String> {
    match auth {
        AuthConfig::None => Vec::new(),
        AuthConfig::ApiKey { env, .. } => env.iter().cloned().collect(),
        AuthConfig::Oauth {
            token_env,
            refresh_env,
            client_secret_env,
            ..
        } => [token_env, refresh_env, client_secret_env]
            .into_iter()
            .filter_map(|value| value.clone())
            .collect(),
        AuthConfig::CodexOauth { token_env, .. }
        | AuthConfig::ClaudeCodeOauth { token_env, .. } => token_env.iter().cloned().collect(),
        AuthConfig::ServiceAccount { key_env, .. } => key_env.iter().cloned().collect(),
        AuthConfig::AwsSigV4 {
            access_key_env,
            secret_key_env,
            session_token_env,
            ..
        } => [
            Some(access_key_env),
            Some(secret_key_env),
            session_token_env.as_ref(),
        ]
        .into_iter()
        .filter_map(|value| value.cloned())
        .collect(),
    }
}

pub(crate) fn provider_auth_env_names(provider: &ProviderConfig) -> Vec<String> {
    let mut names = Vec::new();
    if provider.accounts.is_empty() {
        match &provider.kind {
            ProviderKind::OpenaiCompatible { api_key_env, .. }
            | ProviderKind::Anthropic { api_key_env, .. }
            | ProviderKind::Gemini { api_key_env, .. }
            | ProviderKind::Vertex { api_key_env, .. } => {
                names.extend(api_key_env.iter().cloned());
            }
            ProviderKind::Bedrock {
                access_key_env,
                secret_key_env,
                ..
            } => {
                names.push(access_key_env.clone());
                names.push(secret_key_env.clone());
            }
            ProviderKind::CodexNativeRelay { .. } => {
                names.push("CODEX_ACCESS_TOKEN".to_string());
            }
            ProviderKind::ClaudeCodeNativeRelay { .. } => {
                names.push("CLAUDE_CODE_OAUTH_TOKEN".to_string());
            }
            ProviderKind::Mock => {}
        }
    } else {
        for account in &provider.accounts {
            names.extend(auth_env_names(&account.auth));
        }
    }
    names.sort();
    names.dedup();
    names
}
