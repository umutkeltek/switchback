//! Anthropic **Messages API** (`/v1/messages`) <-> canonical IR. A genuinely
//! different wire format from OpenAI — `x-api-key` auth, a top-level `system`
//! field (not a message), typed content blocks, and named SSE events
//! (`message_start` / `content_block_delta` / …) instead of `choices[].delta`.
//! This module is the *upstream* half (canonical -> Anthropic body, Anthropic
//! stream -> canonical events); the ingress half (Anthropic-shaped client
//! requests) is a separate future step. Like every protocol it translates only
//! to/from the canonical IR — never directly to another wire format.

use sb_core::{
    AiRequest, AiResponse, AiStreamEvent, ContentPart, FinishReason, Message, Role, ToolCallStart,
    Usage,
};
use serde_json::{json, Map, Value};
use std::collections::BTreeSet;

/// Anthropic requires `max_tokens`; use this when the request didn't set one.
const DEFAULT_MAX_TOKENS: u32 = 4096;
/// The Messages API version this module targets.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

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
fn message_content_blocks(message: &Message) -> Option<Vec<Value>> {
    let mut blocks = Vec::new();
    for part in &message.content {
        match part {
            ContentPart::Text { text } => {
                if !text.is_empty() {
                    blocks.push(json!({ "type": "text", "text": text }));
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
        }
    }
    if blocks.is_empty() {
        None
    } else {
        Some(blocks)
    }
}

/// Canonical `AiRequest` -> Anthropic Messages request body.
pub fn request_to_anthropic_wire(req: &AiRequest, upstream_model: &str, stream: bool) -> Value {
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
                let text = message.text();
                if !text.is_empty() {
                    system_chunks.push(text);
                }
            }
            // Tool results are carried back to Anthropic inside a USER turn.
            Role::Tool => {
                if let Some(blocks) = message_content_blocks(message) {
                    messages.push(json!({ "role": "user", "content": blocks }));
                }
            }
            Role::User => {
                if let Some(blocks) = message_content_blocks(message) {
                    messages.push(json!({ "role": "user", "content": blocks }));
                }
            }
            Role::Assistant => {
                if let Some(blocks) = message_content_blocks(message) {
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

    if !req.tools.is_empty() {
        let tools: Vec<Value> = req
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
        body.insert("tools".to_string(), Value::Array(tools));
    }

    if let Some(temperature) = req.temperature {
        if let Some(number) = serde_json::Number::from_f64(f64::from(temperature)) {
            body.insert("temperature".to_string(), Value::Number(number));
        }
    }

    Value::Object(body)
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
            // thinking / redacted_thinking / unknown blocks: ignored in v1.
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
            ContentPart::ToolResult { .. } => None,
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
                        if let Some(text) = delta.and_then(|d| d.get("text")).and_then(Value::as_str)
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
                        if let Some(text) =
                            delta.and_then(|d| d.get("thinking")).and_then(Value::as_str)
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
        });
        req.messages.push(Message {
            role: Role::Tool,
            content: vec![ContentPart::ToolResult {
                tool_use_id: "toolu_1".to_string(),
                content: "18C sunny".to_string(),
                is_error: false,
            }],
        });
        req.tools.push(ToolSpec {
            name: "get_weather".to_string(),
            description: Some("w".to_string()),
            parameters: json!({ "type": "object" }),
        });

        let wire = request_to_anthropic_wire(&req, "claude-3-5-sonnet-latest", true);

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
        let wire = request_to_anthropic_wire(&req, "y", false);
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
        assert!(resp.message.content.iter().any(
            |p| matches!(p, ContentPart::ToolUse { name, args, .. }
                if name == "get_weather" && args["city"] == "Lyon")
        ));
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
            Some(AiStreamEvent::MessageEnd { finish_reason: FinishReason::Stop })
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
            Some(AiStreamEvent::MessageEnd { finish_reason: FinishReason::ToolCalls })
        ));
    }
}
