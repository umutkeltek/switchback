//! Concrete adapters.
//! - `mock`: deterministic, credential-free; the end-to-end test harness.
//! - `codec` + `composed`: the generic `ComposedAdapter` (WireCodec × AuthScheme)
//!   that serves the OpenAI-compatible / Anthropic / Gemini wire formats from one
//!   execute loop. Adding a wire format is a `WireCodec` impl, not a new adapter.
//! - `registry`: maps configured providers to adapter instances and leases.

pub mod codec;
pub mod composed;
pub mod egress;
pub mod event_stream;
pub mod latency;
pub mod mock;
pub mod registry;
pub mod signer;
pub mod sigv4;
pub mod transport;

pub use codec::{
    AnthropicCodec, BedrockCodec, ClaudeCodeNativeRelayCodec, GeminiCodec, OpenAiCodec,
    StreamDecoder, VertexCodec, WireCodec,
};
pub use composed::ComposedAdapter;
pub use egress::EgressPool;
pub use latency::LatencyTracker;
pub use mock::MockAdapter;
pub use registry::AdapterRegistry;
pub use signer::{RequestSigner, SchemeSigner, SigV4Signer};
pub use transport::{EventStreamTransport, HttpTransport, Transport};
