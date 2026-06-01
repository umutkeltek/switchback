//! `CredentialResolver` — the one entry point the server uses to get an
//! account+lease for a provider, and to report the outcome so availability
//! state stays accurate. Selection (fill_first / round_robin-sticky) lives
//! here because it is a credential concern, not a routing one.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sb_core::{
    AuthConfig, Config, CredentialLease, ErrorClass, ProviderConfig, ProviderKind,
    SelectionStrategy,
};

use crate::account::{resolve_auth, Account, AccountId, ResolvedAuth};
use crate::availability::Availability;
use crate::refresh::{
    HttpTokenFetcher, OauthRegistration, RefreshCoordinator, RefreshTokenPersistence,
};

/// The accounts of one provider plus its selection policy.
pub struct ProviderAccounts {
    /// Priority-ascending (index 0 = most preferred under fill_first).
    pub accounts: Vec<Account>,
    pub strategy: SelectionStrategy,
    pub sticky: u32,
}

#[derive(Default)]
struct RrState {
    cursor: usize,
    count: u32,
}

/// A non-secret snapshot of a provider's account-pool health, surfaced to the
/// router (and `/v1/health`). Carries counts + circuit state — never account
/// ids, secrets, or leases.
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct PoolHealth {
    pub total: usize,
    pub healthy: usize,
    pub circuit_open: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct AccountLockHealth {
    pub scope: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub error_class: ErrorClass,
    pub retry_after_ms: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct AccountHealth {
    pub id: String,
    pub healthy: bool,
    pub locks: Vec<AccountLockHealth>,
}

/// What `resolve` decided.
pub enum ResolveOutcome {
    /// An account is available; use this lease.
    Selected {
        account_id: AccountId,
        lease: CredentialLease,
    },
    /// Every account is currently locked; try again later (target-level fallback).
    AllUnavailable { retry_after: Option<Duration> },
    /// The provider has no accounts / is unknown.
    NoAccounts,
}

pub struct CredentialResolver {
    providers: HashMap<String, ProviderAccounts>,
    availability: Availability,
    rr: Mutex<HashMap<String, RrState>>,
    /// `(provider_id, session_id) -> account_id` affinity. Non-secret runtime
    /// state: it names accounts, never stores leases or credential material.
    session_affinity: Mutex<HashMap<(String, String), AccountId>>,
    /// Live OAuth refresh for any account whose auth is `oauth` with a refresh
    /// token. Empty for API-key-only deployments.
    refresh: RefreshCoordinator,
    /// Provider-level circuit breaker (disabled unless configured).
    breaker: crate::breaker::CircuitBreaker,
    /// GCP service-account token minting (for Vertex). Empty otherwise.
    sa_minter: crate::service_account::ServiceAccountMinter,
}

impl CredentialResolver {
    pub fn new(providers: HashMap<String, ProviderAccounts>) -> Self {
        Self::with_parts(
            providers,
            RefreshCoordinator::new(Arc::new(HttpTokenFetcher::new())),
            crate::breaker::CircuitBreaker::new(&sb_core::BreakerConfig::default()),
            crate::service_account::ServiceAccountMinter::new(Arc::new(
                crate::service_account::HttpAssertionExchanger::new(),
            )),
        )
    }

    fn with_parts(
        providers: HashMap<String, ProviderAccounts>,
        refresh: RefreshCoordinator,
        breaker: crate::breaker::CircuitBreaker,
        sa_minter: crate::service_account::ServiceAccountMinter,
    ) -> Self {
        CredentialResolver {
            providers,
            availability: Availability::new(),
            rr: Mutex::new(HashMap::new()),
            session_affinity: Mutex::new(HashMap::new()),
            refresh,
            breaker,
            sa_minter,
        }
    }

    /// May the router attempt this provider right now? `false` = circuit OPEN.
    pub fn circuit_allows(&self, provider_id: &str) -> bool {
        self.breaker.allows(provider_id, Instant::now())
    }

    /// Feed a provider attempt outcome to the circuit breaker.
    pub fn circuit_record(&self, provider_id: &str, ok: bool) {
        self.breaker.record(provider_id, ok, Instant::now());
    }

    /// Build from config. Explicit `accounts:` win; otherwise a single default
    /// account is synthesized from the provider kind's legacy auth (backward
    /// compat), so every known provider always has >=1 account.
    pub fn from_config(cfg: &Config) -> Result<Self, String> {
        // Open the encrypted vault once at startup (if configured) so account
        // resolution can pull secrets by name. Fail-fast if it can't be opened.
        let vault = match &cfg.vault {
            Some(vc) => Some(
                crate::vault::Vault::open(std::path::Path::new(&vc.path), &vc.keychain_service)
                    .map_err(|e| format!("open vault: {e}"))?,
            ),
            None => None,
        };
        Self::from_config_with_vault(cfg, vault.as_ref())
    }

    /// Build with an already-opened vault injected. Lets callers (and tests)
    /// supply a vault without going through the OS keychain.
    pub fn from_config_with_vault(
        cfg: &Config,
        vault: Option<&crate::vault::Vault>,
    ) -> Result<Self, String> {
        let refresh = RefreshCoordinator::new(Arc::new(HttpTokenFetcher::with_policy(
            cfg.server.timeouts,
            cfg.server.block_private_networks,
        )?));
        let sa_minter = crate::service_account::ServiceAccountMinter::new(Arc::new(
            crate::service_account::HttpAssertionExchanger::with_policy(
                cfg.server.timeouts,
                cfg.server.block_private_networks,
            )?,
        ));
        let vault_persistence = cfg.vault.as_ref().map(|vault| {
            (
                std::path::PathBuf::from(vault.path.clone()),
                vault.keychain_service.clone(),
            )
        });
        let mut providers = HashMap::new();
        for provider in &cfg.providers {
            let pa = build_provider_accounts(provider, vault)?;
            // Hand every OAuth/service-account account's initial state to the
            // right minter so its access token is produced live at request time.
            for account in &pa.accounts {
                match &account.auth {
                    ResolvedAuth::Oauth {
                        token,
                        refresh: refresh_token,
                        refresh_vault,
                        token_url,
                        client_id,
                        client_secret,
                    } => {
                        let refresh_persist =
                            match (refresh_vault.as_ref(), vault_persistence.as_ref()) {
                                (Some(secret_name), Some((path, service))) => {
                                    Some(RefreshTokenPersistence::vault(
                                        path.clone(),
                                        service.clone(),
                                        secret_name.clone(),
                                    ))
                                }
                                _ => None,
                            };
                        refresh.register(
                            &provider.id,
                            &account.id,
                            OauthRegistration {
                                access_token: token.clone(),
                                refresh_token: refresh_token.clone(),
                                refresh_persist,
                                token_url: token_url.clone(),
                                client_id: client_id.clone(),
                                client_secret: client_secret.clone(),
                            },
                        );
                    }
                    ResolvedAuth::ServiceAccount { key, scope } => {
                        sa_minter.register(&provider.id, &account.id, key.clone(), scope.clone());
                    }
                    _ => {}
                }
            }
            providers.insert(provider.id.clone(), pa);
        }
        let breaker = crate::breaker::CircuitBreaker::new(&cfg.server.circuit_breaker);
        Ok(Self::with_parts(providers, refresh, breaker, sa_minter))
    }

    pub fn has_provider(&self, provider_id: &str) -> bool {
        self.providers.contains_key(provider_id)
    }

    pub fn account_ids(&self, provider_id: &str) -> Vec<String> {
        self.providers
            .get(provider_id)
            .map(|p| p.accounts.iter().map(|a| a.id.clone()).collect())
            .unwrap_or_default()
    }

    /// A NON-SECRET view of a provider's account pool for `model`: how many
    /// accounts are currently usable (not locked) out of the total, plus whether
    /// the provider's circuit is open. This is the seam that lets the router stop
    /// ranking targets as equally executable when their only accounts are locked
    /// — it never exposes account ids, secrets, or leases. Pass `model = ""` for
    /// a model-agnostic, account-wide view (used by the `/v1/health` surface).
    pub fn pool_health(&self, provider_id: &str, model: &str) -> PoolHealth {
        let now = Instant::now();
        match self.providers.get(provider_id) {
            Some(pa) => {
                let healthy = pa
                    .accounts
                    .iter()
                    .filter(|a| {
                        self.availability
                            .is_available(provider_id, &a.id, model, now)
                    })
                    .count();
                PoolHealth {
                    total: pa.accounts.len(),
                    healthy,
                    circuit_open: !self.breaker.allows(provider_id, now),
                }
            }
            None => PoolHealth {
                total: 0,
                healthy: 0,
                circuit_open: false,
            },
        }
    }

    /// Per-account non-secret availability details for operator surfaces.
    pub fn account_health(&self, provider_id: &str, model: &str) -> Vec<AccountHealth> {
        let now = Instant::now();
        let Some(pa) = self.providers.get(provider_id) else {
            return Vec::new();
        };
        pa.accounts
            .iter()
            .map(|account| {
                let locks = self
                    .availability
                    .locks_for(provider_id, &account.id, model, now)
                    .into_iter()
                    .map(|lock| AccountLockHealth {
                        scope: if lock.model.is_some() {
                            "model".to_string()
                        } else {
                            "account".to_string()
                        },
                        model: lock.model,
                        error_class: lock.error_class,
                        retry_after_ms: lock.retry_after.as_millis() as u64,
                    })
                    .collect();
                AccountHealth {
                    id: account.id.clone(),
                    healthy: self
                        .availability
                        .is_available(provider_id, &account.id, model, now),
                    locks,
                }
            })
            .collect()
    }

    /// Pick an available account for `(provider, model)`, skipping any in
    /// `exclude` (already-tried this request).
    pub fn resolve(
        &self,
        provider_id: &str,
        model: &str,
        exclude: &HashSet<AccountId>,
    ) -> ResolveOutcome {
        self.resolve_with_session(provider_id, model, exclude, None)
    }

    /// Pick an available account, preferring the previous account used by this
    /// `(provider, session)` when it is still available and not excluded.
    pub fn resolve_with_session(
        &self,
        provider_id: &str,
        model: &str,
        exclude: &HashSet<AccountId>,
        session_id: Option<&str>,
    ) -> ResolveOutcome {
        let Some(pa) = self.providers.get(provider_id) else {
            return ResolveOutcome::NoAccounts;
        };
        if pa.accounts.is_empty() {
            return ResolveOutcome::NoAccounts;
        }

        let now = Instant::now();
        let available: Vec<usize> = pa
            .accounts
            .iter()
            .enumerate()
            .filter(|(_, a)| !exclude.contains(&a.id))
            .filter(|(_, a)| {
                self.availability
                    .is_available(provider_id, &a.id, model, now)
            })
            .map(|(i, _)| i)
            .collect();

        if available.is_empty() {
            let ids: Vec<String> = pa.accounts.iter().map(|a| a.id.clone()).collect();
            let retry_after = self
                .availability
                .earliest_unlock(provider_id, &ids, model, now)
                .map(|unlock| unlock.saturating_duration_since(now));
            return ResolveOutcome::AllUnavailable { retry_after };
        }

        let session_id = session_id.filter(|s| !s.is_empty());
        if let Some(session_id) = session_id {
            let key = (provider_id.to_string(), session_id.to_string());
            if let Some(account_id) = self
                .session_affinity
                .lock()
                .expect("session affinity mutex")
                .get(&key)
                .cloned()
            {
                if let Some(idx) = available.iter().copied().find(|&idx| {
                    pa.accounts[idx].id == account_id && !exclude.contains(&account_id)
                }) {
                    let account = &pa.accounts[idx];
                    return ResolveOutcome::Selected {
                        account_id: account.id.clone(),
                        lease: account.lease(),
                    };
                }
            }
        }

        let idx = match pa.strategy {
            // Lowest priority value among the available accounts — computed
            // explicitly so correctness never depends on construction order.
            SelectionStrategy::FillFirst => *available
                .iter()
                .min_by_key(|&&i| pa.accounts[i].priority)
                .expect("available is non-empty here"),
            SelectionStrategy::RoundRobin => {
                self.pick_round_robin(provider_id, pa.accounts.len(), &available, pa.sticky)
            }
        };

        let account = &pa.accounts[idx];
        if let Some(session_id) = session_id {
            self.session_affinity
                .lock()
                .expect("session affinity mutex")
                .insert(
                    (provider_id.to_string(), session_id.to_string()),
                    account.id.clone(),
                );
        }
        ResolveOutcome::Selected {
            account_id: account.id.clone(),
            lease: account.lease(),
        }
    }

    fn pick_round_robin(
        &self,
        provider_id: &str,
        n: usize,
        available: &[usize],
        sticky: u32,
    ) -> usize {
        let mut rr = self.rr.lock().expect("rr mutex");
        let st = rr.entry(provider_id.to_string()).or_default();

        if st.count >= sticky.max(1) {
            st.cursor = st.cursor.wrapping_add(1);
            st.count = 0;
        }

        let start = st.cursor % n;
        // First available position at-or-after the cursor (wrapping).
        for off in 0..n {
            let idx = (start + off) % n;
            if available.contains(&idx) {
                if off != 0 {
                    // had to skip locked/excluded accounts → reset stickiness
                    st.count = 0;
                }
                st.cursor = idx;
                st.count += 1;
                return idx;
            }
        }
        // available is guaranteed non-empty by the caller.
        available[0]
    }

    /// Upgrade a static lease to a freshly-minted token if this account uses a
    /// live credential (OAuth refresh or a GCP service account). For API-key (or
    /// already-valid) accounts the lease is returned unchanged. `Err` means the
    /// mint/refresh itself failed — the caller treats it like an auth failure
    /// and falls over.
    pub async fn fresh_lease(
        &self,
        provider_id: &str,
        account_id: &str,
        lease: CredentialLease,
    ) -> Result<CredentialLease, String> {
        if let Some(result) = self.refresh.access_token(provider_id, account_id).await {
            return result.map(|token| CredentialLease::bearer(account_id.to_string(), token));
        }
        if let Some(result) = self.sa_minter.access_token(provider_id, account_id).await {
            return result.map(|token| CredentialLease::bearer(account_id.to_string(), token));
        }
        Ok(lease) // not a live-credential account → keep the static lease
    }

    /// Report a failed attempt; locks the account per the error class and
    /// returns the cooldown applied.
    pub fn report_failure(
        &self,
        provider_id: &str,
        account_id: &str,
        model: &str,
        class: ErrorClass,
    ) -> Duration {
        self.availability
            .report_failure(provider_id, account_id, model, class, Instant::now())
    }

    /// Report a successful attempt; clears the account's locks and backoff.
    pub fn report_success(&self, provider_id: &str, account_id: &str) {
        self.availability.report_success(provider_id, account_id);
    }

    /// Operator override for an account/model lockout. Returns `None` when the
    /// provider/account pair is unknown; otherwise returns whether a lock was
    /// actually cleared. Secrets and leases stay inside the resolver boundary.
    pub fn reset_lockout(
        &self,
        provider_id: &str,
        account_id: &str,
        model: Option<&str>,
    ) -> Option<bool> {
        let pa = self.providers.get(provider_id)?;
        if !pa.accounts.iter().any(|account| account.id == account_id) {
            return None;
        }
        Some(
            self.availability
                .reset_lockout(provider_id, account_id, model),
        )
    }
}

fn build_provider_accounts(
    provider: &ProviderConfig,
    vault: Option<&crate::vault::Vault>,
) -> Result<ProviderAccounts, String> {
    let mut accounts = Vec::new();

    if provider.accounts.is_empty() {
        // Backward compat: synthesize one "default" account from the kind.
        let auth = default_auth_for_kind(&provider.kind);
        accounts.push(Account {
            id: "default".to_string(),
            provider_id: provider.id.clone(),
            auth: resolve_auth(&auth, vault)
                .map_err(|e| format!("provider {} default account: {e}", provider.id))?,
            priority: 0,
            policy_tags: Vec::new(),
        });
    } else {
        for ac in &provider.accounts {
            accounts.push(Account {
                id: ac.id.clone(),
                provider_id: provider.id.clone(),
                auth: resolve_auth(&ac.auth, vault)
                    .map_err(|e| format!("provider {} account {}: {e}", provider.id, ac.id))?,
                priority: ac.priority,
                policy_tags: ac.policy_tags.clone(),
            });
        }
    }

    // Priority ascending (stable for equal priority).
    accounts.sort_by_key(|a| a.priority);

    Ok(ProviderAccounts {
        accounts,
        strategy: provider.selection,
        sticky: provider.sticky.unwrap_or(1).max(1),
    })
}

/// Map a provider kind's legacy inline auth into an AuthConfig for the
/// synthesized default account.
fn default_auth_for_kind(kind: &ProviderKind) -> AuthConfig {
    match kind {
        ProviderKind::Mock => AuthConfig::None,
        ProviderKind::OpenaiCompatible {
            api_key_env,
            api_key,
            ..
        }
        | ProviderKind::Anthropic {
            api_key_env,
            api_key,
            ..
        }
        | ProviderKind::Gemini {
            api_key_env,
            api_key,
            ..
        }
        | ProviderKind::Vertex {
            api_key_env,
            api_key,
            ..
        } => {
            if api_key.is_some() || api_key_env.is_some() {
                AuthConfig::ApiKey {
                    env: api_key_env.clone(),
                    inline: api_key.clone(),
                    vault: None,
                }
            } else {
                // e.g. local Ollama: no auth needed.
                AuthConfig::None
            }
        }
        // Bedrock signs each request with SigV4 credentials carried by the
        // selected account lease, preserving the target/account boundary.
        ProviderKind::Bedrock {
            access_key_env,
            secret_key_env,
            session_token_env,
            ..
        } => AuthConfig::AwsSigV4 {
            access_key_env: access_key_env.clone(),
            access_key: None,
            secret_key_env: secret_key_env.clone(),
            secret_key: None,
            session_token_env: session_token_env.clone(),
            session_token: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::ResolvedAuth;

    fn acct(id: &str, priority: i32) -> Account {
        Account {
            id: id.to_string(),
            provider_id: "p".to_string(),
            auth: ResolvedAuth::None,
            priority,
            policy_tags: vec![],
        }
    }

    fn resolver(
        strategy: SelectionStrategy,
        sticky: u32,
        accounts: Vec<Account>,
    ) -> CredentialResolver {
        let mut map = HashMap::new();
        map.insert(
            "p".to_string(),
            ProviderAccounts {
                accounts,
                strategy,
                sticky,
            },
        );
        CredentialResolver::new(map)
    }

    fn selected(o: ResolveOutcome) -> String {
        match o {
            ResolveOutcome::Selected { account_id, .. } => account_id,
            ResolveOutcome::AllUnavailable { .. } => "ALL_UNAVAILABLE".into(),
            ResolveOutcome::NoAccounts => "NO_ACCOUNTS".into(),
        }
    }

    #[test]
    fn fill_first_prefers_lowest_priority() {
        let r = resolver(
            SelectionStrategy::FillFirst,
            1,
            vec![acct("b", 1), acct("a", 0)],
        );
        let exclude = HashSet::new();
        assert_eq!(selected(r.resolve("p", "m", &exclude)), "a");
        // fill_first is sticky on the preferred account
        assert_eq!(selected(r.resolve("p", "m", &exclude)), "a");
    }

    #[test]
    fn fill_first_skips_failed_account() {
        let r = resolver(
            SelectionStrategy::FillFirst,
            1,
            vec![acct("a", 0), acct("b", 1)],
        );
        let exclude = HashSet::new();
        // a fails -> locked -> resolve now returns b
        r.report_failure("p", "a", "m", ErrorClass::RateLimited);
        assert_eq!(selected(r.resolve("p", "m", &exclude)), "b");
    }

    #[test]
    fn round_robin_rotates_with_stickiness() {
        let r = resolver(
            SelectionStrategy::RoundRobin,
            2,
            vec![acct("a", 0), acct("b", 1), acct("c", 2)],
        );
        let e = HashSet::new();
        let seq: Vec<String> = (0..6).map(|_| selected(r.resolve("p", "m", &e))).collect();
        // sticky=2 -> AABBCC
        assert_eq!(seq, vec!["a", "a", "b", "b", "c", "c"]);
    }

    #[test]
    fn session_affinity_prefers_the_same_available_account() {
        let r = resolver(
            SelectionStrategy::RoundRobin,
            1,
            vec![acct("a", 0), acct("b", 1)],
        );
        let exclude = HashSet::new();

        assert_eq!(
            selected(r.resolve_with_session("p", "m", &exclude, Some("session-a"))),
            "a"
        );
        assert_eq!(
            selected(r.resolve_with_session("p", "m", &exclude, Some("session-a"))),
            "a",
            "same session stays on the first account"
        );
        assert_eq!(
            selected(r.resolve_with_session("p", "m", &exclude, Some("session-b"))),
            "b",
            "a new session still uses the underlying round-robin cursor"
        );
    }

    #[test]
    fn exclude_set_is_respected() {
        let r = resolver(
            SelectionStrategy::FillFirst,
            1,
            vec![acct("a", 0), acct("b", 1)],
        );
        let mut exclude = HashSet::new();
        exclude.insert("a".to_string());
        assert_eq!(selected(r.resolve("p", "m", &exclude)), "b");
    }

    #[test]
    fn all_locked_yields_all_unavailable_with_retry() {
        let r = resolver(
            SelectionStrategy::FillFirst,
            1,
            vec![acct("a", 0), acct("b", 1)],
        );
        r.report_failure("p", "a", "m", ErrorClass::RateLimited);
        r.report_failure("p", "b", "m", ErrorClass::RateLimited);
        match r.resolve("p", "m", &HashSet::new()) {
            ResolveOutcome::AllUnavailable { retry_after } => assert!(retry_after.is_some()),
            _ => panic!("expected AllUnavailable"),
        }
    }

    #[test]
    fn account_health_reports_model_lock_scope() {
        let r = resolver(SelectionStrategy::FillFirst, 1, vec![acct("a", 0)]);
        r.report_failure("p", "a", "m", ErrorClass::RateLimited);

        let health = r.account_health("p", "m");
        assert_eq!(health.len(), 1);
        assert!(!health[0].healthy);
        assert_eq!(health[0].locks[0].scope, "model");
        assert_eq!(health[0].locks[0].model.as_deref(), Some("m"));
        assert_eq!(health[0].locks[0].error_class, ErrorClass::RateLimited);

        let other_model = r.account_health("p", "other");
        assert!(other_model[0].healthy);
        assert!(other_model[0].locks.is_empty());
    }

    #[test]
    fn unknown_provider_is_no_accounts() {
        let r = resolver(SelectionStrategy::FillFirst, 1, vec![acct("a", 0)]);
        assert!(matches!(
            r.resolve("nope", "m", &HashSet::new()),
            ResolveOutcome::NoAccounts
        ));
    }

    /// Concurrency invariant (the Advisor's review): many threads hammering
    /// resolve + report_failure/success must never deadlock, never poison a
    /// mutex (panic), and only ever return a valid account or AllUnavailable.
    /// Exercises the availability + round-robin lock paths under contention.
    #[test]
    fn concurrent_resolve_is_race_free_and_terminating() {
        use std::sync::Arc;

        let r = Arc::new(resolver(
            SelectionStrategy::RoundRobin,
            2,
            vec![acct("a", 0), acct("b", 1), acct("c", 2)],
        ));
        let valid = ["a", "b", "c", "ALL_UNAVAILABLE"];

        std::thread::scope(|s| {
            for _ in 0..16 {
                let r = Arc::clone(&r);
                s.spawn(move || {
                    for i in 0..300 {
                        let chosen = selected(r.resolve("p", "m", &HashSet::new()));
                        assert!(
                            valid.contains(&chosen.as_str()),
                            "invalid selection: {chosen}"
                        );
                        if i % 5 == 0 {
                            r.report_failure("p", &chosen, "m", ErrorClass::RateLimited);
                        }
                        if i % 9 == 0 {
                            r.report_success("p", &chosen);
                        }
                    }
                });
            }
        });

        // If we got here, no deadlock and no poisoned mutex under contention.
        assert!(!matches!(
            r.resolve("p", "m", &HashSet::new()),
            ResolveOutcome::NoAccounts
        ));
    }

    /// Vault integration: an account that references a vault secret by name
    /// resolves to that secret. Hermetic — builds the encrypted file with an
    /// explicit identity, never touching the OS keychain.
    #[test]
    fn account_resolves_api_key_from_vault() {
        use std::collections::BTreeMap;

        let id = age::x25519::Identity::generate();
        let mut path = std::env::temp_dir();
        path.push(format!("sb-resolver-vault-{}.age", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut map = BTreeMap::new();
        map.insert("or_key".to_string(), "sk-from-the-vault".to_string());
        crate::vault::write_map(&path, &id.to_public(), &map).unwrap();
        let vault = crate::vault::Vault::open_with_identity(&path, &id).unwrap();

        let cfg = Config::from_yaml(
            r#"
providers:
  - id: openrouter
    type: openai_compatible
    base_url: "https://openrouter.ai/api/v1"
    accounts:
      - id: vaulted
        auth: { kind: api_key, vault: or_key }
"#,
        )
        .unwrap();

        let resolver = CredentialResolver::from_config_with_vault(&cfg, Some(&vault)).unwrap();
        match resolver.resolve("openrouter", "any-model", &HashSet::new()) {
            ResolveOutcome::Selected { lease, .. } => {
                assert_eq!(lease.secret.expose(), "sk-from-the-vault")
            }
            _ => panic!("expected Selected from vault-backed account"),
        }

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn circuit_breaker_wired_from_config_opens_at_threshold() {
        let cfg = Config::from_yaml(
            r#"
server:
  bind: "127.0.0.1:0"
  circuit_breaker: { enabled: true, failure_threshold: 2, open_secs: 30 }
providers:
  - id: p
    type: mock
"#,
        )
        .unwrap();
        let r = CredentialResolver::from_config(&cfg).unwrap();
        assert!(r.circuit_allows("p"), "starts closed");
        r.circuit_record("p", false);
        assert!(
            r.circuit_allows("p"),
            "one failure < threshold → still closed"
        );
        r.circuit_record("p", false);
        assert!(!r.circuit_allows("p"), "threshold reached → open");
        assert!(
            r.circuit_allows("other"),
            "a different provider is unaffected"
        );
    }

    #[tokio::test]
    async fn service_account_is_registered_and_fresh_lease_mints() {
        // A vertex provider with a service_account account: fresh_lease must
        // consult the SA minter, which signs a JWT (with the test key, no sign
        // error) and tries to exchange it at the unreachable token_uri → Err.
        let pem = include_str!("testdata/test_rsa_pkcs8.pem");
        let sa = serde_json::json!({
            "client_email": "svc@proj.iam.gserviceaccount.com",
            "private_key": pem,
            "token_uri": "http://127.0.0.1:1/token"
        })
        .to_string();
        std::env::set_var("SB_TEST_SA_JSON", &sa);

        let cfg = Config::from_yaml(
            r#"
providers:
  - id: vertex
    type: vertex
    project: p
    region: us-central1
    accounts:
      - id: a
        auth: { kind: service_account, key_env: SB_TEST_SA_JSON }
"#,
        )
        .unwrap();
        let r = CredentialResolver::from_config(&cfg).unwrap();

        let lease = sb_core::CredentialLease::bearer("a".to_string(), sb_core::Secret::new(""));
        let err = r
            .fresh_lease("vertex", "a", lease)
            .await
            .expect_err("exchange to the unreachable token_uri must fail");
        assert!(
            !err.contains("private key") && !err.contains("sign"),
            "the JWT signed fine; the failure is the (unreachable) token exchange: {err}"
        );
    }
}
