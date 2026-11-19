//! The "wire codec" half of the `AuthScheme × WireCodec` decomposition. A
//! [`WireCodec`] captures everything that differs *at the wire* between
//! providers — URL shape, fixed headers, request-body translation, response
//! parsing, and the streaming decoder — so the generic [`crate::ComposedAdapter`]
//! can run the one execute loop for all of them. New wire formats become a
//! codec impl (mostly delegating to `sb-protocols`), not a whole adapter.

use sb_core::{AiRequest, AiResponse, AiStreamEvent, ContentPart, ToolCallStart};
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

    /// A parsed non-streaming response as canonical events, the ones the
    /// stream decoder would have produced. Default:
    /// [`sb_adapter::response_to_events`], which carries no thinking.
    fn response_events(&self, resp: &AiResponse) -> Vec<AiStreamEvent> {
        sb_adapter::response_to_events(resp)
    }

    /// Whether `req` keeps the thinking this codec decodes. Default yes;
    /// the OpenAI chat codecs drop it when the request carries
    /// [`DROP_CHAT_WIRE_THINKING`].
    fn keeps_reasoning(&self, _req: &AiRequest) -> bool {
        true
    }

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

    /// Audio endpoint URL (`/audio/transcriptions`, `/audio/translations`,
    /// `/audio/speech`), if this wire format has OpenAI's audio API.
    fn audio_url(&self, _base_url: &str, _call: sb_adapter::AudioCall) -> Option<String> {
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

/// Request metadata key (value `"drop"`): the client's wire format has no
/// input form for the thinking an OpenAI-compatible engine sends, so the
/// OpenAI chat codecs drop it for this request, as they did before they
/// decoded it. Set by the `/v1/messages` and `/v1/responses` handlers unless
/// `server.openai_compatible_thinking_on_all_formats` is on.
pub const DROP_CHAT_WIRE_THINKING: &str = "chat_wire_thinking";

fn keeps_chat_wire_thinking(req: &AiRequest) -> bool {
    req.metadata
        .get(DROP_CHAT_WIRE_THINKING)
        .is_none_or(|value| value != "drop")
}

/// [`sb_adapter::response_to_events`] plus the response's thinking as one
/// `ReasoningDelta` right after `MessageStart`, ahead of the answer, where
/// the stream decoder puts it.
fn response_events_with_reasoning(resp: &AiResponse) -> Vec<AiStreamEvent> {
    let mut events = sb_adapter::response_to_events(resp);
    let reasoning: String = resp
        .message
        .content
        .iter()
        .filter_map(|part| match part {
            ContentPart::Reasoning { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    if !reasoning.is_empty() {
        events.insert(1, AiStreamEvent::ReasoningDelta { text: reasoning });
    }
    events
}

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
    /// Thinking (`reasoning_content` / `reasoning`) is kept, as the stream
    /// decoder keeps it, so a collected response carries it too.
    fn response_events(&self, resp: &AiResponse) -> Vec<AiStreamEvent> {
        response_events_with_reasoning(resp)
    }
    fn keeps_reasoning(&self, req: &AiRequest) -> bool {
        keeps_chat_wire_thinking(req)
    }
    fn decoder(&self, _model: &str) -> Box<dyn StreamDecoder> {
        Box::new(OpenAiDecoder(
            sb_protocols::openai::OpenAiStreamDecoder::new(),
        ))
    }
    fn embeddings_url(&self, base_url: &str) -> Option<String> {
        Some(format!("{}/embeddings", base_url.trim_end_matches('/')))
    }
    fn audio_url(&self, base_url: &str, call: sb_adapter::AudioCall) -> Option<String> {
        Some(format!(
            "{}/{}",
            base_url.trim_end_matches('/'),
            call.path()
        ))
    }
    fn models_url(&self, base_url: &str) -> Option<String> {
        Some(format!("{}/models", base_url.trim_end_matches('/')))
    }
    fn parse_models_response(&self, body: &Value) -> Result<Vec<String>, String> {
        string_field_array(body, "data", "id")
    }
}

// --- OpenAI Chat Completions on a named local engine ------------------------

/// OpenAI chat for an endpoint whose engine is named in config
/// (`engine: llama_cpp | vllm | ollama`). Same wire as [`OpenAiCodec`]; the
/// only difference is that request controls the engines spell differently
/// are translated here, at the edge, so a caller writes one request shape.
pub struct EngineChatCodec {
    engine: sb_core::OpenAiEngine,
}

impl EngineChatCodec {
    pub fn new(engine: sb_core::OpenAiEngine) -> Self {
        Self { engine }
    }
}

/// `reasoning_effort` values that ask for thinking. `none` asks for none; any
/// other value is not ours to interpret and is forwarded untouched.
const THINKING_EFFORTS: &[&str] = &["minimal", "low", "medium", "high"];

/// Translate the OpenAI `reasoning_effort` control (carried verbatim in the
/// canonical request's OpenAI passthrough) into what the engine reads.
///
/// - Ollama honors `reasoning_effort` itself: unchanged.
/// - llama.cpp and vLLM switch thinking through chat-template arguments:
///   `none` becomes `chat_template_kwargs.enable_thinking = false` and the
///   top-level `none` is consumed. llama.cpp ignores the top-level field;
///   older vLLM releases reject `none` there, while current ones map it to
///   the same template argument themselves (an explicit template argument
///   takes priority), so the translated body is right for both. A thinking
///   effort becomes `enable_thinking = true` plus the effort as a template
///   argument (gpt-oss style templates read it).
/// - An absent or unknown effort adds nothing.
/// - A caller-sent `chat_template_kwargs` key is never overwritten, and a
///   non-object `chat_template_kwargs` is left exactly as sent.
pub fn apply_engine_reasoning(engine: sb_core::OpenAiEngine, body: &mut Value) {
    use sb_core::OpenAiEngine;
    if !matches!(engine, OpenAiEngine::LlamaCpp | OpenAiEngine::Vllm) {
        return;
    }
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    let Some(effort) = obj
        .get("reasoning_effort")
        .and_then(Value::as_str)
        .map(str::to_ascii_lowercase)
    else {
        return;
    };
    let thinking = if effort == "none" {
        false
    } else if THINKING_EFFORTS.contains(&effort.as_str()) {
        true
    } else {
        return;
    };
    let kwargs = obj
        .entry("chat_template_kwargs")
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    let Some(kwargs) = kwargs.as_object_mut() else {
        return;
    };
    kwargs
        .entry("enable_thinking")
        .or_insert(Value::Bool(thinking));
    if thinking {
        kwargs
            .entry("reasoning_effort")
            .or_insert(Value::String(effort));
    } else {
        obj.remove("reasoning_effort");
    }
}

/// Model settings asked for thinking on without an effort: llama.cpp and
/// vLLM switch it through the chat template. A request's own
/// `enable_thinking` is never overwritten.
pub fn apply_engine_thinking_on(engine: sb_core::OpenAiEngine, body: &mut Value) {
    use sb_core::OpenAiEngine;
    if !matches!(engine, OpenAiEngine::LlamaCpp | OpenAiEngine::Vllm) {
        return;
    }
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    let kwargs = obj
        .entry("chat_template_kwargs")
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    if let Some(kwargs) = kwargs.as_object_mut() {
        kwargs.entry("enable_thinking").or_insert(Value::Bool(true));
    }
}

impl WireCodec for EngineChatCodec {
    fn id(&self) -> &'static str {
        OpenAiCodec.id()
    }
    fn url(&self, base_url: &str, model: &str, stream: bool) -> String {
        OpenAiCodec.url(base_url, model, stream)
    }
    fn request_body(&self, req: &AiRequest, model: &str, stream: bool) -> Result<Value, String> {
        let mut body = OpenAiCodec.request_body(req, model, stream)?;
        apply_engine_reasoning(self.engine, &mut body);
        if req
            .metadata
            .contains_key(sb_core::REASONING_ENABLE_METADATA_KEY)
        {
            apply_engine_thinking_on(self.engine, &mut body);
        }
        Ok(body)
    }
    fn parse_response(&self, body: &Value) -> Result<AiResponse, String> {
        OpenAiCodec.parse_response(body)
    }
    fn response_events(&self, resp: &AiResponse) -> Vec<AiStreamEvent> {
        OpenAiCodec.response_events(resp)
    }
    fn keeps_reasoning(&self, req: &AiRequest) -> bool {
        OpenAiCodec.keeps_reasoning(req)
    }
    fn decoder(&self, model: &str) -> Box<dyn StreamDecoder> {
        OpenAiCodec.decoder(model)
    }
    fn embeddings_url(&self, base_url: &str) -> Option<String> {
        OpenAiCodec.embeddings_url(base_url)
    }
    fn audio_url(&self, base_url: &str, call: sb_adapter::AudioCall) -> Option<String> {
        OpenAiCodec.audio_url(base_url, call)
    }
    fn models_url(&self, base_url: &str) -> Option<String> {
        OpenAiCodec.models_url(base_url)
    }
    fn parse_models_response(&self, body: &Value) -> Result<Vec<String>, String> {
        OpenAiCodec.parse_models_response(body)
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

    /// A plain third-party `/responses` endpoint reached with an API key. Unlike
    /// the codex native relay this sends the request as translated — the ChatGPT
    /// backend's `store: false` / stripped-sampling / Codex-instructions shape is
    /// specific to that backend, not to the Responses wire.
    pub fn openai_compatible() -> Self {
        Self {
            id: "openai_responses",
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

/// The ChatGPT Codex Responses backend rejects draft-6 numeric
/// `exclusiveMinimum`, but accepts the equivalent inclusive `minimum`.
fn downlevel_codex_native_exclusive_minimum(schema: &mut Value) {
    let Value::Object(object) = schema else {
        return;
    };

    for value in object.values_mut() {
        downlevel_codex_native_exclusive_minimum(value);
    }

    let Some(exclusive_minimum) = object.remove("exclusiveMinimum") else {
        return;
    };
    let minimum = if object.get("type").and_then(Value::as_str) == Some("integer") {
        exclusive_minimum
            .as_i64()
            .and_then(|value| value.checked_add(1))
            .map(Value::from)
            .or_else(|| {
                exclusive_minimum
                    .as_u64()
                    .and_then(|value| value.checked_add(1))
                    .map(Value::from)
            })
    } else if exclusive_minimum.is_number() {
        Some(exclusive_minimum)
    } else {
        None
    };

    let Some(minimum) = minimum else {
        return;
    };
    let replacement_bound = minimum.as_f64();
    let current_bound = object.get("minimum").and_then(Value::as_f64);
    let should_replace = match replacement_bound.zip(current_bound) {
        Some((replacement, current)) => current < replacement,
        None => true,
    };
    if should_replace {
        object.insert("minimum".to_string(), minimum);
    }
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
                        out.push(AiStreamEvent::ToolCallStart(ToolCallStart {
                            index,
                            id,
                            name,
                        }));
                        // Some backends inline the opening arguments fragment.
                        if let Some(args) = frame.pointer("/item/arguments").and_then(Value::as_str)
                        {
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
                                "energy": frame
                                    .pointer("/response/energy")
                                    .or_else(|| frame.pointer("/response/usage/energy"))
                                    .cloned()
                                    .unwrap_or(Value::Null),
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
                // The ChatGPT Codex Responses backend (gpt-5.x reasoning models) rejects
                // sampling params with an "upstream 400 error"; a client that sends
                // `temperature`/`top_p` (any value, incl. 0) hard-fails the request, which
                // then trips the per-account cooldown/breaker into a sticky
                // "no eligible target". Strip them so the backend uses its own defaults.
                map.remove("temperature");
                map.remove("top_p");
                if let Some(tools) = map.get_mut("tools").and_then(Value::as_array_mut) {
                    for tool in tools {
                        if let Some(parameters) = tool.get_mut("parameters") {
                            downlevel_codex_native_exclusive_minimum(parameters);
                        }
                    }
                }
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
            // Claude Code subscription OAuth tokens (from `claude setup-token`)
            // are ONLY accepted with the OAuth beta flag; without it Anthropic
            // throttles/refuses the request (observed as upstream 429). `x-app`
            // identifies the first-party CLI surface.
            ("anthropic-beta", "oauth-2025-04-20"),
            ("x-app", "cli"),
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
    fn plain_responses_endpoint_keeps_what_the_codex_backend_strips() {
        let mut req = AiRequest::new("client-model", vec![sb_core::Message::user("hi")]);
        req.max_output_tokens = Some(64);
        req.temperature = Some(1.0);

        let body = OpenAiResponsesCodec::openai_compatible()
            .request_body(&req, "gpt-5.6-sol", false)
            .unwrap();

        // A third-party /responses endpoint accepts these; only the ChatGPT Codex
        // backend rejects them, so its shaping must not leak onto other providers.
        assert_eq!(body["max_output_tokens"], 64);
        assert_eq!(body["temperature"], 1.0);
        assert!(body.get("store").is_none());
        assert!(
            body.get("instructions")
                .and_then(|i| i.as_str())
                .is_none_or(|i| !i.contains("You are Codex")),
            "Codex instructions must not be injected into a third-party endpoint"
        );
        // Still the Responses wire, not chat/completions.
        assert_eq!(
            OpenAiResponsesCodec::openai_compatible().url("https://x/v1", "m", false),
            "https://x/v1/responses"
        );
    }

    #[test]
    fn codex_native_relay_sets_chatgpt_backend_required_shape() {
        let codec = OpenAiResponsesCodec::codex_native_relay();
        let mut req = AiRequest::new("client-model", vec![sb_core::Message::user("hi")]);
        req.max_output_tokens = Some(16);
        // A client (e.g. Claude Code / an eval harness) may send `temperature`; the
        // Codex Responses backend rejects it with an upstream 400. It must be stripped.
        req.temperature = Some(0.0);
        req.passthrough
            .insert("top_p".to_string(), serde_json::json!(0.5));

        let body = codec.request_body(&req, "gpt-5.5", false).unwrap();

        assert_eq!(body["model"], "gpt-5.5");
        assert_eq!(body["stream"], true);
        assert_eq!(body["store"], false);
        assert!(body.get("max_output_tokens").is_none());
        assert!(
            body.get("temperature").is_none(),
            "codex-native relay must strip temperature (backend rejects sampling params)"
        );
        assert!(
            body.get("top_p").is_none(),
            "codex-native relay must strip top_p from passthrough"
        );
        assert_eq!(
            body["instructions"],
            "You are Codex, a helpful coding assistant."
        );
        assert!(codec.upstream_stream(false));
    }

    #[test]
    fn codex_native_relay_downlevels_exclusive_integer_minimum() {
        let codec = OpenAiResponsesCodec::codex_native_relay();
        let mut req = AiRequest::new("client-model", vec![sb_core::Message::user("hi")]);
        req.tools.push(sb_core::ToolSpec {
            name: "Read".to_string(),
            description: Some("Read a file".to_string()),
            parameters: serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "limit": {
                        "type": "integer",
                        "exclusiveMinimum": 0,
                        "maximum": 9007199254740991_u64
                    }
                }
            }),
            defer_loading: false,
        });

        let body = codec.request_body(&req, "gpt-5.6-sol", true).unwrap();
        let limit = &body["tools"][0]["parameters"]["properties"]["limit"];

        assert!(
            limit.get("exclusiveMinimum").is_none(),
            "the Codex Responses backend rejects numeric exclusiveMinimum"
        );
        assert_eq!(
            limit["minimum"], 1,
            "integer bounds must keep their original > 0 semantics"
        );
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
        assert_eq!(
            args, r#"{"city":"Istanbul"}"#,
            "args assembled, not duplicated"
        );

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

    // --- reasoning control per local engine ---------------------------------

    use sb_core::OpenAiEngine;

    /// One chat request as a caller writes it, with an optional
    /// `reasoning_effort`, rendered by the codec for `engine`.
    fn engine_body(engine: OpenAiEngine, effort: Option<&str>) -> Value {
        let mut req = AiRequest::new("local-model", vec![sb_core::Message::user("hi")]);
        req.response_format = Some(sb_core::ResponseFormat::JsonObject);
        if let Some(effort) = effort {
            req.passthrough
                .insert("reasoning_effort".to_string(), serde_json::json!(effort));
        }
        EngineChatCodec::new(engine)
            .request_body(&req, "qwen3", false)
            .unwrap()
    }

    #[test]
    fn llama_cpp_reasoning_none_switches_thinking_off_through_template_kwargs() {
        let body = engine_body(OpenAiEngine::LlamaCpp, Some("none"));
        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], false);
        assert!(
            body.get("reasoning_effort").is_none(),
            "`none` is translated, not forwarded: {body}"
        );
        assert_eq!(body["response_format"]["type"], "json_object");
    }

    #[test]
    fn llama_cpp_reasoning_effort_keeps_thinking_on_and_passes_the_effort() {
        let body = engine_body(OpenAiEngine::LlamaCpp, Some("high"));
        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], true);
        assert_eq!(body["chat_template_kwargs"]["reasoning_effort"], "high");
        assert_eq!(body["reasoning_effort"], "high");
    }

    #[test]
    fn llama_cpp_absent_reasoning_adds_nothing() {
        let body = engine_body(OpenAiEngine::LlamaCpp, None);
        assert!(body.get("chat_template_kwargs").is_none(), "{body}");
        assert!(body.get("reasoning_effort").is_none(), "{body}");
    }

    #[test]
    fn vllm_reasoning_none_switches_thinking_off_through_template_kwargs() {
        let body = engine_body(OpenAiEngine::Vllm, Some("none"));
        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], false);
        assert!(
            body.get("reasoning_effort").is_none(),
            "older vLLM releases reject a top-level `none`: {body}"
        );
    }

    #[test]
    fn vllm_reasoning_effort_keeps_thinking_on_and_passes_the_effort() {
        let body = engine_body(OpenAiEngine::Vllm, Some("low"));
        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], true);
        assert_eq!(body["chat_template_kwargs"]["reasoning_effort"], "low");
        assert_eq!(body["reasoning_effort"], "low");
    }

    #[test]
    fn vllm_absent_reasoning_adds_nothing() {
        let body = engine_body(OpenAiEngine::Vllm, None);
        assert!(body.get("chat_template_kwargs").is_none(), "{body}");
        assert!(body.get("reasoning_effort").is_none(), "{body}");
    }

    #[test]
    fn ollama_reasoning_none_is_forwarded_unchanged() {
        let body = engine_body(OpenAiEngine::Ollama, Some("none"));
        assert_eq!(body["reasoning_effort"], "none");
        assert!(body.get("chat_template_kwargs").is_none(), "{body}");
    }

    #[test]
    fn ollama_reasoning_effort_is_forwarded_unchanged() {
        let body = engine_body(OpenAiEngine::Ollama, Some("medium"));
        assert_eq!(body["reasoning_effort"], "medium");
        assert!(body.get("chat_template_kwargs").is_none(), "{body}");
    }

    #[test]
    fn ollama_absent_reasoning_adds_nothing() {
        let body = engine_body(OpenAiEngine::Ollama, None);
        assert!(body.get("chat_template_kwargs").is_none(), "{body}");
        assert!(body.get("reasoning_effort").is_none(), "{body}");
    }

    #[test]
    fn engine_codec_matches_generic_openai_chat_when_no_control_is_sent() {
        let mut req = AiRequest::new("local-model", vec![sb_core::Message::user("hi")]);
        req.passthrough
            .insert("top_p".to_string(), serde_json::json!(0.9));
        let generic = OpenAiCodec.request_body(&req, "qwen3", true).unwrap();
        for engine in [
            OpenAiEngine::Ollama,
            OpenAiEngine::LlamaCpp,
            OpenAiEngine::Vllm,
        ] {
            let codec = EngineChatCodec::new(engine);
            assert_eq!(codec.request_body(&req, "qwen3", true).unwrap(), generic);
            assert_eq!(
                codec.url("http://executor-1.example:8080/v1", "qwen3", true),
                "http://executor-1.example:8080/v1/chat/completions"
            );
        }
    }

    #[test]
    fn caller_sent_template_kwargs_are_never_overwritten() {
        let mut req = AiRequest::new("local-model", vec![sb_core::Message::user("hi")]);
        req.passthrough
            .insert("reasoning_effort".to_string(), serde_json::json!("none"));
        req.passthrough.insert(
            "chat_template_kwargs".to_string(),
            serde_json::json!({"enable_thinking": true, "custom": 1}),
        );
        let body = EngineChatCodec::new(OpenAiEngine::LlamaCpp)
            .request_body(&req, "qwen3", false)
            .unwrap();
        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], true);
        assert_eq!(body["chat_template_kwargs"]["custom"], 1);

        // A non-object value is the caller's business: left exactly as sent.
        req.passthrough
            .insert("chat_template_kwargs".to_string(), serde_json::json!("raw"));
        let body = EngineChatCodec::new(OpenAiEngine::Vllm)
            .request_body(&req, "qwen3", false)
            .unwrap();
        assert_eq!(body["chat_template_kwargs"], "raw");
    }

    #[test]
    fn streaming_requests_get_the_same_translation() {
        let mut req = AiRequest::new("local-model", vec![sb_core::Message::user("hi")]);
        req.stream = true;
        req.passthrough
            .insert("reasoning_effort".to_string(), serde_json::json!("none"));
        for engine in [OpenAiEngine::LlamaCpp, OpenAiEngine::Vllm] {
            let body = EngineChatCodec::new(engine)
                .request_body(&req, "qwen3", true)
                .unwrap();
            assert_eq!(body["stream"], true, "{body}");
            assert_eq!(body["chat_template_kwargs"]["enable_thinking"], false);
            assert!(body.get("reasoning_effort").is_none(), "{body}");
        }
    }

    #[test]
    fn unknown_effort_values_are_forwarded_untouched() {
        let body = engine_body(OpenAiEngine::LlamaCpp, Some("turbo"));
        assert_eq!(body["reasoning_effort"], "turbo");
        assert!(body.get("chat_template_kwargs").is_none(), "{body}");
    }
}
