//! Concrete adapters.
//! - `mock`: deterministic, credential-free; the end-to-end test harness.
//! - `codec` + `composed`: the generic `ComposedAdapter` (WireCodec × AuthScheme)
//!   that serves the OpenAI-compatible / Anthropic / Gemini wire formats from one
//!   execute loop. Adding a wire format is a `WireCodec` impl, not a new adapter.
//! - `registry`: maps configured providers to adapter instances and leases.

pub mod codec;
pub mod composed;
pub mod egress;
pub mod latency;
pub mod mock;
pub mod registry;

pub use codec::{AnthropicCodec, GeminiCodec, OpenAiCodec, StreamDecoder, VertexCodec, WireCodec};
pub use composed::ComposedAdapter;
pub use egress::EgressPool;
pub use latency::LatencyTracker;
pub use mock::MockAdapter;
pub use registry::AdapterRegistry;

use sb_core::{AuthKind, AuthScheme, CredentialLease};

/// Attach a lease's secret to an outbound request per the provider's
/// [`AuthScheme`]. The single place auth is applied — every adapter composes a
/// scheme (bearer / header / query) rather than hardcoding how the key rides on
/// the wire. This is the "auth" half of the `AuthScheme × WireCodec`
/// decomposition: a provider that differs only in auth is now data, not code.
pub fn apply_auth(
    builder: reqwest::RequestBuilder,
    scheme: &AuthScheme,
    lease: Option<&CredentialLease>,
) -> reqwest::RequestBuilder {
    let Some(lease) = lease else {
        return builder;
    };
    if lease.auth_kind == AuthKind::None || lease.secret.is_empty() {
        return builder;
    }
    let secret = lease.secret.expose();
    match scheme {
        AuthScheme::None => builder,
        AuthScheme::Bearer => builder.bearer_auth(secret),
        AuthScheme::Header { name } => builder.header(name.as_str(), secret),
        AuthScheme::Query { name } => builder.query(&[(name.as_str(), secret)]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sb_core::CredentialLease;

    fn built(scheme: &AuthScheme, lease: Option<&CredentialLease>) -> reqwest::Request {
        let client = reqwest::Client::new();
        apply_auth(client.get("http://example.test/"), scheme, lease)
            .build()
            .unwrap()
    }

    #[test]
    fn auth_scheme_attaches_the_secret_where_declared() {
        let lease = CredentialLease::bearer("acct", "sk-123");

        let bearer = built(&AuthScheme::Bearer, Some(&lease));
        assert_eq!(bearer.headers()["authorization"], "Bearer sk-123");

        let header = built(
            &AuthScheme::Header {
                name: "x-api-key".into(),
            },
            Some(&lease),
        );
        assert_eq!(header.headers()["x-api-key"], "sk-123");
        assert!(header.headers().get("authorization").is_none());

        let query = built(&AuthScheme::Query { name: "key".into() }, Some(&lease));
        assert!(query.url().as_str().contains("key=sk-123"));

        // No lease, or a None scheme -> nothing attached.
        let none = built(&AuthScheme::Bearer, None);
        assert!(none.headers().get("authorization").is_none());
        let scheme_none = built(&AuthScheme::None, Some(&lease));
        assert!(scheme_none.headers().get("authorization").is_none());
    }
}
