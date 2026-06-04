//! Credentials as redacting leases. A `Secret` never reveals itself in
//! `Debug`/`Display`, so it cannot leak into logs accidentally.

use serde::{Deserialize, Serialize};
use std::fmt;

/// A secret string that redacts itself everywhere except explicit `.expose()`.
/// Deliberately NOT `Serialize`/`Deserialize`: a secret can never be emitted to
/// an HTTP response or parsed from untrusted JSON. The vault persists raw strings
/// (encrypted at rest); resolved leases/auth are runtime-only and never serde'd.
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    pub fn new(s: impl Into<String>) -> Self {
        Secret(s.into())
    }
    /// Explicit, auditable access to the raw value. Call only at the HTTP
    /// boundary where the header is actually built — never to log it.
    pub fn expose(&self) -> &str {
        &self.0
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Secret(***)")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "***")
    }
}

impl From<String> for Secret {
    fn from(s: String) -> Self {
        Secret(s)
    }
}
impl From<&str> for Secret {
    fn from(s: &str) -> Self {
        Secret(s.to_string())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuthKind {
    None,
    ApiKey,
    Bearer,
    AwsSigV4,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OwnerScope {
    Personal,
    Team,
    Service,
}

/// A routable credential identity — NOT just an API key. (Seam for the
/// future credential vault / multi-account model; v1 keeps it minimal.)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderAccount {
    pub provider_id: String,
    pub account_id: String,
    pub owner_scope: OwnerScope,
    pub auth_kind: AuthKind,
    #[serde(default)]
    pub policy_tags: Vec<String>,
}

#[derive(Clone)]
pub struct AwsSigV4Lease {
    pub access_key_id: Secret,
    pub secret_access_key: Secret,
    pub session_token: Option<Secret>,
}

impl fmt::Debug for AwsSigV4Lease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AwsSigV4Lease")
            .field("access_key_id", &"[redacted]")
            .field("secret_access_key", &"[redacted]")
            .field(
                "session_token",
                &self.session_token.as_ref().map(|_| "[redacted]"),
            )
            .finish()
    }
}

/// Short-lived credential handed to an adapter at execution time. The
/// secret redacts in `Debug`, so a lease can be logged safely by accident.
#[derive(Debug, Clone)]
pub struct CredentialLease {
    pub provider_account_id: String,
    pub auth_kind: AuthKind,
    pub secret: Secret,
    pub aws_sigv4: Option<AwsSigV4Lease>,
    pub chatgpt_account_id: Option<Secret>,
}

impl CredentialLease {
    pub fn bearer(account: impl Into<String>, key: impl Into<Secret>) -> Self {
        CredentialLease {
            provider_account_id: account.into(),
            auth_kind: AuthKind::Bearer,
            secret: key.into(),
            aws_sigv4: None,
            chatgpt_account_id: None,
        }
    }
    pub fn bearer_with_chatgpt_account(
        account: impl Into<String>,
        key: impl Into<Secret>,
        chatgpt_account_id: impl Into<Secret>,
    ) -> Self {
        CredentialLease {
            provider_account_id: account.into(),
            auth_kind: AuthKind::Bearer,
            secret: key.into(),
            aws_sigv4: None,
            chatgpt_account_id: Some(chatgpt_account_id.into()),
        }
    }
    pub fn none(account: impl Into<String>) -> Self {
        CredentialLease {
            provider_account_id: account.into(),
            auth_kind: AuthKind::None,
            secret: Secret::new(""),
            aws_sigv4: None,
            chatgpt_account_id: None,
        }
    }
    pub fn aws_sigv4(
        account: impl Into<String>,
        access_key_id: impl Into<Secret>,
        secret_access_key: impl Into<Secret>,
        session_token: Option<Secret>,
    ) -> Self {
        CredentialLease {
            provider_account_id: account.into(),
            auth_kind: AuthKind::AwsSigV4,
            secret: Secret::new(""),
            aws_sigv4: Some(AwsSigV4Lease {
                access_key_id: access_key_id.into(),
                secret_access_key: secret_access_key.into(),
                session_token,
            }),
            chatgpt_account_id: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_redacts_in_debug_and_display() {
        let s = Secret::new("sk-supersecret-value");
        assert_eq!(format!("{s:?}"), "Secret(***)");
        assert_eq!(format!("{s}"), "***");
        assert!(!format!("{s:?}").contains("supersecret"));
        assert_eq!(s.expose(), "sk-supersecret-value");
    }

    #[test]
    fn credential_lease_debug_never_leaks_key() {
        let lease = CredentialLease::bearer("acct-1", "sk-do-not-leak");
        let dbg = format!("{lease:?}");
        assert!(
            !dbg.contains("do-not-leak"),
            "lease Debug leaked the key: {dbg}"
        );
        assert!(dbg.contains("***"));
    }

    #[test]
    fn credential_lease_debug_never_leaks_chatgpt_account_id() {
        let lease = CredentialLease::bearer_with_chatgpt_account(
            "acct-1",
            "access-token",
            "chatgpt-account-id",
        );
        let dbg = format!("{lease:?}");
        assert!(!dbg.contains("access-token"));
        assert!(!dbg.contains("chatgpt-account-id"));
        assert!(dbg.contains("***"));
    }

    #[test]
    fn aws_sigv4_lease_debug_never_leaks_keys() {
        let lease = CredentialLease::aws_sigv4(
            "aws",
            "AKIA-DO-NOT-LEAK",
            "secret-access-key",
            Some(Secret::new("session-token")),
        );
        let dbg = format!("{lease:?}");

        assert!(!dbg.contains("AKIA-DO-NOT-LEAK"));
        assert!(!dbg.contains("secret-access-key"));
        assert!(!dbg.contains("session-token"));
        assert!(dbg.contains("[redacted]"));
    }
}
