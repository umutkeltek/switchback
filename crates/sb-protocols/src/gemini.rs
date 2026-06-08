//! Google **Gemini GenerateContent** API <-> canonical IR (upstream half). A
//! third wire paradigm distinct from OpenAI and Anthropic — `x-goog-api-key`
//! auth, `contents[].parts[]` instead of messages, `role: user|model`, a
//! top-level `systemInstruction`, `functionCall`/`functionResponse` parts, and
//! crucially **no tool-call IDs** (Gemini correlates tool results by function
//! *name*). Exercising the IR against this is the real test that the hub
//! generalizes. Like every protocol it translates only to/from the canonical IR.

use std::collections::HashMap;

use sb_core::{
    AiRequest, AiResponse, AiStreamEvent, ContentPart, FinishReason, ImageSource, Message, Role,
    ToolCallStart, Usage,
};
use serde_json::{json, Map, Number, Value};

use crate::schema::{DownlevelResult, SchemaCaps, SchemaWarning};

/// Default public Gemini endpoint.
pub const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com";

fn finish_reason_from_gemini(reason: Option<&str>, had_tool_call: bool) -> FinishReason {
    // Gemini has no distinct tool-call finish reason; infer it from content.
    if had_tool_call {
        return FinishReason::ToolCalls;
    }
    match reason {
        Some("STOP") => FinishReason::Stop,
        Some("MAX_TOKENS") => FinishReason::Length,
        Some("SAFETY") | Some("BLOCKLIST") | Some("PROHIBITED_CONTENT") => {
            FinishReason::ContentFilter
        }
        _ => FinishReason::Stop,
    }
}

fn finish_reason_to_gemini(reason: FinishReason) -> &'static str {
    match reason {
        FinishReason::Stop | FinishReason::ToolCalls | FinishReason::Error => "STOP",
        FinishReason::Length => "MAX_TOKENS",
        FinishReason::ContentFilter => "SAFETY",
    }
}

fn split_data_url(url: &str) -> Option<(&str, &str)> {
    let rest = url.strip_prefix("data:")?;
    let (media_type, data) = rest.split_once(";base64,")?;
    if media_type.is_empty() || data.is_empty() {
        return None;
    }
    Some((media_type, data))
}

fn image_to_gemini_part(part: &ContentPart) -> Result<Option<Value>, String> {
    let ContentPart::Image { source, .. } = part else {
        return Ok(None);
    };

    source.validate()?;
    let part = match source {
        ImageSource::InlineBase64 { media_type, data } => json!({
            "inlineData": {
                "mimeType": media_type,
                "data": data,
            }
        }),
        ImageSource::RemoteUrl { url } => {
            if let Some((media_type, data)) = split_data_url(url) {
                json!({
                    "inlineData": {
                        "mimeType": media_type,
                        "data": data,
                    }
                })
            } else {
                json!({
                    "fileData": {
                        "mimeType": "image/png",
                        "fileUri": url,
                    }
                })
            }
        }
        ImageSource::ProviderFileRef { provider, id } => {
            if provider
                .as_deref()
                .is_some_and(|owner| owner != "gemini" && owner != "vertex")
            {
                return Err(format!(
                    "Gemini cannot encode provider file ref owned by `{}`",
                    provider.as_deref().unwrap_or_default()
                ));
            }
            json!({
                "fileData": {
                    "mimeType": "image/png",
                    "fileUri": id,
                }
            })
        }
    };
    Ok(Some(part))
}

fn user_parts(message: &Message) -> Result<Vec<Value>, String> {
    let mut parts = Vec::new();
    for part in &message.content {
        match part {
            ContentPart::Text { text } if !text.is_empty() => {
                parts.push(json!({ "text": text }));
            }
            ContentPart::Image { .. } => {
                if let Some(image) = image_to_gemini_part(part)? {
                    parts.push(image);
                }
            }
            _ => {}
        }
    }
    Ok(parts)
}

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

fn downlevel_gemini_schema(
    schema: &Value,
    path_prefix: &str,
    warnings: &mut Vec<SchemaWarning>,
) -> Value {
    let DownlevelResult {
        schema,
        warnings: schema_warnings,
    } = crate::schema::downlevel_with_warnings(schema, &SchemaCaps::gemini());
    warnings.extend(
        schema_warnings
            .into_iter()
            .map(|warning| warning.prepend_path(path_prefix)),
    );
    schema
}

/// Return only the target-specific schema downlevel warnings Gemini/Vertex would
/// produce for this request.
pub fn schema_downlevel_warnings(req: &AiRequest) -> Vec<SchemaWarning> {
    request_to_gemini_wire_with_warnings(req)
        .map(|(_, warnings)| warnings)
        .unwrap_or_default()
}

/// Canonical `AiRequest` -> Gemini `generateContent` request body. (The model
/// goes in the URL path, not the body, so it isn't taken here.)
pub fn request_to_gemini_wire(req: &AiRequest) -> Result<Value, String> {
    request_to_gemini_wire_with_warnings(req).map(|(body, _warnings)| body)
}

/// Canonical `AiRequest` -> Gemini `generateContent` request body plus warnings
/// for any lossy target-dialect schema rewrites.
pub fn request_to_gemini_wire_with_warnings(
    req: &AiRequest,
) -> Result<(Value, Vec<SchemaWarning>), String> {
    req.validate_canonical()?;
    let mut warnings = Vec::new();
    // Gemini strips tool-call IDs and correlates tool *results* by function
    // name, so pre-build id -> name from the assistant's tool calls.
    let mut tool_names: HashMap<&str, &str> = HashMap::new();
    for message in &req.messages {
        for part in &message.content {
            if let ContentPart::ToolUse { id, name, .. } = part {
                tool_names.insert(id.as_str(), name.as_str());
            }
        }
    }

    let mut system_chunks = Vec::new();
    if let Some(system) = &req.system {
        if !system.is_empty() {
            system_chunks.push(system.clone());
        }
    }

    let mut contents = Vec::new();
    for message in &req.messages {
        match message.role {
            Role::System => {
                reject_image_parts(&message.content, "Gemini system messages")?;
                let text = message.text();
                if !text.is_empty() {
                    system_chunks.push(text);
                }
            }
            Role::User => {
                let parts = user_parts(message)?;
                if !parts.is_empty() {
                    contents.push(json!({ "role": "user", "parts": parts }));
                }
            }
            Role::Assistant => {
                reject_image_parts(&message.content, "Gemini assistant messages")?;
                let mut parts = Vec::new();
                for part in &message.content {
                    match part {
                        ContentPart::Text { text } if !text.is_empty() => {
                            parts.push(json!({ "text": text }));
                        }
                        ContentPart::ToolUse { name, args, .. } => {
                            parts.push(json!({
                                "functionCall": { "name": name, "args": args }
                            }));
                        }
                        _ => {}
                    }
                }
                if !parts.is_empty() {
                    contents.push(json!({ "role": "model", "parts": parts }));
                }
            }
            Role::Tool => {
                reject_image_parts(&message.content, "Gemini tool messages")?;
                let mut parts = Vec::new();
                for part in &message.content {
                    if let ContentPart::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } = part
                    {
                        // Correlate back to the function name (Gemini has no id).
                        let name = tool_names
                            .get(tool_use_id.as_str())
                            .copied()
                            .unwrap_or(tool_use_id.as_str());
                        parts.push(json!({
                            "functionResponse": {
                                "name": name,
                                "response": { "result": content }
                            }
                        }));
                    }
                }
                if !parts.is_empty() {
                    // Gemini carries tool results back in a `user` turn.
                    contents.push(json!({ "role": "user", "parts": parts }));
                }
            }
        }
    }

    let mut body = Map::new();
    body.insert("contents".to_string(), Value::Array(contents));

    if !system_chunks.is_empty() {
        body.insert(
            "systemInstruction".to_string(),
            json!({ "parts": [{ "text": system_chunks.join("\n") }] }),
        );
    }

    if !req.tools.is_empty() {
        let declarations: Vec<Value> = req
            .tools
            .iter()
            .map(|tool| {
                let mut decl = Map::new();
                decl.insert("name".to_string(), Value::String(tool.name.clone()));
                if let Some(description) = &tool.description {
                    decl.insert(
                        "description".to_string(),
                        Value::String(description.clone()),
                    );
                }
                if !tool.parameters.is_null() {
                    // Downlevel the tool schema to Gemini's restricted dialect
                    // (no anyOf/const/$ref/type-arrays, string enums) instead of
                    // letting a complex schema 400 the request.
                    let prefix = format!("/tools/{}/parameters", tool.name);
                    decl.insert(
                        "parameters".to_string(),
                        downlevel_gemini_schema(&tool.parameters, &prefix, &mut warnings),
                    );
                }
                Value::Object(decl)
            })
            .collect();
        body.insert(
            "tools".to_string(),
            json!([{ "functionDeclarations": declarations }]),
        );
    }

    let mut generation = Map::new();
    if let Some(temperature) = req.temperature {
        if let Some(number) = Number::from_f64(f64::from(temperature)) {
            generation.insert("temperature".to_string(), Value::Number(number));
        }
    }
    if let Some(max_tokens) = req.max_output_tokens {
        generation.insert(
            "maxOutputTokens".to_string(),
            Value::Number(Number::from(max_tokens)),
        );
    }
    // Structured output: Gemini takes a JSON mime type + an (optional) schema on
    // generationConfig — no `response_format` object. A JSON-Schema is run
    // through the downleveler first so features Gemini rejects (anyOf, $ref,
    // additionalProperties, const, type-arrays) don't 400 the request.
    match &req.response_format {
        Some(sb_core::ResponseFormat::JsonObject) => {
            generation.insert(
                "responseMimeType".to_string(),
                Value::String("application/json".to_string()),
            );
        }
        Some(sb_core::ResponseFormat::JsonSchema { schema, .. }) => {
            generation.insert(
                "responseMimeType".to_string(),
                Value::String("application/json".to_string()),
            );
            generation.insert(
                "responseSchema".to_string(),
                downlevel_gemini_schema(schema, "/response_format/schema", &mut warnings),
            );
        }
        Some(sb_core::ResponseFormat::Text) | None => {}
    }
    if !generation.is_empty() {
        body.insert("generationConfig".to_string(), Value::Object(generation));
    }

    Ok((Value::Object(body), warnings))
}

fn usage_from_gemini(meta: Option<&Value>) -> Usage {
    let meta = meta.and_then(Value::as_object);
    Usage {
        input_tokens: meta
            .and_then(|m| m.get("promptTokenCount"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        output_tokens: meta
            .and_then(|m| m.get("candidatesTokenCount"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cached_input_tokens: meta
            .and_then(|m| m.get("cachedContentTokenCount"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        ..Usage::default()
    }
}

/// Parse the parts of `candidates[0].content` into canonical content. Tool calls
/// get a synthesized stable id (`call_<index>`) since Gemini provides none.
fn parse_parts(parts: &[Value], tool_counter: &mut u32) -> Vec<ContentPart> {
    let mut out = Vec::new();
    for part in parts {
        if let Some(text) = part.get("text").and_then(Value::as_str) {
            if !text.is_empty() {
                out.push(ContentPart::text(text));
            }
        } else if let Some(call) = part.get("functionCall").and_then(Value::as_object) {
            let name = call.get("name").and_then(Value::as_str).unwrap_or("");
            let args = call.get("args").cloned().unwrap_or(Value::Null);
            out.push(ContentPart::ToolUse {
                id: format!("call_{tool_counter}"),
                name: name.to_string(),
                args,
            });
            *tool_counter += 1;
        }
    }
    out
}

/// Non-streaming Gemini response -> canonical `AiResponse`.
pub fn parse_gemini_response(body: &Value) -> Result<AiResponse, String> {
    let candidate = body
        .get("candidates")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
        .ok_or_else(|| "missing `candidates[0]`".to_string())?;

    let parts = candidate
        .get("content")
        .and_then(|c| c.get("parts"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut counter = 0u32;
    let content = parse_parts(&parts, &mut counter);
    let had_tool_call = content
        .iter()
        .any(|p| matches!(p, ContentPart::ToolUse { .. }));

    Ok(AiResponse {
        id: body
            .get("responseId")
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .unwrap_or_else(|| sb_core::new_id("resp")),
        model: body
            .get("modelVersion")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        message: Message {
            role: Role::Assistant,
            content,
        },
        finish_reason: finish_reason_from_gemini(
            candidate.get("finishReason").and_then(Value::as_str),
            had_tool_call,
        ),
        usage: usage_from_gemini(body.get("usageMetadata")),
    })
}

/// Optional symmetry helper: canonical `AiResponse` -> Gemini JSON.
pub fn response_to_gemini(resp: &AiResponse) -> Value {
    let parts: Vec<Value> = resp
        .message
        .content
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text } => Some(json!({ "text": text })),
            ContentPart::ToolUse { name, args, .. } => {
                Some(json!({ "functionCall": { "name": name, "args": args } }))
            }
            ContentPart::Image { .. }
            | ContentPart::ToolResult { .. }
            | ContentPart::Reasoning { .. }
            | ContentPart::Citation { .. }
            | ContentPart::ServerToolUse { .. }
            | ContentPart::ServerToolResult { .. } => None,
        })
        .collect();

    json!({
        "candidates": [{
            "content": { "role": "model", "parts": parts },
            "finishReason": finish_reason_to_gemini(resp.finish_reason),
        }],
        "usageMetadata": {
            "promptTokenCount": resp.usage.input_tokens,
            "candidatesTokenCount": resp.usage.output_tokens,
        }
    })
}

/// Decodes the Gemini `streamGenerateContent` SSE stream (each `data:` frame is
/// a whole `GenerateContentResponse`) into canonical `AiStreamEvent`s. Gemini
/// streams text incrementally but sends each `functionCall` whole, so tool calls
/// are emitted as start+args+end together (no incremental arg deltas upstream).
pub struct GeminiStreamDecoder {
    started: bool,
    ended: bool,
    model: String,
    tool_index: u32,
    stop_reason: Option<FinishReason>,
    usage: Usage,
}

impl GeminiStreamDecoder {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            started: false,
            ended: false,
            model: model.into(),
            tool_index: 0,
            stop_reason: None,
            usage: Usage::default(),
        }
    }

    pub fn decode(&mut self, chunk: &Value) -> Vec<AiStreamEvent> {
        let mut events = Vec::new();
        if !self.started {
            self.started = true;
            let id = chunk
                .get("responseId")
                .and_then(Value::as_str)
                .map(ToString::to_string)
                .unwrap_or_default();
            let model = chunk
                .get("modelVersion")
                .and_then(Value::as_str)
                .map(ToString::to_string)
                .unwrap_or_else(|| self.model.clone());
            events.push(AiStreamEvent::MessageStart { id, model });
        }

        if let Some(meta) = chunk.get("usageMetadata") {
            self.usage = usage_from_gemini(Some(meta));
        }

        if let Some(candidate) = chunk
            .get("candidates")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
        {
            if let Some(parts) = candidate
                .get("content")
                .and_then(|c| c.get("parts"))
                .and_then(Value::as_array)
            {
                for part in parts {
                    if let Some(text) = part.get("text").and_then(Value::as_str) {
                        if !text.is_empty() {
                            events.push(AiStreamEvent::TextDelta {
                                text: text.to_string(),
                            });
                        }
                    } else if let Some(call) = part.get("functionCall").and_then(Value::as_object) {
                        let index = self.tool_index;
                        self.tool_index += 1;
                        let name = call.get("name").and_then(Value::as_str).unwrap_or("");
                        let args = call.get("args").cloned().unwrap_or(Value::Null);
                        events.push(AiStreamEvent::ToolCallStart(ToolCallStart {
                            index,
                            id: format!("call_{index}"),
                            name: name.to_string(),
                        }));
                        events.push(AiStreamEvent::ToolCallArgsDelta {
                            index,
                            json: serde_json::to_string(&args).unwrap_or_default(),
                        });
                        events.push(AiStreamEvent::ToolCallEnd { index });
                        self.stop_reason = Some(FinishReason::ToolCalls);
                    }
                }
            }

            if let Some(reason) = candidate.get("finishReason").and_then(Value::as_str) {
                let inferred = finish_reason_from_gemini(
                    Some(reason),
                    matches!(self.stop_reason, Some(FinishReason::ToolCalls)),
                );
                self.stop_reason = Some(inferred);
            }
        }

        events
    }

    /// Emit the terminal usage + `MessageEnd`. Call once when the stream ends
    /// (Gemini has no explicit stop frame).
    pub fn finish(&mut self) -> Vec<AiStreamEvent> {
        if self.ended {
            return Vec::new();
        }
        self.ended = true;
        let mut events = Vec::new();
        if self.started {
            events.push(AiStreamEvent::UsageDelta {
                usage: self.usage.clone(),
            });
            events.push(AiStreamEvent::MessageEnd {
                finish_reason: self.stop_reason.unwrap_or(FinishReason::Stop),
            });
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_maps_system_model_role_and_tool_result_by_name() {
        let mut req = AiRequest::new("gemini/gemini-2.0-flash", Vec::new());
        req.system = Some("be brief".to_string());
        req.max_output_tokens = Some(256);
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
                content: "18C".to_string(),
                is_error: false,
            }],
        });

        let wire = request_to_gemini_wire(&req).unwrap();
        assert_eq!(wire["systemInstruction"]["parts"][0]["text"], "be brief");
        assert_eq!(wire["generationConfig"]["maxOutputTokens"], 256);
        let contents = wire["contents"].as_array().unwrap();
        // user(text) , model(functionCall) , user(functionResponse)
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(contents[1]["role"], "model");
        assert_eq!(
            contents[1]["parts"][0]["functionCall"]["name"],
            "get_weather"
        );
        assert_eq!(contents[2]["role"], "user");
        // tool result correlated back to the function NAME (Gemini has no ids).
        assert_eq!(
            contents[2]["parts"][0]["functionResponse"]["name"],
            "get_weather"
        );
        assert_eq!(
            contents[2]["parts"][0]["functionResponse"]["response"]["result"],
            "18C"
        );
    }

    #[test]
    fn request_maps_image_content_to_inline_data() {
        let req = AiRequest::new(
            "gemini/gemini-2.0-flash",
            vec![Message {
                role: Role::User,
                content: vec![
                    ContentPart::text("inspect this"),
                    ContentPart::image_base64("image/png", "abc"),
                ],
            }],
        );

        let wire = request_to_gemini_wire(&req).unwrap();
        let parts = wire["contents"][0]["parts"].as_array().unwrap();
        assert_eq!(parts[0]["text"], "inspect this");
        assert_eq!(parts[1]["inlineData"]["mimeType"], "image/png");
        assert_eq!(parts[1]["inlineData"]["data"], "abc");
    }

    #[test]
    fn request_maps_image_data_url_to_inline_data() {
        let req = AiRequest::new(
            "gemini/gemini-2.0-flash",
            vec![Message {
                role: Role::User,
                content: vec![ContentPart::image_url("data:image/jpeg;base64,xyz", None)],
            }],
        );

        let wire = request_to_gemini_wire(&req).unwrap();
        let image = &wire["contents"][0]["parts"][0]["inlineData"];
        assert_eq!(image["mimeType"], "image/jpeg");
        assert_eq!(image["data"], "xyz");
    }

    #[test]
    fn request_rejects_foreign_file_ref_images() {
        let req = AiRequest::new(
            "gemini/gemini-2.0-flash",
            vec![Message {
                role: Role::User,
                content: vec![ContentPart::image_file_ref(
                    Some("openai"),
                    "file_123",
                    None,
                )],
            }],
        );

        let err = request_to_gemini_wire(&req).unwrap_err();
        assert!(err.contains("provider file ref owned by `openai`"));
    }

    #[test]
    fn non_stream_response_parses_text_tool_and_usage() {
        let body = json!({
            "candidates": [{
                "content": { "role": "model", "parts": [
                    { "text": "let me check" },
                    { "functionCall": { "name": "get_weather", "args": { "city": "Lyon" } } }
                ]},
                "finishReason": "STOP"
            }],
            "usageMetadata": { "promptTokenCount": 11, "candidatesTokenCount": 6 },
            "modelVersion": "gemini-2.0-flash"
        });

        let resp = parse_gemini_response(&body).unwrap();
        // functionCall present -> inferred ToolCalls, not Stop.
        assert_eq!(resp.finish_reason, FinishReason::ToolCalls);
        assert_eq!(resp.usage.input_tokens, 11);
        assert_eq!(resp.usage.output_tokens, 6);
        assert_eq!(resp.model, "gemini-2.0-flash");
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
    fn streaming_decoder_reconstructs_text_lifecycle() {
        let mut decoder = GeminiStreamDecoder::new("gemini-2.0-flash");
        let frames = vec![
            json!({ "responseId": "r1", "modelVersion": "gemini-2.0-flash",
                "candidates": [{ "content": { "role": "model", "parts": [{ "text": "Hel" }] } }] }),
            json!({ "candidates": [{ "content": { "role": "model", "parts": [{ "text": "lo" }] },
                "finishReason": "STOP" }],
                "usageMetadata": { "promptTokenCount": 4, "candidatesTokenCount": 2 } }),
        ];
        let mut events = Vec::new();
        for frame in &frames {
            events.extend(decoder.decode(frame));
        }
        events.extend(decoder.finish());

        assert!(matches!(
            events.first(),
            Some(AiStreamEvent::MessageStart { .. })
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
            AiStreamEvent::UsageDelta { usage } if usage.output_tokens == 2
        )));
        assert!(matches!(
            events.last(),
            Some(AiStreamEvent::MessageEnd {
                finish_reason: FinishReason::Stop
            })
        ));
    }

    #[test]
    fn streaming_decoder_reconstructs_tool_call() {
        let mut decoder = GeminiStreamDecoder::new("gemini-2.0-flash");
        let frame = json!({ "responseId": "r2",
            "candidates": [{ "content": { "role": "model", "parts": [
                { "functionCall": { "name": "search", "args": { "q": "rust" } } }
            ]}, "finishReason": "STOP" }] });
        let mut events = decoder.decode(&frame);
        events.extend(decoder.finish());

        assert!(events.iter().any(|e| matches!(
            e,
            AiStreamEvent::ToolCallStart(t) if t.name == "search" && t.id == "call_0"
        )));
        assert!(events.iter().any(|e| matches!(
            e,
            AiStreamEvent::ToolCallArgsDelta { json, .. } if json.contains("\"q\":\"rust\"")
        )));
        assert!(events
            .iter()
            .any(|e| matches!(e, AiStreamEvent::ToolCallEnd { index: 0 })));
        // functionCall present -> ToolCalls finish even though Gemini said STOP.
        assert!(matches!(
            events.last(),
            Some(AiStreamEvent::MessageEnd {
                finish_reason: FinishReason::ToolCalls
            })
        ));
    }

    #[test]
    fn request_downlevels_complex_tool_schema_for_gemini() {
        use sb_core::ToolSpec;
        let mut req = AiRequest::new("g", vec![Message::user("hi")]);
        req.tools.push(ToolSpec {
            name: "f".into(),
            description: None,
            parameters: json!({
                "type": "object",
                "properties": {
                    "mode": { "const": "fast" },
                    "val": { "anyOf": [{ "type": "null" }, { "type": "number" }] }
                },
                "additionalProperties": false
            }),
        });

        let wire = request_to_gemini_wire(&req).unwrap();
        let params = &wire["tools"][0]["functionDeclarations"][0]["parameters"];
        // const -> string enum; anyOf -> non-null branch; additionalProperties dropped.
        assert_eq!(params["properties"]["mode"]["enum"], json!(["fast"]));
        assert_eq!(params["properties"]["val"]["type"], "number");
        assert!(params.get("additionalProperties").is_none());

        let warnings = schema_downlevel_warnings(&req);
        assert!(warnings.iter().any(|warning| warning
            .to_string()
            .contains("/tools/f/parameters/properties/mode/const")));
        assert!(warnings.iter().any(|warning| warning
            .to_string()
            .contains("/tools/f/parameters/properties/val/anyOf")));
    }

    #[test]
    fn structured_output_maps_to_response_mime_and_downleveled_schema() {
        use sb_core::ResponseFormat;
        let mut req = AiRequest::new("g", vec![Message::user("hi")]);
        req.response_format = Some(ResponseFormat::JsonSchema {
            name: "out".into(),
            // Complex features Gemini rejects must be downleveled, not passed raw.
            schema: json!({
                "type": "object",
                "properties": {
                    "kind": { "const": "ok" },
                    "n": { "anyOf": [{ "type": "null" }, { "type": "integer" }] }
                },
                "additionalProperties": false
            }),
            strict: true,
        });

        let wire = request_to_gemini_wire(&req).unwrap();
        let gen = &wire["generationConfig"];
        assert_eq!(gen["responseMimeType"], "application/json");
        assert_eq!(
            gen["responseSchema"]["properties"]["kind"]["enum"],
            json!(["ok"])
        );
        assert_eq!(gen["responseSchema"]["properties"]["n"]["type"], "integer");
        assert!(gen["responseSchema"].get("additionalProperties").is_none());

        let warnings = schema_downlevel_warnings(&req);
        assert!(warnings.iter().any(|warning| warning
            .to_string()
            .contains("/response_format/schema/properties/kind/const")));
        assert!(warnings.iter().any(|warning| warning
            .to_string()
            .contains("/response_format/schema/additionalProperties")));
    }

    #[test]
    fn json_object_response_format_sets_mime_only() {
        use sb_core::ResponseFormat;
        let mut req = AiRequest::new("g", vec![Message::user("hi")]);
        req.response_format = Some(ResponseFormat::JsonObject);
        let wire = request_to_gemini_wire(&req).unwrap();
        let gen = &wire["generationConfig"];
        assert_eq!(gen["responseMimeType"], "application/json");
        assert!(
            gen.get("responseSchema").is_none(),
            "json_object carries no schema"
        );
    }
}
