//! Live OAuth token refresh.
//!
//! The hard requirement (the bug 9router's `dedupRefresh` exists to stop): when
//! many concurrent requests hit one expiring token, exactly ONE refresh must
//! fire — otherwise providers with rotating one-time refresh tokens revoke the
//! whole token family and brick the account. We get this for free with a
//! per-account async lock: concurrent callers serialize on it, and after the
//! first refresher updates the cached token the rest see it fresh.
//!
//! The token-endpoint HTTP call is behind a [`TokenFetcher`] trait so the
//! dedup/expiry/rotation logic is fully testable without a network.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use sb_core::{Secret, Timeouts};

/// What a token-endpoint refresh returned.
#[derive(Debug, Clone)]
pub struct TokenResponse {
    pub access_token: String,
    pub expires_in_secs: Option<u64>,
    /// Some providers rotate the refresh token on each refresh.
    pub refresh_token: Option<String>,
}

/// The token-endpoint HTTP call. A trait so the coordinator's dedup/expiry logic
/// is testable with a mock (no network).
#[async_trait]
pub trait TokenFetcher: Send + Sync {
    async fn refresh(
        &self,
        token_url: &str,
        client_id: Option<&str>,
        client_secret: Option<&str>,
        refresh_token: &str,
    ) -> Result<TokenResponse, String>;
}

/// Production fetcher: `POST grant_type=refresh_token` to the token endpoint.
pub struct HttpTokenFetcher {
    http: reqwest::Client,
}

impl HttpTokenFetcher {
    pub fn new() -> Self {
        Self::with_timeouts(Timeouts::default()).expect("build token refresh HTTP client")
    }

    pub fn with_timeouts(timeouts: Timeouts) -> Result<Self, String> {
        Ok(Self {
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_millis(timeouts.connect_ms))
                .read_timeout(Duration::from_millis(timeouts.read_ms))
                .build()
                .map_err(|e| e.to_string())?,
        })
    }
}

impl Default for HttpTokenFetcher {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TokenFetcher for HttpTokenFetcher {
    async fn refresh(
        &self,
        token_url: &str,
        client_id: Option<&str>,
        client_secret: Option<&str>,
        refresh_token: &str,
    ) -> Result<TokenResponse, String> {
        let mut form: Vec<(&str, &str)> = vec![
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
        ];
        if let Some(id) = client_id {
            form.push(("client_id", id));
        }
        if let Some(secret) = client_secret {
            form.push(("client_secret", secret));
        }
        let resp = self
            .http
            .post(token_url)
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
        let json: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
        let access_token = json
            .get("access_token")
            .and_then(|v| v.as_str())
            .ok_or("token response missing `access_token`")?
            .to_string();
        Ok(TokenResponse {
            access_token,
            expires_in_secs: json.get("expires_in").and_then(|v| v.as_u64()),
            refresh_token: json
                .get("refresh_token")
                .and_then(|v| v.as_str())
                .map(String::from),
        })
    }
}

/// Per-account OAuth state — mutated only under the per-account async lock.
struct OauthState {
    access_token: Option<Secret>,
    expires_at: Option<Instant>,
    refresh_token: Option<Secret>,
    refresh_persist: Option<RefreshTokenPersistence>,
    token_url: Option<String>,
    client_id: Option<String>,
    client_secret: Option<Secret>,
}

/// Initial OAuth state for one account (from config).
#[derive(Default)]
pub struct OauthRegistration {
    pub access_token: Option<Secret>,
    pub refresh_token: Option<Secret>,
    pub refresh_persist: Option<RefreshTokenPersistence>,
    pub token_url: Option<String>,
    pub client_id: Option<String>,
    pub client_secret: Option<Secret>,
}

/// Where a rotating OAuth refresh token should be written back. Only vault
/// references are persisted; env/inline sources remain operator-managed.
#[derive(Debug, Clone)]
pub struct RefreshTokenPersistence {
    pub vault_path: PathBuf,
    pub keychain_service: String,
    pub secret_name: String,
}

impl RefreshTokenPersistence {
    pub fn vault(
        path: impl Into<PathBuf>,
        keychain_service: impl Into<String>,
        secret_name: impl Into<String>,
    ) -> Self {
        RefreshTokenPersistence {
            vault_path: path.into(),
            keychain_service: keychain_service.into(),
            secret_name: secret_name.into(),
        }
    }

    fn persist(&self, refresh_token: &str) -> Result<(), String> {
        crate::vault::set_secret(
            &self.vault_path,
            &self.keychain_service,
            &self.secret_name,
            refresh_token,
        )
        .map_err(|e| format!("persist rotated refresh token to vault: {e}"))
    }
}

/// De-duplicates and caches OAuth access tokens per `(provider, account)`.
pub struct RefreshCoordinator {
    states: Mutex<HashMap<String, Arc<tokio::sync::Mutex<OauthState>>>>,
    fetcher: Arc<dyn TokenFetcher>,
    /// Refresh this long before the actual expiry (clock skew + call latency).
    skew: Duration,
}

impl RefreshCoordinator {
    pub fn new(fetcher: Arc<dyn TokenFetcher>) -> Self {
        Self {
            states: Mutex::new(HashMap::new()),
            fetcher,
            skew: Duration::from_secs(60),
        }
    }

    fn key(provider: &str, account: &str) -> String {
        format!("{provider}/{account}")
    }

    /// Register an OAuth account's initial state (at config load).
    pub fn register(&self, provider: &str, account: &str, reg: OauthRegistration) {
        let state = OauthState {
            access_token: reg.access_token,
            expires_at: None,
            refresh_token: reg.refresh_token,
            refresh_persist: reg.refresh_persist,
            token_url: reg.token_url,
            client_id: reg.client_id,
            client_secret: reg.client_secret,
        };
        self.states.lock().expect("states mutex").insert(
            Self::key(provider, account),
            Arc::new(tokio::sync::Mutex::new(state)),
        );
    }

    /// Whether this `(provider, account)` is OAuth-managed here.
    pub fn manages(&self, provider: &str, account: &str) -> bool {
        self.states
            .lock()
            .expect("states mutex")
            .contains_key(&Self::key(provider, account))
    }

    /// The current access token, refreshing if expired. `None` = not OAuth here;
    /// `Some(Err)` = OAuth but refresh failed. The per-account async lock makes
    /// concurrent callers share exactly one refresh.
    pub async fn access_token(
        &self,
        provider: &str,
        account: &str,
    ) -> Option<Result<Secret, String>> {
        let arc = self
            .states
            .lock()
            .expect("states mutex")
            .get(&Self::key(provider, account))
            .cloned()?;
        let mut state = arc.lock().await; // serialize per account == dedup
        Some(self.ensure_fresh(&mut state).await)
    }

    async fn ensure_fresh(&self, state: &mut OauthState) -> Result<Secret, String> {
        let now = Instant::now();
        if let Some(token) = &state.access_token {
            match state.expires_at {
                // Known expiry and still in the future (minus skew) -> cached.
                Some(expiry) if now + self.skew < expiry => return Ok(token.clone()),
                // Unknown expiry -> assume valid (we only refresh on known expiry
                // or when there is no token yet).
                None => return Ok(token.clone()),
                // Expired -> fall through to refresh.
                Some(_) => {}
            }
        }

        let refresh = state
            .refresh_token
            .as_ref()
            .ok_or("no access token and no refresh token")?;
        let url = state
            .token_url
            .as_deref()
            .ok_or("oauth refresh requires token_url")?;
        let resp = self
            .fetcher
            .refresh(
                url,
                state.client_id.as_deref(),
                state.client_secret.as_ref().map(Secret::expose),
                refresh.expose(),
            )
            .await?;

        let token = Secret::new(resp.access_token);
        state.access_token = Some(token.clone());
        state.expires_at = resp.expires_in_secs.map(|s| now + Duration::from_secs(s));
        if let Some(rotated) = resp.refresh_token {
            if let Some(persist) = &state.refresh_persist {
                persist.persist(&rotated)?;
            }
            state.refresh_token = Some(Secret::new(rotated)); // refresh-token rotation
        }
        Ok(token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use age::secrecy::ExposeSecret;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::OnceLock;

    struct MockFetcher {
        calls: AtomicUsize,
        seen_refresh: Mutex<Vec<String>>,
        expires_in: Option<u64>,
        rotate: bool,
    }
    impl MockFetcher {
        fn new(expires_in: Option<u64>, rotate: bool) -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicUsize::new(0),
                seen_refresh: Mutex::new(Vec::new()),
                expires_in,
                rotate,
            })
        }
    }
    #[async_trait]
    impl TokenFetcher for MockFetcher {
        async fn refresh(
            &self,
            _url: &str,
            _id: Option<&str>,
            _secret: Option<&str>,
            refresh_token: &str,
        ) -> Result<TokenResponse, String> {
            self.seen_refresh
                .lock()
                .unwrap()
                .push(refresh_token.to_string());
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(TokenResponse {
                access_token: format!("access-{n}"),
                expires_in_secs: self.expires_in,
                refresh_token: if self.rotate {
                    Some(format!("refresh-{}", n + 1))
                } else {
                    None
                },
            })
        }
    }

    fn reg(refresh: &str, url: &str) -> OauthRegistration {
        OauthRegistration {
            refresh_token: Some(Secret::new(refresh)),
            token_url: Some(url.to_string()),
            ..Default::default()
        }
    }

    fn temp_vault_path(tag: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("sb-oauth-vault-{tag}-{}.age", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }

    fn vault_env_lock() -> &'static tokio::sync::Mutex<()> {
        static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
    }

    #[tokio::test]
    async fn mints_then_caches_within_expiry() {
        let fetcher = MockFetcher::new(Some(3600), false);
        let coord = RefreshCoordinator::new(fetcher.clone());
        coord.register("p", "a", reg("refresh-0", "https://token"));

        assert_eq!(
            coord
                .access_token("p", "a")
                .await
                .unwrap()
                .unwrap()
                .expose(),
            "access-0"
        );
        // second call within expiry -> cached, no second refresh
        assert_eq!(
            coord
                .access_token("p", "a")
                .await
                .unwrap()
                .unwrap()
                .expose(),
            "access-0"
        );
        assert_eq!(
            fetcher.calls.load(Ordering::SeqCst),
            1,
            "cached, not re-refreshed"
        );
    }

    #[tokio::test]
    async fn concurrent_callers_share_one_refresh() {
        let fetcher = MockFetcher::new(Some(3600), false);
        let coord = Arc::new(RefreshCoordinator::new(fetcher.clone()));
        coord.register("p", "a", reg("r", "u"));

        let mut handles = Vec::new();
        for _ in 0..8 {
            let coord = coord.clone();
            handles.push(tokio::spawn(async move {
                coord
                    .access_token("p", "a")
                    .await
                    .unwrap()
                    .unwrap()
                    .expose()
                    .to_string()
            }));
        }
        for h in handles {
            assert_eq!(h.await.unwrap(), "access-0");
        }
        assert_eq!(
            fetcher.calls.load(Ordering::SeqCst),
            1,
            "concurrent callers on one token must trigger exactly one refresh"
        );
    }

    #[tokio::test]
    async fn rotates_refresh_token_on_each_refresh() {
        let fetcher = MockFetcher::new(Some(0), true); // expires_in 0 -> always stale
        let coord = RefreshCoordinator::new(fetcher.clone());
        coord.register("p", "a", reg("refresh-0", "u"));

        coord.access_token("p", "a").await.unwrap().unwrap(); // uses refresh-0, rotates to refresh-1
        coord.access_token("p", "a").await.unwrap().unwrap(); // must use the rotated refresh-1
        let seen = fetcher.seen_refresh.lock().unwrap().clone();
        assert_eq!(
            seen,
            vec!["refresh-0", "refresh-1"],
            "rotated refresh token must be used"
        );
    }

    #[tokio::test]
    async fn rotated_refresh_token_persists_to_vault_reference() {
        let _guard = vault_env_lock().lock().await;
        let previous_key = std::env::var(crate::vault::KEY_ENV).ok();
        let path = temp_vault_path("rotated");
        let identity = age::x25519::Identity::generate();
        std::env::set_var(crate::vault::KEY_ENV, identity.to_string().expose_secret());

        crate::vault::set_secret(&path, "switchback-test", "oauth-refresh", "refresh-0").unwrap();

        let fetcher = MockFetcher::new(Some(0), true);
        let coord = RefreshCoordinator::new(fetcher);
        coord.register(
            "p",
            "a",
            OauthRegistration {
                refresh_token: Some(Secret::new("refresh-0")),
                refresh_persist: Some(RefreshTokenPersistence::vault(
                    path.clone(),
                    "switchback-test",
                    "oauth-refresh",
                )),
                token_url: Some("u".to_string()),
                ..Default::default()
            },
        );

        coord.access_token("p", "a").await.unwrap().unwrap();

        let vault = crate::vault::Vault::open_with_identity(&path, &identity).unwrap();
        assert_eq!(vault.get("oauth-refresh").unwrap().expose(), "refresh-1");

        if let Some(key) = previous_key {
            std::env::set_var(crate::vault::KEY_ENV, key);
        } else {
            std::env::remove_var(crate::vault::KEY_ENV);
        }
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn unmanaged_account_returns_none() {
        let coord = RefreshCoordinator::new(MockFetcher::new(None, false));
        assert!(coord.access_token("p", "unknown").await.is_none());
    }

    #[tokio::test]
    async fn refresh_failure_surfaces() {
        struct Failing;
        #[async_trait]
        impl TokenFetcher for Failing {
            async fn refresh(
                &self,
                _: &str,
                _: Option<&str>,
                _: Option<&str>,
                _: &str,
            ) -> Result<TokenResponse, String> {
                Err("token endpoint returned 401".into())
            }
        }
        let coord = RefreshCoordinator::new(Arc::new(Failing));
        coord.register("p", "a", reg("r", "u"));
        let result = coord.access_token("p", "a").await.unwrap();
        assert!(result.is_err());
    }
}
