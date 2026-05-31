//! GCP service-account auth: mint a short-lived access token from a service-
//! account JSON key via the JWT-bearer grant, then cache + refresh it.
//!
//! The flow (RFC 7523 + Google's token endpoint): build a JWT asserting the
//! service account's identity and the requested scope, sign it RS256 with the
//! key's private key, and exchange it at `token_uri` for an `access_token`. The
//! same per-account async-mutex dedup as `RefreshCoordinator` ensures exactly
//! one mint under concurrent load.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde::{Deserialize, Serialize};

use sb_core::{Secret, Timeouts};

const DEFAULT_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";

/// The fields we use from a GCP service-account JSON key file.
#[derive(Debug, Clone, Deserialize)]
pub struct ServiceAccountKey {
    pub client_email: String,
    pub private_key: String,
    #[serde(default = "default_token_uri")]
    pub token_uri: String,
}

fn default_token_uri() -> String {
    "https://oauth2.googleapis.com/token".to_string()
}

impl ServiceAccountKey {
    /// Parse a service-account key from its JSON.
    pub fn from_json(json: &str) -> Result<Self, String> {
        serde_json::from_str(json).map_err(|e| format!("parse service-account key: {e}"))
    }
}

#[derive(Serialize)]
struct Claims<'a> {
    iss: &'a str,
    scope: &'a str,
    aud: &'a str,
    iat: u64,
    exp: u64,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    expires_in: Option<u64>,
}

/// The token-exchange HTTP call, behind a trait so the dedup/cache logic is
/// testable without a network or a real Google endpoint.
#[async_trait::async_trait]
pub trait AssertionExchanger: Send + Sync {
    async fn exchange(
        &self,
        token_uri: &str,
        assertion: &str,
    ) -> Result<(String, Option<u64>), String>;
}

/// Production exchanger: POST `grant_type=jwt-bearer&assertion=<jwt>`.
pub struct HttpAssertionExchanger {
    http: reqwest::Client,
}

impl HttpAssertionExchanger {
    pub fn new() -> Self {
        Self::with_timeouts(Timeouts::default()).expect("build service-account HTTP client")
    }

    pub fn with_timeouts(timeouts: Timeouts) -> Result<Self, String> {
        Ok(Self {
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_millis(timeouts.connect_ms))
                .read_timeout(Duration::from_millis(timeouts.read_ms))
                .build()
                .map_err(|e| e.to_string())?,
        })
    }
}

impl Default for HttpAssertionExchanger {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl AssertionExchanger for HttpAssertionExchanger {
    async fn exchange(
        &self,
        token_uri: &str,
        assertion: &str,
    ) -> Result<(String, Option<u64>), String> {
        let form = [
            ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
            ("assertion", assertion),
        ];
        let resp = self
            .http
            .post(token_uri)
            .form(&form)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!(
                "token endpoint returned {}",
                resp.status().as_u16()
            ));
        }
        let body: TokenResponse = resp.json().await.map_err(|e| e.to_string())?;
        Ok((body.access_token, body.expires_in))
    }
}

struct SaState {
    key: ServiceAccountKey,
    scope: String,
    access_token: Option<Secret>,
    expires_at: Option<Instant>,
}

/// Mints + caches Vertex access tokens per `(provider, account)`, deduping
/// concurrent mints with a per-account async lock.
pub struct ServiceAccountMinter {
    states: Mutex<HashMap<String, Arc<tokio::sync::Mutex<SaState>>>>,
    exchanger: Arc<dyn AssertionExchanger>,
    /// Mint this long before expiry (clock skew + JWT lifetime headroom).
    skew: Duration,
    /// Unix-epoch seconds source (injected so signing is testable).
    now_unix: fn() -> u64,
}

impl ServiceAccountMinter {
    pub fn new(exchanger: Arc<dyn AssertionExchanger>) -> Self {
        Self {
            states: Mutex::new(HashMap::new()),
            exchanger,
            skew: Duration::from_secs(60),
            now_unix: default_now_unix,
        }
    }

    fn key(provider: &str, account: &str) -> String {
        format!("{provider}/{account}")
    }

    pub fn register(
        &self,
        provider: &str,
        account: &str,
        key: ServiceAccountKey,
        scope: Option<String>,
    ) {
        let state = SaState {
            key,
            scope: scope.unwrap_or_else(|| DEFAULT_SCOPE.to_string()),
            access_token: None,
            expires_at: None,
        };
        self.states.lock().expect("sa mutex").insert(
            Self::key(provider, account),
            Arc::new(tokio::sync::Mutex::new(state)),
        );
    }

    pub fn manages(&self, provider: &str, account: &str) -> bool {
        self.states
            .lock()
            .expect("sa mutex")
            .contains_key(&Self::key(provider, account))
    }

    /// Current access token, minting/refreshing if needed. `None` = not a
    /// service-account here; `Some(Err)` = minting failed.
    pub async fn access_token(
        &self,
        provider: &str,
        account: &str,
    ) -> Option<Result<Secret, String>> {
        let arc = self
            .states
            .lock()
            .expect("sa mutex")
            .get(&Self::key(provider, account))
            .cloned()?;
        let mut state = arc.lock().await; // serialize per account == dedup
        Some(self.ensure_fresh(&mut state).await)
    }

    async fn ensure_fresh(&self, state: &mut SaState) -> Result<Secret, String> {
        if let (Some(token), Some(expiry)) = (&state.access_token, state.expires_at) {
            if Instant::now() + self.skew < expiry {
                return Ok(token.clone());
            }
        }
        let assertion = self.sign_assertion(&state.key, &state.scope)?;
        let (access_token, expires_in) = self
            .exchanger
            .exchange(&state.key.token_uri, &assertion)
            .await?;
        let token = Secret::new(access_token);
        state.access_token = Some(token.clone());
        state.expires_at = expires_in.map(|s| Instant::now() + Duration::from_secs(s));
        Ok(token)
    }

    fn sign_assertion(&self, key: &ServiceAccountKey, scope: &str) -> Result<String, String> {
        let iat = (self.now_unix)();
        let claims = Claims {
            iss: &key.client_email,
            scope,
            aud: &key.token_uri,
            iat,
            exp: iat + 3600,
        };
        let encoding = EncodingKey::from_rsa_pem(key.private_key.as_bytes())
            .map_err(|e| format!("service-account private key: {e}"))?;
        jsonwebtoken::encode(&Header::new(Algorithm::RS256), &claims, &encoding)
            .map_err(|e| format!("sign assertion: {e}"))
    }
}

fn default_now_unix() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // A throwaway RSA key (PKCS#8 PEM) for signing in tests — not a real secret.
    const TEST_KEY: &str = include_str!("testdata/test_rsa_pkcs8.pem");

    fn sa_json() -> String {
        format!(
            r#"{{"client_email":"svc@proj.iam.gserviceaccount.com","private_key":{key:?},"token_uri":"https://oauth2.example/token"}}"#,
            key = TEST_KEY
        )
    }

    struct MockExchanger {
        calls: AtomicUsize,
    }
    #[async_trait::async_trait]
    impl AssertionExchanger for MockExchanger {
        async fn exchange(
            &self,
            _uri: &str,
            assertion: &str,
        ) -> Result<(String, Option<u64>), String> {
            assert!(assertion.split('.').count() == 3, "assertion is a JWT");
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            Ok((format!("minted-{n}"), Some(3600)))
        }
    }

    #[test]
    fn parses_service_account_json() {
        let key = ServiceAccountKey::from_json(&sa_json()).unwrap();
        assert_eq!(key.client_email, "svc@proj.iam.gserviceaccount.com");
        assert_eq!(key.token_uri, "https://oauth2.example/token");
    }

    #[tokio::test]
    async fn mints_then_caches_within_expiry() {
        let ex = Arc::new(MockExchanger {
            calls: AtomicUsize::new(0),
        });
        let minter = ServiceAccountMinter::new(ex.clone());
        let key = ServiceAccountKey::from_json(&sa_json()).unwrap();
        minter.register("vertex", "a", key, None);

        assert_eq!(
            minter
                .access_token("vertex", "a")
                .await
                .unwrap()
                .unwrap()
                .expose(),
            "minted-0"
        );
        // cached within expiry — no second mint
        assert_eq!(
            minter
                .access_token("vertex", "a")
                .await
                .unwrap()
                .unwrap()
                .expose(),
            "minted-0"
        );
        assert_eq!(ex.calls.load(Ordering::SeqCst), 1, "cached, not re-minted");
    }

    #[tokio::test]
    async fn unmanaged_account_returns_none() {
        let minter = ServiceAccountMinter::new(Arc::new(MockExchanger {
            calls: AtomicUsize::new(0),
        }));
        assert!(minter.access_token("vertex", "nope").await.is_none());
    }
}
