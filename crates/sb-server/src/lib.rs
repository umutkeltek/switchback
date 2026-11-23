mod activator;
mod admission;
mod alerts;
mod app;
mod artifact_store;
mod auth;
mod body_audit;
mod cli;
mod config_cli;
mod controlplane;
mod cp;
mod doctor_cli;
mod engine_control;
mod engine_truth;
mod eval_cli;
mod fal_probe;
mod forward_proxy;
mod forward_proxy_capture;
mod handlers;
mod holds;
mod http_capture;
mod http_response;
mod idempotency;
mod lane_cli;
mod lane_profile_cli;
mod lane_slots;
mod lease;
mod local_probe;
mod mcp_cli;
mod native_cli;
mod native_history_cli;
mod optimize;
mod otel;
mod provider_cli;
mod provider_preset;
mod schema_cli;
mod scheme_auth;
mod serve;
mod setup_cli;
mod sse;
mod state;
mod tap;
mod tenancy;
mod vault_cli;
mod workloads;

#[cfg(test)]
mod test_temp;
#[cfg(test)]
mod tests;

pub use app::build_app;
pub use state::AppState;

pub(crate) use cli::print_json;
pub(crate) use serve::{engine_from_config, route_preview_json};

pub fn run() -> anyhow::Result<()> {
    cli::run()
}
