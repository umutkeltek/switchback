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

/// Short-lived credential handed to an adapter at execution time. The
/// secret redacts in `Debug`, so a lease can be logged safely by accident.
#[derive(Debug, Clone)]
pub struct CredentialLease {
    pub provider_account_id: String,
    pub auth_kind: AuthKind,
    pub secret: Secret,
}

impl CredentialLease {
    pub fn bearer(account: impl Into<String>, key: impl Into<Secret>) -> Self {
        CredentialLease {
            provider_account_id: account.into(),
            auth_kind: AuthKind::Bearer,
            secret: key.into(),
        }
    }
    pub fn none(account: impl Into<String>) -> Self {
        CredentialLease {
            provider_account_id: account.into(),
            auth_kind: AuthKind::None,
            secret: Secret::new(""),
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
}
