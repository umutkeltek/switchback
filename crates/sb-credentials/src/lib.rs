//! `sb-credentials` — the authentication & multi-account subsystem.
//!
//! SEPARATION OF CONCERNS (this is the whole point of the crate):
//! - `sb-router` decides *which target* (provider/model).
//! - `sb-credentials` (here) decides *which account* of that provider to use,
//!   holds the secret material as redacting leases, and tracks per-(account,
//!   model) availability so a rate-limited account is skipped, not the whole
//!   provider.
//! - `sb-adapters` *executes* with the lease it is handed.
//! - `sb-server` *orchestrates* the two-level fallback (account, then target).
//!
//! Nothing here knows about HTTP, routing, or wire formats. It depends on
//! `sb-core` only. That boundary is what keeps credential bugs contained.

pub mod account;
pub mod availability;
pub mod breaker;
pub mod refresh;
pub mod resolver;
pub mod service_account;
pub mod vault;

pub use account::{Account, AccountId, ResolvedAuth};
pub use availability::Availability;
pub use refresh::RefreshCoordinator;
pub use resolver::{CredentialResolver, ResolveOutcome};
pub use vault::Vault;
