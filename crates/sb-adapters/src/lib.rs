//! Concrete adapters.
//! - `mock`: deterministic, credential-free; the end-to-end test harness.
//! - `openai_compatible`: any OpenAI-shaped endpoint (OpenAI/OpenRouter/Ollama/vLLM).
//! - `anthropic`: the Anthropic Messages API (`/v1/messages`).
//! - `registry`: maps configured providers to adapter instances and leases.

pub mod anthropic;
pub mod mock;
pub mod openai_compatible;
pub mod registry;

pub use anthropic::AnthropicAdapter;
pub use mock::MockAdapter;
pub use openai_compatible::OpenAiCompatibleAdapter;
pub use registry::AdapterRegistry;
