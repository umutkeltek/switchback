//! Canonical request/response/stream IR. Provider-agnostic by construction.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Convenience alias for opaque JSON (tool args, schemas).
pub type Json = serde_json::Value;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// One typed piece of message content. Tool calls/results are first-class,
/// not stringly-typed — so translation never has to guess.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text {
        text: String,
    },
    /// Assistant asking to call a tool.
    ToolUse {
        id: String,
        name: String,
        args: Json,
    },
    /// Result of a tool call, fed back to the model.
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default)]
        is_error: bool,
    },
}

impl ContentPart {
    pub fn text(s: impl Into<String>) -> Self {
        ContentPart::Text { text: s.into() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentPart>,
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Message { role: Role::User, content: vec![ContentPart::text(text)] }
    }
    pub fn assistant(text: impl Into<String>) -> Self {
        Message { role: Role::Assistant, content: vec![ContentPart::text(text)] }
    }
    /// Concatenated plain text of this message (ignores tool parts).
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|p| match p {
                ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// JSON Schema for the tool's parameters.
    pub parameters: Json,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ResponseFormat {
    Text,
    JsonObject,
    JsonSchema {
        name: String,
        schema: Json,
        #[serde(default)]
        strict: bool,
    },
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Priority {
    Low,
    #[default]
    Normal,
    High,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PrivacyClass {
    #[default]
    Standard,
    Sensitive,
    Confidential,
}

/// The canonical request. Every inbound protocol is translated INTO this;
/// no provider-specific fields live here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiRequest {
    pub id: String,
    /// `provider/model` (e.g. `mock/echo`) or a route name (e.g. `coding`).
    pub model: String,
    #[serde(default)]
    pub system: Option<String>,
    pub messages: Vec<Message>,
    #[serde(default)]
    pub tools: Vec<ToolSpec>,
    #[serde(default)]
    pub response_format: Option<ResponseFormat>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub priority: Priority,
    #[serde(default)]
    pub privacy_class: PrivacyClass,
    /// Free-form passthrough metadata (never secrets).
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

impl AiRequest {
    pub fn new(model: impl Into<String>, messages: Vec<Message>) -> Self {
        AiRequest {
            id: crate::new_id("req"),
            model: model.into(),
            system: None,
            messages,
            tools: Vec::new(),
            response_format: None,
            stream: false,
            max_output_tokens: None,
            temperature: None,
            priority: Priority::Normal,
            privacy_class: PrivacyClass::Standard,
            metadata: BTreeMap::new(),
        }
    }

    pub fn requires_tools(&self) -> bool {
        !self.tools.is_empty()
    }

    /// Last user message's plain text, if any.
    pub fn last_user_text(&self) -> Option<String> {
        self.messages
            .iter()
            .rev()
            .find(|m| m.role == Role::User)
            .map(|m| m.text())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    Stop,
    Length,
    ToolCalls,
    ContentFilter,
    Error,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cached_input_tokens: u64,
    #[serde(default)]
    pub reasoning_tokens: u64,
}

impl Usage {
    pub fn total(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }
}

/// A fully-assembled (non-streamed, or collected-from-stream) response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiResponse {
    pub id: String,
    pub model: String,
    pub message: Message,
    pub finish_reason: FinishReason,
    pub usage: Usage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallStart {
    pub index: u32,
    pub id: String,
    pub name: String,
}

/// The normalized streaming event. EVERY adapter emits this stream; the
/// non-streaming response is produced by collecting it. One path.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum AiStreamEvent {
    MessageStart { id: String, model: String },
    TextDelta { text: String },
    ReasoningDelta { text: String },
    ToolCallStart(ToolCallStart),
    ToolCallArgsDelta { index: u32, json: String },
    ToolCallEnd { index: u32 },
    UsageDelta { usage: Usage },
    MessageEnd { finish_reason: FinishReason },
    Error { message: String, class: crate::ErrorClass },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_text_concats_text_parts_only() {
        let m = Message {
            role: Role::Assistant,
            content: vec![
                ContentPart::text("hello "),
                ContentPart::ToolUse { id: "1".into(), name: "x".into(), args: Json::Null },
                ContentPart::text("world"),
            ],
        };
        assert_eq!(m.text(), "hello world");
    }

    #[test]
    fn request_round_trips_json() {
        let req = AiRequest::new("mock/echo", vec![Message::user("hi")]);
        let s = serde_json::to_string(&req).unwrap();
        let back: AiRequest = serde_json::from_str(&s).unwrap();
        assert_eq!(back.model, "mock/echo");
        assert_eq!(back.last_user_text().as_deref(), Some("hi"));
    }
}
