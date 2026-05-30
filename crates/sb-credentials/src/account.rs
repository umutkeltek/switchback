//! A resolved account: config auth turned into concrete (redacting) secrets.

use sb_core::{AuthConfig, CredentialLease, Secret};

pub type AccountId = String;

/// The concrete credential material for one account. Secrets redact in Debug.
#[derive(Debug, Clone)]
pub enum ResolvedAuth {
    None,
    ApiKey(Secret),
    /// Static OAuth bearer in v1. `refresh` is carried for the future live
    /// refresh seam (see `RefreshCoordinator`) but not exercised yet.
    Oauth {
        token: Secret,
        refresh: Option<Secret>,
    },
}

#[derive(Debug, Clone)]
pub struct Account {
    pub id: AccountId,
    pub provider_id: String,
    pub auth: ResolvedAuth,
    pub priority: i32,
    pub policy_tags: Vec<String>,
}

impl Account {
    /// Issue a short-lived lease for this account's current credential.
    pub fn lease(&self) -> CredentialLease {
        match &self.auth {
            ResolvedAuth::None => CredentialLease::none(self.id.clone()),
            ResolvedAuth::ApiKey(secret) => CredentialLease::bearer(self.id.clone(), secret.clone()),
            ResolvedAuth::Oauth { token, .. } => {
                CredentialLease::bearer(self.id.clone(), token.clone())
            }
        }
    }
}

/// Turn a config `AuthConfig` (env names / inline values) into concrete
/// secrets at startup. Env is preferred; inline is a discouraged fallback.
pub fn resolve_auth(auth: &AuthConfig) -> Result<ResolvedAuth, String> {
    match auth {
        AuthConfig::None => Ok(ResolvedAuth::None),
        AuthConfig::ApiKey { env, inline } => {
            Ok(ResolvedAuth::ApiKey(resolve_secret(env.as_deref(), inline.as_deref(), "api_key")?))
        }
        AuthConfig::Oauth {
            token_env,
            token,
            refresh_env,
            refresh,
        } => Ok(ResolvedAuth::Oauth {
            token: resolve_secret(token_env.as_deref(), token.as_deref(), "oauth token")?,
            refresh: resolve_optional_secret(refresh_env.as_deref(), refresh.as_deref()),
        }),
    }
}

fn resolve_secret(env: Option<&str>, inline: Option<&str>, what: &str) -> Result<Secret, String> {
    if let Some(name) = env {
        match std::env::var(name) {
            Ok(value) if !value.is_empty() => return Ok(Secret::new(value)),
            _ => {
                // Fall through to inline if env is missing/empty.
            }
        }
    }
    if let Some(value) = inline.filter(|v| !v.is_empty()) {
        return Ok(Secret::new(value));
    }
    Err(format!(
        "no {what}: set env {:?} (or inline) — got neither",
        env.unwrap_or("<none>")
    ))
}

fn resolve_optional_secret(env: Option<&str>, inline: Option<&str>) -> Option<Secret> {
    resolve_secret(env, inline, "optional").ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_redacts_and_carries_token() {
        let acct = Account {
            id: "a1".into(),
            provider_id: "p".into(),
            auth: ResolvedAuth::ApiKey(Secret::new("sk-secret-xyz")),
            priority: 0,
            policy_tags: vec![],
        };
        let lease = acct.lease();
        assert_eq!(lease.secret.expose(), "sk-secret-xyz");
        assert!(!format!("{lease:?}").contains("secret-xyz")); // redacted in Debug
    }

    #[test]
    fn inline_api_key_resolves_when_env_absent() {
        let auth = AuthConfig::ApiKey {
            env: Some("SB_DEFINITELY_UNSET_ENV_VAR".into()),
            inline: Some("inline-key".into()),
        };
        match resolve_auth(&auth).unwrap() {
            ResolvedAuth::ApiKey(s) => assert_eq!(s.expose(), "inline-key"),
            _ => panic!("expected api key"),
        }
    }

    #[test]
    fn missing_credential_is_an_error() {
        let auth = AuthConfig::ApiKey {
            env: Some("SB_DEFINITELY_UNSET_ENV_VAR".into()),
            inline: None,
        };
        assert!(resolve_auth(&auth).is_err());
    }
}
