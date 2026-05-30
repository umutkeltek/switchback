//! The "wire codec" half of the `AuthScheme × WireCodec` decomposition. A
//! [`WireCodec`] captures everything that differs *at the wire* between
//! providers — URL shape, fixed headers, request-body translation, response
//! parsing, and the streaming decoder — so the generic [`crate::ComposedAdapter`]
//! can run the one execute loop for all of them. New wire formats become a
//! codec impl (mostly delegating to `sb-protocols`), not a whole adapter.

use sb_core::{AiRequest, AiResponse, AiStreamEvent};
use serde_json::Value;

/// A fresh, stateful decoder for one streamed response: each SSE `data:` frame's
/// JSON is fed to `decode`; `finish` flushes any terminal events.
pub trait StreamDecoder: Send {
    fn decode(&mut self, frame: &Value) -> Vec<AiStreamEvent>;
    fn finish(&mut self) -> Vec<AiStreamEvent>;
}

/// Everything provider-specific about a wire format. Composed with an
/// [`sb_core::AuthScheme`] by [`crate::ComposedAdapter`].
pub trait WireCodec: Send + Sync {
    fn id(&self) -> &'static str;

    /// Upstream URL for a model + stream flag.
    fn url(&self, base_url: &str, model: &str, stream: bool) -> String;

    /// Fixed headers the codec always sends (e.g. `anthropic-version`).
    fn headers(&self) -> Vec<(&'static str, &'static str)> {
        Vec::new()
    }

    /// Canonical request -> upstream wire body.
    fn request_body(&self, req: &AiRequest, model: &str, stream: bool) -> Value;

    /// Parse a non-streaming upstream response -> canonical.
    fn parse_response(&self, body: &Value) -> Result<AiResponse, String>;

    /// A fresh stateful stream decoder for one request (`model` is a fallback
    /// for formats whose stream omits it, e.g. Gemini).
    fn decoder(&self, model: &str) -> Box<dyn StreamDecoder>;

    /// Embeddings endpoint URL, if this wire format supports embeddings.
    fn embeddings_url(&self, _base_url: &str) -> Option<String> {
        None
    }
}

// --- OpenAI Chat Completions ------------------------------------------------

pub struct OpenAiCodec;

struct OpenAiDecoder(sb_protocols::openai::OpenAiStreamDecoder);
impl StreamDecoder for OpenAiDecoder {
    fn decode(&mut self, frame: &Value) -> Vec<AiStreamEvent> {
        self.0.decode(frame)
    }
    fn finish(&mut self) -> Vec<AiStreamEvent> {
        self.0.finish()
    }
}

impl WireCodec for OpenAiCodec {
    fn id(&self) -> &'static str {
        "openai_compatible"
    }
    fn url(&self, base_url: &str, _model: &str, _stream: bool) -> String {
        format!("{}/chat/completions", base_url.trim_end_matches('/'))
    }
    fn request_body(&self, req: &AiRequest, model: &str, stream: bool) -> Value {
        sb_protocols::openai::request_to_openai_wire(req, model, stream)
    }
    fn parse_response(&self, body: &Value) -> Result<AiResponse, String> {
        sb_protocols::openai::parse_openai_chat_response(body)
    }
    fn decoder(&self, _model: &str) -> Box<dyn StreamDecoder> {
        Box::new(OpenAiDecoder(sb_protocols::openai::OpenAiStreamDecoder::new()))
    }
    fn embeddings_url(&self, base_url: &str) -> Option<String> {
        Some(format!("{}/embeddings", base_url.trim_end_matches('/')))
    }
}

// --- Anthropic Messages -----------------------------------------------------

pub struct AnthropicCodec;

struct AnthropicDecoder(sb_protocols::anthropic::AnthropicStreamDecoder);
impl StreamDecoder for AnthropicDecoder {
    fn decode(&mut self, frame: &Value) -> Vec<AiStreamEvent> {
        self.0.decode(frame)
    }
    fn finish(&mut self) -> Vec<AiStreamEvent> {
        self.0.finish()
    }
}

impl WireCodec for AnthropicCodec {
    fn id(&self) -> &'static str {
        "anthropic"
    }
    fn url(&self, base_url: &str, _model: &str, _stream: bool) -> String {
        format!("{}/v1/messages", base_url.trim_end_matches('/'))
    }
    fn headers(&self) -> Vec<(&'static str, &'static str)> {
        vec![(
            "anthropic-version",
            sb_protocols::anthropic::ANTHROPIC_VERSION,
        )]
    }
    fn request_body(&self, req: &AiRequest, model: &str, stream: bool) -> Value {
        sb_protocols::anthropic::request_to_anthropic_wire(req, model, stream)
    }
    fn parse_response(&self, body: &Value) -> Result<AiResponse, String> {
        sb_protocols::anthropic::parse_anthropic_response(body)
    }
    fn decoder(&self, _model: &str) -> Box<dyn StreamDecoder> {
        Box::new(AnthropicDecoder(
            sb_protocols::anthropic::AnthropicStreamDecoder::new(),
        ))
    }
}

// --- Google Gemini GenerateContent ------------------------------------------

pub struct GeminiCodec;

struct GeminiDecoder(sb_protocols::gemini::GeminiStreamDecoder);
impl StreamDecoder for GeminiDecoder {
    fn decode(&mut self, frame: &Value) -> Vec<AiStreamEvent> {
        self.0.decode(frame)
    }
    fn finish(&mut self) -> Vec<AiStreamEvent> {
        self.0.finish()
    }
}

impl WireCodec for GeminiCodec {
    fn id(&self) -> &'static str {
        "gemini"
    }
    fn url(&self, base_url: &str, model: &str, stream: bool) -> String {
        let method = if stream {
            "streamGenerateContent?alt=sse"
        } else {
            "generateContent"
        };
        format!(
            "{}/v1beta/models/{model}:{method}",
            base_url.trim_end_matches('/')
        )
    }
    fn request_body(&self, req: &AiRequest, _model: &str, _stream: bool) -> Value {
        // Gemini carries the model in the URL and the stream flag in the method.
        sb_protocols::gemini::request_to_gemini_wire(req)
    }
    fn parse_response(&self, body: &Value) -> Result<AiResponse, String> {
        sb_protocols::gemini::parse_gemini_response(body)
    }
    fn decoder(&self, model: &str) -> Box<dyn StreamDecoder> {
        Box::new(GeminiDecoder(sb_protocols::gemini::GeminiStreamDecoder::new(
            model,
        )))
    }
}

// --- Google Vertex AI (Gemini wire, GCP project endpoint) -------------------

/// Vertex speaks the same GenerateContent wire as Gemini, on a project/region
/// URL, authenticated with an OAuth Bearer token. So it's the Gemini codec with
/// a different URL — a new cloud provider as (mostly) data on the seam.
pub struct VertexCodec {
    project: String,
    region: String,
}

impl VertexCodec {
    pub fn new(project: String, region: String) -> Self {
        Self { project, region }
    }
}

impl WireCodec for VertexCodec {
    fn id(&self) -> &'static str {
        "vertex"
    }
    fn url(&self, base_url: &str, model: &str, stream: bool) -> String {
        let method = if stream {
            "streamGenerateContent?alt=sse"
        } else {
            "generateContent"
        };
        format!(
            "{}/v1/projects/{}/locations/{}/publishers/google/models/{model}:{method}",
            base_url.trim_end_matches('/'),
            self.project,
            self.region,
        )
    }
    fn request_body(&self, req: &AiRequest, _model: &str, _stream: bool) -> Value {
        sb_protocols::gemini::request_to_gemini_wire(req)
    }
    fn parse_response(&self, body: &Value) -> Result<AiResponse, String> {
        sb_protocols::gemini::parse_gemini_response(body)
    }
    fn decoder(&self, model: &str) -> Box<dyn StreamDecoder> {
        Box::new(GeminiDecoder(sb_protocols::gemini::GeminiStreamDecoder::new(
            model,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vertex_url_is_project_scoped() {
        let codec = VertexCodec::new("my-proj".into(), "us-central1".into());
        assert_eq!(
            codec.url(
                "https://us-central1-aiplatform.googleapis.com",
                "gemini-2.0-flash",
                false
            ),
            "https://us-central1-aiplatform.googleapis.com/v1/projects/my-proj/locations/us-central1/publishers/google/models/gemini-2.0-flash:generateContent"
        );
    }

    #[test]
    fn codec_urls_match_each_wire_format() {
        assert_eq!(
            OpenAiCodec.url("https://x/v1", "gpt-4o", true),
            "https://x/v1/chat/completions"
        );
        assert_eq!(
            AnthropicCodec.url("https://api.anthropic.com", "claude", false),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            GeminiCodec.url("https://g", "gemini-2.0-flash", true),
            "https://g/v1beta/models/gemini-2.0-flash:streamGenerateContent?alt=sse"
        );
        assert_eq!(
            GeminiCodec.url("https://g", "gemini-2.0-flash", false),
            "https://g/v1beta/models/gemini-2.0-flash:generateContent"
        );
        // anthropic carries a fixed version header; only openai does embeddings.
        assert!(!AnthropicCodec.headers().is_empty());
        assert!(OpenAiCodec.embeddings_url("https://x/v1").is_some());
        assert!(GeminiCodec.embeddings_url("https://g").is_none());
    }
}
