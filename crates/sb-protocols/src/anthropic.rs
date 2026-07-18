//! Anthropic **Messages API** (`/v1/messages`) <-> canonical IR. A genuinely
//! different wire format from OpenAI — `x-api-key` auth, a top-level `system`
//! field (not a message), typed content blocks, and named SSE events
//! (`message_start` / `content_block_delta` / …) instead of `choices[].delta`.
//! This module is the *upstream* half (canonical -> Anthropic body, Anthropic
//! stream -> canonical events); the ingress half (Anthropic-shaped client
//! requests) is a separate future step. Like every protocol it translates only
//! to/from the canonical IR — never directly to another wire format.

use sb_core::{
    AiRequest, AiResponse, AiStreamEvent, ContentPart, FinishReason, ImageSource, Message, Role,
    ServerToolProtocol, ServerToolSpec, ToolCallStart, ToolSpec, Usage,
};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};

/// Anthropic requires `max_tokens`; use this when the request didn't set one.
const DEFAULT_MAX_TOKENS: u32 = 4096;
/// The Messages API version this module targets.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";
const ANTHROPIC_PROTOCOL: &str = "anthropic";
const ANTHROPIC_PASSTHROUGH_KEYS: &[&str] = &[
    "thinking",
    "tool_choice",
    "stop_sequences",
    "top_p",
    "top_k",
    "service_tier",
    "metadata",
    "container",
    "mcp_servers",
];

fn split_data_url(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("data:")?;
    let (media_type, data) = rest.split_once(";base64,")?;
    if media_type.is_empty() || data.is_empty() {
        return None;
    }
    Some((media_type.to_string(), data.to_string()))
}

fn image_from_url(url: String) -> ContentPart {
    let source = if let Some((media_type, data)) = split_data_url(&url) {
        ImageSource::InlineBase64 { media_type, data }
    } else {
        ImageSource::RemoteUrl { url }
    };
    ContentPart::Image {
        source,
        detail: None,
    }
}

fn image_from_anthropic_source(source: Option<&Value>) -> Result<ContentPart, String> {
    let source = source.ok_or_else(|| "Anthropic image block missing `source`".to_string())?;
    match source.get("type").and_then(Value::as_str) {
        Some("base64") => {
            let media_type = source
                .get("media_type")
                .and_then(Value::as_str)
                .ok_or_else(|| "Anthropic base64 image missing `media_type`".to_string())?;
            let data = source
                .get("data")
                .and_then(Value::as_str)
                .ok_or_else(|| "Anthropic base64 image missing `data`".to_string())?;
            Ok(ContentPart::image_base64(media_type, data))
        }
        Some("url") => {
            let url = source
                .get("url")
                .and_then(Value::as_str)
                .ok_or_else(|| "Anthropic url image missing `url`".to_string())?;
            Ok(image_from_url(url.to_string()))
        }
        Some("file") => {
            let file_id = source
                .get("file_id")
                .and_then(Value::as_str)
                .ok_or_else(|| "Anthropic file image missing `file_id`".to_string())?;
            Ok(ContentPart::Image {
                source: ImageSource::ProviderFileRef {
                    provider: Some("anthropic".to_string()),
                    id: file_id.to_string(),
                },
                detail: None,
            })
        }
        Some(other) => Err(format!("unsupported Anthropic image source `{other}`")),
        None => Err("Anthropic image source missing string `type`".to_string()),
    }
}

fn image_to_anthropic_block(part: &ContentPart) -> Result<Option<Value>, String> {
    let ContentPart::Image { source, .. } = part else {
        return Ok(None);
    };
    source.validate()?;
    let source = match source {
        ImageSource::InlineBase64 { media_type, data } => {
            json!({
                "type": "base64",
                "media_type": media_type,
                "data": data,
            })
        }
        ImageSource::RemoteUrl { url } => {
            json!({
                "type": "url",
                "url": url,
            })
        }
        ImageSource::ProviderFileRef { .. } => {
            return Err(
                "Anthropic file image references require Files API beta header support".to_string(),
            )
        }
    };
    Ok(Some(json!({ "type": "image", "source": source })))
}

fn stop_reason_to_finish(reason: Option<&str>) -> FinishReason {
    match reason {
        Some("end_turn") | Some("stop_sequence") => FinishReason::Stop,
        Some("max_tokens") => FinishReason::Length,
        Some("tool_use") => FinishReason::ToolCalls,
        _ => FinishReason::Stop,
    }
}

fn finish_to_stop_reason(reason: FinishReason) -> &'static str {
    match reason {
        FinishReason::Stop => "end_turn",
        FinishReason::Length => "max_tokens",
        FinishReason::ToolCalls => "tool_use",
        FinishReason::ContentFilter => "end_turn",
        FinishReason::Error => "end_turn",
    }
}

/// Content blocks for one canonical message, in Anthropic shape. Returns
/// `None` when the message yields no blocks (Anthropic rejects empty content).
fn reject_image_parts(parts: &[ContentPart], where_: &str) -> Result<(), String> {
    if parts
        .iter()
        .any(|part| matches!(part, ContentPart::Image { .. }))
    {
        Err(format!("{where_} cannot contain image content"))
    } else {
        Ok(())
    }
}

fn message_content_blocks(message: &Message) -> Result<Option<Vec<Value>>, String> {
    let mut blocks = Vec::new();
    for part in &message.content {
        match part {
            ContentPart::Text { text } => {
                if !text.is_empty() {
                    blocks.push(json!({ "type": "text", "text": text }));
                }
            }
            ContentPart::Image { .. } => {
                if message.role != Role::User {
                    return Err(
                        "Anthropic can only encode image content in user messages".to_string()
                    );
                }
                if let Some(block) = image_to_anthropic_block(part)? {
                    blocks.push(block);
                }
            }
            ContentPart::ToolUse { id, name, args } => {
                blocks.push(json!({
                    "type": "tool_use",
                    "id": id,
                    "name": name,
                    "input": args,
                }));
            }
            ContentPart::ToolResult {
                tool_use_id,
                content,
                is_error,
                ..
            } => {
                let mut block = Map::new();
                block.insert("type".to_string(), Value::String("tool_result".to_string()));
                block.insert(
                    "tool_use_id".to_string(),
                    Value::String(tool_use_id.clone()),
                );
                block.insert("content".to_string(), Value::String(content.clone()));
                if *is_error {
                    block.insert("is_error".to_string(), Value::Bool(true));
                }
                blocks.push(Value::Object(block));
            }
            ContentPart::Reasoning { text, signature } => {
                // Anthropic accepts a thinking block on replay only with its
                // original signature; an unsigned summary can't be verified, so
                // it is dropped rather than risk a 400 on the next turn.
                if let Some(signature) = signature {
                    blocks.push(json!({
                        "type": "thinking",
                        "thinking": text,
                        "signature": signature,
                    }));
                }
            }
            // Citations are output annotations, not re-submittable input blocks.
            // Server tools run on the provider; the gateway never re-submits them.
            ContentPart::Citation { .. }
            | ContentPart::ServerToolUse { .. }
            | ContentPart::ServerToolResult { .. } => {}
        }
    }
    if blocks.is_empty() {
        Ok(None)
    } else {
        Ok(Some(blocks))
    }
}

/// Canonical `AiRequest` -> Anthropic Messages request body.
pub fn request_to_anthropic_wire(
    req: &AiRequest,
    upstream_model: &str,
    stream: bool,
) -> Result<Value, String> {
    req.validate_canonical()?;
    // `system` is a TOP-LEVEL field in Anthropic, never a message. Fold in the
    // request's system prompt plus any stray `system`-role messages.
    let mut system_chunks = Vec::new();
    if let Some(system) = &req.system {
        if !system.is_empty() {
            system_chunks.push(system.clone());
        }
    }

    let mut messages = Vec::new();
    for message in &req.messages {
        match message.role {
            Role::System => {
                reject_image_parts(&message.content, "Anthropic system messages")?;
                let text = message.text();
                if !text.is_empty() {
                    system_chunks.push(text);
                }
            }
            // Tool results are carried back to Anthropic inside a USER turn.
            Role::Tool => {
                if let Some(blocks) = message_content_blocks(message)? {
                    messages.push(json!({ "role": "user", "content": blocks }));
                }
            }
            Role::User => {
                if let Some(blocks) = message_content_blocks(message)? {
                    messages.push(json!({ "role": "user", "content": blocks }));
                }
            }
            Role::Assistant => {
                if let Some(blocks) = message_content_blocks(message)? {
                    messages.push(json!({ "role": "assistant", "content": blocks }));
                }
            }
        }
    }

    let mut body = Map::new();
    body.insert(
        "model".to_string(),
        Value::String(upstream_model.to_string()),
    );
    body.insert(
        "max_tokens".to_string(),
        Value::Number(serde_json::Number::from(
            req.max_output_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        )),
    );
    body.insert("stream".to_string(), Value::Bool(stream));
    body.insert("messages".to_string(), Value::Array(messages));

    if !system_chunks.is_empty() {
        body.insert(
            "system".to_string(),
            Value::String(system_chunks.join("\n")),
        );
    }

    if !req.tools.is_empty() || !req.server_tools.is_empty() {
        let mut tools: Vec<Value> = req
            .tools
            .iter()
            .map(|tool| {
                let mut value = Map::new();
                value.insert("name".to_string(), Value::String(tool.name.clone()));
                if let Some(description) = &tool.description {
                    value.insert(
                        "description".to_string(),
                        Value::String(description.clone()),
                    );
                }
                // Anthropic calls the JSON Schema `input_schema`.
                value.insert("input_schema".to_string(), tool.parameters.clone());
                Value::Object(value)
            })
            .collect();
        for tool in &req.server_tools {
            if tool.protocol != ServerToolProtocol::Anthropic {
                return Err(format!(
                    "Anthropic cannot encode {} server tool `{}`",
                    tool.protocol.as_str(),
                    tool.kind
                ));
            }
            tools.push(tool.config.clone());
        }
        body.insert("tools".to_string(), Value::Array(tools));
    }

    if let Some(temperature) = req.temperature {
        if let Some(number) = serde_json::Number::from_f64(f64::from(temperature)) {
            body.insert("temperature".to_string(), Value::Number(number));
        }
    }
    if let Some(passthrough) = req.protocol_passthrough.get(ANTHROPIC_PROTOCOL) {
        for (key, value) in passthrough {
            body.entry(key.clone()).or_insert_with(|| value.clone());
        }
    }

    Ok(Value::Object(body))
}

fn usage_from_json(usage: Option<&Value>) -> Usage {
    let usage = usage.and_then(Value::as_object);
    Usage {
        input_tokens: usage
            .and_then(|u| u.get("input_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        output_tokens: usage
            .and_then(|u| u.get("output_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cached_input_tokens: usage
            .and_then(|u| u.get("cache_read_input_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        ..Usage::default()
    }
}

/// Non-streaming Anthropic Messages response -> canonical `AiResponse`.
pub fn parse_anthropic_response(body: &Value) -> Result<AiResponse, String> {
    let content = body
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| "missing array `content`".to_string())?;

    let mut parts = Vec::new();
    for block in content {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    if !text.is_empty() {
                        parts.push(ContentPart::text(text));
                    }
                }
            }
            Some("tool_use") => {
                let id = block
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "tool_use block missing string `id`".to_string())?;
                let name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "tool_use block missing string `name`".to_string())?;
                parts.push(ContentPart::ToolUse {
                    id: id.to_string(),
                    name: name.to_string(),
                    args: block.get("input").cloned().unwrap_or(Value::Null),
                });
            }
            Some("thinking") => {
                if let Some(text) = block.get("thinking").and_then(Value::as_str) {
                    if !text.is_empty() {
                        parts.push(ContentPart::Reasoning {
                            text: text.to_string(),
                            signature: block
                                .get("signature")
                                .and_then(Value::as_str)
                                .map(str::to_string),
                        });
                    }
                }
            }
            // redacted_thinking / unknown blocks: ignored in v1.
            _ => {}
        }
    }

    Ok(AiResponse {
        id: body
            .get("id")
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .unwrap_or_else(|| sb_core::new_id("resp")),
        model: body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        message: Message {
            role: Role::Assistant,
            content: parts,
            cache_hint: None,
        },
        finish_reason: stop_reason_to_finish(body.get("stop_reason").and_then(Value::as_str)),
        usage: usage_from_json(body.get("usage")),
    })
}

/// Optional ingress helper: canonical `AiResponse` -> Anthropic Messages JSON.
/// Present for symmetry / future Anthropic egress; the upstream path doesn't
/// use it (it streams), but keeping the pair complete keeps the hub honest.
pub fn response_to_anthropic(resp: &AiResponse) -> Value {
    let content: Vec<Value> = resp
        .message
        .content
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text } => Some(json!({ "type": "text", "text": text })),
            ContentPart::ToolUse { id, name, args } => Some(json!({
                "type": "tool_use",
                "id": id,
                "name": name,
                "input": args,
            })),
            ContentPart::Reasoning { text, signature } => {
                let mut block = json!({ "type": "thinking", "thinking": text });
                if let Some(signature) = signature {
                    block["signature"] = json!(signature);
                }
                Some(block)
            }
            ContentPart::Image { .. }
            | ContentPart::ToolResult { .. }
            | ContentPart::Citation { .. }
            | ContentPart::ServerToolUse { .. }
            | ContentPart::ServerToolResult { .. } => None,
        })
        .collect();

    json!({
        "id": resp.id,
        "type": "message",
        "role": "assistant",
        "model": resp.model,
        "content": content,
        "stop_reason": finish_to_stop_reason(resp.finish_reason),
        "stop_sequence": Value::Null,
        "usage": {
            "input_tokens": resp.usage.input_tokens,
            "output_tokens": resp.usage.output_tokens,
        }
    })
}

fn index_of(data: &Value) -> u32 {
    data.get("index")
        .and_then(Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
        .unwrap_or(0)
}

/// Decodes the Anthropic SSE event stream into canonical `AiStreamEvent`s.
/// Dispatches on each frame's `data.type` (`message_start`,
/// `content_block_delta`, …), so the caller can ignore the `event:` line.
pub struct AnthropicStreamDecoder {
    started: bool,
    ended: bool,
    /// Content-block indices that are `tool_use` (so `content_block_stop`
    /// knows to emit a `ToolCallEnd`).
    tool_blocks: BTreeSet<u32>,
    stop_reason: Option<FinishReason>,
    usage: Usage,
}

impl AnthropicStreamDecoder {
    pub fn new() -> Self {
        Self {
            started: false,
            ended: false,
            tool_blocks: BTreeSet::new(),
            stop_reason: None,
            usage: Usage::default(),
        }
    }

    pub fn decode(&mut self, data: &Value) -> Vec<AiStreamEvent> {
        let mut events = Vec::new();
        match data.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                let message = data.get("message");
                self.started = true;
                self.usage = usage_from_json(message.and_then(|m| m.get("usage")));
                events.push(AiStreamEvent::MessageStart {
                    id: message
                        .and_then(|m| m.get("id"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    model: message
                        .and_then(|m| m.get("model"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                });
            }
            Some("content_block_start") => {
                let index = index_of(data);
                let block = data.get("content_block");
                match block.and_then(|b| b.get("type")).and_then(Value::as_str) {
                    Some("tool_use") => {
                        self.tool_blocks.insert(index);
                        events.push(AiStreamEvent::ToolCallStart(ToolCallStart {
                            index,
                            id: block
                                .and_then(|b| b.get("id"))
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                            name: block
                                .and_then(|b| b.get("name"))
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                        }));
                    }
                    Some("text") => {
                        if let Some(text) = block
                            .and_then(|b| b.get("text"))
                            .and_then(Value::as_str)
                            .filter(|t| !t.is_empty())
                        {
                            events.push(AiStreamEvent::TextDelta {
                                text: text.to_string(),
                            });
                        }
                    }
                    _ => {}
                }
            }
            Some("content_block_delta") => {
                let index = index_of(data);
                let delta = data.get("delta");
                match delta.and_then(|d| d.get("type")).and_then(Value::as_str) {
                    Some("text_delta") => {
                        if let Some(text) =
                            delta.and_then(|d| d.get("text")).and_then(Value::as_str)
                        {
                            events.push(AiStreamEvent::TextDelta {
                                text: text.to_string(),
                            });
                        }
                    }
                    Some("input_json_delta") => {
                        if let Some(json) = delta
                            .and_then(|d| d.get("partial_json"))
                            .and_then(Value::as_str)
                        {
                            events.push(AiStreamEvent::ToolCallArgsDelta {
                                index,
                                json: json.to_string(),
                            });
                        }
                    }
                    Some("thinking_delta") => {
                        if let Some(text) = delta
                            .and_then(|d| d.get("thinking"))
                            .and_then(Value::as_str)
                        {
                            events.push(AiStreamEvent::ReasoningDelta {
                                text: text.to_string(),
                            });
                        }
                    }
                    _ => {}
                }
            }
            Some("content_block_stop") => {
                let index = index_of(data);
                if self.tool_blocks.contains(&index) {
                    events.push(AiStreamEvent::ToolCallEnd { index });
                }
            }
            Some("message_delta") => {
                if let Some(reason) = data
                    .get("delta")
                    .and_then(|d| d.get("stop_reason"))
                    .and_then(Value::as_str)
                {
                    self.stop_reason = Some(stop_reason_to_finish(Some(reason)));
                }
                if let Some(output) = data
                    .get("usage")
                    .and_then(|u| u.get("output_tokens"))
                    .and_then(Value::as_u64)
                {
                    self.usage.output_tokens = output;
                }
                events.push(AiStreamEvent::UsageDelta {
                    usage: self.usage.clone(),
                });
            }
            Some("message_stop") => {
                self.ended = true;
                events.push(AiStreamEvent::MessageEnd {
                    finish_reason: self.stop_reason.unwrap_or(FinishReason::Stop),
                });
            }
            // ping / error / unknown frames: nothing to emit.
            _ => {}
        }
        events
    }

    /// Emit a terminal `MessageEnd` if the upstream stream cut off before
    /// sending `message_stop`.
    pub fn finish(&mut self) -> Vec<AiStreamEvent> {
        if self.started && !self.ended {
            self.ended = true;
            vec![AiStreamEvent::MessageEnd {
                finish_reason: self.stop_reason.unwrap_or(FinishReason::Stop),
            }]
        } else {
            Vec::new()
        }
    }
}

impl Default for AnthropicStreamDecoder {
    fn default() -> Self {
        Self::new()
    }
}

// ===========================================================================
// Ingress: an Anthropic-shaped client request -> canonical, and the canonical
// event stream -> Anthropic SSE. This is what lets Claude Code / the Anthropic
// SDK point at `/v1/messages` and be routed to ANY provider.
// ===========================================================================

/// Anthropic `tool_result.content` may be a plain string or an array of blocks.
fn tool_result_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// Anthropic Messages request body -> canonical `AiRequest`.
pub fn request_from_anthropic(body: &Value) -> Result<AiRequest, String> {
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing or invalid `model`".to_string())?
        .to_string();

    let mut req = AiRequest::new(model, Vec::new());

    // `system` is top-level: a string OR an array of text blocks.
    match body.get("system") {
        Some(Value::String(s)) if !s.is_empty() => req.system = Some(s.clone()),
        Some(Value::Array(blocks)) => {
            let s = blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("");
            if !s.is_empty() {
                req.system = Some(s);
            }
        }
        _ => {}
    }

    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| "missing or invalid `messages`".to_string())?;

    for message in messages {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .ok_or_else(|| "message missing string `role`".to_string())?;
        let content = message.get("content");

        match role {
            "system" => match content {
                Some(Value::String(text)) if !text.is_empty() => {
                    req.system = Some(match req.system.take() {
                        Some(existing) if !existing.is_empty() => format!("{existing}\n{text}"),
                        _ => text.clone(),
                    });
                }
                Some(Value::Array(blocks)) => {
                    let text = blocks
                        .iter()
                        .filter_map(|block| match block.get("type").and_then(Value::as_str) {
                            Some("text") => block.get("text").and_then(Value::as_str),
                            _ => None,
                        })
                        .filter(|s| !s.is_empty())
                        .collect::<Vec<_>>()
                        .join("");
                    if !text.is_empty() {
                        req.system = Some(match req.system.take() {
                            Some(existing) if !existing.is_empty() => format!("{existing}\n{text}"),
                            _ => text,
                        });
                    }
                }
                _ => {}
            },
            "user" => match content {
                Some(Value::String(text)) => req.messages.push(Message::user(text.clone())),
                Some(Value::Array(blocks)) => {
                    // A user turn may mix plain text and tool_result blocks. Text
                    // -> a User message; each tool_result -> a Tool message
                    // (canonical carries tool results as Role::Tool).
                    let mut content_parts = Vec::new();
                    for block in blocks {
                        match block.get("type").and_then(Value::as_str) {
                            Some("text") => {
                                if let Some(text) = block.get("text").and_then(Value::as_str) {
                                    content_parts.push(ContentPart::text(text));
                                }
                            }
                            Some("image") => {
                                content_parts
                                    .push(image_from_anthropic_source(block.get("source"))?);
                            }
                            Some("tool_result") => {
                                if !content_parts.is_empty() {
                                    req.messages.push(Message {
                                        role: Role::User,
                                        content: std::mem::take(&mut content_parts),
                                        cache_hint: None,
                                    });
                                }
                                let tool_use_id = block
                                    .get("tool_use_id")
                                    .and_then(Value::as_str)
                                    .ok_or_else(|| {
                                        "tool_result missing string `tool_use_id`".to_string()
                                    })?;
                                req.messages.push(Message {
                                    role: Role::Tool,
                                    content: vec![ContentPart::ToolResult {
                                        tool_use_id: tool_use_id.to_string(),
                                        content: tool_result_text(block.get("content")),
                                        content_parts: Vec::new(),
                                        is_error: block
                                            .get("is_error")
                                            .and_then(Value::as_bool)
                                            .unwrap_or(false),
                                    }],
                                    cache_hint: None,
                                });
                            }
                            other => {
                                let block_type = other.unwrap_or("unknown");
                                return Err(format!(
                                    "unsupported Anthropic user content block `{block_type}`; multimodal content is not supported in this build"
                                ));
                            }
                        }
                    }
                    if !content_parts.is_empty() {
                        req.messages.push(Message {
                            role: Role::User,
                            content: content_parts,
                            cache_hint: None,
                        });
                    }
                }
                _ => {}
            },
            "assistant" => {
                let mut parts = Vec::new();
                match content {
                    Some(Value::String(text)) if !text.is_empty() => {
                        parts.push(ContentPart::text(text.clone()))
                    }
                    Some(Value::Array(blocks)) => {
                        for block in blocks {
                            match block.get("type").and_then(Value::as_str) {
                                Some("text") => {
                                    if let Some(text) = block.get("text").and_then(Value::as_str) {
                                        parts.push(ContentPart::text(text));
                                    }
                                }
                                Some("tool_use") => {
                                    let id = block.get("id").and_then(Value::as_str).ok_or_else(
                                        || "tool_use missing string `id`".to_string(),
                                    )?;
                                    let name =
                                        block.get("name").and_then(Value::as_str).ok_or_else(
                                            || "tool_use missing string `name`".to_string(),
                                        )?;
                                    parts.push(ContentPart::ToolUse {
                                        id: id.to_string(),
                                        name: name.to_string(),
                                        args: block.get("input").cloned().unwrap_or(Value::Null),
                                    });
                                }
                                other => {
                                    let block_type = other.unwrap_or("unknown");
                                    return Err(format!(
                                        "unsupported Anthropic assistant content block `{block_type}`"
                                    ));
                                }
                            }
                        }
                    }
                    _ => {}
                }
                req.messages.push(Message {
                    role: Role::Assistant,
                    content: parts,
                    cache_hint: None,
                });
            }
            other => return Err(format!("unsupported message role `{other}`")),
        }
    }

    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        for tool in tools {
            if let Some(kind) = tool.get("type").and_then(Value::as_str) {
                req.server_tools.push(ServerToolSpec::new(
                    ServerToolProtocol::Anthropic,
                    kind,
                    tool.clone(),
                ));
                continue;
            }
            let name = tool
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| "tool missing string `name`".to_string())?;
            req.tools.push(ToolSpec {
                name: name.to_string(),
                description: tool
                    .get("description")
                    .and_then(Value::as_str)
                    .map(ToString::to_string),
                // Anthropic's `input_schema` is our `parameters`.
                parameters: tool.get("input_schema").cloned().unwrap_or(Value::Null),
            });
        }
    }

    req.stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    req.temperature = body
        .get("temperature")
        .and_then(Value::as_f64)
        .map(|value| value as f32);
    req.max_output_tokens = body
        .get("max_tokens")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok());
    if let Some(obj) = body.as_object() {
        let passthrough = req
            .protocol_passthrough
            .entry(ANTHROPIC_PROTOCOL.to_string())
            .or_default();
        for key in ANTHROPIC_PASSTHROUGH_KEYS {
            if let Some(value) = obj.get(*key) {
                passthrough.insert((*key).to_string(), value.clone());
            }
        }
        if passthrough.is_empty() {
            req.protocol_passthrough.remove(ANTHROPIC_PROTOCOL);
        }
    }
    req.id = sb_core::new_id("req");

    Ok(req)
}

/// Rough token estimate for `/v1/messages/count_tokens` — a chars/4 heuristic
/// over system + message text + tool schemas. Not provider-exact, but the shape
/// Anthropic clients expect (documented as approximate).
pub fn estimate_input_tokens(req: &AiRequest) -> u64 {
    let mut chars = req.system.as_deref().map(str::len).unwrap_or(0);
    let mut image_tokens = 0;
    for message in &req.messages {
        for part in &message.content {
            match part {
                ContentPart::Text { text } => chars += text.len(),
                ContentPart::Reasoning { text, .. } => chars += text.len(),
                ContentPart::Citation { url, .. } => chars += url.len(),
                ContentPart::ServerToolResult { content, .. } => chars += content.len(),
                ContentPart::ServerToolUse { name, .. } => chars += name.len(),
                ContentPart::Image { .. } => image_tokens += 1_600,
                ContentPart::ToolUse { name, args, .. } => {
                    chars += name.len() + args.to_string().len()
                }
                ContentPart::ToolResult { content, .. } => chars += content.len(),
            }
        }
    }
    for tool in &req.tools {
        chars += tool.name.len() + tool.parameters.to_string().len();
    }
    (chars as u64).div_ceil(4) + image_tokens
}

/// Which content block (if any) the encoder currently has open. Anthropic block
/// indices are assigned sequentially across text and tool_use blocks.
enum OpenBlock {
    Text(u32),
    Reasoning(u32),
    Tool(u32),
}

/// Renders the canonical event stream as Anthropic Messages SSE
/// (`message_start` / `content_block_*` / `message_delta` / `message_stop`), so
/// an Anthropic-shaped client gets bytes in exactly the format it expects —
/// regardless of which provider actually served the request.
///
/// v1 note: `message_start.usage.input_tokens` is 0 (input usage isn't known
/// until the upstream reports it, carried out in `message_delta`); the
/// non-streaming path reports both input and output exactly.
pub struct AnthropicStreamEncoder {
    id: String,
    model: String,
    message_started: bool,
    ended: bool,
    open_block: Option<OpenBlock>,
    next_index: u32,
    tool_index_map: BTreeMap<u32, u32>,
    usage: Usage,
}

impl AnthropicStreamEncoder {
    pub fn new(id: String, model: String) -> Self {
        Self {
            id,
            model,
            message_started: false,
            ended: false,
            open_block: None,
            next_index: 0,
            tool_index_map: BTreeMap::new(),
            usage: Usage::default(),
        }
    }

    fn frame(event: &str, data: Value) -> String {
        format!("event: {event}\ndata: {data}\n\n")
    }

    fn ensure_started(&mut self, out: &mut Vec<String>) {
        if self.message_started {
            return;
        }
        self.message_started = true;
        out.push(Self::frame(
            "message_start",
            json!({
                "type": "message_start",
                "message": {
                    "id": self.id,
                    "type": "message",
                    "role": "assistant",
                    "model": self.model,
                    "content": [],
                    "stop_reason": Value::Null,
                    "stop_sequence": Value::Null,
                    "usage": { "input_tokens": self.usage.input_tokens, "output_tokens": 0 },
                }
            }),
        ));
    }

    fn close_block(&mut self, out: &mut Vec<String>) {
        if let Some(open) = self.open_block.take() {
            let index = match open {
                OpenBlock::Text(i) | OpenBlock::Reasoning(i) | OpenBlock::Tool(i) => i,
            };
            out.push(Self::frame(
                "content_block_stop",
                json!({ "type": "content_block_stop", "index": index }),
            ));
        }
    }

    pub fn encode(&mut self, ev: &AiStreamEvent) -> Vec<String> {
        let mut out = Vec::new();
        if self.ended {
            return out;
        }
        match ev {
            AiStreamEvent::MessageStart { id, model } => {
                if self.id.is_empty() {
                    self.id = id.clone();
                }
                if self.model.is_empty() {
                    self.model = model.clone();
                }
                self.ensure_started(&mut out);
            }
            // Generated images aren't part of the Messages output wire; citation
            // streaming would need the citations_delta protocol — both are
            // dropped from this surface rather than emitted malformed.
            AiStreamEvent::OutputImage { .. }
            | AiStreamEvent::Citation { .. }
            | AiStreamEvent::ServerToolCall { .. } => {}
            AiStreamEvent::TextDelta { text } => {
                if text.is_empty() {
                    return out;
                }
                self.ensure_started(&mut out);
                if !matches!(self.open_block, Some(OpenBlock::Text(_))) {
                    self.close_block(&mut out);
                    let index = self.next_index;
                    self.next_index += 1;
                    self.open_block = Some(OpenBlock::Text(index));
                    out.push(Self::frame(
                        "content_block_start",
                        json!({
                            "type": "content_block_start",
                            "index": index,
                            "content_block": { "type": "text", "text": "" }
                        }),
                    ));
                }
                let index = match self.open_block {
                    Some(OpenBlock::Text(i)) => i,
                    _ => 0,
                };
                out.push(Self::frame(
                    "content_block_delta",
                    json!({
                        "type": "content_block_delta",
                        "index": index,
                        "delta": { "type": "text_delta", "text": text }
                    }),
                ));
            }
            AiStreamEvent::ReasoningDelta { text } => {
                self.ensure_started(&mut out);
                if !matches!(self.open_block, Some(OpenBlock::Reasoning(_))) {
                    self.close_block(&mut out);
                    let index = self.next_index;
                    self.next_index += 1;
                    self.open_block = Some(OpenBlock::Reasoning(index));
                    out.push(Self::frame(
                        "content_block_start",
                        json!({
                            "type": "content_block_start",
                            "index": index,
                            "content_block": { "type": "thinking", "thinking": "" }
                        }),
                    ));
                }
                let index = match self.open_block {
                    Some(OpenBlock::Reasoning(i)) => i,
                    _ => 0,
                };
                out.push(Self::frame(
                    "content_block_delta",
                    json!({
                        "type": "content_block_delta",
                        "index": index,
                        "delta": { "type": "thinking_delta", "thinking": text }
                    }),
                ));
            }
            AiStreamEvent::ToolCallStart(tool) => {
                self.ensure_started(&mut out);
                self.close_block(&mut out);
                let index = self.next_index;
                self.next_index += 1;
                self.tool_index_map.insert(tool.index, index);
                self.open_block = Some(OpenBlock::Tool(index));
                out.push(Self::frame(
                    "content_block_start",
                    json!({
                        "type": "content_block_start",
                        "index": index,
                        "content_block": {
                            "type": "tool_use",
                            "id": tool.id,
                            "name": tool.name,
                            "input": {}
                        }
                    }),
                ));
            }
            AiStreamEvent::ToolCallArgsDelta { index, json } => {
                self.ensure_started(&mut out);
                let block_index = self.tool_index_map.get(index).copied().unwrap_or(0);
                out.push(Self::frame(
                    "content_block_delta",
                    json!({
                        "type": "content_block_delta",
                        "index": block_index,
                        "delta": { "type": "input_json_delta", "partial_json": json }
                    }),
                ));
            }
            AiStreamEvent::ToolCallEnd { index } => {
                if let Some(OpenBlock::Tool(open_index)) = self.open_block {
                    let mapped = self
                        .tool_index_map
                        .get(index)
                        .copied()
                        .unwrap_or(open_index);
                    if mapped == open_index {
                        self.close_block(&mut out);
                    }
                }
            }
            AiStreamEvent::UsageDelta { usage } => {
                // Anthropic carries usage in `message_delta`, not inline.
                self.usage = usage.clone();
            }
            AiStreamEvent::MessageEnd { finish_reason } => {
                self.ensure_started(&mut out);
                self.close_block(&mut out);
                out.push(Self::frame(
                    "message_delta",
                    json!({
                        "type": "message_delta",
                        "delta": {
                            "stop_reason": finish_to_stop_reason(*finish_reason),
                            "stop_sequence": Value::Null
                        },
                        "usage": { "output_tokens": self.usage.output_tokens }
                    }),
                ));
                out.push(Self::frame(
                    "message_stop",
                    json!({ "type": "message_stop" }),
                ));
                self.ended = true;
            }
            AiStreamEvent::Error { .. } => {
                // Surfaced by the handler's error_frame, never silently dropped.
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sb_core::ToolSpec;

    #[test]
    fn request_maps_system_tools_and_tool_results() {
        let mut req = AiRequest::new("anthropic/claude-3-5-sonnet", Vec::new());
        req.system = Some("be terse".to_string());
        req.messages.push(Message::user("weather in Paris?"));
        req.messages.push(Message {
            role: Role::Assistant,
            content: vec![ContentPart::ToolUse {
                id: "toolu_1".to_string(),
                name: "get_weather".to_string(),
                args: json!({ "city": "Paris" }),
            }],
            cache_hint: None,
        });
        req.messages.push(Message {
            role: Role::Tool,
            content: vec![ContentPart::ToolResult {
                tool_use_id: "toolu_1".to_string(),
                content: "18C sunny".to_string(),
                content_parts: Vec::new(),
                is_error: false,
            }],
            cache_hint: None,
        });
        req.tools.push(ToolSpec {
            name: "get_weather".to_string(),
            description: Some("w".to_string()),
            parameters: json!({ "type": "object" }),
        });

        let wire = request_to_anthropic_wire(&req, "claude-3-5-sonnet-latest", true).unwrap();

        // system is top-level, not a message.
        assert_eq!(wire["system"], "be terse");
        assert_eq!(wire["max_tokens"], DEFAULT_MAX_TOKENS);
        assert_eq!(wire["stream"], true);
        // tools use `input_schema`, not `parameters`/`function`.
        assert_eq!(wire["tools"][0]["name"], "get_weather");
        assert_eq!(wire["tools"][0]["input_schema"]["type"], "object");

        let messages = wire["messages"].as_array().unwrap();
        // user(text) , assistant(tool_use) , user(tool_result)
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"][0]["type"], "text");
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["content"][0]["type"], "tool_use");
        assert_eq!(messages[1]["content"][0]["input"]["city"], "Paris");
        // tool result re-enters as a user turn with a tool_result block.
        assert_eq!(messages[2]["role"], "user");
        assert_eq!(messages[2]["content"][0]["type"], "tool_result");
        assert_eq!(messages[2]["content"][0]["tool_use_id"], "toolu_1");
    }

    #[test]
    fn request_defaults_and_keeps_explicit_max_tokens() {
        let mut req = AiRequest::new("x/y", vec![Message::user("hi")]);
        req.max_output_tokens = Some(128);
        let wire = request_to_anthropic_wire(&req, "y", false).unwrap();
        assert_eq!(wire["max_tokens"], 128);
        assert_eq!(wire["stream"], false);
        assert!(wire.get("system").is_none());
        assert!(wire.get("tools").is_none());
    }

    #[test]
    fn non_stream_response_parses_text_tool_and_usage() {
        let body = json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "claude-3-5-sonnet",
            "content": [
                { "type": "text", "text": "let me check" },
                { "type": "tool_use", "id": "toolu_9", "name": "get_weather",
                  "input": { "city": "Lyon" } }
            ],
            "stop_reason": "tool_use",
            "usage": { "input_tokens": 12, "output_tokens": 7 }
        });

        let resp = parse_anthropic_response(&body).unwrap();
        assert_eq!(resp.id, "msg_1");
        assert_eq!(resp.finish_reason, FinishReason::ToolCalls);
        assert_eq!(resp.usage.input_tokens, 12);
        assert_eq!(resp.usage.output_tokens, 7);
        assert!(resp
            .message
            .content
            .iter()
            .any(|p| matches!(p, ContentPart::Text { text } if text == "let me check")));
        assert!(resp
            .message
            .content
            .iter()
            .any(|p| matches!(p, ContentPart::ToolUse { name, args, .. }
                if name == "get_weather" && args["city"] == "Lyon")));
    }

    #[test]
    fn non_stream_response_preserves_thinking_blocks() {
        let body = json!({
            "id": "msg_thinking",
            "model": "claude-sonnet-4-6",
            "content": [
                {
                    "type": "thinking",
                    "thinking": "checked constraints",
                    "signature": "sig_123"
                },
                {
                    "type": "text",
                    "text": "answer"
                }
            ],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 2, "output_tokens": 3}
        });

        let resp = parse_anthropic_response(&body).unwrap();

        assert!(resp.message.content.iter().any(|part| matches!(
            part,
            ContentPart::Reasoning { text, signature }
                if text == "checked constraints" && signature.as_deref() == Some("sig_123")
        )));
    }

    #[test]
    fn ingress_preserves_anthropic_request_knobs_and_server_tools() {
        let body = json!({
            "model": "claude-sonnet-4-6",
            "max_tokens": 2048,
            "thinking": {"type": "enabled", "budget_tokens": 1024},
            "tool_choice": {"type": "tool", "name": "web_search"},
            "stop_sequences": ["STOP"],
            "top_p": 0.8,
            "top_k": 20,
            "messages": [{"role": "user", "content": "search"}],
            "tools": [{
                "type": "web_search_20260318",
                "name": "web_search",
                "max_uses": 2
            }]
        });

        let req = request_from_anthropic(&body).unwrap();
        let wire = request_to_anthropic_wire(&req, "claude-sonnet-4-6", false).unwrap();

        assert_eq!(wire["thinking"]["budget_tokens"], 1024);
        assert_eq!(wire["tool_choice"]["name"], "web_search");
        assert_eq!(wire["stop_sequences"][0], "STOP");
        assert_eq!(wire["top_p"], 0.8);
        assert_eq!(wire["top_k"], 20);
        assert_eq!(wire["tools"][0]["type"], "web_search_20260318");
        assert_eq!(wire["tools"][0]["max_uses"], 2);
    }

    /// A realistic Anthropic SSE text stream, frame-by-frame, decoded into the
    /// canonical lifecycle. This is the streaming-fidelity proof.
    #[test]
    fn streaming_decoder_reconstructs_text_lifecycle() {
        let mut decoder = AnthropicStreamDecoder::new();
        let frames = vec![
            json!({ "type": "message_start", "message": {
                "id": "msg_2", "model": "claude-3-5-sonnet",
                "usage": { "input_tokens": 25, "output_tokens": 1 } } }),
            json!({ "type": "content_block_start", "index": 0,
                "content_block": { "type": "text", "text": "" } }),
            json!({ "type": "content_block_delta", "index": 0,
                "delta": { "type": "text_delta", "text": "Hel" } }),
            json!({ "type": "content_block_delta", "index": 0,
                "delta": { "type": "text_delta", "text": "lo" } }),
            json!({ "type": "content_block_stop", "index": 0 }),
            json!({ "type": "message_delta",
                "delta": { "stop_reason": "end_turn" },
                "usage": { "output_tokens": 5 } }),
            json!({ "type": "message_stop" }),
        ];

        let mut events = Vec::new();
        for frame in &frames {
            events.extend(decoder.decode(frame));
        }
        events.extend(decoder.finish());

        assert!(matches!(
            events.first(),
            Some(AiStreamEvent::MessageStart { id, .. }) if id == "msg_2"
        ));
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                AiStreamEvent::TextDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hello");
        assert!(events.iter().any(|e| matches!(
            e,
            AiStreamEvent::UsageDelta { usage } if usage.input_tokens == 25 && usage.output_tokens == 5
        )));
        assert!(matches!(
            events.last(),
            Some(AiStreamEvent::MessageEnd {
                finish_reason: FinishReason::Stop
            })
        ));
    }

    /// Tool-call streaming: `content_block_start{tool_use}` ->
    /// `input_json_delta`* -> `content_block_stop` must yield
    /// ToolCallStart -> ToolCallArgsDelta -> ToolCallEnd with a stable index.
    #[test]
    fn streaming_decoder_reconstructs_tool_call() {
        let mut decoder = AnthropicStreamDecoder::new();
        let frames = vec![
            json!({ "type": "message_start", "message": {
                "id": "msg_3", "model": "claude-3-5-sonnet",
                "usage": { "input_tokens": 30 } } }),
            json!({ "type": "content_block_start", "index": 0,
                "content_block": { "type": "tool_use", "id": "toolu_x", "name": "search" } }),
            json!({ "type": "content_block_delta", "index": 0,
                "delta": { "type": "input_json_delta", "partial_json": "{\"q\":" } }),
            json!({ "type": "content_block_delta", "index": 0,
                "delta": { "type": "input_json_delta", "partial_json": "\"rust\"}" } }),
            json!({ "type": "content_block_stop", "index": 0 }),
            json!({ "type": "message_delta",
                "delta": { "stop_reason": "tool_use" },
                "usage": { "output_tokens": 9 } }),
            json!({ "type": "message_stop" }),
        ];

        let mut events = Vec::new();
        for frame in &frames {
            events.extend(decoder.decode(frame));
        }

        assert!(events.iter().any(|e| matches!(
            e,
            AiStreamEvent::ToolCallStart(t) if t.index == 0 && t.id == "toolu_x" && t.name == "search"
        )));
        let args: String = events
            .iter()
            .filter_map(|e| match e {
                AiStreamEvent::ToolCallArgsDelta { json, .. } => Some(json.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(args, "{\"q\":\"rust\"}");
        assert!(events
            .iter()
            .any(|e| matches!(e, AiStreamEvent::ToolCallEnd { index: 0 })));
        assert!(matches!(
            events.last(),
            Some(AiStreamEvent::MessageEnd {
                finish_reason: FinishReason::ToolCalls
            })
        ));
    }

    #[test]
    fn ingress_maps_system_messages_tools_and_tool_result() {
        let body = json!({
            "model": "claude-3-5-sonnet",
            "system": "be terse",
            "max_tokens": 100,
            "stream": true,
            "messages": [
                { "role": "user", "content": "weather?" },
                { "role": "assistant", "content": [
                    { "type": "text", "text": "checking" },
                    { "type": "tool_use", "id": "toolu_1", "name": "get_weather",
                      "input": { "city": "Paris" } }
                ]},
                { "role": "user", "content": [
                    { "type": "tool_result", "tool_use_id": "toolu_1", "content": "18C" }
                ]}
            ],
            "tools": [
                { "name": "get_weather", "description": "w", "input_schema": { "type": "object" } }
            ]
        });

        let req = request_from_anthropic(&body).unwrap();
        assert_eq!(req.system.as_deref(), Some("be terse"));
        assert_eq!(req.max_output_tokens, Some(100));
        assert!(req.stream);
        assert_eq!(req.tools.len(), 1);
        assert_eq!(req.tools[0].name, "get_weather");
        // user , assistant(text+tool_use) , tool(tool_result)
        assert_eq!(req.messages.len(), 3);
        assert_eq!(req.messages[0].role, Role::User);
        assert_eq!(req.messages[1].role, Role::Assistant);
        assert!(req.messages[1]
            .content
            .iter()
            .any(|p| matches!(p, ContentPart::ToolUse { name, .. } if name == "get_weather")));
        assert_eq!(req.messages[2].role, Role::Tool);
        assert!(req.messages[2].content.iter().any(|p| matches!(
            p,
            ContentPart::ToolResult { tool_use_id, content, .. }
                if tool_use_id == "toolu_1" && content == "18C"
        )));
    }

    #[test]
    fn ingress_maps_image_block() {
        let body = json!({
            "model": "claude-3-5-sonnet",
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": "image/png",
                        "data": "abc"
                    }
                }]
            }]
        });

        let req = request_from_anthropic(&body).unwrap();
        assert!(req.requires_vision());
        assert!(req.messages[0].content.iter().any(|part| matches!(
            part,
            ContentPart::Image {
                source: ImageSource::InlineBase64 { media_type, data },
                ..
            } if media_type == "image/png" && data == "abc"
        )));

        let wire = request_to_anthropic_wire(&req, "claude-3-5-sonnet", false).unwrap();
        assert_eq!(wire["messages"][0]["content"][0]["type"], "image");
        assert_eq!(
            wire["messages"][0]["content"][0]["source"]["media_type"],
            "image/png"
        );
        assert_eq!(wire["messages"][0]["content"][0]["source"]["data"], "abc");
    }

    #[test]
    fn anthropic_file_ref_requires_beta_header_support() {
        let req = AiRequest::new(
            "x",
            vec![Message {
                role: Role::User,
                content: vec![ContentPart::image_file_ref(
                    Some("anthropic"),
                    "file_123",
                    None,
                )],
                cache_hint: None,
            }],
        );

        let err = request_to_anthropic_wire(&req, "claude-x", false).unwrap_err();
        assert!(err.contains("Files API beta header support"));
    }

    #[test]
    fn image_token_estimate_does_not_count_base64_as_text() {
        let req = AiRequest::new(
            "x",
            vec![Message {
                role: Role::User,
                content: vec![ContentPart::image_base64(
                    "image/png",
                    "a".repeat(1_000_000),
                )],
                cache_hint: None,
            }],
        );

        let estimate = estimate_input_tokens(&req);
        assert!(
            estimate < 10_000,
            "base64 image data should not be counted as text tokens, got {estimate}"
        );
    }

    /// Anthropic ingress -> canonical -> Anthropic wire round-trips the core
    /// shape (the hub is lossless for the fields it models).
    #[test]
    fn ingress_round_trips_to_anthropic_wire() {
        let body = json!({
            "model": "claude-x",
            "system": "sys",
            "max_tokens": 64,
            "messages": [{ "role": "user", "content": "hi" }]
        });
        let req = request_from_anthropic(&body).unwrap();
        let wire = request_to_anthropic_wire(&req, "claude-x", false).unwrap();
        assert_eq!(wire["system"], "sys");
        assert_eq!(wire["max_tokens"], 64);
        assert_eq!(wire["messages"][0]["role"], "user");
        assert_eq!(wire["messages"][0]["content"][0]["text"], "hi");
    }

    #[test]
    fn ingress_accepts_stray_system_role_messages() {
        let body = json!({
            "model": "claude-x",
            "system": "top",
            "max_tokens": 64,
            "messages": [
                { "role": "system", "content": "inside" },
                { "role": "user", "content": "hi" }
            ]
        });
        let req = request_from_anthropic(&body).unwrap();
        let wire = request_to_anthropic_wire(&req, "claude-x", false).unwrap();
        assert_eq!(wire["system"], "top\ninside");
        assert_eq!(wire["messages"][0]["role"], "user");
        assert_eq!(wire["messages"][0]["content"][0]["text"], "hi");
    }

    #[test]
    fn encoder_emits_anthropic_text_lifecycle() {
        let mut enc = AnthropicStreamEncoder::new("msg_1".into(), "claude-test".into());
        let mut frames = Vec::new();
        frames.extend(enc.encode(&AiStreamEvent::MessageStart {
            id: "msg_1".into(),
            model: "claude-test".into(),
        }));
        frames.extend(enc.encode(&AiStreamEvent::TextDelta {
            text: "Hello".into(),
        }));
        frames.extend(enc.encode(&AiStreamEvent::UsageDelta {
            usage: Usage {
                input_tokens: 5,
                output_tokens: 2,
                ..Usage::default()
            },
        }));
        frames.extend(enc.encode(&AiStreamEvent::MessageEnd {
            finish_reason: FinishReason::Stop,
        }));
        let joined = frames.join("");

        assert!(joined.contains("event: message_start"));
        assert!(joined.contains("event: content_block_start"));
        assert!(joined.contains("\"text_delta\""));
        assert!(joined.contains("\"text\":\"Hello\""));
        assert!(joined.contains("event: content_block_stop"));
        assert!(joined.contains("\"stop_reason\":\"end_turn\""));
        assert!(joined.contains("\"output_tokens\":2"));
        assert!(joined.contains("event: message_stop"));
    }

    #[test]
    fn encoder_emits_anthropic_thinking_lifecycle() {
        let mut enc = AnthropicStreamEncoder::new("msg_1".into(), "claude-test".into());
        let mut frames = Vec::new();
        frames.extend(enc.encode(&AiStreamEvent::MessageStart {
            id: "msg_1".into(),
            model: "claude-test".into(),
        }));
        frames.extend(enc.encode(&AiStreamEvent::ReasoningDelta {
            text: "thinking".into(),
        }));
        frames.extend(enc.encode(&AiStreamEvent::TextDelta {
            text: "answer".into(),
        }));
        frames.extend(enc.encode(&AiStreamEvent::MessageEnd {
            finish_reason: FinishReason::Stop,
        }));

        let joined = frames.join("");
        assert!(joined.contains("\"type\":\"thinking\""));
        assert!(joined.contains("\"type\":\"thinking_delta\""));
        assert!(joined.contains("\"thinking\":\"thinking\""));
        assert!(joined.find("\"type\":\"thinking\"") < joined.find("\"type\":\"text\""));
    }

    #[test]
    fn encoder_ignores_empty_and_late_text_after_message_stop() {
        let mut enc = AnthropicStreamEncoder::new("msg_1".into(), "claude-test".into());
        let mut frames = Vec::new();
        frames.extend(enc.encode(&AiStreamEvent::MessageStart {
            id: "msg_1".into(),
            model: "claude-test".into(),
        }));
        frames.extend(enc.encode(&AiStreamEvent::TextDelta { text: "".into() }));
        frames.extend(enc.encode(&AiStreamEvent::TextDelta {
            text: "Hello".into(),
        }));
        frames.extend(enc.encode(&AiStreamEvent::MessageEnd {
            finish_reason: FinishReason::Stop,
        }));
        frames.extend(enc.encode(&AiStreamEvent::TextDelta {
            text: "late".into(),
        }));
        frames.extend(enc.encode(&AiStreamEvent::MessageEnd {
            finish_reason: FinishReason::Stop,
        }));

        let joined = frames.join("");
        assert!(joined.contains("\"text\":\"Hello\""));
        assert_eq!(joined.matches("\"text_delta\"").count(), 1);
        assert!(!joined.contains("late"));
        assert_eq!(joined.matches("event: message_stop").count(), 1);
        assert_eq!(joined.matches("event: content_block_start").count(), 1);
    }

    #[test]
    fn encoder_emits_anthropic_tool_lifecycle() {
        let mut enc = AnthropicStreamEncoder::new("msg_2".into(), "claude-test".into());
        let mut frames = Vec::new();
        frames.extend(enc.encode(&AiStreamEvent::MessageStart {
            id: "msg_2".into(),
            model: "claude-test".into(),
        }));
        frames.extend(enc.encode(&AiStreamEvent::ToolCallStart(ToolCallStart {
            index: 0,
            id: "toolu_1".into(),
            name: "search".into(),
        })));
        frames.extend(enc.encode(&AiStreamEvent::ToolCallArgsDelta {
            index: 0,
            json: "{\"q\":\"rust\"}".into(),
        }));
        frames.extend(enc.encode(&AiStreamEvent::ToolCallEnd { index: 0 }));
        frames.extend(enc.encode(&AiStreamEvent::MessageEnd {
            finish_reason: FinishReason::ToolCalls,
        }));
        let joined = frames.join("");

        assert!(joined.contains("\"type\":\"tool_use\""));
        assert!(joined.contains("\"name\":\"search\""));
        assert!(joined.contains("\"input_json_delta\""));
        assert!(joined.contains("\"stop_reason\":\"tool_use\""));
        assert!(joined.contains("event: message_stop"));
    }
}
