use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub const NORMALIZATION_VERSION: &str = "provider-alias/v1";
pub const POLICY_VERSION: &str = "provider-reconciliation/v0";

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProviderAccountId(pub String);

impl std::fmt::Display for ProviderAccountId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProviderKey(pub String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderAccountAlias {
    OpenAiAccountUuid(String),
    OpenAiOrgId(String),
    CodexBarAccountKey(String),
    CodexMultiAuthAccountId(String),
    Email(String),
    Label(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AliasScheme {
    OpenAiAccountUuid,
    CodexBarAccountKey,
    CodexMultiAuthAccountId,
    OpenAiOrgId,
    Email,
    Label,
}

impl AliasScheme {
    pub fn rank(self) -> u8 {
        match self {
            Self::OpenAiAccountUuid => 1,
            Self::CodexBarAccountKey => 2,
            Self::CodexMultiAuthAccountId => 3,
            Self::OpenAiOrgId => 4,
            Self::Email => 5,
            Self::Label => 6,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiAccountUuid => "openai.account_uuid",
            Self::CodexBarAccountKey => "codexbar.account_key",
            Self::CodexMultiAuthAccountId => "codex_multi_auth.account_id",
            Self::OpenAiOrgId => "openai.org_id",
            Self::Email => "email",
            Self::Label => "label",
        }
    }
}

impl std::str::FromStr for AliasScheme {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "openai.account_uuid" | "account_uuid" | "uuid" => Ok(Self::OpenAiAccountUuid),
            "codexbar.account_key" | "account_key" => Ok(Self::CodexBarAccountKey),
            "codex_multi_auth.account_id" | "account_id" => Ok(Self::CodexMultiAuthAccountId),
            "openai.org_id" | "org_id" => Ok(Self::OpenAiOrgId),
            "email" => Ok(Self::Email),
            "label" => Ok(Self::Label),
            _ => Err(format!("unsupported alias scheme: {value}")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NormalizedAlias {
    pub scheme: AliasScheme,
    pub value: String,
    pub display: String,
    pub rank: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportSource {
    CodexActiveAuth,
    SwitchbackCodexRegistry,
    CodexMultiAuth,
    CodexMultiAuthQuota,
    CodexBar,
}
impl ImportSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CodexActiveAuth => "codex_active_auth",
            Self::SwitchbackCodexRegistry => "switchback_codex_registry",
            Self::CodexMultiAuth => "codex_multi_auth",
            Self::CodexMultiAuthQuota => "codex_multi_auth_quota",
            Self::CodexBar => "codex_bar",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AliasBindingState {
    Bound,
    ParkedConflict,
    Superseded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AliasBinding {
    pub scheme: AliasScheme,
    pub normalized_value: String,
    pub display: String,
    pub source: ImportSource,
    pub source_record_key: String,
    pub first_seen_at_ms: i64,
    pub last_seen_at_ms: i64,
    pub binding_state: AliasBindingState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnrollmentState {
    Enrolled,
    Parked,
    Retired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderAccountEnrollment {
    pub id: ProviderAccountId,
    pub provider: ProviderKey,
    pub state: EnrollmentState,
    pub aliases: Vec<AliasBinding>,
    pub created_revision: u64,
    pub updated_revision: u64,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub superseded_by: Option<ProviderAccountId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CredentialPointer {
    SwitchbackCodexRegistry {
        slot: String,
        json_pointer: NativeTokenPointer,
    },
    CodexActiveAuth {
        json_pointer: NativeTokenPointer,
    },
    ConfiguredAccount {
        provider_id: String,
        account_id: String,
    },
}

impl CredentialPointer {
    pub fn slot(&self) -> Option<&str> {
        match self {
            Self::SwitchbackCodexRegistry { slot, .. } => Some(slot),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeTokenPointer {
    AccessToken,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialKind {
    OAuth,
    ApiKey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialReadiness {
    Ready,
    Missing,
    Expired,
    Conflict,
    Stale,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialMetadata {
    pub account_id: ProviderAccountId,
    pub pointer: CredentialPointer,
    pub credential_kind: CredentialKind,
    pub access_present: bool,
    pub refresh_present: bool,
    pub id_token_present: bool,
    pub expires_at_ms: Option<i64>,
    pub source_revision: String,
    pub observed_at_ms: i64,
    pub readiness: CredentialReadiness,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Freshness {
    Fresh,
    Stale,
    Unknown,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeClientKind {
    Codex,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeSessionMode {
    Shared,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActiveAccountSource {
    CodexActiveAuth,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveRun {
    pub pid: u32,
    pub account_label: String,
    pub started: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveClientState {
    pub client: NativeClientKind,
    pub session_mode: NativeSessionMode,
    pub active_account: Option<ProviderAccountId>,
    pub active_source: ActiveAccountSource,
    pub active_runs: Vec<ActiveRun>,
    pub source_revision: String,
    pub observed_at_ms: i64,
    pub freshness: Freshness,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapacityWindowKind {
    Primary,
    Secondary,
    RequestsPerMinute,
    TokensPerMinute,
    ConcurrentSessions,
    ProviderDefined(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CapacityUsed {
    Percent { used_percent: f64 },
    Requests { used: u64, limit: Option<u64> },
    Tokens { used: u64, limit: Option<u64> },
    Concurrent { in_use: u32, limit: Option<u32> },
    Unknown,
}

impl Eq for CapacityUsed {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CapacityReset {
    At { resets_at_ms: i64 },
    Rolling,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapacityWindow {
    pub window_kind: CapacityWindowKind,
    pub window_minutes: Option<u32>,
    pub used: CapacityUsed,
    pub resets_at: CapacityReset,
    pub source: ImportSource,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountCapacityHealth {
    pub account_id: ProviderAccountId,
    pub model: Option<String>,
    pub windows: Vec<CapacityWindow>,
    pub sampled_at_ms: i64,
    pub ingested_at_ms: i64,
    pub source_revision: String,
    pub freshness: Freshness,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderAccountRevision {
    pub revision: u64,
    pub input_digest: String,
    pub normalization_version: String,
    pub policy_version: String,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceReadStatus {
    Ok,
    Missing,
    Malformed,
    UnsafePermissions,
    ConcurrentWrite,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportSourceStatus {
    pub source: ImportSource,
    pub fingerprint: Option<String>,
    pub status: SourceReadStatus,
    pub observed_at_ms: i64,
    pub last_good_revision: Option<u64>,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderAccountSnapshot {
    pub schema: String,
    pub revision: u64,
    pub freshness: Freshness,
    pub accounts: Vec<ProviderAccountEnrollment>,
    pub credentials: Vec<CredentialMetadata>,
    pub active_clients: Vec<ActiveClientState>,
    pub capacity: Vec<AccountCapacityHealth>,
    pub sources: Vec<ImportSourceStatus>,
    pub conflicts: Vec<String>,
    pub metadata_only: bool,
}

impl ProviderAccountSnapshot {
    pub fn empty() -> Self {
        Self {
            schema: "switchback/provider-account-snapshot@1".into(),
            revision: 0,
            freshness: Freshness::Unknown,
            accounts: vec![],
            credentials: vec![],
            active_clients: vec![],
            capacity: vec![],
            sources: vec![],
            conflicts: vec![],
            metadata_only: true,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SourcePaths {
    pub codex_auth: Option<PathBuf>,
    pub switchback_auth_registry: Option<PathBuf>,
    pub codex_multi_auth: Option<PathBuf>,
    pub quota_cache: Option<PathBuf>,
    pub codexbar_history: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct ReconcileRequest {
    pub sources: SourcePaths,
    pub dry_run: bool,
    pub now_ms: Option<i64>,
}
impl ReconcileRequest {
    pub fn apply(sources: SourcePaths) -> Self {
        Self {
            sources,
            dry_run: false,
            now_ms: None,
        }
    }
    pub fn dry_run(sources: SourcePaths) -> Self {
        Self {
            sources,
            dry_run: true,
            now_ms: None,
        }
    }
    pub fn with_now_ms(mut self, now_ms: i64) -> Self {
        self.now_ms = Some(now_ms);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconcileResult {
    pub schema: String,
    pub base_revision: u64,
    pub would_revision: u64,
    pub revision: u64,
    pub changed: bool,
    pub counts: ReconcileCounts,
    pub snapshot: ProviderAccountSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconcileCounts {
    pub accounts: usize,
    pub aliases: usize,
    pub credentials: usize,
    pub active_clients: usize,
    pub capacity_samples: usize,
    pub conflicts: usize,
    pub sources: usize,
}

impl ReconcileCounts {
    pub fn from_snapshot(snapshot: &ProviderAccountSnapshot) -> Self {
        Self {
            accounts: snapshot.accounts.len(),
            aliases: snapshot
                .accounts
                .iter()
                .map(|account| account.aliases.len())
                .sum(),
            credentials: snapshot.credentials.len(),
            active_clients: snapshot.active_clients.len(),
            capacity_samples: snapshot.capacity.len(),
            conflicts: snapshot.conflicts.len(),
            sources: snapshot.sources.len(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct AccountResolutionQuery {
    pub provider: String,
    pub client: String,
    pub alias_scheme: AliasScheme,
    pub alias_value: String,
    pub expected_revision: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountResolution {
    pub schema: String,
    pub authority_revision: u64,
    pub account_id: ProviderAccountId,
    pub credential_pointer: CredentialPointer,
    pub selection_reason: String,
    pub fresh: bool,
}

#[derive(Debug, Clone)]
pub enum AdjudicationCommand {
    Merge {
        from: ProviderAccountId,
        into: ProviderAccountId,
        expected_revision: u64,
    },
    Split {
        account: ProviderAccountId,
        binding: String,
        expected_revision: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdjudicationResult {
    pub schema: String,
    pub revision: u64,
    pub changed: bool,
    pub snapshot: ProviderAccountSnapshot,
}
