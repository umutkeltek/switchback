mod admission;
mod app;
mod auth;
mod cli;
mod config_cli;
mod controlplane;
mod cp;
mod doctor_cli;
mod handlers;
mod http_response;
mod idempotency;
mod lease;
mod mcp_cli;
mod otel;
mod provider_cli;
mod provider_preset;
mod schema_cli;
mod serve;
mod sse;
mod state;
mod tenancy;
mod vault_cli;

#[cfg(test)]
mod tests;

pub use app::build_app;
pub use state::AppState;

pub(crate) use cli::print_json;
pub(crate) use serve::{engine_from_config, route_preview_json};

pub fn run() -> anyhow::Result<()> {
    cli::run()
}
