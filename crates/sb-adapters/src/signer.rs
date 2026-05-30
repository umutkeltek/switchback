//! The **Signer** seam of the `Codec × Signer × Transport` decomposition.
//!
//! A [`RequestSigner`] turns credentials into request mutations. The simple path
//! ([`SchemeSigner`]) attaches a bearer/header/query secret from the lease — the
//! old `apply_auth`. The hard path ([`SigV4Signer`]) signs OVER the built request
//! (method + host + path + body), which is why it needs [`SignTarget`] and could
//! never fit the simple bearer/header model. Both produce [`SignedAdditions`],
//! which the one execute loop applies — so adding a request-signing provider no
//! longer means a bespoke adapter.

use sb_core::{AuthKind, AuthScheme, CredentialLease};

/// The built request a signer may need to see. Borrowed; the signer reads, the
/// execute loop owns the request.
pub struct SignTarget<'a> {
    pub method: &'a str,
    pub host: &'a str,
    pub path: &'a str,
    pub query: &'a str,
    pub body: &'a [u8],
}

/// What a signer adds to the outbound request: headers and/or query params.
#[derive(Default)]
pub struct SignedAdditions {
    pub headers: Vec<(String, String)>,
    pub query: Vec<(String, String)>,
}

/// Produces the auth mutations for one outbound request.
pub trait RequestSigner: Send + Sync {
    fn sign(&self, target: &SignTarget, lease: Option<&CredentialLease>) -> SignedAdditions;
}

/// The simple path: attach a lease secret per an [`AuthScheme`] (bearer / header
/// / query). Equivalent to the old `apply_auth`; ignores the request body.
pub struct SchemeSigner(pub AuthScheme);

impl RequestSigner for SchemeSigner {
    fn sign(&self, _target: &SignTarget, lease: Option<&CredentialLease>) -> SignedAdditions {
        let mut add = SignedAdditions::default();
        let Some(lease) = lease else {
            return add;
        };
        if lease.auth_kind == AuthKind::None || lease.secret.is_empty() {
            return add;
        }
        let secret = lease.secret.expose();
        match &self.0 {
            AuthScheme::None => {}
            AuthScheme::Bearer => add
                .headers
                .push(("authorization".to_string(), format!("Bearer {secret}"))),
            AuthScheme::Header { name } => add.headers.push((name.clone(), secret.to_string())),
            AuthScheme::Query { name } => add.query.push((name.clone(), secret.to_string())),
        }
        add
    }
}

/// AWS SigV4: sign the built request with credentials carried by the signer
/// (resolved from env at startup, not from a per-request lease). Adds the signed
/// `Authorization` + `x-amz-*` headers.
pub struct SigV4Signer {
    pub creds: crate::sigv4::AwsCredentials,
    pub region: String,
    pub service: String,
}

impl RequestSigner for SigV4Signer {
    fn sign(&self, target: &SignTarget, _lease: Option<&CredentialLease>) -> SignedAdditions {
        let signed = crate::sigv4::sign(
            &crate::sigv4::CanonicalRequest {
                method: target.method,
                host: target.host,
                path: target.path,
                query: target.query,
                body: target.body,
            },
            &self.creds,
            &self.region,
            &self.service,
            &amz_date(),
        );
        SignedAdditions {
            headers: signed.into_iter().map(|h| (h.name, h.value)).collect(),
            query: Vec::new(),
        }
    }
}

/// UTC timestamp as `YYYYMMDDTHHMMSSZ` for `x-amz-date`.
fn amz_date() -> String {
    let now = time::OffsetDateTime::now_utc();
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second()
    )
}

/// Split a full URL into `(host, path, query)` for signing. Scheme-agnostic;
/// `path` keeps its leading `/`, `query` excludes the `?`.
pub fn split_url(url: &str) -> (String, String, String) {
    let after_scheme = url.split("://").nth(1).unwrap_or(url);
    let (authority_path, query) = match after_scheme.split_once('?') {
        Some((ap, q)) => (ap, q.to_string()),
        None => (after_scheme, String::new()),
    };
    match authority_path.split_once('/') {
        Some((host, path)) => (host.to_string(), format!("/{path}"), query),
        None => (authority_path.to_string(), "/".to_string(), query),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheme_signer_matches_apply_auth_semantics() {
        let lease = CredentialLease::bearer("acct", "sk-123");
        let target = SignTarget {
            method: "POST",
            host: "x",
            path: "/",
            query: "",
            body: b"",
        };

        let bearer = SchemeSigner(AuthScheme::Bearer).sign(&target, Some(&lease));
        assert_eq!(
            bearer.headers,
            vec![("authorization".to_string(), "Bearer sk-123".to_string())]
        );

        let header = SchemeSigner(AuthScheme::Header {
            name: "x-api-key".into(),
        })
        .sign(&target, Some(&lease));
        assert_eq!(
            header.headers,
            vec![("x-api-key".to_string(), "sk-123".to_string())]
        );

        let query =
            SchemeSigner(AuthScheme::Query { name: "key".into() }).sign(&target, Some(&lease));
        assert_eq!(query.query, vec![("key".to_string(), "sk-123".to_string())]);

        // No lease / None scheme → nothing added.
        assert!(SchemeSigner(AuthScheme::Bearer)
            .sign(&target, None)
            .headers
            .is_empty());
    }

    #[test]
    fn sigv4_signer_adds_amz_headers() {
        let signer = SigV4Signer {
            creds: crate::sigv4::AwsCredentials {
                access_key_id: "AKIA".into(),
                secret_access_key: "secret".into(),
                session_token: None,
            },
            region: "us-east-1".into(),
            service: "bedrock".into(),
        };
        let target = SignTarget {
            method: "POST",
            host: "bedrock-runtime.us-east-1.amazonaws.com",
            path: "/model/x/invoke",
            query: "",
            body: b"{}",
        };
        let add = signer.sign(&target, None);
        let names: Vec<_> = add.headers.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"authorization"));
        assert!(names.iter().any(|n| n.starts_with("x-amz-date")));
    }

    #[test]
    fn split_url_extracts_host_path_query() {
        assert_eq!(
            split_url("https://host.example.com/model/foo:bar/invoke"),
            (
                "host.example.com".to_string(),
                "/model/foo:bar/invoke".to_string(),
                "".to_string()
            )
        );
        assert_eq!(
            split_url("https://h/p?a=1&b=2"),
            ("h".to_string(), "/p".to_string(), "a=1&b=2".to_string())
        );
        assert_eq!(
            split_url("https://h"),
            ("h".to_string(), "/".to_string(), "".to_string())
        );
    }
}
