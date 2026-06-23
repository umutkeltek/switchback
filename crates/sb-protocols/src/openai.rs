use sb_core::{
    AiRequest, AiResponse, AiStreamEvent, ContentPart, EnergyUsage, FinishReason, ImageDetail,
    ImageSource, Message, ResponseFormat, Role, ToolCallStart, ToolSpec, Usage,
};
use serde_json::{json, Map, Value};
use std::collections::BTreeSet;
use std::time::{SystemTime, UNIX_EPOCH};

fn unix_secs_now() -> u64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_secs(),
        Err(_) => 0,
    }
}

fn parse_energy_usage(value: Option<&Value>) -> Option<EnergyUsage> {
    let value = value.and_then(Value::as_object)?;
    let energy = EnergyUsage {
        energy_joules: value.get("energy_joules").and_then(Value::as_f64),
        energy_kwh: value.get("energy_kwh").and_then(Value::as_f64),
        duration_seconds: value.get("duration_seconds").and_then(Value::as_f64),
        measurement_available: value.get("measurement_available").and_then(Value::as_bool),
        attribution_method: value
            .get("attribution_method")
            .and_then(Value::as_str)
            .map(str::to_string),
        energy_kwh_consumed: value.get("energy_kwh_consumed").and_then(Value::as_f64),
        energy_kwh_charged: value.get("energy_kwh_charged").and_then(Value::as_f64),
        accounting_method: value
            .get("accounting_method")
            .and_then(Value::as_str)
            .map(str::to_string),
        total_cost_usd: value.get("total_cost_usd").and_then(Value::as_f64),
    };
    if energy.has_measured_energy()
        || energy.duration_seconds.is_some()
        || energy.measurement_available.is_some()
        || energy.attribution_method.is_some()
        || energy.accounting_method.is_some()
        || energy.total_cost_usd.is_some()
    {
        Some(energy)
    } else {
        None
    }
}

fn energy_usage_json(energy: &EnergyUsage) -> Value {
    serde_json::to_value(energy).unwrap_or(Value::Null)
}

fn openai_usage_json(usage: &Usage) -> Value {
    let mut value = json!({
        "prompt_tokens": usage.input_tokens,
        "completion_tokens": usage.output_tokens,
        "total_tokens": usage.total(),
    });
    if let Some(energy) = &usage.energy {
        value["energy"] = energy_usage_json(energy);
    }
    value
}

fn split_data_url(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("data:")?;
    let (media_type, data) = rest.split_once(";base64,")?;
    if media_type.is_empty() || data.is_empty() {
        return None;
    }
    Some((media_type.to_string(), data.to_string()))
}

fn parse_image_detail(value: Option<&str>) -> Result<Option<ImageDetail>, String> {
    value.map(ImageDetail::parse).transpose()
}

fn image_from_url(url: String, detail: Option<ImageDetail>) -> ContentPart {
    let source = if let Some((media_type, data)) = split_data_url(&url) {
        ImageSource::InlineBase64 { media_type, data }
    } else {
        ImageSource::RemoteUrl { url }
    };
    ContentPart::Image { source, detail }
}

fn parse_openai_content_parts(content: Option<&Value>) -> Result<Vec<ContentPart>, String> {
    match content {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::String(text)) => Ok(vec![ContentPart::text(text.clone())]),
        Some(Value::Array(parts)) => {
            let mut content = Vec::new();
            for part in parts {
                match part.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        let text = part
                            .get("text")
                            .and_then(Value::as_str)
                            .ok_or_else(|| "message text part missing string `text`".to_string())?;
                        content.push(ContentPart::text(text));
                    }
                    Some("image_url") => {
                        let image_url = part.get("image_url").ok_or_else(|| {
                            "OpenAI image_url part missing `image_url`".to_string()
                        })?;
                        let detail = parse_image_detail(
                            image_url
                                .get("detail")
                                .or_else(|| part.get("detail"))
                                .and_then(Value::as_str),
                        )?;
                        let url = match image_url {
                            Value::String(url) => url.as_str(),
                            Value::Object(_) => image_url
                                .get("url")
                                .and_then(Value::as_str)
                                .ok_or_else(|| {
                                    "OpenAI image_url part missing string `image_url.url`"
                                        .to_string()
                                })?,
                            _ => {
                                return Err(
                                    "OpenAI image_url part must be a string or object".to_string()
                                )
                            }
                        };
                        content.push(image_from_url(url.to_string(), detail));
                    }
                    Some(other) => {
                        return Err(format!("unsupported OpenAI content part `{other}`"));
                    }
                    None => {
                        return Err("message content part missing string `type`".to_string());
                    }
                }
            }
            Ok(content)
        }
        Some(_) => Err("message content must be a string, null, or array".to_string()),
    }
}

fn parse_text_content(content: Option<&Value>) -> Result<String, String> {
    let parts = parse_openai_content_parts(content)?;
    reject_image_parts(&parts, "OpenAI non-user messages")?;
    Ok(content_parts_to_text(&parts))
}

fn parse_tool_args(arguments: &str) -> Value {
    match serde_json::from_str(arguments) {
        Ok(value) => value,
        Err(_) => Value::String(arguments.to_string()),
    }
}

fn finish_reason_to_openai(reason: FinishReason) -> &'static str {
    match reason {
        FinishReason::Stop => "stop",
        FinishReason::Length => "length",
        FinishReason::ToolCalls => "tool_calls",
        FinishReason::ContentFilter => "content_filter",
        FinishReason::Error => "stop",
    }
}

fn finish_reason_from_openai(reason: Option<&str>) -> FinishReason {
    match reason {
        Some("stop") => FinishReason::Stop,
        Some("length") => FinishReason::Length,
        Some("tool_calls") => FinishReason::ToolCalls,
        Some("content_filter") => FinishReason::ContentFilter,
        Some("error") => FinishReason::Error,
        Some(_) | None => FinishReason::Stop,
    }
}

fn role_to_openai(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

fn content_parts_to_text(parts: &[ContentPart]) -> String {
    parts
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
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

fn image_to_openai_url(
    part: &ContentPart,
) -> Result<Option<(String, Option<ImageDetail>)>, String> {
    let ContentPart::Image { source, detail } = part else {
        return Ok(None);
    };
    source.validate()?;
    let url = match source {
        ImageSource::InlineBase64 { media_type, data } => {
            format!("data:{media_type};base64,{data}")
        }
        ImageSource::RemoteUrl { url } => url.clone(),
        ImageSource::ProviderFileRef { .. } => {
            return Err(
                "OpenAI Chat Completions cannot encode provider file image references".to_string(),
            )
        }
    };
    Ok(Some((url, *detail)))
}

fn content_parts_to_openai_message_content(parts: &[ContentPart]) -> Result<Value, String> {
    let has_image = parts
        .iter()
        .any(|part| matches!(part, ContentPart::Image { .. }));
    if !has_image {
        return Ok(Value::String(content_parts_to_text(parts)));
    }

    let mut content = Vec::new();
    for part in parts {
        match part {
            ContentPart::Text { text } if !text.is_empty() => {
                content.push(json!({ "type": "text", "text": text }));
            }
            ContentPart::Image { .. } => {
                if let Some((url, detail)) = image_to_openai_url(part)? {
                    let mut image_url = Map::new();
                    image_url.insert("url".to_string(), Value::String(url));
                    if let Some(detail) = detail {
                        image_url.insert(
                            "detail".to_string(),
                            Value::String(detail.as_str().to_string()),
                        );
                    }
                    content.push(json!({
                        "type": "image_url",
                        "image_url": Value::Object(image_url),
                    }));
                }
            }
            _ => {}
        }
    }
    Ok(Value::Array(content))
}

pub fn request_from_openai_chat(body: &Value) -> Result<AiRequest, String> {
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing or invalid `model`".to_string())?
        .to_string();
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| "missing or invalid `messages`".to_string())?;

    let mut request = AiRequest::new(model, Vec::new());
    let mut system_chunks = Vec::new();

    for message in messages {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .ok_or_else(|| "message missing string `role`".to_string())?;

        match role {
            "system" | "developer" => {
                let system = parse_text_content(message.get("content"))?;
                if !system.is_empty() {
                    system_chunks.push(system);
                }
            }
            "user" => {
                let content = parse_openai_content_parts(message.get("content"))?;
                request.messages.push(Message {
                    role: Role::User,
                    content,
                });
            }
            "assistant" => {
                let mut content = Vec::new();
                let text = parse_text_content(message.get("content"))?;
                if !text.is_empty() {
                    content.push(ContentPart::text(text));
                }

                if let Some(tool_calls) = message.get("tool_calls") {
                    let tool_calls = tool_calls
                        .as_array()
                        .ok_or_else(|| "`tool_calls` must be an array".to_string())?;
                    for tool_call in tool_calls {
                        let id = tool_call
                            .get("id")
                            .and_then(Value::as_str)
                            .ok_or_else(|| "tool call missing string `id`".to_string())?;
                        let function = tool_call
                            .get("function")
                            .and_then(Value::as_object)
                            .ok_or_else(|| "tool call missing object `function`".to_string())?;
                        let name =
                            function
                                .get("name")
                                .and_then(Value::as_str)
                                .ok_or_else(|| {
                                    "tool call missing string `function.name`".to_string()
                                })?;
                        let arguments = function
                            .get("arguments")
                            .and_then(Value::as_str)
                            .ok_or_else(|| {
                                "tool call missing string `function.arguments`".to_string()
                            })?;
                        content.push(ContentPart::ToolUse {
                            id: id.to_string(),
                            name: name.to_string(),
                            args: parse_tool_args(arguments),
                        });
                    }
                }

                request.messages.push(Message {
                    role: Role::Assistant,
                    content,
                });
            }
            "tool" => {
                let tool_use_id = message
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "tool message missing string `tool_call_id`".to_string())?;
                let content = parse_text_content(message.get("content"))?;
                request.messages.push(Message {
                    role: Role::Tool,
                    content: vec![ContentPart::ToolResult {
                        tool_use_id: tool_use_id.to_string(),
                        content,
                        content_parts: Vec::new(),
                        is_error: false,
                    }],
                });
            }
            other => return Err(format!("unsupported message role `{other}`")),
        }
    }

    if !system_chunks.is_empty() {
        request.system = Some(system_chunks.join("\n"));
    }

    if let Some(tools) = body.get("tools") {
        let tools = tools
            .as_array()
            .ok_or_else(|| "`tools` must be an array".to_string())?;
        for tool in tools {
            if tool.get("type").and_then(Value::as_str) == Some("function") {
                let function = tool
                    .get("function")
                    .and_then(Value::as_object)
                    .ok_or_else(|| "tool missing object `function`".to_string())?;
                let name = function
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "tool missing string `function.name`".to_string())?;
                let description = function
                    .get("description")
                    .and_then(Value::as_str)
                    .map(ToString::to_string);
                let parameters = function.get("parameters").cloned().unwrap_or(Value::Null);
                request.tools.push(ToolSpec {
                    name: name.to_string(),
                    description,
                    parameters,
                });
            }
        }
    }

    if let Some(response_format) = body.get("response_format") {
        request.response_format = match response_format.get("type").and_then(Value::as_str) {
            Some("text") => Some(ResponseFormat::Text),
            Some("json_object") => Some(ResponseFormat::JsonObject),
            Some("json_schema") => {
                let schema = response_format
                    .get("json_schema")
                    .and_then(Value::as_object)
                    .ok_or_else(|| "response_format.json_schema must be an object".to_string())?;
                let name = schema
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "response_format.json_schema.name missing".to_string())?;
                let schema_value = schema.get("schema").cloned().unwrap_or(Value::Null);
                let strict = schema
                    .get("strict")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                Some(ResponseFormat::JsonSchema {
                    name: name.to_string(),
                    schema: schema_value,
                    strict,
                })
            }
            Some(_) | None => None,
        };
    }

    request.stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    request.temperature = body
        .get("temperature")
        .and_then(Value::as_f64)
        .map(|value| value as f32);
    request.max_output_tokens = body
        .get("max_completion_tokens")
        .and_then(Value::as_u64)
        .or_else(|| body.get("max_tokens").and_then(Value::as_u64))
        .and_then(|value| u32::try_from(value).ok());

    // Capture every OpenAI param we DON'T model as a typed field, verbatim, so
    // it flows through to OpenAI-compatible upstreams (top_p, stop, seed,
    // frequency/presence_penalty, n, tool_choice, parallel_tool_calls,
    // logit_bias, logprobs, stream_options, user, ...). Full API fidelity.
    const MODELED_KEYS: &[&str] = &[
        "model",
        "messages",
        "tools",
        "response_format",
        "stream",
        "temperature",
        "max_tokens",
        "max_completion_tokens",
    ];
    if let Some(obj) = body.as_object() {
        for (key, value) in obj {
            if !MODELED_KEYS.contains(&key.as_str()) {
                request.passthrough.insert(key.clone(), value.clone());
            }
        }
    }

    request.id = sb_core::new_id("req");

    Ok(request)
}

pub fn response_to_openai_chat(resp: &AiResponse) -> Value {
    let content = content_parts_to_text(&resp.message.content);
    let tool_calls: Vec<Value> = resp
        .message
        .content
        .iter()
        .filter_map(|part| match part {
            ContentPart::ToolUse { id, name, args } => Some(json!({
                "id": id,
                "type": "function",
                "function": {
                    "name": name,
                    "arguments": serde_json::to_string(args)
                        .unwrap_or_else(|_| "null".to_string()),
                }
            })),
            _ => None,
        })
        .collect();

    let mut message = Map::new();
    message.insert("role".to_string(), Value::String("assistant".to_string()));
    if content.is_empty() {
        message.insert("content".to_string(), Value::Null);
    } else {
        message.insert("content".to_string(), Value::String(content));
    }
    if !tool_calls.is_empty() {
        message.insert("tool_calls".to_string(), Value::Array(tool_calls));
    }

    let mut response = json!({
        "id": resp.id,
        "object": "chat.completion",
        "created": unix_secs_now(),
        "model": resp.model,
        "choices": [{
            "index": 0,
            "message": Value::Object(message),
            "finish_reason": finish_reason_to_openai(resp.finish_reason),
        }],
        "usage": openai_usage_json(&resp.usage)
    });
    if let Some(energy) = &resp.usage.energy {
        response["energy"] = energy_usage_json(energy);
    }
    response
}

pub struct OpenAiStreamEncoder {
    id: String,
    model: String,
    created: u64,
    message_started: bool,
}

impl OpenAiStreamEncoder {
    pub fn new(id: String, model: String) -> Self {
        Self {
            id,
            model,
            created: unix_secs_now(),
            message_started: false,
        }
    }

    fn chunk(&self, choices: Value, usage: Option<Value>) -> Vec<String> {
        let mut payload = Map::new();
        payload.insert("id".to_string(), Value::String(self.id.clone()));
        payload.insert(
            "object".to_string(),
            Value::String("chat.completion.chunk".to_string()),
        );
        payload.insert(
            "created".to_string(),
            Value::Number(serde_json::Number::from(self.created)),
        );
        payload.insert("model".to_string(), Value::String(self.model.clone()));
        payload.insert("choices".to_string(), choices);
        if let Some(usage) = usage {
            payload.insert("usage".to_string(), usage);
        }
        vec![format!("data: {}\n\n", Value::Object(payload))]
    }

    pub fn encode(&mut self, ev: &AiStreamEvent) -> Vec<String> {
        match ev {
            AiStreamEvent::MessageStart { .. } => {
                if self.message_started {
                    Vec::new()
                } else {
                    self.message_started = true;
                    self.chunk(
                        json!([{
                            "index": 0,
                            "delta": { "role": "assistant" },
                            "finish_reason": Value::Null,
                        }]),
                        None,
                    )
                }
            }
            // Chat Completions has no wire frame for generated images or
            // citations; they're dropped from this surface (carried on the
            // Responses/Anthropic surfaces instead).
            AiStreamEvent::OutputImage { .. }
            | AiStreamEvent::Citation { .. }
            | AiStreamEvent::ServerToolCall { .. } => Vec::new(),
            AiStreamEvent::TextDelta { text } => self.chunk(
                json!([{
                    "index": 0,
                    "delta": { "content": text },
                    "finish_reason": Value::Null,
                }]),
                None,
            ),
            AiStreamEvent::ToolCallStart(tool_call) => self.chunk(
                json!([{
                    "index": 0,
                    "delta": {
                        "tool_calls": [{
                            "index": tool_call.index,
                            "id": tool_call.id,
                            "type": "function",
                            "function": {
                                "name": tool_call.name,
                                "arguments": "",
                            }
                        }]
                    },
                    "finish_reason": Value::Null,
                }]),
                None,
            ),
            AiStreamEvent::ToolCallArgsDelta { index, json } => self.chunk(
                json!([{
                    "index": 0,
                    "delta": {
                        "tool_calls": [{
                            "index": index,
                            "function": {
                                "arguments": json,
                            }
                        }]
                    },
                    "finish_reason": Value::Null,
                }]),
                None,
            ),
            AiStreamEvent::ToolCallEnd { .. } => Vec::new(),
            AiStreamEvent::UsageDelta { usage } => {
                self.chunk(Value::Array(Vec::new()), Some(openai_usage_json(usage)))
            }
            AiStreamEvent::ReasoningDelta { .. } => Vec::new(),
            AiStreamEvent::MessageEnd { finish_reason } => self.chunk(
                json!([{
                    "index": 0,
                    "delta": {},
                    "finish_reason": finish_reason_to_openai(*finish_reason),
                }]),
                None,
            ),
            AiStreamEvent::Error { .. } => Vec::new(),
        }
    }

    pub fn done(&self) -> String {
        "data: [DONE]\n\n".to_string()
    }
}

pub fn request_to_openai_wire(
    req: &AiRequest,
    upstream_model: &str,
    stream: bool,
) -> Result<Value, String> {
    req.validate_canonical()?;
    let mut messages = Vec::new();

    if let Some(system) = &req.system {
        messages.push(json!({
            "role": "system",
            "content": system,
        }));
    }

    for message in &req.messages {
        match message.role {
            Role::Tool => {
                for part in &message.content {
                    match part {
                        ContentPart::ToolResult {
                            tool_use_id,
                            content,
                            ..
                        } => {
                            messages.push(json!({
                                "role": "tool",
                                "tool_call_id": tool_use_id,
                                "content": content,
                            }));
                        }
                        ContentPart::Text { text } => {
                            messages.push(json!({
                                "role": "tool",
                                "content": text,
                            }));
                        }
                        ContentPart::Image { .. } => {
                            return Err("OpenAI Chat cannot encode image content in tool messages"
                                .to_string());
                        }
                        ContentPart::ToolUse { .. }
                        | ContentPart::Reasoning { .. }
                        | ContentPart::Citation { .. }
                        | ContentPart::ServerToolUse { .. }
                        | ContentPart::ServerToolResult { .. } => {}
                    }
                }
            }
            Role::Assistant => {
                reject_image_parts(&message.content, "OpenAI assistant messages")?;
                let text = content_parts_to_text(&message.content);
                let tool_calls: Vec<Value> = message
                    .content
                    .iter()
                    .filter_map(|part| match part {
                        ContentPart::ToolUse { id, name, args } => Some(json!({
                            "id": id,
                            "type": "function",
                            "function": {
                                "name": name,
                                "arguments": serde_json::to_string(args)
                                    .unwrap_or_else(|_| "null".to_string()),
                            }
                        })),
                        _ => None,
                    })
                    .collect();

                if tool_calls.is_empty() {
                    messages.push(json!({
                        "role": role_to_openai(message.role),
                        "content": text,
                    }));
                } else {
                    let content = if text.is_empty() {
                        Value::Null
                    } else {
                        Value::String(text)
                    };
                    messages.push(json!({
                        "role": role_to_openai(message.role),
                        "content": content,
                        "tool_calls": tool_calls,
                    }));
                }
            }
            Role::System => {
                reject_image_parts(&message.content, "OpenAI system messages")?;
                messages.push(json!({
                    "role": role_to_openai(message.role),
                    "content": content_parts_to_openai_message_content(&message.content)?,
                }));
            }
            Role::User => {
                messages.push(json!({
                    "role": role_to_openai(message.role),
                    "content": content_parts_to_openai_message_content(&message.content)?,
                }));
            }
        }
    }

    let tools = if req.tools.is_empty() {
        None
    } else {
        Some(
            req.tools
                .iter()
                .map(|tool| {
                    let mut function = Map::new();
                    function.insert("name".to_string(), Value::String(tool.name.clone()));
                    if let Some(description) = &tool.description {
                        function.insert(
                            "description".to_string(),
                            Value::String(description.clone()),
                        );
                    }
                    function.insert("parameters".to_string(), tool.parameters.clone());
                    let mut tool_value = Map::new();
                    tool_value.insert("type".to_string(), Value::String("function".to_string()));
                    tool_value.insert("function".to_string(), Value::Object(function));
                    Value::Object(tool_value)
                })
                .collect::<Vec<_>>(),
        )
    };

    let response_format = match &req.response_format {
        Some(ResponseFormat::Text) => Some(json!({ "type": "text" })),
        Some(ResponseFormat::JsonObject) => Some(json!({ "type": "json_object" })),
        Some(ResponseFormat::JsonSchema {
            name,
            schema,
            strict,
        }) => Some(json!({
            "type": "json_schema",
            "json_schema": {
                "name": name,
                "schema": schema,
                "strict": strict,
            }
        })),
        None => None,
    };

    let mut body = Map::new();
    body.insert(
        "model".to_string(),
        Value::String(upstream_model.to_string()),
    );
    body.insert("messages".to_string(), Value::Array(messages));
    body.insert("stream".to_string(), Value::Bool(stream));
    if let Some(max_tokens) = req.max_output_tokens {
        body.insert(
            "max_tokens".to_string(),
            Value::Number(serde_json::Number::from(max_tokens)),
        );
    }
    if let Some(temperature) = req.temperature {
        if let Some(number) = serde_json::Number::from_f64(f64::from(temperature)) {
            body.insert("temperature".to_string(), Value::Number(number));
        }
    }
    if let Some(tools) = tools {
        body.insert("tools".to_string(), Value::Array(tools));
    }
    if let Some(response_format) = response_format {
        body.insert("response_format".to_string(), response_format);
    }

    // Forward captured OpenAI params verbatim. Explicit fields set above win
    // (never overwritten); everything else the client sent flows through.
    for (key, value) in &req.passthrough {
        body.entry(key.clone()).or_insert_with(|| value.clone());
    }

    Ok(Value::Object(body))
}

pub fn parse_openai_chat_response(body: &Value) -> Result<AiResponse, String> {
    let choice = body
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .ok_or_else(|| "missing `choices[0]`".to_string())?;
    let message = choice
        .get("message")
        .and_then(Value::as_object)
        .ok_or_else(|| "missing object `choices[0].message`".to_string())?;

    let mut content = Vec::new();
    let text = parse_text_content(message.get("content"))?;
    if !text.is_empty() {
        content.push(ContentPart::text(text));
    }

    if let Some(tool_calls) = message.get("tool_calls") {
        let tool_calls = tool_calls
            .as_array()
            .ok_or_else(|| "`choices[0].message.tool_calls` must be an array".to_string())?;
        for tool_call in tool_calls {
            let id = tool_call
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| "tool call missing string `id`".to_string())?;
            let function = tool_call
                .get("function")
                .and_then(Value::as_object)
                .ok_or_else(|| "tool call missing object `function`".to_string())?;
            let name = function
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| "tool call missing string `function.name`".to_string())?;
            let arguments = function
                .get("arguments")
                .and_then(Value::as_str)
                .ok_or_else(|| "tool call missing string `function.arguments`".to_string())?;
            content.push(ContentPart::ToolUse {
                id: id.to_string(),
                name: name.to_string(),
                args: parse_tool_args(arguments),
            });
        }
    }

    let usage = body.get("usage").and_then(Value::as_object);
    let usage = Usage {
        input_tokens: usage
            .and_then(|usage| usage.get("prompt_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        output_tokens: usage
            .and_then(|usage| usage.get("completion_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        energy: parse_energy_usage(body.get("energy"))
            .or_else(|| parse_energy_usage(usage.and_then(|usage| usage.get("energy")))),
        ..Usage::default()
    };

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
            content,
        },
        finish_reason: finish_reason_from_openai(
            choice.get("finish_reason").and_then(Value::as_str),
        ),
        usage,
    })
}

pub struct OpenAiStreamDecoder {
    started: bool,
    ended: bool,
    seen_tool_calls: BTreeSet<u32>,
}

impl OpenAiStreamDecoder {
    pub fn new() -> Self {
        Self {
            started: false,
            ended: false,
            seen_tool_calls: BTreeSet::new(),
        }
    }

    pub fn decode(&mut self, chunk_json: &Value) -> Vec<AiStreamEvent> {
        let mut events = Vec::new();
        if !self.started {
            self.started = true;
            events.push(AiStreamEvent::MessageStart {
                id: chunk_json
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                model: chunk_json
                    .get("model")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            });
        }

        let usage = chunk_json.get("usage").and_then(Value::as_object);
        let energy = parse_energy_usage(chunk_json.get("energy"))
            .or_else(|| parse_energy_usage(usage.and_then(|usage| usage.get("energy"))));
        if usage.is_some() || energy.is_some() {
            events.push(AiStreamEvent::UsageDelta {
                usage: Usage {
                    input_tokens: usage
                        .and_then(|usage| usage.get("prompt_tokens"))
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                    output_tokens: usage
                        .and_then(|usage| usage.get("completion_tokens"))
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                    energy,
                    ..Usage::default()
                },
            });
        }

        let Some(choice) = chunk_json
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
        else {
            return events;
        };

        if let Some(content) = choice
            .get("delta")
            .and_then(Value::as_object)
            .and_then(|delta| delta.get("content"))
            .and_then(Value::as_str)
        {
            if !content.is_empty() {
                events.push(AiStreamEvent::TextDelta {
                    text: content.to_string(),
                });
            }
        }

        if let Some(tool_calls) = choice
            .get("delta")
            .and_then(Value::as_object)
            .and_then(|delta| delta.get("tool_calls"))
            .and_then(Value::as_array)
        {
            for tool_call in tool_calls {
                let index = tool_call
                    .get("index")
                    .and_then(Value::as_u64)
                    .and_then(|value| u32::try_from(value).ok())
                    .unwrap_or(0);
                if !self.seen_tool_calls.contains(&index) {
                    let function = tool_call
                        .get("function")
                        .and_then(Value::as_object)
                        .cloned()
                        .unwrap_or_default();
                    let id = tool_call
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let name = function
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    events.push(AiStreamEvent::ToolCallStart(ToolCallStart {
                        index,
                        id,
                        name,
                    }));
                    self.seen_tool_calls.insert(index);
                }

                if let Some(arguments) = tool_call
                    .get("function")
                    .and_then(Value::as_object)
                    .and_then(|function| function.get("arguments"))
                    .and_then(Value::as_str)
                {
                    events.push(AiStreamEvent::ToolCallArgsDelta {
                        index,
                        json: arguments.to_string(),
                    });
                }
            }
        }

        if !choice
            .get("finish_reason")
            .unwrap_or(&Value::Null)
            .is_null()
        {
            self.ended = true;
            events.push(AiStreamEvent::MessageEnd {
                finish_reason: finish_reason_from_openai(
                    choice.get("finish_reason").and_then(Value::as_str),
                ),
            });
        }

        events
    }

    pub fn finish(&mut self) -> Vec<AiStreamEvent> {
        if self.started && !self.ended {
            self.ended = true;
            vec![AiStreamEvent::MessageEnd {
                finish_reason: FinishReason::Stop,
            }]
        } else {
            Vec::new()
        }
    }
}

impl Default for OpenAiStreamDecoder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_from_openai_maps_core_fields() {
        let body = json!({
            "model": "mock/echo",
            "messages": [
                { "role": "system", "content": "be helpful" },
                { "role": "user", "content": "hi" }
            ],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "lookup",
                    "description": "Find data",
                    "parameters": { "type": "object" }
                }
            }],
            "stream": true,
            "max_tokens": 77
        });

        let request = request_from_openai_chat(&body).unwrap();
        assert_eq!(request.model, "mock/echo");
        assert_eq!(request.system.as_deref(), Some("be helpful"));
        assert_eq!(request.last_user_text().as_deref(), Some("hi"));
        assert_eq!(request.tools.len(), 1);
        assert!(request.stream);
        assert_eq!(request.max_output_tokens, Some(77));
    }

    #[test]
    fn request_from_openai_maps_image_content() {
        let body = json!({
            "model": "mock/echo",
            "messages": [{
                "role": "user",
                "content": [
                    { "type": "text", "text": "inspect this" },
                    { "type": "image_url", "image_url": { "url": "data:image/png;base64,abc", "detail": "low" } }
                ]
            }]
        });

        let request = request_from_openai_chat(&body).unwrap();
        assert!(request.requires_vision());
        assert_eq!(request.last_user_text().as_deref(), Some("inspect this"));
        assert!(request.messages[0].content.iter().any(|part| matches!(
            part,
            ContentPart::Image {
                source: ImageSource::InlineBase64 { media_type, data },
                detail: Some(detail),
            } if media_type == "image/png"
                && data == "abc"
                && *detail == ImageDetail::Low
        )));

        let wire = request_to_openai_wire(&request, "mock/echo", false).unwrap();
        assert_eq!(wire["messages"][0]["content"][0]["type"], "text");
        assert_eq!(
            wire["messages"][0]["content"][1]["image_url"]["url"],
            "data:image/png;base64,abc"
        );
        assert_eq!(
            wire["messages"][0]["content"][1]["image_url"]["detail"],
            "low"
        );
    }

    #[test]
    fn request_to_openai_rejects_file_ref_images() {
        let request = AiRequest::new(
            "x",
            vec![Message {
                role: Role::User,
                content: vec![
                    ContentPart::text("inspect this"),
                    ContentPart::image_file_ref(Some("openai"), "file_123", None),
                ],
            }],
        );

        let err = request_to_openai_wire(&request, "x", false).unwrap_err();
        assert!(err.contains("provider file image references"));
    }

    #[test]
    fn response_to_openai_has_expected_shape() {
        let response = AiResponse {
            id: "resp_1".to_string(),
            model: "mock/echo".to_string(),
            message: Message::assistant("hello"),
            finish_reason: FinishReason::Stop,
            usage: Usage {
                input_tokens: 2,
                output_tokens: 3,
                ..Usage::default()
            },
        };

        let json = response_to_openai_chat(&response);
        assert_eq!(json["object"], "chat.completion");
        assert_eq!(json["choices"][0]["message"]["content"], "hello");
        assert_eq!(json["usage"]["total_tokens"], 5);
    }

    #[test]
    fn openai_response_round_trips_energy_metadata() {
        let body = json!({
            "id": "chatcmpl_1",
            "model": "glm-5.2",
            "choices": [{
                "message": {"role": "assistant", "content": "pong"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 3, "total_tokens": 13},
            "energy": {
                "energy_joules": 5.23,
                "energy_kwh": 0.00000145,
                "duration_seconds": 0.0183,
                "measurement_available": true,
                "attribution_method": "time_weighted"
            }
        });
        let parsed = parse_openai_chat_response(&body).unwrap();
        let energy = parsed.usage.energy.as_ref().expect("energy metadata");
        assert_eq!(energy.energy_joules, Some(5.23));
        assert_eq!(energy.energy_kwh, Some(0.00000145));
        assert_eq!(energy.attribution_method.as_deref(), Some("time_weighted"));

        let emitted = response_to_openai_chat(&parsed);
        assert_eq!(emitted["energy"]["energy_joules"], 5.23);
        assert_eq!(emitted["usage"]["energy"]["energy_kwh"], 0.00000145);
    }

    #[test]
    fn encoder_emits_framed_chunks_and_done() {
        let mut encoder = OpenAiStreamEncoder::new("req_1".to_string(), "mock/echo".to_string());
        let mut frames = Vec::new();
        frames.extend(encoder.encode(&AiStreamEvent::MessageStart {
            id: "req_1".to_string(),
            model: "mock/echo".to_string(),
        }));
        frames.extend(encoder.encode(&AiStreamEvent::TextDelta {
            text: "hi".to_string(),
        }));
        frames.extend(encoder.encode(&AiStreamEvent::MessageEnd {
            finish_reason: FinishReason::Stop,
        }));
        frames.push(encoder.done());

        assert!(frames
            .iter()
            .all(|frame| frame.starts_with("data: ") && frame.ends_with("\n\n")));
        assert!(frames
            .iter()
            .any(|frame| frame.contains("\"content\":\"hi\"")));
        assert!(frames
            .iter()
            .any(|frame| frame.contains("\"finish_reason\":\"stop\"")));
        assert_eq!(frames.last().map(String::as_str), Some("data: [DONE]\n\n"));
    }

    #[test]
    fn decoder_reconstructs_message_lifecycle() {
        let mut decoder = OpenAiStreamDecoder::new();
        let first = json!({
            "id": "resp_1",
            "model": "mock/echo",
            "choices": [{
                "index": 0,
                "delta": { "content": "hi" },
                "finish_reason": Value::Null
            }]
        });
        let second = json!({
            "id": "resp_1",
            "model": "mock/echo",
            "choices": [{
                "index": 0,
                "delta": {},
                "finish_reason": "stop"
            }]
        });

        let mut events = decoder.decode(&first);
        events.extend(decoder.decode(&second));

        assert!(matches!(
            events.first(),
            Some(AiStreamEvent::MessageStart { .. })
        ));
        assert!(events
            .iter()
            .any(|event| matches!(event, AiStreamEvent::TextDelta { text } if text == "hi")));
        assert!(events.iter().any(|event| matches!(
            event,
            AiStreamEvent::MessageEnd {
                finish_reason: FinishReason::Stop
            }
        )));
    }

    #[test]
    fn unmodeled_openai_params_pass_through_to_upstream() {
        let body = serde_json::json!({
            "model": "ollama/qwen2.5-coder:7b",
            "messages": [{"role": "user", "content": "hi"}],
            "top_p": 0.9,
            "stop": ["\n\n"],
            "seed": 42,
            "frequency_penalty": 0.5,
            "presence_penalty": 0.25,
            "n": 1,
            "tool_choice": "auto",
            "parallel_tool_calls": false,
            "logit_bias": {"123": -100},
            "stream_options": {"include_usage": true},
            "user": "user-abc"
        });

        let req = request_from_openai_chat(&body).unwrap();

        // Captured into passthrough, but modeled fields are NOT duplicated there.
        assert_eq!(req.passthrough.get("top_p"), Some(&serde_json::json!(0.9)));
        assert!(req.passthrough.contains_key("tool_choice"));
        assert!(req.passthrough.contains_key("stream_options"));
        assert!(!req.passthrough.contains_key("model"));
        assert!(!req.passthrough.contains_key("messages"));

        // Re-emitted verbatim onto the upstream wire body.
        let wire = request_to_openai_wire(&req, "qwen2.5-coder:7b", false).unwrap();
        assert_eq!(wire.get("top_p"), Some(&serde_json::json!(0.9)));
        assert_eq!(wire.get("stop"), Some(&serde_json::json!(["\n\n"])));
        assert_eq!(wire.get("seed"), Some(&serde_json::json!(42)));
        assert_eq!(wire.get("frequency_penalty"), Some(&serde_json::json!(0.5)));
        assert_eq!(wire.get("tool_choice"), Some(&serde_json::json!("auto")));
        assert_eq!(
            wire.get("parallel_tool_calls"),
            Some(&serde_json::json!(false))
        );
        assert_eq!(
            wire.get("stream_options"),
            Some(&serde_json::json!({"include_usage": true}))
        );
        assert_eq!(wire.get("user"), Some(&serde_json::json!("user-abc")));
        // explicit fields still win and aren't clobbered
        assert_eq!(
            wire.get("model"),
            Some(&serde_json::json!("qwen2.5-coder:7b"))
        );
        assert_eq!(wire.get("stream"), Some(&serde_json::json!(false)));
    }

    #[test]
    fn tool_call_round_trip_through_canonical_ir() {
        // Request carrying a tool def + a prior assistant tool_call + a tool result.
        let body = serde_json::json!({
            "model": "x/y",
            "messages": [
                {"role": "user", "content": "weather in Paris?"},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "call_1", "type": "function",
                     "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}}
                ]},
                {"role": "tool", "tool_call_id": "call_1", "content": "18C sunny"}
            ],
            "tools": [{"type": "function", "function": {
                "name": "get_weather", "description": "w",
                "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}
            }}]
        });

        let req = request_from_openai_chat(&body).unwrap();
        assert_eq!(req.tools.len(), 1);
        assert_eq!(req.tools[0].name, "get_weather");

        // Wire re-emits tools[], the assistant tool_calls, and the tool message.
        let wire = request_to_openai_wire(&req, "y", false).unwrap();
        assert_eq!(wire["tools"].as_array().unwrap().len(), 1);
        let msgs = wire["messages"].as_array().unwrap();
        let assistant = msgs.iter().find(|m| m["role"] == "assistant").unwrap();
        assert_eq!(
            assistant["tool_calls"][0]["function"]["name"],
            "get_weather"
        );
        let tool_msg = msgs.iter().find(|m| m["role"] == "tool").unwrap();
        assert_eq!(tool_msg["tool_call_id"], "call_1");
        assert_eq!(tool_msg["content"], "18C sunny");

        // A response with structured tool_calls parses back into a ToolUse part.
        let resp_json = serde_json::json!({
            "choices": [{"index": 0, "finish_reason": "tool_calls", "message": {
                "role": "assistant", "content": null,
                "tool_calls": [{"id": "call_2", "type": "function",
                    "function": {"name": "get_weather", "arguments": "{\"city\":\"Lyon\"}"}}]
            }}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        });
        let parsed = parse_openai_chat_response(&resp_json).unwrap();
        assert!(parsed.message.content.iter().any(
            |p| matches!(p, sb_core::ContentPart::ToolUse { name, .. } if name == "get_weather")
        ));
    }
}
