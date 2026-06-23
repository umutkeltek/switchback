//! OpenAI **Responses API** (`/v1/responses`) <-> canonical IR. This is the
//! API Codex speaks. Like every protocol it translates only to/from the
//! canonical IR (never directly to Chat Completions); the openai_compatible
//! adapter then speaks Chat Completions to the upstream. Hub-and-spoke.

use sb_core::{
    AiRequest, AiResponse, AiStreamEvent, ContentPart, EnergyUsage, FinishReason, ImageDetail,
    ImageSource, Message, Role, ToolResultContentPart, ToolSpec, Usage,
};
use serde_json::{json, Value};

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
            let parts = content_to_parts(item.get("content"))?;
            let text = content_parts_to_text(&parts);
            match role {
                "system" | "developer" => {
                    reject_image_parts(&parts, "Responses system/developer messages")?;
                    if !text.is_empty() {
                        req.system = Some(match req.system.take() {
                            Some(existing) => format!("{existing}\n{text}"),
                            None => text,
                        });
                    }
                }
                "assistant" => {
                    reject_image_parts(&parts, "Responses assistant messages")?;
                    req.messages.push(Message {
                        role: Role::Assistant,
                        content: parts,
                    });
                }
                _ => req.messages.push(Message {
                    role: Role::User,
                    content: parts,
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
            let (content, content_parts) = parse_tool_result_output(item.get("output"))?;
            req.messages.push(Message {
                role: Role::Tool,
                content: vec![ContentPart::ToolResult {
                    tool_use_id: id,
                    content,
                    content_parts,
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
    Ok(content_parts_to_text(&content_to_parts(content)?))
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

fn tool_result_parts_to_text(parts: &[ToolResultContentPart]) -> String {
    parts
        .iter()
        .filter_map(|part| match part {
            ToolResultContentPart::Text { text } if !text.is_empty() => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn parse_tool_result_output(
    output: Option<&Value>,
) -> Result<(String, Vec<ToolResultContentPart>), String> {
    match output {
        None | Some(Value::Null) => Ok((String::new(), Vec::new())),
        Some(Value::String(text)) => {
            let parts = if text.is_empty() {
                Vec::new()
            } else {
                vec![ToolResultContentPart::text(text.clone())]
            };
            Ok((text.clone(), parts))
        }
        Some(Value::Array(items)) => {
            let mut parts = Vec::new();
            for item in items {
                match item.get("type").and_then(Value::as_str) {
                    Some("input_text") => {
                        let text = item.get("text").and_then(Value::as_str).ok_or_else(|| {
                            "Responses tool result input_text missing string `text`".to_string()
                        })?;
                        parts.push(ToolResultContentPart::text(text));
                    }
                    Some("input_image") => {
                        let detail =
                            parse_image_detail(item.get("detail").and_then(Value::as_str))?;
                        let source = match (
                            item.get("image_url").and_then(Value::as_str),
                            item.get("file_id").and_then(Value::as_str),
                        ) {
                            (Some(url), _) => {
                                if let Some((media_type, data)) = split_data_url(url) {
                                    ImageSource::InlineBase64 { media_type, data }
                                } else {
                                    ImageSource::RemoteUrl {
                                        url: url.to_string(),
                                    }
                                }
                            }
                            (None, Some(id)) => ImageSource::ProviderFileRef {
                                provider: Some("openai".to_string()),
                                id: id.to_string(),
                            },
                            (None, None) => {
                                return Err(
                                    "Responses tool result input_image missing `image_url` or `file_id`"
                                        .to_string(),
                                )
                            }
                        };
                        parts.push(ToolResultContentPart::Image { source, detail });
                    }
                    Some(other) => {
                        return Err(format!(
                            "unsupported Responses tool result content part `{other}`"
                        ))
                    }
                    None => {
                        return Err(
                            "Responses tool result content part missing string `type`".to_string()
                        )
                    }
                }
            }
            let content = tool_result_parts_to_text(&parts);
            Ok((content, parts))
        }
        Some(_) => Err(
            "Responses function_call_output `output` must be a string, null, or array".to_string(),
        ),
    }
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

fn content_to_parts(content: Option<&Value>) -> Result<Vec<ContentPart>, String> {
    match content {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::String(s)) => Ok(vec![ContentPart::text(s.clone())]),
        Some(Value::Array(parts)) => {
            let mut content = Vec::new();
            for part in parts {
                match part.get("type").and_then(Value::as_str) {
                    Some("input_text") | Some("output_text") => {
                        if let Some(part_text) = part.get("text").and_then(Value::as_str) {
                            content.push(ContentPart::text(part_text));
                        } else {
                            return Err(
                                "Responses text content part missing string `text`".to_string()
                            );
                        }
                    }
                    Some("input_image") => {
                        let detail =
                            parse_image_detail(part.get("detail").and_then(Value::as_str))?;
                        let image_url = part.get("image_url").and_then(Value::as_str);
                        let file_id = part.get("file_id").and_then(Value::as_str);
                        match (image_url, file_id) {
                            (Some(url), _) => content.push(image_from_url(url.to_string(), detail)),
                            (None, Some(file_id)) => content.push(ContentPart::Image {
                                source: ImageSource::ProviderFileRef {
                                    provider: Some("openai".to_string()),
                                    id: file_id.to_string(),
                                },
                                detail,
                            }),
                            (None, None) => {
                                return Err(
                                    "Responses input_image part missing `image_url` or `file_id`"
                                        .to_string(),
                                )
                            }
                        }
                    }
                    Some(other) => {
                        return Err(format!("unsupported Responses content part `{other}`"));
                    }
                    None => return Err("Responses content part missing string `type`".to_string()),
                }
            }
            Ok(content)
        }
        Some(_) => Err("Responses message content must be a string, null, or array".to_string()),
    }
}

fn image_to_responses_content(part: &ContentPart) -> Result<Option<Value>, String> {
    let ContentPart::Image { source, detail } = part else {
        return Ok(None);
    };

    source.validate()?;
    let mut image = serde_json::Map::new();
    image.insert("type".to_string(), Value::String("input_image".to_string()));
    image.insert(
        "detail".to_string(),
        Value::String(detail.unwrap_or(ImageDetail::Auto).as_str().to_string()),
    );
    match source {
        ImageSource::InlineBase64 { media_type, data } => {
            image.insert(
                "image_url".to_string(),
                Value::String(format!("data:{media_type};base64,{data}")),
            );
        }
        ImageSource::RemoteUrl { url } => {
            image.insert("image_url".to_string(), Value::String(url.clone()));
        }
        ImageSource::ProviderFileRef { provider, id } => {
            if provider.as_deref().is_some_and(|owner| owner != "openai") {
                return Err(format!(
                    "OpenAI Responses cannot encode provider file ref owned by `{}`",
                    provider.as_deref().unwrap_or_default()
                ));
            }
            image.insert("file_id".to_string(), Value::String(id.clone()));
        }
    }
    Ok(Some(Value::Object(image)))
}

fn tool_result_image_to_responses_content(
    source: &ImageSource,
    detail: &Option<ImageDetail>,
) -> Result<Value, String> {
    source.validate()?;
    let mut image = serde_json::Map::new();
    image.insert("type".to_string(), Value::String("input_image".to_string()));
    image.insert(
        "detail".to_string(),
        Value::String(detail.unwrap_or(ImageDetail::Auto).as_str().to_string()),
    );
    match source {
        ImageSource::InlineBase64 { media_type, data } => {
            image.insert(
                "image_url".to_string(),
                Value::String(format!("data:{media_type};base64,{data}")),
            );
        }
        ImageSource::RemoteUrl { url } => {
            image.insert("image_url".to_string(), Value::String(url.clone()));
        }
        ImageSource::ProviderFileRef { provider, id } => {
            if provider.as_deref().is_some_and(|owner| owner != "openai") {
                return Err(format!(
                    "OpenAI Responses cannot encode provider file ref owned by `{}`",
                    provider.as_deref().unwrap_or_default()
                ));
            }
            image.insert("file_id".to_string(), Value::String(id.clone()));
        }
    }
    Ok(Value::Object(image))
}

fn tool_result_output_to_responses(
    content: &str,
    content_parts: &[ToolResultContentPart],
) -> Result<Value, String> {
    if content_parts.is_empty()
        || matches!(
            content_parts,
            [ToolResultContentPart::Text { text }] if text == content
        )
    {
        return Ok(Value::String(content.to_string()));
    }

    let mut output = Vec::new();
    for part in content_parts {
        match part {
            ToolResultContentPart::Text { text } => {
                output.push(json!({ "type": "input_text", "text": text }));
            }
            ToolResultContentPart::Image { source, detail } => {
                output.push(tool_result_image_to_responses_content(source, detail)?);
            }
        }
    }
    Ok(Value::Array(output))
}

fn message_content_to_responses_parts(message: &Message) -> Result<Vec<Value>, String> {
    let text_type = if message.role == Role::Assistant {
        "output_text"
    } else {
        "input_text"
    };
    let mut content = Vec::new();
    for part in &message.content {
        match part {
            ContentPart::Text { text } if !text.is_empty() => {
                content.push(json!({ "type": text_type, "text": text }));
            }
            ContentPart::Image { .. } => {
                if message.role != Role::User {
                    return Err(
                        "OpenAI Responses can only encode image content in user messages"
                            .to_string(),
                    );
                }
                if let Some(image) = image_to_responses_content(part)? {
                    content.push(image);
                }
            }
            _ => {}
        }
    }
    Ok(content)
}

/// Canonical `AiResponse` -> a non-streaming Responses object.
pub fn response_to_openai_responses(resp: &AiResponse) -> Value {
    let mut output = Vec::new();

    // Reasoning leads the output array (thinking, then the answer message).
    for part in &resp.message.content {
        if let ContentPart::Reasoning { text, .. } = part {
            if !text.is_empty() {
                output.push(json!({
                    "type": "reasoning",
                    "id": sb_core::new_id("rs"),
                    "summary": [{"type": "summary_text", "text": text}],
                }));
            }
        }
    }

    // URL citations ride as annotations on the answer text item.
    let annotations: Vec<Value> = resp
        .message
        .content
        .iter()
        .filter_map(|part| match part {
            ContentPart::Citation { url, title, .. } => {
                Some(json!({"type":"url_citation","url":url,"title":title}))
            }
            _ => None,
        })
        .collect();

    let text = resp.message.text();
    if !text.is_empty() {
        output.push(json!({
            "type": "message",
            "id": sb_core::new_id("msg"),
            "status": "completed",
            "role": "assistant",
            "content": [{"type": "output_text", "text": text, "annotations": annotations}],
        }));
    }

    for part in &resp.message.content {
        match part {
            ContentPart::ToolUse { id, name, args } => output.push(json!({
                "type": "function_call",
                "id": sb_core::new_id("fc"),
                "call_id": id,
                "name": name,
                "arguments": serde_json::to_string(args).unwrap_or_else(|_| "{}".to_string()),
                "status": "completed",
            })),
            // An assistant Image part is a model-generated image.
            ContentPart::Image {
                source: sb_core::ImageSource::InlineBase64 { media_type, data },
                ..
            } => output.push(json!({
                "type": "image_generation_call",
                "id": sb_core::new_id("img"),
                "status": "completed",
                "result": data,
                "output_format": media_type,
            })),
            ContentPart::ServerToolUse { id, name, args } => output.push(json!({
                "type": format!("{name}_call"),
                "id": id,
                "status": "completed",
                "action": args,
            })),
            _ => {}
        }
    }

    let mut response = json!({
        "id": resp.id,
        "object": "response",
        "status": "completed",
        "model": resp.model,
        "output": output,
        "usage": usage_json(&resp.usage),
    });
    if let Some(energy) = &resp.usage.energy {
        response["energy"] = energy_usage_json(energy);
    }
    response
}

/// Canonical `AiRequest` -> upstream Responses request body.
pub fn request_to_openai_responses_wire(
    req: &AiRequest,
    model: &str,
    stream: bool,
) -> Result<Value, String> {
    req.validate_canonical()?;
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
                reject_image_parts(&message.content, "Responses system messages")?;
                let text = message.text();
                if !text.is_empty() {
                    body.insert("instructions".to_string(), Value::String(text));
                }
            }
            Role::User | Role::Assistant => {
                let content = message_content_to_responses_parts(message)?;
                if !content.is_empty() {
                    let role = if message.role == Role::Assistant {
                        "assistant"
                    } else {
                        "user"
                    };
                    input.push(json!({
                        "type": "message",
                        "role": role,
                        "content": content,
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
                    match part {
                        ContentPart::ToolResult {
                            tool_use_id,
                            content,
                            content_parts,
                            ..
                        } => {
                            input.push(json!({
                                "type": "function_call_output",
                                "call_id": tool_use_id,
                                "output": tool_result_output_to_responses(content, content_parts)?,
                            }));
                        }
                        ContentPart::Image { .. } => {
                            return Err(
                                "OpenAI Responses cannot encode image content in tool messages"
                                    .to_string(),
                            );
                        }
                        _ => {}
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
    Ok(Value::Object(body))
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
        usage: parse_usage(
            body.get("usage"),
            body.get("energy").or_else(|| body.pointer("/usage/energy")),
        ),
    })
}

fn finish_reason_from_status(status: Option<&str>) -> FinishReason {
    match status {
        Some("incomplete") => FinishReason::Length,
        Some("failed") => FinishReason::Error,
        Some(_) | None => FinishReason::Stop,
    }
}

fn parse_usage(value: Option<&Value>, energy_value: Option<&Value>) -> Usage {
    let Some(value) = value else {
        return Usage {
            energy: parse_energy_usage(energy_value),
            ..Usage::default()
        };
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
        energy: parse_energy_usage(energy_value),
    }
}

fn usage_json(usage: &Usage) -> Value {
    let mut value = json!({
        "input_tokens": usage.input_tokens,
        "output_tokens": usage.output_tokens,
        "total_tokens": usage.total(),
    });
    if let Some(energy) = &usage.energy {
        value["energy"] = energy_usage_json(energy);
    }
    value
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
    // Reasoning summary item (gpt-5.x thinking), emitted before the message item.
    reasoning_id: String,
    reasoning: String,
    reasoning_open: bool,
    reasoning_item: Option<Value>,
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
            reasoning_id: sb_core::new_id("rs"),
            reasoning: String::new(),
            reasoning_open: false,
            reasoning_item: None,
        }
    }

    /// Close the reasoning summary item (idempotent), bumping `output_index` so
    /// the message/tool items that follow get fresh indices. The finished item
    /// is stashed for the `response.completed` output array.
    fn close_reasoning(&mut self, out: &mut Vec<String>) {
        if !self.reasoning_open || self.reasoning_item.is_some() {
            return;
        }
        out.push(Self::frame(
            "response.reasoning_summary_text.done",
            json!({"type":"response.reasoning_summary_text.done","item_id":self.reasoning_id,
                "output_index":self.output_index,"summary_index":0,"text":self.reasoning}),
        ));
        let item = json!({"type":"reasoning","id":self.reasoning_id,
            "summary":[{"type":"summary_text","text":self.reasoning}]});
        out.push(Self::frame(
            "response.output_item.done",
            json!({"type":"response.output_item.done","output_index":self.output_index,"item":item.clone()}),
        ));
        self.reasoning_item = Some(item);
        self.output_index += 1;
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
            AiStreamEvent::ServerToolCall { id, name, status } => {
                // Re-emit the provider-side tool lifecycle (web_search_call,
                // code_interpreter_call, …) so the client sees the same status.
                out.push(Self::frame(
                    &format!("response.{name}_call.{status}"),
                    json!({"type":format!("response.{name}_call.{status}"),
                        "item_id":id,"output_index":self.output_index}),
                ));
            }
            AiStreamEvent::OutputImage { media_type, data } => {
                // Reasoning leads; close it so the image item gets a fresh index.
                self.close_reasoning(&mut out);
                let item = json!({"type":"image_generation_call","id":sb_core::new_id("img"),
                    "status":"completed","result":data,"output_format":media_type});
                out.push(Self::frame(
                    "response.output_item.added",
                    json!({"type":"response.output_item.added","output_index":self.output_index,"item":item.clone()}),
                ));
                out.push(Self::frame(
                    "response.output_item.done",
                    json!({"type":"response.output_item.done","output_index":self.output_index,"item":item}),
                ));
                self.output_index += 1;
            }
            AiStreamEvent::Citation { url, title } => {
                // Annotate the open text item with a URL citation.
                out.push(Self::frame(
                    "response.output_text.annotation.added",
                    json!({"type":"response.output_text.annotation.added","item_id":self.item_id,
                        "output_index":self.output_index,"content_index":0,
                        "annotation":{"type":"url_citation","url":url,"title":title}}),
                ));
            }
            AiStreamEvent::TextDelta { text } => {
                // Reasoning always precedes the answer; close its item first so
                // the message item gets the next output_index.
                self.close_reasoning(&mut out);
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
            AiStreamEvent::ReasoningDelta { text } => {
                if !self.reasoning_open {
                    self.reasoning_open = true;
                    out.push(Self::frame(
                        "response.output_item.added",
                        json!({"type":"response.output_item.added","output_index":self.output_index,
                            "item":{"type":"reasoning","id":self.reasoning_id,"summary":[]}}),
                    ));
                    out.push(Self::frame(
                        "response.reasoning_summary_part.added",
                        json!({"type":"response.reasoning_summary_part.added","item_id":self.reasoning_id,
                            "output_index":self.output_index,"summary_index":0,
                            "part":{"type":"summary_text","text":""}}),
                    ));
                }
                self.reasoning.push_str(text);
                out.push(Self::frame(
                    "response.reasoning_summary_text.delta",
                    json!({"type":"response.reasoning_summary_text.delta","item_id":self.reasoning_id,
                        "output_index":self.output_index,"summary_index":0,"delta":text}),
                ));
            }
        }
        out
    }

    /// Close out reasoning + text + tool items and emit `response.completed`.
    fn finish(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        // Reasoning leads the output (thinking, then answer); close it first so
        // it occupies output_index 0 ahead of the message/tool items.
        self.close_reasoning(&mut out);
        let mut output_items: Vec<Value> = self.reasoning_item.clone().into_iter().collect();

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

        let mut completed_response = json!({
            "id": self.response_id,
            "object": "response",
            "status": self.status,
            "model": self.model,
            "output": output_items,
            "usage": usage_json(&self.usage),
        });
        if let Some(energy) = &self.usage.energy {
            completed_response["energy"] = energy_usage_json(energy);
        }
        out.push(Self::frame(
            "response.completed",
            json!({"type":"response.completed","response": completed_response}),
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
    fn parses_and_emits_structured_tool_result_output() {
        let body = json!({"model":"x/y","input":[
            {"type":"function_call","call_id":"c1","name":"inspect","arguments":"{}"},
            {"type":"function_call_output","call_id":"c1","output":[
                {"type":"input_text","text":"looked at image"},
                {"type":"input_image","image_url":"data:image/png;base64,abc","detail":"low"}
            ]}
        ]});

        let req = request_from_openai_responses(&body).unwrap();
        let ContentPart::ToolResult {
            content,
            content_parts,
            ..
        } = &req.messages[1].content[0]
        else {
            panic!("expected tool result");
        };
        assert_eq!(content, "looked at image");
        assert_eq!(content_parts.len(), 2);

        let wire = request_to_openai_responses_wire(&req, "x/y", false).unwrap();
        let output = wire["input"][1]["output"]
            .as_array()
            .expect("structured output");
        assert_eq!(output[0]["type"], "input_text");
        assert_eq!(output[0]["text"], "looked at image");
        assert_eq!(output[1]["type"], "input_image");
        assert_eq!(output[1]["image_url"], "data:image/png;base64,abc");
        assert_eq!(output[1]["detail"], "low");
    }

    #[test]
    fn parses_and_emits_image_content() {
        let body = json!({"model":"x/y","input":[{
            "type":"message",
            "role":"user",
            "content":[{"type":"input_image","image_url":"data:image/png;base64,abc","detail":"low"}]
        }]});

        let req = request_from_openai_responses(&body).unwrap();
        assert!(req.requires_vision());
        assert!(req.messages[0].content.iter().any(|part| matches!(
            part,
            ContentPart::Image {
                source: ImageSource::InlineBase64 { media_type, data },
                detail: Some(detail),
            } if media_type == "image/png"
                && data == "abc"
                && *detail == ImageDetail::Low
        )));

        let wire = request_to_openai_responses_wire(&req, "x/y", false).unwrap();
        assert_eq!(wire["input"][0]["content"][0]["type"], "input_image");
        assert_eq!(
            wire["input"][0]["content"][0]["image_url"],
            "data:image/png;base64,abc"
        );
        assert_eq!(wire["input"][0]["content"][0]["detail"], "low");
    }

    #[test]
    fn parses_and_emits_openai_file_ref_image() {
        let body = json!({"model":"x/y","input":[{
            "type":"message",
            "role":"user",
            "content":[{"type":"input_image","file_id":"file_123"}]
        }]});

        let req = request_from_openai_responses(&body).unwrap();
        assert!(req.messages[0].content.iter().any(|part| matches!(
            part,
            ContentPart::Image {
                source: ImageSource::ProviderFileRef { provider: Some(provider), id },
                ..
            } if provider == "openai" && id == "file_123"
        )));

        let wire = request_to_openai_responses_wire(&req, "x/y", false).unwrap();
        assert_eq!(wire["input"][0]["content"][0]["file_id"], "file_123");
    }

    #[test]
    fn responses_rejects_foreign_provider_file_ref() {
        let req = AiRequest::new(
            "x",
            vec![Message {
                role: Role::User,
                content: vec![ContentPart::image_file_ref(
                    Some("anthropic"),
                    "file_123",
                    None,
                )],
            }],
        );

        let err = request_to_openai_responses_wire(&req, "x/y", false).unwrap_err();
        assert!(err.contains("provider file ref owned by `anthropic`"));
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

        let body = request_to_openai_responses_wire(&req, "gpt-5.5", false).unwrap();

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
            },
            "energy": {
                "energy_joules": 5.23,
                "energy_kwh": 0.00000145,
                "duration_seconds": 0.0183,
                "measurement_available": true,
                "attribution_method": "time_weighted"
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
        let energy = parsed.usage.energy.as_ref().expect("energy metadata");
        assert_eq!(energy.energy_joules, Some(5.23));
        assert_eq!(energy.attribution_method.as_deref(), Some("time_weighted"));

        let emitted = response_to_openai_responses(&parsed);
        assert_eq!(emitted["energy"]["energy_kwh"], 0.00000145);
        assert_eq!(emitted["usage"]["energy"]["energy_joules"], 5.23);
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

    #[test]
    fn streaming_encoder_renders_reasoning_before_message() {
        let mut enc = OpenAiResponsesStreamEncoder::new("resp_1".into(), "x/y".into());
        let mut frames = String::new();
        for ev in [
            AiStreamEvent::MessageStart {
                id: "resp_1".into(),
                model: "x/y".into(),
            },
            AiStreamEvent::ReasoningDelta {
                text: "let me think".into(),
            },
            AiStreamEvent::TextDelta {
                text: "answer".into(),
            },
            AiStreamEvent::MessageEnd {
                finish_reason: FinishReason::Stop,
            },
        ] {
            frames.push_str(&enc.encode(&ev).join(""));
        }

        // Reasoning item opened, summary streamed, and closed before the answer.
        assert!(
            frames.contains("\"type\":\"reasoning\""),
            "reasoning item present"
        );
        assert!(frames.contains("event: response.reasoning_summary_text.delta"));
        assert!(frames.contains("\"delta\":\"let me think\""));
        assert!(frames.contains("event: response.reasoning_summary_text.done"));
        // Reasoning must precede the answer text in the stream.
        let r = frames.find("reasoning_summary_text.delta").unwrap();
        let t = frames.find("output_text.delta").unwrap();
        assert!(r < t, "reasoning streams before the answer");
        // Final output array carries the reasoning item then the message.
        let completed = frames
            .rsplit("event: response.completed")
            .next()
            .unwrap_or("");
        assert!(completed.contains("\"type\":\"reasoning\""));
        assert!(completed.contains("\"summary_text\""));
    }
}
