//! The typed data-model seams (deconstruction §13.3). These are the ENTITIES
//! kept separate from day one so the "OpenRouter later" path stays reachable
//! without a rewrite: `Provider` / `Model` / `Account` / `Credential` / `Price`
//! are distinct types, never collapsed into one primitive. Each carries a
//! tenant scope (always `TenantId::SINGLE` today) and references others by typed
//! id (FK-by-id), and `Price` is a *ledger with history* so cost is auditable.
//!
//! v1 is **seams, not machinery**: these are types + an in-memory [`Catalog`]
//! that can validate referential integrity and resolve effective prices. A
//! DB-backed store (sqlx) later *implements the same shape* — no migration of
//! the collapsed-primitive kind §13.3 warns against.

use serde::{Deserialize, Serialize};

use crate::CapabilityProfile;

/// Tenant/owner scope present on every entity — always `SINGLE` (1) in v1, but
/// carried so multi-tenancy is an additive change, never a schema migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TenantId(pub u64);

impl TenantId {
    pub const SINGLE: TenantId = TenantId(1);
}

impl Default for TenantId {
    fn default() -> Self {
        TenantId::SINGLE
    }
}

/// How a provider's wire API is shaped — selects the adapter/protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiKind {
    OpenAiCompatible,
    Anthropic,
    Gemini,
    Mock,
}

impl ApiKind {
    /// A sensible default capability profile for a provider of this kind, used
    /// when the catalog has no per-model entry. Conservative where providers
    /// genuinely differ (e.g. Gemini's restricted `functionDeclarations` schema
    /// can't take arbitrary JSON Schema), permissive otherwise. `None` context
    /// window means "unknown" — the router won't reject on context size.
    pub fn default_capabilities(&self) -> crate::CapabilityProfile {
        use crate::CapabilityProfile;
        match self {
            ApiKind::Mock => CapabilityProfile::default(),
            ApiKind::OpenAiCompatible => CapabilityProfile {
                json_schema: true,
                parallel_tool_calls: true,
                ..CapabilityProfile::default()
            },
            ApiKind::Anthropic => CapabilityProfile {
                json_schema: true,
                vision_in: true,
                ..CapabilityProfile::default()
            },
            ApiKind::Gemini => CapabilityProfile {
                // Gemini speaks a restricted JSON-Schema dialect, but the
                // downleveler maps `response_format` → generationConfig
                // .responseSchema (stripping anyOf/$ref/const/etc.), so
                // structured output works and the router may route it here.
                json_schema: true,
                vision_in: true,
                ..CapabilityProfile::default()
            },
        }
    }
}

/// Lifecycle status shared by catalog entities.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityStatus {
    #[default]
    Active,
    Disabled,
    Deprecated,
}

/// A provider — one entity, not half-in-code (§13.3).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provider {
    pub id: String,
    #[serde(default)]
    pub tenant_id: TenantId,
    pub name: String,
    pub api_kind: ApiKind,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub status: EntityStatus,
}

/// Input/output modalities a model supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Modality {
    TextIn,
    TextOut,
    VisionIn,
    AudioIn,
    ImageOut,
    Embeddings,
}

/// A model catalog entry — a real row with a provider FK, context window,
/// modalities and capabilities, replacing string-typing + kv overlays (§13.3).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Model {
    /// Upstream model id, e.g. `claude-3-5-sonnet-latest`.
    pub id: String,
    #[serde(default)]
    pub tenant_id: TenantId,
    /// FK -> [`Provider::id`].
    pub provider_id: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub context_window: Option<u32>,
    #[serde(default)]
    pub modalities: Vec<Modality>,
    #[serde(default)]
    pub capabilities: CapabilityProfile,
    #[serde(default)]
    pub status: EntityStatus,
    /// RFC3339 instant; string to keep sb-core free of a time dependency.
    #[serde(default)]
    pub deprecated_at: Option<String>,
}

impl Model {
    /// The capability profile the router should filter on for this model. Starts
    /// from the model's declared `capabilities`, then overlays the catalog's
    /// richer per-model facts: `context_window` -> `max_context_tokens`, and
    /// `vision_in` from `modalities`. (A catalog entry is authoritative; if you
    /// add one, declare its capabilities.)
    pub fn capability_profile(&self) -> crate::CapabilityProfile {
        let mut caps = self.capabilities.clone();
        if let Some(context) = self.context_window {
            caps.max_context_tokens = Some(context);
        }
        if !self.modalities.is_empty() {
            caps.vision_in = self.modalities.contains(&Modality::VisionIn);
        }
        caps
    }
}

/// An authenticated account belonging to a provider. The CREDENTIAL is a
/// separate entity — vault material is never collapsed in here (§13.3).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub id: String,
    #[serde(default)]
    pub tenant_id: TenantId,
    /// FK -> [`Provider::id`].
    pub provider_id: String,
    #[serde(default)]
    pub owner_id: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub priority: i32,
    #[serde(default = "default_true")]
    pub active: bool,
}

fn default_true() -> bool {
    true
}

/// How a credential authenticates — the *kind*, never the secret.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialKind {
    ApiKey,
    Oauth,
    None,
}

/// A credential entity — separate from the account, pointing at WHERE the secret
/// lives (a vault secret name / env var), with an optional expiry. The secret
/// material itself is never stored here (§13.3: "vault separate from account").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Credential {
    pub id: String,
    #[serde(default)]
    pub tenant_id: TenantId,
    /// FK -> [`Account::id`].
    pub account_id: String,
    pub kind: CredentialKind,
    /// Where the secret lives (vault secret name / env var) — never the value.
    #[serde(default)]
    pub source_ref: Option<String>,
    #[serde(default)]
    pub expires_at: Option<String>,
}

/// Which token a price applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenKind {
    Input,
    Output,
    CachedInput,
    Reasoning,
}

/// A price LEDGER entry with history (`effective_from`/`effective_to`), so cost
/// is auditable and you can mark up later (§13.3). Many entries may exist per
/// `(model, token_kind)` over time; money is integer micro-USD, never a float.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Price {
    #[serde(default)]
    pub tenant_id: TenantId,
    /// FK -> [`Model::id`].
    pub model_id: String,
    pub token_kind: TokenKind,
    /// Price per million tokens, in micro-USD (integer — no float money).
    pub unit_price_micros_per_mtok: u64,
    /// RFC3339 (UTC `Z`); lexicographic order == chronological order.
    pub effective_from: String,
    #[serde(default)]
    pub effective_to: Option<String>,
}

/// In-memory catalog — the seam a DB-backed store later implements. Holds the
/// typed entities, validates referential integrity, and resolves effective
/// prices. No DB, no machinery; just the shape that keeps the model honest.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Catalog {
    #[serde(default)]
    pub providers: Vec<Provider>,
    #[serde(default)]
    pub models: Vec<Model>,
    #[serde(default)]
    pub accounts: Vec<Account>,
    #[serde(default)]
    pub credentials: Vec<Credential>,
    #[serde(default)]
    pub prices: Vec<Price>,
}

impl Catalog {
    pub fn provider(&self, id: &str) -> Option<&Provider> {
        self.providers.iter().find(|p| p.id == id)
    }

    pub fn model(&self, id: &str) -> Option<&Model> {
        self.models.iter().find(|m| m.id == id)
    }

    pub fn models_for_provider<'a>(
        &'a self,
        provider_id: &'a str,
    ) -> impl Iterator<Item = &'a Model> {
        self.models
            .iter()
            .filter(move |m| m.provider_id == provider_id)
    }

    pub fn accounts_for_provider<'a>(
        &'a self,
        provider_id: &'a str,
    ) -> impl Iterator<Item = &'a Account> {
        self.accounts
            .iter()
            .filter(move |a| a.provider_id == provider_id)
    }

    pub fn credentials_for_account<'a>(
        &'a self,
        account_id: &'a str,
    ) -> impl Iterator<Item = &'a Credential> {
        self.credentials
            .iter()
            .filter(move |c| c.account_id == account_id)
    }

    /// The effective price for `(model, token_kind)` at an RFC3339 instant `at`,
    /// honoring `effective_from <= at < effective_to`. Among overlapping rows the
    /// one with the latest `effective_from` wins. Relies on RFC3339-UTC strings
    /// comparing lexicographically (== chronologically).
    pub fn effective_price(&self, model_id: &str, kind: TokenKind, at: &str) -> Option<&Price> {
        self.prices
            .iter()
            .filter(|p| p.model_id == model_id && p.token_kind == kind)
            .filter(|p| p.effective_from.as_str() <= at)
            .filter(|p| p.effective_to.as_deref().map_or(true, |end| at < end))
            .max_by(|a, b| a.effective_from.cmp(&b.effective_from))
    }

    /// The current price for `(model, token_kind)` — the latest open-ended
    /// ledger entry (`effective_to` is null). Used to price a request as it
    /// happens, without needing a "now" timestamp; `effective_price` is for
    /// auditing/re-pricing a past request against the ledger's history.
    pub fn current_price(&self, model_id: &str, kind: TokenKind) -> Option<&Price> {
        self.prices
            .iter()
            .filter(|p| p.model_id == model_id && p.token_kind == kind && p.effective_to.is_none())
            .max_by(|a, b| a.effective_from.cmp(&b.effective_from))
    }

    /// Validate referential integrity — every FK must resolve. Returns the list
    /// of dangling references (empty = clean). The seam that catches a collapsed
    /// or mistyped reference before it becomes an expensive bug.
    pub fn validate(&self) -> Vec<String> {
        let mut problems = Vec::new();
        let has_provider = |id: &str| self.providers.iter().any(|p| p.id == id);
        let has_account = |id: &str| self.accounts.iter().any(|a| a.id == id);
        let has_model = |id: &str| self.models.iter().any(|m| m.id == id);

        for m in &self.models {
            if !has_provider(&m.provider_id) {
                problems.push(format!(
                    "model `{}` -> unknown provider `{}`",
                    m.id, m.provider_id
                ));
            }
        }
        for a in &self.accounts {
            if !has_provider(&a.provider_id) {
                problems.push(format!(
                    "account `{}` -> unknown provider `{}`",
                    a.id, a.provider_id
                ));
            }
        }
        for c in &self.credentials {
            if !has_account(&c.account_id) {
                problems.push(format!(
                    "credential `{}` -> unknown account `{}`",
                    c.id, c.account_id
                ));
            }
        }
        for (i, p) in self.prices.iter().enumerate() {
            if !has_model(&p.model_id) {
                problems.push(format!("price[{i}] -> unknown model `{}`", p.model_id));
            }
        }
        problems
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Catalog {
        Catalog {
            providers: vec![Provider {
                id: "anthropic".into(),
                tenant_id: TenantId::SINGLE,
                name: "Anthropic".into(),
                api_kind: ApiKind::Anthropic,
                base_url: Some("https://api.anthropic.com".into()),
                status: EntityStatus::Active,
            }],
            models: vec![Model {
                id: "claude-3-5-sonnet-latest".into(),
                tenant_id: TenantId::SINGLE,
                provider_id: "anthropic".into(),
                display_name: Some("Claude 3.5 Sonnet".into()),
                context_window: Some(200_000),
                modalities: vec![Modality::TextIn, Modality::TextOut, Modality::VisionIn],
                capabilities: CapabilityProfile::default(),
                status: EntityStatus::Active,
                deprecated_at: None,
            }],
            accounts: vec![Account {
                id: "anthropic-personal".into(),
                tenant_id: TenantId::SINGLE,
                provider_id: "anthropic".into(),
                owner_id: Some("umut".into()),
                display_name: None,
                priority: 0,
                active: true,
            }],
            credentials: vec![Credential {
                id: "cred-1".into(),
                tenant_id: TenantId::SINGLE,
                account_id: "anthropic-personal".into(),
                kind: CredentialKind::ApiKey,
                source_ref: Some("anthropic_key".into()),
                expires_at: None,
            }],
            prices: vec![
                Price {
                    tenant_id: TenantId::SINGLE,
                    model_id: "claude-3-5-sonnet-latest".into(),
                    token_kind: TokenKind::Input,
                    unit_price_micros_per_mtok: 3_000_000, // $3 / Mtok
                    effective_from: "2025-01-01T00:00:00Z".into(),
                    effective_to: Some("2026-01-01T00:00:00Z".into()),
                },
                Price {
                    tenant_id: TenantId::SINGLE,
                    model_id: "claude-3-5-sonnet-latest".into(),
                    token_kind: TokenKind::Input,
                    unit_price_micros_per_mtok: 2_500_000, // price drop in 2026
                    effective_from: "2026-01-01T00:00:00Z".into(),
                    effective_to: None,
                },
            ],
        }
    }

    #[test]
    fn entities_stay_separate_and_fks_resolve() {
        let catalog = sample();
        assert!(
            catalog.validate().is_empty(),
            "clean catalog has no dangling FKs"
        );
        assert_eq!(
            catalog
                .model("claude-3-5-sonnet-latest")
                .unwrap()
                .context_window,
            Some(200_000)
        );
        assert_eq!(catalog.models_for_provider("anthropic").count(), 1);
        assert_eq!(
            catalog
                .credentials_for_account("anthropic-personal")
                .count(),
            1
        );
    }

    #[test]
    fn validate_catches_dangling_references() {
        let mut catalog = sample();
        catalog.models[0].provider_id = "ghost".into();
        catalog.credentials[0].account_id = "missing".into();
        let problems = catalog.validate();
        assert_eq!(problems.len(), 2, "got: {problems:?}");
        assert!(problems
            .iter()
            .any(|p| p.contains("unknown provider `ghost`")));
        assert!(problems
            .iter()
            .any(|p| p.contains("unknown account `missing`")));
    }

    #[test]
    fn price_ledger_resolves_by_time_window() {
        let catalog = sample();
        let m = "claude-3-5-sonnet-latest";
        // 2025 -> the $3 row; 2026 -> the $2.5 row (the ledger has history).
        let p2025 = catalog
            .effective_price(m, TokenKind::Input, "2025-06-01T00:00:00Z")
            .unwrap();
        assert_eq!(p2025.unit_price_micros_per_mtok, 3_000_000);
        let p2026 = catalog
            .effective_price(m, TokenKind::Input, "2026-06-01T00:00:00Z")
            .unwrap();
        assert_eq!(p2026.unit_price_micros_per_mtok, 2_500_000);
        // before any price window -> none.
        assert!(catalog
            .effective_price(m, TokenKind::Input, "2024-01-01T00:00:00Z")
            .is_none());
        // a token kind with no price -> none.
        assert!(catalog
            .effective_price(m, TokenKind::Output, "2026-06-01T00:00:00Z")
            .is_none());
    }

    #[test]
    fn current_price_is_the_latest_open_ended_entry() {
        let catalog = sample();
        // sample() has a closed $3 row (2025) and an open-ended $2.5 row (2026).
        let current = catalog
            .current_price("claude-3-5-sonnet-latest", TokenKind::Input)
            .unwrap();
        assert_eq!(current.unit_price_micros_per_mtok, 2_500_000);
        // a token kind with no price -> none.
        assert!(catalog
            .current_price("claude-3-5-sonnet-latest", TokenKind::Output)
            .is_none());
    }

    #[test]
    fn model_capability_profile_overlays_context_and_modalities() {
        let model = Model {
            id: "m".into(),
            tenant_id: TenantId::SINGLE,
            provider_id: "p".into(),
            display_name: None,
            context_window: Some(128_000),
            modalities: vec![Modality::TextIn, Modality::VisionIn],
            capabilities: CapabilityProfile::default(),
            status: EntityStatus::Active,
            deprecated_at: None,
        };
        let caps = model.capability_profile();
        assert_eq!(caps.max_context_tokens, Some(128_000));
        assert!(caps.vision_in);
    }

    #[test]
    fn json_schema_supported_across_api_kinds() {
        // All three support structured output — OpenAI/Anthropic natively, Gemini
        // via the response_format → responseSchema downleveler.
        assert!(ApiKind::OpenAiCompatible.default_capabilities().json_schema);
        assert!(ApiKind::Anthropic.default_capabilities().json_schema);
        assert!(ApiKind::Gemini.default_capabilities().json_schema);
    }

    #[test]
    fn tenant_scope_defaults_to_single() {
        assert_eq!(TenantId::default(), TenantId::SINGLE);
        // round-trips through serde with the default applied.
        let json = r#"{"id":"p","name":"P","api_kind":"mock"}"#;
        let provider: Provider = serde_json::from_str(json).unwrap();
        assert_eq!(provider.tenant_id, TenantId::SINGLE);
        assert_eq!(provider.status, EntityStatus::Active);
    }
}
