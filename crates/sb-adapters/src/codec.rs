//! The "wire codec" half of the `AuthScheme × WireCodec` decomposition. A
//! [`WireCodec`] captures everything that differs *at the wire* between
//! providers — URL shape, fixed headers, request-body translation, response
//! parsing, and the streaming decoder — so the generic [`crate::ComposedAdapter`]
//! can run the one execute loop for all of them. New wire formats become a
//! codec impl (mostly delegating to `sb-protocols`), not a whole adapter.

use sb_core::{AiRequest, AiResponse, AiStreamEvent, ToolCallStart};
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
    fn request_body(&self, req: &AiRequest, model: &str, stream: bool) -> Result<Value, String>;

    /// Metadata-only warnings predictable from request translation alone.
    fn request_warnings(&self, _req: &AiRequest, _model: &str) -> Vec<String> {
        Vec::new()
    }

    /// Whether the upstream request must be streamed even if the inbound client
    /// asked for a collected response. The adapter always returns canonical
    /// events, so the runtime can still collect them for non-stream clients.
    fn upstream_stream(&self, requested_stream: bool) -> bool {
        requested_stream
    }

    /// Parse a non-streaming upstream response -> canonical.
    fn parse_response(&self, body: &Value) -> Result<AiResponse, String>;

    /// A fresh stateful stream decoder for one request (`model` is a fallback
    /// for formats whose stream omits it, e.g. Gemini).
    fn decoder(&self, model: &str) -> Box<dyn StreamDecoder>;

    /// Embeddings endpoint URL, if this wire format supports embeddings.
    fn embeddings_url(&self, _base_url: &str) -> Option<String> {
        None
    }

    /// Model-list endpoint URL, if this wire format supports discovery.
    fn models_url(&self, _base_url: &str) -> Option<String> {
        None
    }

    /// Parse a model-list response into upstream model ids.
    fn parse_models_response(&self, _body: &Value) -> Result<Vec<String>, String> {
        Err("model listing not supported by this wire format".to_string())
    }
}

fn string_field_array(
    body: &Value,
    array_key: &str,
    field_key: &str,
) -> Result<Vec<String>, String> {
    let Some(items) = body.get(array_key).and_then(Value::as_array) else {
        return Err(format!("models response missing `{array_key}` array"));
    };
    Ok(items
        .iter()
        .filter_map(|item| item.get(field_key).and_then(Value::as_str))
        .map(ToString::to_string)
        .collect())
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
    fn request_body(&self, req: &AiRequest, model: &str, stream: bool) -> Result<Value, String> {
        sb_protocols::openai::request_to_openai_wire(req, model, stream)
    }
    fn parse_response(&self, body: &Value) -> Result<AiResponse, String> {
        sb_protocols::openai::parse_openai_chat_response(body)
    }
    fn decoder(&self, _model: &str) -> Box<dyn StreamDecoder> {
        Box::new(OpenAiDecoder(
            sb_protocols::openai::OpenAiStreamDecoder::new(),
        ))
    }
    fn embeddings_url(&self, base_url: &str) -> Option<String> {
        Some(format!("{}/embeddings", base_url.trim_end_matches('/')))
    }
    fn models_url(&self, base_url: &str) -> Option<String> {
        Some(format!("{}/models", base_url.trim_end_matches('/')))
    }
    fn parse_models_response(&self, body: &Value) -> Result<Vec<String>, String> {
        string_field_array(body, "data", "id")
    }
}

// --- OpenAI Responses / Codex native relay ---------------------------------

pub struct OpenAiResponsesCodec {
    id: &'static str,
}

impl OpenAiResponsesCodec {
    pub fn codex_native_relay() -> Self {
        Self {
            id: "codex_native_relay",
        }
    }

    fn is_codex_native_relay(&self) -> bool {
        self.id == "codex_native_relay"
    }
}

struct OpenAiResponsesDecoder {
    model: String,
    /// `output_index` values already opened as a `function_call` item, so a
    /// repeated `output_item.added` (or a late `arguments.done`) does not emit a
    /// second `ToolCallStart`. Mirrors `OpenAiStreamDecoder::seen_tool_calls`.
    tool_indices: std::collections::HashSet<u32>,
    /// `output_index` values that have already streamed at least one argument
    /// fragment, so the terminal `arguments.done` only re-emits the full
    /// `arguments` for backends that skip incremental deltas.
    tool_args_streamed: std::collections::HashSet<u32>,
}

/// `output_index` of the current streamed item, used as the canonical tool-call
/// index so `ToolCallStart`/`ArgsDelta`/`End` agree (every Responses output item
/// has a stable, unique `output_index`).
fn responses_output_index(frame: &Value) -> u32 {
    frame
        .get("output_index")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(0)
}

impl StreamDecoder for OpenAiResponsesDecoder {
    fn decode(&mut self, frame: &Value) -> Vec<AiStreamEvent> {
        let mut out = Vec::new();
        match frame.get("type").and_then(Value::as_str) {
            Some("response.created") => {
                let id = frame
                    .pointer("/response/id")
                    .and_then(Value::as_str)
                    .unwrap_or("resp")
                    .to_string();
                let model = frame
                    .pointer("/response/model")
                    .and_then(Value::as_str)
                    .unwrap_or(&self.model)
                    .to_string();
                out.push(AiStreamEvent::MessageStart { id, model });
            }
            Some("response.output_text.delta") => {
                if let Some(text) = frame.get("delta").and_then(Value::as_str) {
                    out.push(AiStreamEvent::TextDelta {
                        text: text.to_string(),
                    });
                }
            }
            // Reasoning summary deltas (gpt-5.x thinking) -> canonical reasoning.
            Some("response.reasoning_summary_text.delta" | "response.reasoning_text.delta") => {
                if let Some(text) = frame.get("delta").and_then(Value::as_str) {
                    out.push(AiStreamEvent::ReasoningDelta {
                        text: text.to_string(),
                    });
                }
            }
            // A new output item opened. Only `function_call` items start a tool
            // call; `message`/`reasoning` items stream their content via the
            // dedicated text/reasoning delta events handled above.
            Some("response.output_item.added") => {
                if frame.pointer("/item/type").and_then(Value::as_str) == Some("function_call") {
                    let index = responses_output_index(frame);
                    if self.tool_indices.insert(index) {
                        let id = frame
                            .pointer("/item/call_id")
                            .or_else(|| frame.pointer("/item/id"))
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let name = frame
                            .pointer("/item/name")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        out.push(AiStreamEvent::ToolCallStart(ToolCallStart { index, id, name }));
                        // Some backends inline the opening arguments fragment.
                        if let Some(args) = frame.pointer("/item/arguments").and_then(Value::as_str) {
                            if !args.is_empty() {
                                self.tool_args_streamed.insert(index);
                                out.push(AiStreamEvent::ToolCallArgsDelta {
                                    index,
                                    json: args.to_string(),
                                });
                            }
                        }
                    }
                }
            }
            Some("response.function_call_arguments.delta") => {
                if let Some(delta) = frame.get("delta").and_then(Value::as_str) {
                    let index = responses_output_index(frame);
                    self.tool_args_streamed.insert(index);
                    out.push(AiStreamEvent::ToolCallArgsDelta {
                        index,
                        json: delta.to_string(),
                    });
                }
            }
            Some("response.function_call_arguments.done") => {
                let index = responses_output_index(frame);
                if self.tool_indices.contains(&index) {
                    // Backends that skip incremental deltas carry the full
                    // arguments only here; re-emit them once so the tool call is
                    // never empty.
                    if !self.tool_args_streamed.contains(&index) {
                        if let Some(args) = frame.get("arguments").and_then(Value::as_str) {
                            if !args.is_empty() {
                                out.push(AiStreamEvent::ToolCallArgsDelta {
                                    index,
                                    json: args.to_string(),
                                });
                            }
                        }
                    }
                    out.push(AiStreamEvent::ToolCallEnd { index });
                }
            }
            Some("response.completed") => {
                let usage = frame
                    .get("response")
                    .and_then(|response| response.get("usage"))
                    .map(|usage| serde_json::json!({ "usage": usage }))
                    .and_then(|wrapped| {
                        sb_protocols::responses::parse_openai_responses_response(
                            &serde_json::json!({
                                "id": "resp",
                                "model": self.model,
                                "status": "completed",
                                "output": [],
                                "usage": wrapped.get("usage").cloned().unwrap_or(Value::Null),
                            }),
                        )
                        .ok()
                    })
                    .map(|response| response.usage)
                    .unwrap_or_default();
                out.push(AiStreamEvent::UsageDelta { usage });
                out.push(AiStreamEvent::MessageEnd {
                    finish_reason: sb_core::FinishReason::Stop,
                });
            }
            Some("response.failed") => {
                let message = frame
                    .pointer("/response/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("Responses stream failed")
                    .to_string();
                out.push(AiStreamEvent::Error {
                    message,
                    class: sb_core::ErrorClass::ServerError,
                });
            }
            // A URL citation/annotation attached to the output text (web search).
            Some("response.output_text.annotation.added") => {
                if let Some(url) = frame.pointer("/annotation/url").and_then(Value::as_str) {
                    out.push(AiStreamEvent::Citation {
                        url: url.to_string(),
                        title: frame
                            .pointer("/annotation/title")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                    });
                }
            }
            // A completed model-generated image (`result` carries the base64).
            Some("response.image_generation_call.completed") => {
                if let Some(data) = frame
                    .get("result")
                    .or_else(|| frame.pointer("/item/result"))
                    .and_then(Value::as_str)
                {
                    out.push(AiStreamEvent::OutputImage {
                        media_type: frame
                            .get("output_format")
                            .and_then(Value::as_str)
                            .unwrap_or("image/png")
                            .to_string(),
                        data: data.to_string(),
                    });
                }
            }
            // Provider-run server-tool lifecycle: response.<name>_call.<status>
            // for web_search / code_interpreter / file_search.
            Some(t)
                if t.starts_with("response.")
                    && t.contains("_call.")
                    && (t.contains("web_search")
                        || t.contains("code_interpreter")
                        || t.contains("file_search")) =>
            {
                if let Some((name, status)) = t
                    .strip_prefix("response.")
                    .and_then(|rest| rest.split_once("_call."))
                {
                    out.push(AiStreamEvent::ServerToolCall {
                        id: frame
                            .get("item_id")
                            .or_else(|| frame.pointer("/item/id"))
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        name: name.to_string(),
                        status: status.to_string(),
                    });
                }
            }
            _ => {}
        }
        out
    }

    fn finish(&mut self) -> Vec<AiStreamEvent> {
        Vec::new()
    }
}

impl WireCodec for OpenAiResponsesCodec {
    fn id(&self) -> &'static str {
        self.id
    }
    fn url(&self, base_url: &str, _model: &str, _stream: bool) -> String {
        format!("{}/responses", base_url.trim_end_matches('/'))
    }
    fn headers(&self) -> Vec<(&'static str, &'static str)> {
        vec![
            ("openai-beta", "responses=experimental"),
            ("x-responsesapi-include-timing-metrics", "true"),
        ]
    }
    fn request_body(&self, req: &AiRequest, model: &str, stream: bool) -> Result<Value, String> {
        let mut body = sb_protocols::responses::request_to_openai_responses_wire(
            req,
            model,
            self.upstream_stream(stream),
        )?;
        if self.is_codex_native_relay() {
            if let Some(map) = body.as_object_mut() {
                map.insert("store".to_string(), Value::Bool(false));
                map.remove("max_output_tokens");
                let needs_instructions = map
                    .get("instructions")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .unwrap_or_default()
                    .is_empty();
                if needs_instructions {
                    map.insert(
                        "instructions".to_string(),
                        Value::String("You are Codex, a helpful coding assistant.".to_string()),
                    );
                }
            }
        }
        Ok(body)
    }
    fn upstream_stream(&self, requested_stream: bool) -> bool {
        if self.is_codex_native_relay() {
            true
        } else {
            requested_stream
        }
    }
    fn parse_response(&self, body: &Value) -> Result<AiResponse, String> {
        sb_protocols::responses::parse_openai_responses_response(body)
    }
    fn decoder(&self, model: &str) -> Box<dyn StreamDecoder> {
        Box::new(OpenAiResponsesDecoder {
            model: model.to_string(),
            tool_indices: std::collections::HashSet::new(),
            tool_args_streamed: std::collections::HashSet::new(),
        })
    }
    fn models_url(&self, base_url: &str) -> Option<String> {
        Some(format!("{}/models", base_url.trim_end_matches('/')))
    }
    fn parse_models_response(&self, body: &Value) -> Result<Vec<String>, String> {
        string_field_array(body, "data", "id")
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
    fn request_body(&self, req: &AiRequest, model: &str, stream: bool) -> Result<Value, String> {
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
    fn models_url(&self, base_url: &str) -> Option<String> {
        Some(format!("{}/v1/models", base_url.trim_end_matches('/')))
    }
    fn parse_models_response(&self, body: &Value) -> Result<Vec<String>, String> {
        string_field_array(body, "data", "id")
    }
}

// --- Claude Code first-party relay (Anthropic wire + native attribution) ----

/// Claude Code's subscription relay still speaks Anthropic Messages at the wire
/// level. The native part is the account source (`claude_code_oauth`) plus the
/// first-party attribution header the client sends with those requests.
pub struct ClaudeCodeNativeRelayCodec;

impl WireCodec for ClaudeCodeNativeRelayCodec {
    fn id(&self) -> &'static str {
        "claude_code_native_relay"
    }
    fn url(&self, base_url: &str, model: &str, stream: bool) -> String {
        AnthropicCodec.url(base_url, model, stream)
    }
    fn headers(&self) -> Vec<(&'static str, &'static str)> {
        vec![
            (
                "anthropic-version",
                sb_protocols::anthropic::ANTHROPIC_VERSION,
            ),
            (
                "x-anthropic-billing-header",
                "cc_version=switchback; cc_entrypoint=switchback-native-relay; cch=00000;",
            ),
        ]
    }
    fn request_body(&self, req: &AiRequest, model: &str, stream: bool) -> Result<Value, String> {
        AnthropicCodec.request_body(req, model, stream)
    }
    fn parse_response(&self, body: &Value) -> Result<AiResponse, String> {
        AnthropicCodec.parse_response(body)
    }
    fn decoder(&self, model: &str) -> Box<dyn StreamDecoder> {
        AnthropicCodec.decoder(model)
    }
    fn models_url(&self, base_url: &str) -> Option<String> {
        AnthropicCodec.models_url(base_url)
    }
    fn parse_models_response(&self, body: &Value) -> Result<Vec<String>, String> {
        AnthropicCodec.parse_models_response(body)
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
    fn request_body(&self, req: &AiRequest, _model: &str, _stream: bool) -> Result<Value, String> {
        // Gemini carries the model in the URL and the stream flag in the method.
        sb_protocols::gemini::request_to_gemini_wire(req)
    }
    fn request_warnings(&self, req: &AiRequest, _model: &str) -> Vec<String> {
        sb_protocols::gemini::schema_downlevel_warnings(req)
            .into_iter()
            .map(|warning| warning.to_string())
            .collect()
    }
    fn parse_response(&self, body: &Value) -> Result<AiResponse, String> {
        sb_protocols::gemini::parse_gemini_response(body)
    }
    fn decoder(&self, model: &str) -> Box<dyn StreamDecoder> {
        Box::new(GeminiDecoder(
            sb_protocols::gemini::GeminiStreamDecoder::new(model),
        ))
    }
    fn models_url(&self, base_url: &str) -> Option<String> {
        Some(format!("{}/v1beta/models", base_url.trim_end_matches('/')))
    }
    fn parse_models_response(&self, body: &Value) -> Result<Vec<String>, String> {
        let Some(items) = body.get("models").and_then(Value::as_array) else {
            return Err("models response missing `models` array".to_string());
        };
        Ok(items
            .iter()
            .filter(|item| {
                let methods = item
                    .get("supportedGenerationMethods")
                    .or_else(|| item.get("supported_actions"))
                    .and_then(Value::as_array);
                methods
                    .map(|methods| {
                        methods
                            .iter()
                            .filter_map(Value::as_str)
                            .any(|method| method == "generateContent")
                    })
                    .unwrap_or(true)
            })
            .filter_map(|item| {
                item.get("baseModelId")
                    .and_then(Value::as_str)
                    .or_else(|| item.get("name").and_then(Value::as_str))
            })
            .map(|id| id.strip_prefix("models/").unwrap_or(id).to_string())
            .collect())
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
    fn request_body(&self, req: &AiRequest, _model: &str, _stream: bool) -> Result<Value, String> {
        sb_protocols::gemini::request_to_gemini_wire(req)
    }
    fn request_warnings(&self, req: &AiRequest, _model: &str) -> Vec<String> {
        sb_protocols::gemini::schema_downlevel_warnings(req)
            .into_iter()
            .map(|warning| warning.to_string())
            .collect()
    }
    fn parse_response(&self, body: &Value) -> Result<AiResponse, String> {
        sb_protocols::gemini::parse_gemini_response(body)
    }
    fn decoder(&self, model: &str) -> Box<dyn StreamDecoder> {
        Box::new(GeminiDecoder(
            sb_protocols::gemini::GeminiStreamDecoder::new(model),
        ))
    }
}

// --- AWS Bedrock (Anthropic Messages wire on the Bedrock runtime) -----------

/// Bedrock speaks the Anthropic Messages wire, but: the model + stream action go
/// in the URL (`/model/{id}/{invoke|invoke-with-response-stream}`), and the
/// `anthropic_version` rides in the BODY, not a header. Composed with a
/// [`crate::signer::SigV4Signer`] + [`crate::transport::EventStreamTransport`],
/// this is a codec — not the bespoke adapter it used to need.
pub struct BedrockCodec;

impl WireCodec for BedrockCodec {
    fn id(&self) -> &'static str {
        "bedrock"
    }
    fn url(&self, base_url: &str, model: &str, stream: bool) -> String {
        let action = if stream {
            "invoke-with-response-stream"
        } else {
            "invoke"
        };
        format!(
            "{}/model/{}/{}",
            base_url.trim_end_matches('/'),
            percent_encode_segment(model),
            action
        )
    }
    fn request_body(&self, req: &AiRequest, model: &str, _stream: bool) -> Result<Value, String> {
        // Anthropic body minus `model`/`stream` (both live in the URL) plus the
        // Bedrock `anthropic_version`.
        let mut body = sb_protocols::anthropic::request_to_anthropic_wire(req, model, false)?;
        if let Value::Object(map) = &mut body {
            map.remove("model");
            map.remove("stream");
            map.insert(
                "anthropic_version".to_string(),
                Value::String("bedrock-2023-05-31".to_string()),
            );
        }
        Ok(body)
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

/// Percent-encode a URL path segment (Bedrock model ids contain `:` etc.). The
/// same encoding is used for the request URL and the SigV4 canonical path.
fn percent_encode_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
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
            OpenAiResponsesCodec::codex_native_relay().url(
                "https://chatgpt.com/backend-api/codex",
                "gpt-5.5",
                false
            ),
            "https://chatgpt.com/backend-api/codex/responses"
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
        assert_eq!(
            ClaudeCodeNativeRelayCodec.url("https://api.anthropic.com", "claude", false),
            "https://api.anthropic.com/v1/messages"
        );
        assert!(OpenAiCodec.embeddings_url("https://x/v1").is_some());
        assert!(GeminiCodec.embeddings_url("https://g").is_none());
    }

    #[test]
    fn claude_code_native_relay_adds_native_attribution_header() {
        let headers = ClaudeCodeNativeRelayCodec.headers();
        assert!(headers.iter().any(|(k, _)| *k == "anthropic-version"));
        assert!(headers.iter().any(|(k, v)| {
            *k == "x-anthropic-billing-header"
                && v.contains("cc_entrypoint=switchback-native-relay")
        }));
    }

    #[test]
    fn codex_native_relay_sets_chatgpt_backend_required_shape() {
        let codec = OpenAiResponsesCodec::codex_native_relay();
        let mut req = AiRequest::new("client-model", vec![sb_core::Message::user("hi")]);
        req.max_output_tokens = Some(16);

        let body = codec.request_body(&req, "gpt-5.5", false).unwrap();

        assert_eq!(body["model"], "gpt-5.5");
        assert_eq!(body["stream"], true);
        assert_eq!(body["store"], false);
        assert!(body.get("max_output_tokens").is_none());
        assert_eq!(
            body["instructions"],
            "You are Codex, a helpful coding assistant."
        );
        assert!(codec.upstream_stream(false));
    }

    #[test]
    fn responses_decoder_emits_tool_calls_and_reasoning() {
        let codec = OpenAiResponsesCodec::codex_native_relay();
        let mut decoder = codec.decoder("gpt-5.5");

        // The frame sequence the real Codex/Responses backend streams for a
        // single function call, interleaved with a reasoning summary delta.
        let frames = [
            serde_json::json!({"type":"response.created","response":{"id":"resp_1","model":"gpt-5.5"}}),
            serde_json::json!({"type":"response.reasoning_summary_text.delta","delta":"thinking"}),
            serde_json::json!({"type":"response.output_item.added","output_index":1,
                "item":{"type":"function_call","id":"fc_1","call_id":"call_abc","name":"get_weather","arguments":""}}),
            serde_json::json!({"type":"response.function_call_arguments.delta","output_index":1,"delta":"{\"city\":"}),
            serde_json::json!({"type":"response.function_call_arguments.delta","output_index":1,"delta":"\"Istanbul\"}"}),
            serde_json::json!({"type":"response.function_call_arguments.done","output_index":1,
                "arguments":"{\"city\":\"Istanbul\"}"}),
            serde_json::json!({"type":"response.completed",
                "response":{"usage":{"input_tokens":10,"output_tokens":5,"total_tokens":15}}}),
        ];

        let mut events = Vec::new();
        for frame in &frames {
            events.extend(decoder.decode(frame));
        }

        // Tool call: exactly one start, with the call_id + name, and assembled args.
        let starts: Vec<&ToolCallStart> = events
            .iter()
            .filter_map(|e| match e {
                AiStreamEvent::ToolCallStart(start) => Some(start),
                _ => None,
            })
            .collect();
        assert_eq!(starts.len(), 1, "exactly one tool call started");
        assert_eq!(starts[0].index, 1);
        assert_eq!(starts[0].id, "call_abc");
        assert_eq!(starts[0].name, "get_weather");

        let args: String = events
            .iter()
            .filter_map(|e| match e {
                AiStreamEvent::ToolCallArgsDelta { index: 1, json } => Some(json.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(args, r#"{"city":"Istanbul"}"#, "args assembled, not duplicated");

        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, AiStreamEvent::ToolCallEnd { index: 1 }))
                .count(),
            1,
            "exactly one tool call end"
        );

        // Reasoning summary survives as a canonical reasoning delta.
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AiStreamEvent::ReasoningDelta { text } if text == "thinking")),
            "reasoning delta preserved"
        );
    }

    #[test]
    fn responses_decoder_recovers_args_when_only_done_carries_them() {
        // A backend that skips incremental deltas and only sends the full
        // arguments on `.done` must still produce a non-empty tool call.
        let codec = OpenAiResponsesCodec::codex_native_relay();
        let mut decoder = codec.decoder("gpt-5.5");
        let frames = [
            serde_json::json!({"type":"response.output_item.added","output_index":0,
                "item":{"type":"function_call","call_id":"call_x","name":"noop","arguments":""}}),
            serde_json::json!({"type":"response.function_call_arguments.done","output_index":0,
                "arguments":"{\"a\":1}"}),
        ];
        let mut events = Vec::new();
        for frame in &frames {
            events.extend(decoder.decode(frame));
        }
        let args: String = events
            .iter()
            .filter_map(|e| match e {
                AiStreamEvent::ToolCallArgsDelta { json, .. } => Some(json.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(args, r#"{"a":1}"#, "full args recovered from done event");
    }

    #[test]
    fn responses_decoder_emits_citations_and_output_images() {
        let codec = OpenAiResponsesCodec::codex_native_relay();
        let mut decoder = codec.decoder("gpt-5.5");
        let frames = [
            serde_json::json!({"type":"response.output_text.annotation.added",
                "annotation":{"type":"url_citation","url":"https://example.com","title":"Example"}}),
            serde_json::json!({"type":"response.image_generation_call.completed",
                "result":"aGVsbG8=","output_format":"image/png"}),
        ];
        let mut events = Vec::new();
        for frame in &frames {
            events.extend(decoder.decode(frame));
        }
        assert!(events.iter().any(|e| matches!(e,
            AiStreamEvent::Citation { url, title }
                if url == "https://example.com" && title.as_deref() == Some("Example"))));
        assert!(events.iter().any(|e| matches!(e,
            AiStreamEvent::OutputImage { media_type, data }
                if media_type == "image/png" && data == "aGVsbG8=")));
    }

    #[test]
    fn responses_decoder_emits_server_tool_calls() {
        let codec = OpenAiResponsesCodec::codex_native_relay();
        let mut decoder = codec.decoder("gpt-5.5");
        let frames = [
            serde_json::json!({"type":"response.web_search_call.in_progress","item_id":"ws_1"}),
            serde_json::json!({"type":"response.web_search_call.completed","item_id":"ws_1"}),
            serde_json::json!({"type":"response.code_interpreter_call.completed","item_id":"ci_1"}),
        ];
        let mut events = Vec::new();
        for frame in &frames {
            events.extend(decoder.decode(frame));
        }
        let calls: Vec<(&str, &str, &str)> = events
            .iter()
            .filter_map(|e| match e {
                AiStreamEvent::ServerToolCall { id, name, status } => {
                    Some((id.as_str(), name.as_str(), status.as_str()))
                }
                _ => None,
            })
            .collect();
        assert!(calls.contains(&("ws_1", "web_search", "in_progress")));
        assert!(calls.contains(&("ws_1", "web_search", "completed")));
        assert!(calls.contains(&("ci_1", "code_interpreter", "completed")));
    }

    #[test]
    fn model_list_urls_match_supported_wire_formats() {
        assert_eq!(
            OpenAiCodec.models_url("https://x/v1").as_deref(),
            Some("https://x/v1/models")
        );
        assert_eq!(
            AnthropicCodec
                .models_url("https://api.anthropic.com")
                .as_deref(),
            Some("https://api.anthropic.com/v1/models")
        );
        assert_eq!(
            GeminiCodec.models_url("https://g").as_deref(),
            Some("https://g/v1beta/models")
        );
    }

    #[test]
    fn model_list_parsers_extract_upstream_model_ids() {
        let openai = serde_json::json!({
            "object": "list",
            "data": [{ "id": "gpt-test" }, { "id": "owner/model" }]
        });
        assert_eq!(
            OpenAiCodec.parse_models_response(&openai).unwrap(),
            vec!["gpt-test", "owner/model"]
        );

        let anthropic = serde_json::json!({
            "data": [{ "id": "claude-sonnet-4-6" }]
        });
        assert_eq!(
            AnthropicCodec.parse_models_response(&anthropic).unwrap(),
            vec!["claude-sonnet-4-6"]
        );

        let gemini = serde_json::json!({
            "models": [
                {
                    "name": "models/gemini-2.0-flash",
                    "supportedGenerationMethods": ["generateContent"]
                },
                {
                    "name": "models/text-embedding-004",
                    "supportedGenerationMethods": ["embedContent"]
                },
                {
                    "baseModelId": "gemini-2.5-pro",
                    "supportedGenerationMethods": ["generateContent"]
                }
            ]
        });
        assert_eq!(
            GeminiCodec.parse_models_response(&gemini).unwrap(),
            vec!["gemini-2.0-flash", "gemini-2.5-pro"]
        );
    }
}
