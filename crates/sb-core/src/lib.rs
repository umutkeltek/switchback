//! `sb-core` — the canonical, provider-agnostic core of Switchback.
//!
//! INVARIANT: nothing in this crate may reference a provider wire format
//! (no `"choices"`, no `chat.completion`, no Anthropic `content_block`).
//! Provider shapes live in `sb-protocols` and the adapters, translated at
//! the edges. This crate is the hub every other crate depends on.

pub mod catalog;
pub mod config;
pub mod credential;
pub mod error;
pub mod execution;
pub mod ir;
pub mod load_settings;
pub mod model_settings;
pub mod optimize;
pub mod routing;
pub mod target;
pub mod workload;

pub use catalog::*;
pub use config::*;
pub use credential::*;
pub use error::*;
pub use execution::*;
pub use ir::*;
pub use load_settings::*;
pub use model_settings::*;
pub use optimize::*;
pub use routing::*;
pub use target::*;
pub use workload::*;

/// Generate a prefixed unique id, e.g. `req_3f9c1a...`.
pub fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", uuid::Uuid::new_v4().simple())
}
