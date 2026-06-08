use std::collections::BTreeMap;

use futures::StreamExt;
use sb_adapter::{AdapterError, EventStream};
use sb_core::{
    AiResponse, AiStreamEvent, ContentPart, ErrorClass, FinishReason, Message, Role, Usage,
};

/// Collect a canonical event stream into a single `AiResponse` (the
/// non-streaming path is just collection of the one streaming path).
pub(crate) async fn collect_response(
    mut stream: EventStream,
    req_id: String,
    model: String,
    max_bytes: Option<u64>,
) -> Result<AiResponse, AdapterError> {
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut images: Vec<ContentPart> = Vec::new();
    let mut citations: Vec<ContentPart> = Vec::new();
    let mut server_tools: Vec<ContentPart> = Vec::new();
    let mut tool_uses: BTreeMap<u32, (String, String, String)> = BTreeMap::new();
    let mut finish_reason = None;
    let mut usage = Usage::default();
    // Running tally of assembled bytes — the collect-path ceiling (Oracle #8)
    // aborts rather than buffering an unbounded non-streaming response.
    let mut assembled: u64 = 0;
    let over_cap = |assembled: u64| -> Option<AdapterError> {
        max_bytes.filter(|max| assembled > *max).map(|max| {
            AdapterError::new(
                ErrorClass::ServerError,
                format!("response exceeded max_response_bytes ({max})"),
            )
        })
    };

    while let Some(item) = stream.next().await {
        match item? {
            AiStreamEvent::TextDelta { text } => {
                assembled += text.len() as u64;
                if let Some(err) = over_cap(assembled) {
                    return Err(err);
                }
                content.push_str(&text);
            }
            AiStreamEvent::ToolCallStart(start) => {
                tool_uses.insert(start.index, (start.id, start.name, String::new()));
            }
            AiStreamEvent::ToolCallArgsDelta { index, json } => {
                assembled += json.len() as u64;
                if let Some(err) = over_cap(assembled) {
                    return Err(err);
                }
                if let Some((_, _, args)) = tool_uses.get_mut(&index) {
                    args.push_str(&json);
                }
            }
            AiStreamEvent::ToolCallEnd { .. } => {}
            AiStreamEvent::UsageDelta { usage: delta } => {
                usage = delta;
            }
            AiStreamEvent::MessageEnd {
                finish_reason: finish,
            } => {
                finish_reason = Some(finish);
            }
            AiStreamEvent::Error { message, class } => {
                return Err(AdapterError::new(class, message));
            }
            AiStreamEvent::ReasoningDelta { text } => {
                assembled += text.len() as u64;
                if let Some(err) = over_cap(assembled) {
                    return Err(err);
                }
                reasoning.push_str(&text);
            }
            AiStreamEvent::OutputImage { media_type, data } => {
                assembled += data.len() as u64;
                if let Some(err) = over_cap(assembled) {
                    return Err(err);
                }
                images.push(ContentPart::image_base64(media_type, data));
            }
            AiStreamEvent::Citation { url, title } => {
                citations.push(ContentPart::Citation {
                    url,
                    title,
                    snippet: None,
                });
            }
            AiStreamEvent::ServerToolCall { id, name, status } => {
                // Record the call once it completes; lifecycle states are transient.
                if status == "completed" {
                    server_tools.push(ContentPart::ServerToolUse {
                        id,
                        name,
                        args: serde_json::Value::Null,
                    });
                }
            }
            AiStreamEvent::MessageStart { .. } => {}
        }
    }

    let mut parts = Vec::new();
    // Reasoning precedes the answer (thinking, then output), matching the order
    // the model streamed it.
    if !reasoning.is_empty() {
        parts.push(ContentPart::Reasoning {
            text: reasoning,
            signature: None,
        });
    }
    if !content.is_empty() {
        parts.push(ContentPart::text(content));
    }
    parts.append(&mut images);
    parts.append(&mut citations);
    parts.append(&mut server_tools);

    for (_, (id, name, args)) in tool_uses {
        parts.push(ContentPart::ToolUse {
            id,
            name,
            args: serde_json::from_str(&args).unwrap_or(serde_json::Value::String(args)),
        });
    }

    Ok(AiResponse {
        id: req_id,
        model,
        message: Message {
            role: Role::Assistant,
            content: parts,
        },
        finish_reason: finish_reason.unwrap_or(FinishReason::Stop),
        usage,
    })
}

/// Streaming fallback is legal only before Switchback commits the first
/// downstream event to the client. Peek one upstream event before returning the
/// stream to the HTTP edge; an upstream error or empty stream at this point can
/// still fall over to another account/target without sending a partial response.
pub(crate) async fn precommit_stream(mut stream: EventStream) -> Result<EventStream, AdapterError> {
    match stream.next().await {
        Some(Ok(first)) => Ok(futures::stream::once(async move { Ok(first) })
            .chain(stream)
            .boxed()),
        Some(Err(error)) => Err(error),
        None => Err(AdapterError::new(
            ErrorClass::StreamInterrupted,
            "upstream stream ended before first event",
        )),
    }
}
