use clap::Subcommand;
use sb_core::{FinishReason, Usage};
use serde::Serialize;

use crate::provider_preset::ProviderPreset;

#[derive(Subcommand)]
pub(crate) enum ProviderCmd {
    /// List provider presets and their default onboarding settings.
    Presets,
    /// Print provider readiness manifests for agents and operators.
    Readiness {
        /// Optional preset to print. Omit to list every manifest.
        preset: Option<ProviderPreset>,
    },
    /// Append or replace a provider entry. Secrets are referenced by env var only.
    Add {
        preset: ProviderPreset,
        /// Override the provider id written to config.
        #[arg(long)]
        id: Option<String>,
        /// Override the upstream base URL.
        #[arg(long)]
        base_url: Option<String>,
        /// Override the API-key env var name. Empty value is treated as no auth.
        #[arg(long)]
        api_key_env: Option<String>,
        /// Optional upstream model id to add as an exact route target.
        #[arg(long)]
        model: Option<String>,
        /// Optional inbound route/alias for --model. Defaults to provider/model.
        #[arg(long)]
        route: Option<String>,
        /// Replace an existing provider or exact route with the same id/alias.
        #[arg(long)]
        force: bool,
    },
    /// Execute a tiny request against one configured provider/model.
    Test {
        provider: String,
        /// Upstream model id to test. Defaults to the first discoverable model.
        #[arg(long)]
        model: Option<String>,
        /// Exercise the provider's streaming path.
        #[arg(long)]
        stream: bool,
    },
    /// List upstream models visible to one configured provider/account.
    Models { provider: String },
    /// Discover upstream models and add exact provider/model routes.
    SyncRoutes {
        provider: String,
        /// Optional local route prefix. Defaults to the provider id.
        #[arg(long)]
        prefix: Option<String>,
        /// Replace existing routes with the same local model id.
        #[arg(long)]
        force: bool,
    },
    /// Run model discovery, route preview, chat, stream, and embeddings checks.
    Doctor {
        provider: String,
        /// Upstream model id to test. Defaults to the first discoverable model.
        #[arg(long)]
        model: Option<String>,
    },
    /// Produce a stable end-to-end readiness report for one provider.
    Certify {
        provider: String,
        /// Upstream model id to certify. Defaults to model_hint or discovery.
        #[arg(long)]
        model: Option<String>,
    },
    /// Produce readiness certifications for every configured provider.
    CertifyAll {
        /// Skip providers whose required credential env vars are absent.
        #[arg(long)]
        skip_missing_env: bool,
    },
    /// Run provider doctor across every configured provider.
    Matrix,
}

#[derive(Debug)]
pub(crate) struct ProviderAddSummary {
    pub(crate) provider_id: String,
    pub(crate) api_key_env: Option<String>,
    pub(crate) route_model: Option<String>,
    pub(crate) target: Option<String>,
}

pub(crate) struct ProviderAddRequest {
    pub(crate) preset: ProviderPreset,
    pub(crate) id: Option<String>,
    pub(crate) base_url: Option<String>,
    pub(crate) api_key_env: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) route: Option<String>,
    pub(crate) force: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct ProviderTestSummary {
    pub(crate) ok: bool,
    pub(crate) revision: u64,
    pub(crate) provider_id: String,
    pub(crate) model: String,
    pub(crate) target: String,
    pub(crate) stream: bool,
    pub(crate) summary: String,
    pub(crate) output_chars: usize,
    pub(crate) event_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) response_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) finish_reason: Option<FinishReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) usage: Option<Usage>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ProviderModelSummary {
    pub(crate) id: String,
    pub(crate) switchback_model: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct ProviderModelsSummary {
    pub(crate) ok: bool,
    pub(crate) revision: u64,
    pub(crate) provider_id: String,
    pub(crate) models: Vec<ProviderModelSummary>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ProviderSyncRoutesSummary {
    pub(crate) ok: bool,
    pub(crate) provider_id: String,
    pub(crate) prefix: String,
    pub(crate) discovered: usize,
    pub(crate) added: usize,
    pub(crate) skipped: usize,
    pub(crate) replaced: usize,
}

#[derive(Debug, Serialize)]
pub(crate) struct ProviderDoctorCheck {
    pub(crate) name: String,
    pub(crate) ok: bool,
    pub(crate) required: bool,
    pub(crate) status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) detail: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ProviderDoctorSummary {
    pub(crate) ok: bool,
    pub(crate) revision: u64,
    pub(crate) provider_id: String,
    pub(crate) model: String,
    pub(crate) target: String,
    pub(crate) checks: Vec<ProviderDoctorCheck>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ProviderCertificationCounts {
    pub(crate) required_passed: usize,
    pub(crate) required_failed: usize,
    pub(crate) optional_passed: usize,
    pub(crate) optional_failed: usize,
    pub(crate) optional_unsupported: usize,
}

#[derive(Debug, Serialize)]
pub(crate) struct ProviderCertificationSummary {
    pub(crate) schema: &'static str,
    pub(crate) ok: bool,
    pub(crate) status: String,
    pub(crate) provider_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) revision: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) target: Option<String>,
    pub(crate) summary: ProviderCertificationCounts,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) verified_capabilities: Vec<String>,
    pub(crate) checks: Vec<ProviderDoctorCheck>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) missing_env: Vec<String>,
    pub(crate) recommendations: Vec<String>,
    pub(crate) next_commands: Vec<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ProviderCertifyAllSummary {
    pub(crate) schema: &'static str,
    pub(crate) ok: bool,
    pub(crate) total: usize,
    pub(crate) certified: usize,
    pub(crate) skipped: usize,
    pub(crate) blocked: usize,
    pub(crate) failed: usize,
    pub(crate) providers: Vec<ProviderCertificationSummary>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ProviderMatrixProviderSummary {
    pub(crate) provider_id: String,
    pub(crate) status: String,
    pub(crate) ok: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) missing_env: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) doctor: Option<ProviderDoctorSummary>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ProviderMatrixSummary {
    pub(crate) schema: &'static str,
    pub(crate) ok: bool,
    pub(crate) total: usize,
    pub(crate) checked: usize,
    pub(crate) skipped: usize,
    pub(crate) failed: usize,
    pub(crate) providers: Vec<ProviderMatrixProviderSummary>,
}
