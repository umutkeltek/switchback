//! OpenAI **Responses API** (`/v1/responses`) <-> canonical IR. This is the
//! API Codex speaks. Like every protocol it translates only to/from the
//! canonical IR (never directly to Chat Completions); the openai_compatible
//! adapter then speaks Chat Completions to the upstream. Hub-and-spoke.

use sb_core::{
    AiRequest, AiResponse, AiStreamEvent, ContentPart, FinishReason, Message, Role, ToolSpec, Usage,
};
use serde_json::{json, Value};

/// Responses request body -> canonical `AiRequest`.
pub fn request_from_openai_responses(body: &Value) -> Result<AiRequest, String> {
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing or invalid `model`".to_string())?
        .to_string();

    let mut req = AiRequest::new(model, Vec::new());

    // Top-level `instructions` is the system prompt.
    if let Some(instructions) = body.get("instructions").and_then(Value::as_str) {
        if !instructions.is_empty() {
            req.system = Some(instructions.to_string());
        }
    }

    // `input` is a plain string OR an array of typed input items.
    match body.get("input") {
        Some(Value::String(text)) => req.messages.push(Message::user(text.clone())),
        Some(Value::Array(items)) => {
            for item in items {
                parse_input_item(item, &mut req)?;
            }
        }
        _ => return Err("missing or invalid `input`".to_string()),
    }

    // Responses tools are flat: {type:"function", name, description, parameters}.
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        for tool in tools {
            if tool.get("type").and_then(Value::as_str) == Some("function") {
                if let Some(name) = tool.get("name").and_then(Value::as_str) {
                    req.tools.push(ToolSpec {
                        name: name.to_string(),
                        description: tool
                            .get("description")
                            .and_then(Value::as_str)
                            .map(ToString::to_string),
                        parameters: tool.get("parameters").cloned().unwrap_or(Value::Null),
                    });
                }
            }
        }
    }

    req.stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    req.temperature = body
        .get("temperature")
        .and_then(Value::as_f64)
        .map(|v| v as f32);
    req.max_output_tokens = body
        .get("max_output_tokens")
        .and_then(Value::as_u64)
        .and_then(|v| u32::try_from(v).ok());

    const MODELED: &[&str] = &[
        "model",
        "instructions",
        "input",
        "tools",
        "stream",
        "temperature",
        "max_output_tokens",
    ];
    if let Some(obj) = body.as_object() {
        for (key, value) in obj {
            if !MODELED.contains(&key.as_str()) {
                req.passthrough.insert(key.clone(), value.clone());
            }
        }
    }

    req.id = sb_core::new_id("req");
    Ok(req)
}

fn parse_input_item(item: &Value, req: &mut AiRequest) -> Result<(), String> {
    match item
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("message")
    {
        "message" => {
            let role = item.get("role").and_then(Value::as_str).unwrap_or("user");
            let text = content_to_text(item.get("content"))?;
            match role {
                "system" | "developer" => {
                    if !text.is_empty() {
                        req.system = Some(match req.system.take() {
                            Some(existing) => format!("{existing}\n{text}"),
                            None => text,
                        });
                    }
                }
                "assistant" => req.messages.push(Message {
                    role: Role::Assistant,
                    content: vec![ContentPart::text(text)],
                }),
                _ => req.messages.push(Message {
                    role: Role::User,
                    content: vec![ContentPart::text(text)],
                }),
            }
        }
        "function_call" => {
            let id = item
                .get("call_id")
                .or_else(|| item.get("id"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let name = item
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let args = item
                .get("arguments")
                .and_then(Value::as_str)
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or(Value::Null);
            req.messages.push(Message {
                role: Role::Assistant,
                content: vec![ContentPart::ToolUse { id, name, args }],
            });
        }
        "function_call_output" => {
            let id = item
                .get("call_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let output = item
                .get("output")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            req.messages.push(Message {
                role: Role::Tool,
                content: vec![ContentPart::ToolResult {
                    tool_use_id: id,
                    content: output,
                    is_error: false,
                }],
            });
        }
        other => {
            return Err(format!("unsupported Responses input item `{other}`"));
        }
    }
    Ok(())
}

fn content_to_text(content: Option<&Value>) -> Result<String, String> {
    match content {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(s)) => Ok(s.clone()),
        Some(Value::Array(parts)) => {
            let mut text = String::new();
            for part in parts {
                match part.get("type").and_then(Value::as_str) {
                    Some("input_text") | Some("output_text") => {
                        if let Some(part_text) = part.get("text").and_then(Value::as_str) {
                            text.push_str(part_text);
                        } else {
                            return Err(
                                "Responses text content part missing string `text`".to_string()
                            );
                        }
                    }
                    Some(other) => {
                        return Err(format!(
                            "unsupported Responses content part `{other}`; multimodal content is not supported in this build"
                        ));
                    }
                    None => return Err("Responses content part missing string `type`".to_string()),
                }
            }
            Ok(text)
        }
        Some(_) => Err("Responses message content must be a string, null, or array".to_string()),
    }
}

/// Canonical `AiResponse` -> a non-streaming Responses object.
pub fn response_to_openai_responses(resp: &AiResponse) -> Value {
    let mut output = Vec::new();

    let text = resp.message.text();
    if !text.is_empty() {
        output.push(json!({
            "type": "message",
            "id": sb_core::new_id("msg"),
            "status": "completed",
            "role": "assistant",
            "content": [{"type": "output_text", "text": text, "annotations": []}],
        }));
    }

    for part in &resp.message.content {
        if let ContentPart::ToolUse { id, name, args } = part {
            output.push(json!({
                "type": "function_call",
                "id": sb_core::new_id("fc"),
                "call_id": id,
                "name": name,
                "arguments": serde_json::to_string(args).unwrap_or_else(|_| "{}".to_string()),
                "status": "completed",
            }));
        }
    }

    json!({
        "id": resp.id,
        "object": "response",
        "status": "completed",
        "model": resp.model,
        "output": output,
        "usage": usage_json(&resp.usage),
    })
}

/// Canonical `AiRequest` -> upstream Responses request body.
pub fn request_to_openai_responses_wire(req: &AiRequest, model: &str, stream: bool) -> Value {
    let mut body = serde_json::Map::new();
    body.insert("model".to_string(), Value::String(model.to_string()));
    body.insert("stream".to_string(), Value::Bool(stream));
    if let Some(system) = req.system.as_deref().filter(|system| !system.is_empty()) {
        body.insert(
            "instructions".to_string(),
            Value::String(system.to_string()),
        );
    }

    let mut input = Vec::new();
    for message in &req.messages {
        match message.role {
            Role::System => {
                let text = message.text();
                if !text.is_empty() {
                    body.insert("instructions".to_string(), Value::String(text));
                }
            }
            Role::User | Role::Assistant => {
                let text = message.text();
                if !text.is_empty() {
                    let part_type = if message.role == Role::Assistant {
                        "output_text"
                    } else {
                        "input_text"
                    };
                    let role = if message.role == Role::Assistant {
                        "assistant"
                    } else {
                        "user"
                    };
                    input.push(json!({
                        "type": "message",
                        "role": role,
                        "content": [{ "type": part_type, "text": text }],
                    }));
                }
                for part in &message.content {
                    if let ContentPart::ToolUse { id, name, args } = part {
                        input.push(json!({
                            "type": "function_call",
                            "call_id": id,
                            "name": name,
                            "arguments": serde_json::to_string(args).unwrap_or_else(|_| "{}".to_string()),
                        }));
                    }
                }
            }
            Role::Tool => {
                for part in &message.content {
                    if let ContentPart::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } = part
                    {
                        input.push(json!({
                            "type": "function_call_output",
                            "call_id": tool_use_id,
                            "output": content,
                        }));
                    }
                }
            }
        }
    }
    body.insert("input".to_string(), Value::Array(input));

    if !req.tools.is_empty() {
        body.insert(
            "tools".to_string(),
            Value::Array(
                req.tools
                    .iter()
                    .map(|tool| {
                        json!({
                            "type": "function",
                            "name": tool.name,
                            "description": tool.description,
                            "parameters": tool.parameters,
                        })
                    })
                    .collect(),
            ),
        );
    }
    if let Some(value) = req.temperature {
        body.insert("temperature".to_string(), json!(value));
    }
    if let Some(value) = req.max_output_tokens {
        body.insert("max_output_tokens".to_string(), json!(value));
    }
    for (key, value) in &req.passthrough {
        body.entry(key.clone()).or_insert_with(|| value.clone());
    }
    Value::Object(body)
}

/// Upstream Responses object -> canonical `AiResponse`.
pub fn parse_openai_responses_response(body: &Value) -> Result<AiResponse, String> {
    let id = body
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("resp")
        .to_string();
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let mut content = Vec::new();

    let output = body
        .get("output")
        .and_then(Value::as_array)
        .ok_or_else(|| "Responses response missing `output` array".to_string())?;
    for item in output {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                let text = content_to_text(item.get("content"))?;
                if !text.is_empty() {
                    content.push(ContentPart::text(text));
                }
            }
            Some("function_call") => {
                let id = item
                    .get("call_id")
                    .or_else(|| item.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let name = item
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let args = item
                    .get("arguments")
                    .and_then(Value::as_str)
                    .and_then(|args| serde_json::from_str(args).ok())
                    .unwrap_or(Value::Null);
                content.push(ContentPart::ToolUse { id, name, args });
            }
            Some(_) | None => {}
        }
    }

    Ok(AiResponse {
        id,
        model,
        message: Message {
            role: Role::Assistant,
            content,
        },
        finish_reason: finish_reason_from_status(body.get("status").and_then(Value::as_str)),
        usage: parse_usage(body.get("usage")),
    })
}

fn finish_reason_from_status(status: Option<&str>) -> FinishReason {
    match status {
        Some("incomplete") => FinishReason::Length,
        Some("failed") => FinishReason::Error,
        Some(_) | None => FinishReason::Stop,
    }
}

fn parse_usage(value: Option<&Value>) -> Usage {
    let Some(value) = value else {
        return Usage::default();
    };
    Usage {
        input_tokens: value
            .get("input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or_default(),
        cached_input_tokens: value
            .pointer("/input_tokens_details/cached_tokens")
            .and_then(Value::as_u64)
            .unwrap_or_default(),
        output_tokens: value
            .get("output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or_default(),
        reasoning_tokens: value
            .pointer("/output_tokens_details/reasoning_tokens")
            .and_then(Value::as_u64)
            .unwrap_or_default(),
    }
}

fn usage_json(usage: &Usage) -> Value {
    json!({
        "input_tokens": usage.input_tokens,
        "output_tokens": usage.output_tokens,
        "total_tokens": usage.total(),
    })
}

fn finish_str(reason: FinishReason) -> &'static str {
    match reason {
        FinishReason::Stop => "completed",
        FinishReason::Length => "incomplete",
        FinishReason::ToolCalls => "completed",
        FinishReason::ContentFilter => "incomplete",
        FinishReason::Error => "failed",
    }
}

/// Streaming encoder: canonical `AiStreamEvent`s -> Responses-API SSE events.
/// Handles text streaming fully (the common Codex path) and emits any tool
/// calls as complete `function_call` output items at finish.
pub struct OpenAiResponsesStreamEncoder {
    response_id: String,
    model: String,
    item_id: String,
    output_index: u32,
    text_open: bool,
    text: String,
    usage: Usage,
    status: &'static str,
    tool_calls: Vec<(String, String, String)>, // (call_id, name, args)
    cur_tool: Option<(String, String, String)>,
}

impl OpenAiResponsesStreamEncoder {
    pub fn new(response_id: String, model: String) -> Self {
        let item_id = sb_core::new_id("msg");
        OpenAiResponsesStreamEncoder {
            response_id,
            model,
            item_id,
            output_index: 0,
            text_open: false,
            text: String::new(),
            usage: Usage::default(),
            status: "completed",
            tool_calls: Vec::new(),
            cur_tool: None,
        }
    }

    fn frame(event: &str, data: Value) -> String {
        format!("event: {event}\ndata: {data}\n\n")
    }

    fn created(&self) -> String {
        Self::frame(
            "response.created",
            json!({"type":"response.created","response":{
                "id": self.response_id, "object":"response", "status":"in_progress", "model": self.model, "output":[]
            }}),
        )
    }

    /// Translate one canonical event into zero or more Responses SSE frames.
    pub fn encode(&mut self, event: &AiStreamEvent) -> Vec<String> {
        let mut out = Vec::new();
        match event {
            AiStreamEvent::MessageStart { .. } => {
                out.push(self.created());
            }
            AiStreamEvent::TextDelta { text } => {
                if !self.text_open {
                    // lazily open the message item + text content part
                    self.text_open = true;
                    out.push(Self::frame(
                        "response.output_item.added",
                        json!({"type":"response.output_item.added","output_index":self.output_index,
                            "item":{"type":"message","id":self.item_id,"status":"in_progress","role":"assistant","content":[]}}),
                    ));
                    out.push(Self::frame(
                        "response.content_part.added",
                        json!({"type":"response.content_part.added","item_id":self.item_id,
                            "output_index":self.output_index,"content_index":0,
                            "part":{"type":"output_text","text":"","annotations":[]}}),
                    ));
                }
                self.text.push_str(text);
                out.push(Self::frame(
                    "response.output_text.delta",
                    json!({"type":"response.output_text.delta","item_id":self.item_id,
                        "output_index":self.output_index,"content_index":0,"delta":text}),
                ));
            }
            AiStreamEvent::ToolCallStart(start) => {
                if let Some(tc) = self.cur_tool.take() {
                    self.tool_calls.push(tc);
                }
                self.cur_tool = Some((start.id.clone(), start.name.clone(), String::new()));
            }
            AiStreamEvent::ToolCallArgsDelta { json, .. } => {
                if let Some((_, _, args)) = self.cur_tool.as_mut() {
                    args.push_str(json);
                }
            }
            AiStreamEvent::ToolCallEnd { .. } => {
                if let Some(tc) = self.cur_tool.take() {
                    self.tool_calls.push(tc);
                }
            }
            AiStreamEvent::UsageDelta { usage } => self.usage = usage.clone(),
            AiStreamEvent::MessageEnd { finish_reason } => {
                self.status = finish_str(*finish_reason);
                out.extend(self.finish());
            }
            AiStreamEvent::Error { message, .. } => {
                self.status = "failed";
                out.push(Self::frame(
                    "response.failed",
                    json!({"type":"response.failed","response":{"id":self.response_id,"status":"failed",
                        "error":{"message":message}}}),
                ));
            }
            AiStreamEvent::ReasoningDelta { .. } => {}
        }
        out
    }

    /// Close out text + tool items and emit `response.completed`.
    fn finish(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        let mut output_items = Vec::new();

        if self.text_open {
            out.push(Self::frame(
                "response.output_text.done",
                json!({"type":"response.output_text.done","item_id":self.item_id,
                    "output_index":self.output_index,"content_index":0,"text":self.text}),
            ));
            out.push(Self::frame(
                "response.content_part.done",
                json!({"type":"response.content_part.done","item_id":self.item_id,
                    "output_index":self.output_index,"content_index":0,
                    "part":{"type":"output_text","text":self.text,"annotations":[]}}),
            ));
            let item = json!({"type":"message","id":self.item_id,"status":"completed","role":"assistant",
                "content":[{"type":"output_text","text":self.text,"annotations":[]}]});
            out.push(Self::frame(
                "response.output_item.done",
                json!({"type":"response.output_item.done","output_index":self.output_index,"item":item.clone()}),
            ));
            output_items.push(item);
            self.output_index += 1;
        }

        if let Some(tc) = self.cur_tool.take() {
            self.tool_calls.push(tc);
        }
        for (call_id, name, args) in &self.tool_calls {
            let fc_id = sb_core::new_id("fc");
            let item = json!({"type":"function_call","id":fc_id,"call_id":call_id,"name":name,
                "arguments":args,"status":"completed"});
            out.push(Self::frame(
                "response.output_item.added",
                json!({"type":"response.output_item.added","output_index":self.output_index,"item":item.clone()}),
            ));
            out.push(Self::frame(
                "response.output_item.done",
                json!({"type":"response.output_item.done","output_index":self.output_index,"item":item.clone()}),
            ));
            output_items.push(item);
            self.output_index += 1;
        }

        out.push(Self::frame(
            "response.completed",
            json!({"type":"response.completed","response":{
                "id":self.response_id,"object":"response","status":self.status,"model":self.model,
                "output":output_items,"usage":usage_json(&self.usage)
            }}),
        ));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_string_input_and_instructions() {
        let body = json!({"model":"x/y","instructions":"be terse","input":"hello"});
        let req = request_from_openai_responses(&body).unwrap();
        assert_eq!(req.system.as_deref(), Some("be terse"));
        assert_eq!(req.last_user_text().as_deref(), Some("hello"));
    }

    #[test]
    fn parses_array_input_with_message_and_function_items() {
        let body = json!({"model":"x/y","input":[
            {"type":"message","role":"user","content":[{"type":"input_text","text":"weather?"}]},
            {"type":"function_call","call_id":"c1","name":"get_weather","arguments":"{\"city\":\"Paris\"}"},
            {"type":"function_call_output","call_id":"c1","output":"sunny"}
        ]});
        let req = request_from_openai_responses(&body).unwrap();
        assert_eq!(req.messages.len(), 3);
        assert!(matches!(
            req.messages[1].content[0],
            ContentPart::ToolUse { .. }
        ));
        assert!(matches!(
            req.messages[2].content[0],
            ContentPart::ToolResult { .. }
        ));
    }

    #[test]
    fn rejects_image_content_not_silent() {
        let body = json!({"model":"x/y","input":[{
            "type":"message",
            "role":"user",
            "content":[{"type":"input_image","image_url":"data:image/png;base64,abc"}]
        }]});

        let err = request_from_openai_responses(&body).unwrap_err();
        assert!(err.contains("unsupported Responses content part `input_image`"));
    }

    #[test]
    fn non_stream_response_has_message_output_item() {
        let resp = AiResponse {
            id: "resp_1".into(),
            model: "x/y".into(),
            message: Message::assistant("hi there"),
            finish_reason: FinishReason::Stop,
            usage: Usage {
                input_tokens: 3,
                output_tokens: 2,
                ..Usage::default()
            },
        };
        let v = response_to_openai_responses(&resp);
        assert_eq!(v["object"], "response");
        assert_eq!(v["status"], "completed");
        assert_eq!(v["output"][0]["type"], "message");
        assert_eq!(v["output"][0]["content"][0]["text"], "hi there");
        assert_eq!(v["usage"]["total_tokens"], 5);
    }

    #[test]
    fn canonical_request_maps_to_responses_upstream_body() {
        let mut req = AiRequest::new("client-model", vec![Message::user("hi")]);
        req.system = Some("be terse".into());
        req.tools.push(ToolSpec {
            name: "lookup".into(),
            description: Some("lookup things".into()),
            parameters: json!({"type":"object"}),
        });
        req.max_output_tokens = Some(8);

        let body = request_to_openai_responses_wire(&req, "gpt-5.5", false);

        assert_eq!(body["model"], "gpt-5.5");
        assert_eq!(body["stream"], false);
        assert_eq!(body["instructions"], "be terse");
        assert_eq!(body["input"][0]["type"], "message");
        assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(body["tools"][0]["name"], "lookup");
        assert_eq!(body["max_output_tokens"], 8);
    }

    #[test]
    fn parses_responses_upstream_object() {
        let body = json!({
            "id": "resp_fake",
            "object": "response",
            "status": "completed",
            "model": "gpt-5.5",
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{ "type": "output_text", "text": "pong", "annotations": [] }]
            }],
            "usage": {
                "input_tokens": 10,
                "input_tokens_details": { "cached_tokens": 2 },
                "output_tokens": 3,
                "output_tokens_details": { "reasoning_tokens": 1 }
            }
        });

        let parsed = parse_openai_responses_response(&body).unwrap();
        assert_eq!(parsed.id, "resp_fake");
        assert_eq!(parsed.model, "gpt-5.5");
        assert_eq!(parsed.message.text(), "pong");
        assert_eq!(parsed.usage.input_tokens, 10);
        assert_eq!(parsed.usage.cached_input_tokens, 2);
        assert_eq!(parsed.usage.output_tokens, 3);
        assert_eq!(parsed.usage.reasoning_tokens, 1);
    }

    #[test]
    fn streaming_encoder_emits_created_delta_completed() {
        let mut enc = OpenAiResponsesStreamEncoder::new("resp_1".into(), "x/y".into());
        let mut frames = String::new();
        frames.push_str(
            &enc.encode(&AiStreamEvent::MessageStart {
                id: "resp_1".into(),
                model: "x/y".into(),
            })
            .join(""),
        );
        frames.push_str(
            &enc.encode(&AiStreamEvent::TextDelta { text: "Hel".into() })
                .join(""),
        );
        frames.push_str(
            &enc.encode(&AiStreamEvent::TextDelta { text: "lo".into() })
                .join(""),
        );
        frames.push_str(
            &enc.encode(&AiStreamEvent::MessageEnd {
                finish_reason: FinishReason::Stop,
            })
            .join(""),
        );

        assert!(frames.contains("event: response.created"));
        assert!(frames.contains("event: response.output_item.added"));
        assert!(frames.contains("event: response.output_text.delta"));
        assert!(frames.contains("\"delta\":\"Hel\""));
        assert!(frames.contains("event: response.output_text.done"));
        assert!(frames.contains("\"text\":\"Hello\""));
        assert!(frames.contains("event: response.completed"));
    }
}
