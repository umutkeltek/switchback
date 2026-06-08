//! A resolved account: config auth turned into concrete (redacting) secrets.

use std::path::PathBuf;

use sb_core::{AuthConfig, CredentialLease, Secret};

use crate::vault::Vault;

pub type AccountId = String;

/// The concrete credential material for one account. Secrets redact in Debug.
#[derive(Debug, Clone)]
pub enum ResolvedAuth {
    None,
    ApiKey(Secret),
    /// OAuth. `token` is the initial access token (optional). When a `refresh`
    /// token + `token_url` are present, `RefreshCoordinator` mints/refreshes the
    /// access token live (the static `token` is only the lease fallback).
    Oauth {
        token: Option<Secret>,
        refresh: Option<Secret>,
        refresh_vault: Option<String>,
        token_url: Option<String>,
        client_id: Option<String>,
        client_secret: Option<Secret>,
    },
    /// Native client OAuth access-token source. The token is read at lease time
    /// so Codex/Claude Code can keep their own stores fresh.
    NativeOauth(NativeOauthSource),
    /// GCP service account. The access token is minted from the key by
    /// `ServiceAccountMinter` via the resolver's `fresh_lease`.
    ServiceAccount {
        key: crate::service_account::ServiceAccountKey,
        scope: Option<String>,
    },
    AwsSigV4 {
        access_key_id: Secret,
        secret_access_key: Secret,
        session_token: Option<Secret>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeOauthKind {
    Codex,
    ClaudeCode,
}

impl NativeOauthKind {
    fn label(self) -> &'static str {
        match self {
            Self::Codex => "codex_oauth",
            Self::ClaudeCode => "claude_code_oauth",
        }
    }

    /// How the user re-mints this native token. Switchback never writes back to
    /// the native auth store (NATIVE_RELAY_SPEC), so an expired token is fixed by
    /// the native client refreshing its own store.
    fn refresh_hint(self) -> &'static str {
        match self {
            Self::Codex => "run `codex` (or `codex login`) to refresh ~/.codex/auth.json",
            Self::ClaudeCode => {
                "run `claude setup-token` or relaunch Claude Code to refresh the token"
            }
        }
    }
}

/// Best-effort: if `token` is a JWT whose `exp` claim is in the past, return that
/// expiry (unix seconds). Opaque / unparseable tokens return `None`, so a token
/// without a readable expiry simply proceeds and lets the upstream 401 surface.
/// Reads only the non-secret `exp` claim — the token value is never logged.
fn jwt_expiry_unix(token: &str) -> Option<i64> {
    use base64::Engine as _;
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    claims.get("exp").and_then(serde_json::Value::as_i64)
}

/// Current unix time in seconds (0 if the clock is before the epoch).
fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[derive(Debug, Clone)]
pub struct NativeOauthSource {
    pub kind: NativeOauthKind,
    pub token: Option<Secret>,
    pub token_env: Option<String>,
    pub token_file: Option<String>,
    pub access_token_pointer: String,
}

impl NativeOauthSource {
    pub fn access_token(&self) -> Result<Secret, String> {
        if let Some(token) = &self.token {
            if !token.is_empty() {
                return Ok(token.clone());
            }
        }
        if let Some(name) = self
            .token_env
            .as_deref()
            .filter(|name| !name.trim().is_empty())
        {
            if let Ok(value) = std::env::var(name) {
                if !value.trim().is_empty() {
                    return Ok(Secret::new(value));
                }
            }
        }
        if let Some(file) = self
            .token_file
            .as_deref()
            .filter(|file| !file.trim().is_empty())
        {
            return read_json_token(file, &self.access_token_pointer, self.kind);
        }
        Err(format!(
            "{}: no native OAuth access token source configured",
            self.kind.label()
        ))
    }

    pub fn lease(&self, account_id: &str) -> Result<CredentialLease, String> {
        let access_token = self.access_token()?;
        // Proactively reject an already-expired native token with an actionable
        // message, instead of leasing it and eating a cryptic upstream 401. The
        // token is re-read from the native store every lease, so a refresh by the
        // native client is picked up automatically on the next request.
        if let Some(exp) = jwt_expiry_unix(access_token.expose()) {
            if exp <= now_unix() {
                return Err(format!(
                    "{}: native OAuth access token expired (exp {exp}); {}",
                    self.kind.label(),
                    self.kind.refresh_hint()
                ));
            }
        }
        if self.kind == NativeOauthKind::Codex {
            if let Some(chatgpt_account_id) = self.codex_chatgpt_account_id()? {
                return Ok(CredentialLease::bearer_with_chatgpt_account(
                    account_id.to_string(),
                    access_token,
                    chatgpt_account_id,
                ));
            }
        }
        Ok(CredentialLease::bearer(
            account_id.to_string(),
            access_token,
        ))
    }

    fn codex_chatgpt_account_id(&self) -> Result<Option<Secret>, String> {
        if let Ok(value) = std::env::var("CODEX_ACCOUNT_ID") {
            if !value.trim().is_empty() {
                return Ok(Some(Secret::new(value)));
            }
        }
        let Some(file) = self
            .token_file
            .as_deref()
            .filter(|file| !file.trim().is_empty())
        else {
            return Ok(None);
        };
        match read_json_token(file, "/tokens/account_id", self.kind) {
            Ok(value) => Ok(Some(value)),
            Err(err) if err.contains("missing string at `/tokens/account_id`") => Ok(None),
            Err(err) => Err(err),
        }
    }
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
            // The live token comes from RefreshCoordinator via the resolver's
            // `fresh_lease`; this static token is only a fallback (empty when
            // refresh-only, which `apply_auth` skips until it's refreshed).
            ResolvedAuth::Oauth { token, .. } => CredentialLease::bearer(
                self.id.clone(),
                token.clone().unwrap_or_else(|| Secret::new("")),
            ),
            // Resolved by `CredentialResolver::fresh_lease` immediately before
            // execution, because native clients may refresh their files/envs
            // while Switchback is running.
            ResolvedAuth::NativeOauth(_) => {
                CredentialLease::bearer(self.id.clone(), Secret::new(""))
            }
            // Token is minted by ServiceAccountMinter in `fresh_lease`; this
            // empty placeholder is replaced before the request goes out.
            ResolvedAuth::ServiceAccount { .. } => {
                CredentialLease::bearer(self.id.clone(), Secret::new(""))
            }
            ResolvedAuth::AwsSigV4 {
                access_key_id,
                secret_access_key,
                session_token,
            } => CredentialLease::aws_sigv4(
                self.id.clone(),
                access_key_id.clone(),
                secret_access_key.clone(),
                session_token.clone(),
            ),
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
            token_vault,
            refresh_env,
            refresh,
            refresh_vault,
            token_url,
            client_id,
            client_secret_env,
            client_secret,
            client_secret_vault,
        } => Ok(ResolvedAuth::Oauth {
            token: resolve_optional_secret(
                token_vault.as_deref(),
                token_env.as_deref(),
                token.as_deref(),
                vault,
                "oauth access token",
            )?,
            refresh: resolve_optional_secret(
                refresh_vault.as_deref(),
                refresh_env.as_deref(),
                refresh.as_deref(),
                vault,
                "oauth refresh token",
            )?,
            refresh_vault: refresh_vault.clone(),
            token_url: token_url.clone(),
            client_id: client_id.clone(),
            client_secret: resolve_optional_secret(
                client_secret_vault.as_deref(),
                client_secret_env.as_deref(),
                client_secret.as_deref(),
                vault,
                "oauth client secret",
            )?,
        }),
        AuthConfig::CodexOauth {
            token_env,
            token_vault,
            token_file,
            access_token_pointer,
        } => Ok(ResolvedAuth::NativeOauth(NativeOauthSource {
            kind: NativeOauthKind::Codex,
            token: resolve_optional_secret(
                token_vault.as_deref(),
                None,
                None,
                vault,
                "codex oauth access token",
            )?,
            token_env: token_env.clone(),
            token_file: token_file.clone(),
            access_token_pointer: access_token_pointer.clone(),
        })),
        AuthConfig::ClaudeCodeOauth {
            token_env,
            token_vault,
            token_file,
            access_token_pointer,
        } => Ok(ResolvedAuth::NativeOauth(NativeOauthSource {
            kind: NativeOauthKind::ClaudeCode,
            token: resolve_optional_secret(
                token_vault.as_deref(),
                None,
                None,
                vault,
                "claude code oauth access token",
            )?,
            token_env: token_env.clone(),
            token_file: token_file.clone(),
            access_token_pointer: access_token_pointer.clone(),
        })),
        AuthConfig::ServiceAccount {
            key_file,
            key_env,
            scope,
        } => {
            let json = match (key_env.as_deref(), key_file.as_deref()) {
                (Some(name), _) => std::env::var(name)
                    .map_err(|_| format!("service_account: env `{name}` not set for key JSON"))?,
                (None, Some(path)) => std::fs::read_to_string(path)
                    .map_err(|e| format!("service_account: read key file `{path}`: {e}"))?,
                (None, None) => {
                    return Err("service_account: provide key_file or key_env".to_string())
                }
            };
            let key = crate::service_account::ServiceAccountKey::from_json(&json)?;
            Ok(ResolvedAuth::ServiceAccount {
                key,
                scope: scope.clone(),
            })
        }
        AuthConfig::AwsSigV4 {
            access_key_env,
            access_key,
            secret_key_env,
            secret_key,
            session_token_env,
            session_token,
        } => Ok(ResolvedAuth::AwsSigV4 {
            access_key_id: resolve_secret(
                None,
                Some(access_key_env.as_str()),
                access_key.as_deref(),
                vault,
                "aws_access_key_id",
            )?,
            secret_access_key: resolve_secret(
                None,
                Some(secret_key_env.as_str()),
                secret_key.as_deref(),
                vault,
                "aws_secret_access_key",
            )?,
            session_token: resolve_optional_secret(
                None,
                session_token_env.as_deref(),
                session_token.as_deref(),
                vault,
                "aws session token",
            )?,
        }),
    }
}

fn read_json_token(file: &str, pointer: &str, kind: NativeOauthKind) -> Result<Secret, String> {
    let path = expand_path(file)?;
    let body = std::fs::read_to_string(&path).map_err(|e| {
        format!(
            "{}: read native OAuth token file `{}`: {e}",
            kind.label(),
            path.display()
        )
    })?;
    let json: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| format!("{}: parse native OAuth token JSON: {e}", kind.label()))?;
    let token = json
        .pointer(pointer)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            format!(
                "{}: native OAuth token file `{}` missing string at `{pointer}`",
                kind.label(),
                path.display()
            )
        })?;
    Ok(Secret::new(token.to_string()))
}

fn expand_path(path: &str) -> Result<PathBuf, String> {
    let home = std::env::var("HOME").map_err(|_| "HOME is not set".to_string())?;
    let expanded = if path == "~" {
        home
    } else if let Some(rest) = path.strip_prefix("~/") {
        format!("{home}/{rest}")
    } else if let Some(rest) = path.strip_prefix("${HOME}") {
        format!("{home}{rest}")
    } else if let Some(rest) = path.strip_prefix("$HOME") {
        format!("{home}{rest}")
    } else {
        path.to_string()
    };
    Ok(PathBuf::from(expanded))
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
    vault_key: Option<&str>,
    env: Option<&str>,
    inline: Option<&str>,
    vault: Option<&Vault>,
    what: &str,
) -> Result<Option<Secret>, String> {
    let has_vault = vault_key.is_some_and(|name| !name.trim().is_empty());
    let has_env = env.is_some_and(|name| !name.trim().is_empty());
    let has_inline = inline.is_some_and(|value| !value.trim().is_empty());
    if has_vault || has_env || has_inline {
        resolve_secret(vault_key, env, inline, vault, what).map(Some)
    } else {
        Ok(None)
    }
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

    #[test]
    fn native_codex_oauth_reads_access_token_from_auth_json() {
        let path = temp_file("codex-native-auth");
        std::fs::write(
            &path,
            r#"{"tokens":{"access_token":"codex-file-token","account_id":"acct-file","refresh_token":"unused"}}"#,
        )
        .unwrap();
        let auth = AuthConfig::CodexOauth {
            token_env: None,
            token_vault: None,
            token_file: Some(path.to_string_lossy().into_owned()),
            access_token_pointer: "/tokens/access_token".to_string(),
        };

        match resolve_auth(&auth, None).unwrap() {
            ResolvedAuth::NativeOauth(source) => {
                assert_eq!(source.access_token().unwrap().expose(), "codex-file-token");
                let lease = source.lease("codex").unwrap();
                assert_eq!(lease.secret.expose(), "codex-file-token");
                assert_eq!(lease.chatgpt_account_id.unwrap().expose(), "acct-file");
            }
            other => panic!("expected native oauth, got {other:?}"),
        }
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn native_oauth_lease_rejects_expired_jwt_with_actionable_message() {
        use base64::Engine as _;
        let jwt = |exp: i64| {
            let enc = |v: &serde_json::Value| {
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(v).unwrap())
            };
            format!(
                "{}.{}.sig",
                enc(&serde_json::json!({ "alg": "RS256", "typ": "JWT" })),
                enc(&serde_json::json!({ "exp": exp })),
            )
        };
        let source = |token: String| NativeOauthSource {
            kind: NativeOauthKind::Codex,
            token: Some(Secret::new(token)),
            token_env: None,
            token_file: None,
            access_token_pointer: "/tokens/access_token".to_string(),
        };

        // Expired -> actionable Err, not a doomed lease.
        let err = source(jwt(now_unix() - 3600)).lease("codex").unwrap_err();
        assert!(err.contains("expired"), "message names the failure: {err}");
        assert!(err.contains("codex"), "message names the refresh path: {err}");

        // Still-valid JWT leases fine.
        assert!(source(jwt(now_unix() + 3600)).lease("codex").is_ok());

        // Opaque (non-JWT) token is not gated — upstream 401 would surface it.
        assert!(source("opaque-token".to_string()).lease("codex").is_ok());
    }

    #[test]
    fn native_claude_code_oauth_reads_portable_credentials_file() {
        let path = temp_file("claude-code-native-auth");
        std::fs::write(
            &path,
            r#"{"claudeAiOauth":{"accessToken":"claude-file-token","subscriptionType":"max"}}"#,
        )
        .unwrap();
        let auth = AuthConfig::ClaudeCodeOauth {
            token_env: None,
            token_vault: None,
            token_file: Some(path.to_string_lossy().into_owned()),
            access_token_pointer: "/claudeAiOauth/accessToken".to_string(),
        };

        match resolve_auth(&auth, None).unwrap() {
            ResolvedAuth::NativeOauth(source) => {
                assert_eq!(source.access_token().unwrap().expose(), "claude-file-token");
            }
            other => panic!("expected native oauth, got {other:?}"),
        }
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn account_debug_redacts_service_account_private_key() {
        let private_key = format!("redact-me-{}", "account-key");
        let acct = Account {
            id: "svc".into(),
            provider_id: "vertex".into(),
            auth: ResolvedAuth::ServiceAccount {
                key: crate::service_account::ServiceAccountKey {
                    client_email: "svc".into(),
                    private_key,
                    token_uri: "https://oauth2.example/token".into(),
                },
                scope: None,
            },
            priority: 0,
            policy_tags: vec![],
        };
        let debug = format!("{acct:?}");

        assert!(debug.contains("[redacted]"));
        assert!(!debug.contains("redact-me-account-key"));
    }

    #[test]
    fn aws_sigv4_auth_resolves_to_redacting_lease() {
        let auth = AuthConfig::AwsSigV4 {
            access_key_env: "SB_DEFINITELY_UNSET_AWS_ACCESS".into(),
            access_key: Some("AKIA-INLINE".into()),
            secret_key_env: "SB_DEFINITELY_UNSET_AWS_SECRET".into(),
            secret_key: Some("aws-secret".into()),
            session_token_env: None,
            session_token: Some("sts-token".into()),
        };

        let resolved = resolve_auth(&auth, None).unwrap();
        let acct = Account {
            id: "aws".into(),
            provider_id: "bedrock".into(),
            auth: resolved,
            priority: 0,
            policy_tags: vec![],
        };
        let lease = acct.lease();
        let aws = lease.aws_sigv4.as_ref().expect("sigv4 lease");

        assert_eq!(lease.auth_kind, sb_core::AuthKind::AwsSigV4);
        assert_eq!(aws.access_key_id.expose(), "AKIA-INLINE");
        assert_eq!(aws.secret_access_key.expose(), "aws-secret");
        assert_eq!(aws.session_token.as_ref().unwrap().expose(), "sts-token");

        let debug = format!("{lease:?}");
        assert!(!debug.contains("AKIA-INLINE"));
        assert!(!debug.contains("aws-secret"));
        assert!(!debug.contains("sts-token"));
    }

    fn temp_file(tag: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("sb-{tag}-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }
}
