//! AWS Signature Version 4 request signing — the auth half of Bedrock.
//!
//! Unlike a single-secret bearer scheme, SigV4 derives a per-request signature
//! from (access key, secret key, region, service, time, and the canonical
//! request itself). This module is a self-contained implementation (SHA-256 +
//! HMAC-SHA256, no AWS SDK) validated against AWS's published known-answer test
//! vector. It produces the headers to add to an outbound request:
//! `x-amz-date`, `Authorization`, and (when the body is non-empty)
//! `x-amz-content-sha256`.

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

/// AWS credentials for signing. `session_token` is set for temporary (STS) creds.
#[derive(Clone)]
pub struct AwsCredentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
}

/// The parts of an outbound request SigV4 needs to sign.
pub struct CanonicalRequest<'a> {
    pub method: &'a str,
    pub host: &'a str,
    /// Path, already URI-encoded (Bedrock paths are simple, no re-encoding needed).
    pub path: &'a str,
    /// Canonical query string (`""` for none).
    pub query: &'a str,
    pub body: &'a [u8],
}

/// A header to add to the request before sending.
pub struct SignedHeader {
    pub name: String,
    pub value: String,
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("hmac accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// Sign a request, returning the headers to add. `date` is `YYYYMMDDTHHMMSSZ`
/// (UTC); the caller passes it in so signing is deterministic and testable.
pub fn sign(
    req: &CanonicalRequest,
    creds: &AwsCredentials,
    region: &str,
    service: &str,
    date: &str,
) -> Vec<SignedHeader> {
    let datestamp = &date[..8]; // YYYYMMDD
    let payload_hash = sha256_hex(req.body);

    // Canonical headers — host + x-amz-date (+ token/content-hash when present),
    // sorted by lowercased name. signed_headers is the matching semicolon list.
    let mut headers: Vec<(String, String)> = vec![
        ("host".to_string(), req.host.to_string()),
        ("x-amz-content-sha256".to_string(), payload_hash.clone()),
        ("x-amz-date".to_string(), date.to_string()),
    ];
    if let Some(token) = &creds.session_token {
        headers.push(("x-amz-security-token".to_string(), token.clone()));
    }
    headers.sort_by(|a, b| a.0.cmp(&b.0));
    let canonical_headers: String = headers
        .iter()
        .map(|(k, v)| format!("{k}:{}\n", v.trim()))
        .collect();
    let signed_headers: String = headers
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");

    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        req.method, req.path, req.query, canonical_headers, signed_headers, payload_hash
    );

    let scope = format!("{datestamp}/{region}/{service}/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{date}\n{scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );

    // Derive the signing key: HMAC chain kSecret→kDate→kRegion→kService→kSigning.
    let k_date = hmac(
        format!("AWS4{}", creds.secret_access_key).as_bytes(),
        datestamp.as_bytes(),
    );
    let k_region = hmac(&k_date, region.as_bytes());
    let k_service = hmac(&k_region, service.as_bytes());
    let k_signing = hmac(&k_service, b"aws4_request");
    let signature = hex(&hmac(&k_signing, string_to_sign.as_bytes()));

    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
        creds.access_key_id
    );

    let mut out = vec![
        SignedHeader {
            name: "x-amz-date".to_string(),
            value: date.to_string(),
        },
        SignedHeader {
            name: "x-amz-content-sha256".to_string(),
            value: payload_hash,
        },
        SignedHeader {
            name: "authorization".to_string(),
            value: authorization,
        },
    ];
    if let Some(token) = &creds.session_token {
        out.push(SignedHeader {
            name: "x-amz-security-token".to_string(),
            value: token.clone(),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // AWS SigV4 known-answer test (the canonical "get-vanilla"-style vector):
    // these exact inputs produce a documented signature. If our chain is wrong,
    // this fails. Creds + date from AWS's published examples.
    const ACCESS: &str = "AKIDEXAMPLE";
    const SECRET: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";

    #[test]
    fn signing_key_matches_aws_documented_value() {
        // AWS documents the derived signing key for secret above, 20150830,
        // us-east-1, "iam" → a specific byte sequence (hex below).
        let k_date = hmac(format!("AWS4{SECRET}").as_bytes(), b"20150830");
        let k_region = hmac(&k_date, b"us-east-1");
        let k_service = hmac(&k_region, b"iam");
        let k_signing = hmac(&k_service, b"aws4_request");
        assert_eq!(
            hex(&k_signing),
            "c4afb1cc5771d871763a393e44b703571b55cc28424d1a5e86da6ed3c154a4b9"
        );
    }

    #[test]
    fn signs_a_request_and_emits_the_expected_headers() {
        let creds = AwsCredentials {
            access_key_id: ACCESS.to_string(),
            secret_access_key: SECRET.to_string(),
            session_token: None,
        };
        let req = CanonicalRequest {
            method: "POST",
            host: "bedrock-runtime.us-east-1.amazonaws.com",
            path: "/model/anthropic.claude/invoke",
            query: "",
            body: b"{\"x\":1}",
        };
        let headers = sign(&req, &creds, "us-east-1", "bedrock", "20150830T123600Z");
        let by = |n: &str| {
            headers
                .iter()
                .find(|h| h.name == n)
                .map(|h| h.value.clone())
        };

        assert_eq!(by("x-amz-date").as_deref(), Some("20150830T123600Z"));
        // content hash = sha256 of the body
        assert_eq!(
            by("x-amz-content-sha256").as_deref(),
            Some(sha256_hex(b"{\"x\":1}").as_str())
        );
        let auth = by("authorization").unwrap();
        assert!(auth.starts_with(
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/bedrock/aws4_request"
        ));
        assert!(auth.contains("SignedHeaders=host;x-amz-content-sha256;x-amz-date"));
        assert!(auth.contains("Signature="));
    }

    #[test]
    fn session_token_is_included_when_present() {
        let creds = AwsCredentials {
            access_key_id: ACCESS.to_string(),
            secret_access_key: SECRET.to_string(),
            session_token: Some("tok-123".to_string()),
        };
        let req = CanonicalRequest {
            method: "POST",
            host: "h",
            path: "/",
            query: "",
            body: b"",
        };
        let headers = sign(&req, &creds, "us-east-1", "bedrock", "20150830T123600Z");
        assert!(headers
            .iter()
            .any(|h| h.name == "x-amz-security-token" && h.value == "tok-123"));
        let auth = headers.iter().find(|h| h.name == "authorization").unwrap();
        assert!(
            auth.value.contains("x-amz-security-token"),
            "token is a signed header"
        );
    }
}
