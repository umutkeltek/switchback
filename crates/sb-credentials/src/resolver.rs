//! `CredentialResolver` — the one entry point the server uses to get an
//! account+lease for a provider, and to report the outcome so availability
//! state stays accurate. Selection (fill_first / round_robin-sticky) lives
//! here because it is a credential concern, not a routing one.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use sb_core::{
    AuthConfig, Config, CredentialLease, ErrorClass, ProviderConfig, ProviderKind,
    SelectionStrategy,
};

use crate::account::{resolve_auth, Account, AccountId};
use crate::availability::Availability;

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
}

impl CredentialResolver {
    pub fn new(providers: HashMap<String, ProviderAccounts>) -> Self {
        CredentialResolver {
            providers,
            availability: Availability::new(),
            rr: Mutex::new(HashMap::new()),
        }
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
        let mut providers = HashMap::new();
        for provider in &cfg.providers {
            providers.insert(
                provider.id.clone(),
                build_provider_accounts(provider, vault)?,
            );
        }
        Ok(Self::new(providers))
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

    /// Pick an available account for `(provider, model)`, skipping any in
    /// `exclude` (already-tried this request).
    pub fn resolve(
        &self,
        provider_id: &str,
        model: &str,
        exclude: &HashSet<AccountId>,
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
                let valid = valid;
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
}
