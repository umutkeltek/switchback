//! A resolved account: config auth turned into concrete (redacting) secrets.

use sb_core::{AuthConfig, CredentialLease, Secret};

use crate::vault::Vault;

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
            ResolvedAuth::ApiKey(secret) => {
                CredentialLease::bearer(self.id.clone(), secret.clone())
            }
            ResolvedAuth::Oauth { token, .. } => {
                CredentialLease::bearer(self.id.clone(), token.clone())
            }
        }
    }
}

/// Turn a config `AuthConfig` (vault names / env names / inline values) into
/// concrete secrets at startup. Precedence: vault > env > inline.
pub fn resolve_auth(auth: &AuthConfig, vault: Option<&Vault>) -> Result<ResolvedAuth, String> {
    match auth {
        AuthConfig::None => Ok(ResolvedAuth::None),
        AuthConfig::ApiKey {
            env,
            inline,
            vault: vault_key,
        } => Ok(ResolvedAuth::ApiKey(resolve_secret(
            vault_key.as_deref(),
            env.as_deref(),
            inline.as_deref(),
            vault,
            "api_key",
        )?)),
        AuthConfig::Oauth {
            token_env,
            token,
            refresh_env,
            refresh,
        } => Ok(ResolvedAuth::Oauth {
            token: resolve_secret(
                None,
                token_env.as_deref(),
                token.as_deref(),
                vault,
                "oauth token",
            )?,
            refresh: resolve_optional_secret(refresh_env.as_deref(), refresh.as_deref(), vault),
        }),
    }
}

/// Resolve one secret. Precedence: vault (most secure) > env > inline. A vault
/// name that's set MUST resolve (no silent fall-through to a weaker source).
fn resolve_secret(
    vault_key: Option<&str>,
    env: Option<&str>,
    inline: Option<&str>,
    vault: Option<&Vault>,
    what: &str,
) -> Result<Secret, String> {
    if let Some(name) = vault_key {
        let vault = vault.ok_or_else(|| {
            format!("{what}: vault secret `{name}` referenced but no vault is configured")
        })?;
        return vault
            .get(name)
            .ok_or_else(|| format!("{what}: secret `{name}` not found in vault"));
    }
    if let Some(name) = env {
        if let Ok(value) = std::env::var(name) {
            if !value.is_empty() {
                return Ok(Secret::new(value));
            }
        }
    }
    if let Some(value) = inline.filter(|v| !v.is_empty()) {
        return Ok(Secret::new(value));
    }
    Err(format!(
        "no {what}: set vault, env {:?}, or inline — got none",
        env.unwrap_or("<none>")
    ))
}

fn resolve_optional_secret(
    env: Option<&str>,
    inline: Option<&str>,
    vault: Option<&Vault>,
) -> Option<Secret> {
    resolve_secret(None, env, inline, vault, "optional").ok()
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
            vault: None,
        };
        match resolve_auth(&auth, None).unwrap() {
            ResolvedAuth::ApiKey(s) => assert_eq!(s.expose(), "inline-key"),
            _ => panic!("expected api key"),
        }
    }

    #[test]
    fn missing_credential_is_an_error() {
        let auth = AuthConfig::ApiKey {
            env: Some("SB_DEFINITELY_UNSET_ENV_VAR".into()),
            inline: None,
            vault: None,
        };
        assert!(resolve_auth(&auth, None).is_err());
    }

    #[test]
    fn vault_reference_without_vault_is_an_error() {
        let auth = AuthConfig::ApiKey {
            env: None,
            inline: None,
            vault: Some("some-secret".into()),
        };
        // A vault name is set but no vault configured -> hard error, never a
        // silent fall-through to a weaker (or absent) source.
        assert!(resolve_auth(&auth, None).is_err());
    }
}
