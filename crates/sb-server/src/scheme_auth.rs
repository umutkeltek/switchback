//! A provider's simple credential (an [`AuthScheme`] and a lease) on a plain
//! HTTP call that does not go through a text adapter: ComfyUI jobs, a
//! managed lane's health probe, the provider doctor. The lease comes from
//! the [`CredentialResolver`] and the wire form from the adapters'
//! [`SchemeSigner`], so a ComfyUI behind a token proxy is configured exactly
//! like an `openai_compatible` provider (`api_key_env` + `auth_scheme`).
//!
//! The secret stays a lease: it is never logged, never put in an error
//! text (errors drop the URL, which a `query` scheme would carry), and the
//! `Debug` form shows only the scheme.

use std::collections::HashSet;

use sb_adapters::{RequestSigner, SchemeSigner};
use sb_core::{AuthKind, AuthScheme, CredentialLease, ProviderConfig, ProviderKind};
use sb_credentials::{CredentialResolver, ResolveOutcome};

/// One provider credential, ready to attach.
#[derive(Clone)]
pub struct SchemeCredential {
    scheme: AuthScheme,
    lease: CredentialLease,
}

impl std::fmt::Debug for SchemeCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SchemeCredential")
            .field("scheme", &self.scheme)
            .field("lease", &"<redacted>")
            .finish()
    }
}

/// How a provider's credential goes on a plain call: `Some` only for a
/// provider kind whose plain calls carry one (today `comfyui`; the text
/// kinds go through their adapter). Bearer unless `auth_scheme` says
/// otherwise, as for `openai_compatible`.
pub(crate) fn plain_call_scheme(provider: &ProviderConfig) -> Option<AuthScheme> {
    match &provider.kind {
        ProviderKind::ComfyUi { auth_scheme, .. } => {
            Some(auth_scheme.clone().unwrap_or(AuthScheme::Bearer))
        }
        _ => None,
    }
}

impl SchemeCredential {
    /// Resolve `provider`'s credential for a plain call. `Ok(None)` when the
    /// provider carries none (no key configured, or a kind without plain
    /// calls), so the call goes out without one, as before.
    pub(crate) async fn resolve(
        resolver: &CredentialResolver,
        provider: &ProviderConfig,
        model: &str,
    ) -> Result<Option<Self>, String> {
        let Some(scheme) = plain_call_scheme(provider) else {
            return Ok(None);
        };
        let (account_id, lease) = match resolver.resolve(&provider.id, model, &HashSet::new()) {
            ResolveOutcome::Selected { account_id, lease } => (account_id, lease),
            ResolveOutcome::NoAccounts => return Ok(None),
            ResolveOutcome::AllUnavailable { .. } => {
                return Err(format!(
                    "provider `{}` has no available credential account",
                    provider.id
                ))
            }
        };
        let lease = resolver
            .fresh_lease(&provider.id, &account_id, lease)
            .await
            .map_err(|error| {
                format!(
                    "resolve the credential of provider `{}`: {error}",
                    provider.id
                )
            })?;
        Ok(Self::from_lease(scheme, lease))
    }

    /// `None` for a lease that carries no secret.
    pub(crate) fn from_lease(scheme: AuthScheme, lease: CredentialLease) -> Option<Self> {
        (lease.auth_kind != AuthKind::None && !lease.secret.is_empty())
            .then_some(Self { scheme, lease })
    }

    /// Attach the credential to one request (header or query, per scheme).
    pub(crate) fn apply(&self, mut request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let target = sb_adapters::signer::SignTarget {
            method: "",
            host: "",
            path: "",
            query: "",
            body: &[],
        };
        let added = SchemeSigner(self.scheme.clone()).sign(&target, Some(&self.lease));
        for (name, value) in added.headers {
            match reqwest::header::HeaderValue::from_str(&value) {
                Ok(mut value) => {
                    value.set_sensitive(true);
                    request = request.header(name, value);
                }
                Err(_) => tracing::warn!(
                    header = %name,
                    "credential is not a valid header value; sent without it"
                ),
            }
        }
        if !added.query.is_empty() {
            request = request.query(&added.query);
        }
        request
    }
}

/// The error text of a failed call. With a credential attached it never
/// quotes the URL (a `query` scheme puts the secret there); without one it
/// is the error as before.
pub(crate) fn error_text(error: reqwest::Error, credential: Option<&SchemeCredential>) -> String {
    if credential.is_some() {
        error.without_url().to_string()
    } else {
        error.to_string()
    }
}

/// The redirect rule of a client for calls that carry `credential`: none
/// followed, as for the text adapters' calls. reqwest drops only
/// `Authorization` (and cookies) on a redirect to another origin, so a
/// `header` scheme's credential would follow it there. Without a
/// credential, the default as before.
pub(crate) fn redirect_policy(credential: Option<&SchemeCredential>) -> reqwest::redirect::Policy {
    match credential {
        Some(_) => reqwest::redirect::Policy::none(),
        None => reqwest::redirect::Policy::default(),
    }
}

/// Attach `credential`, when there is one.
pub(crate) fn with_credential(
    request: reqwest::RequestBuilder,
    credential: Option<&SchemeCredential>,
) -> reqwest::RequestBuilder {
    match credential {
        Some(credential) => credential.apply(request),
        None => request,
    }
}

/// The status of an answer that refused the credential (401 or 403).
pub(crate) fn refused(status: reqwest::StatusCode) -> bool {
    matches!(
        status,
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "s3cret-proxy-value";

    fn credential(scheme: AuthScheme) -> SchemeCredential {
        SchemeCredential::from_lease(scheme, CredentialLease::bearer("default", SECRET))
            .expect("a lease with a secret")
    }

    #[test]
    fn debug_shows_the_scheme_and_never_the_secret() {
        let shown = format!(
            "{:?}",
            credential(AuthScheme::Header {
                name: "x-comfy-key".into()
            })
        );
        assert!(shown.contains("x-comfy-key"), "{shown}");
        assert!(!shown.contains(SECRET), "{shown}");
    }

    #[test]
    fn a_lease_without_a_secret_is_no_credential() {
        assert!(SchemeCredential::from_lease(
            AuthScheme::Bearer,
            CredentialLease::bearer("default", "")
        )
        .is_none());
    }

    // With a query scheme the URL carries the secret: an error text must
    // not quote it.
    #[tokio::test]
    async fn an_error_never_quotes_a_url_that_carries_the_secret() {
        let credential = credential(AuthScheme::Query {
            name: "token".into(),
        });
        let error = credential
            .apply(reqwest::Client::new().get("http://127.0.0.1:1/system_stats"))
            .send()
            .await
            .expect_err("nothing listens on port 1");
        assert!(
            error.to_string().contains(SECRET),
            "the raw error does quote it"
        );
        let text = error_text(error, Some(&credential));
        assert!(!text.contains(SECRET), "{text}");
    }
}
