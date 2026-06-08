//! Canonical request/response/stream IR. Provider-agnostic by construction.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

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

fn split_base64_data_url(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("data:")?;
    let (media_type, data) = rest.split_once(";base64,")?;
    if media_type.is_empty() || data.is_empty() {
        return None;
    }
    Some((media_type.to_string(), data.to_string()))
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ImageDetail {
    Low,
    High,
    Auto,
    Original,
}

impl ImageDetail {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "low" => Ok(Self::Low),
            "high" => Ok(Self::High),
            "auto" => Ok(Self::Auto),
            "original" => Ok(Self::Original),
            other => Err(format!("unsupported image detail `{other}`")),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::High => "high",
            Self::Auto => "auto",
            Self::Original => "original",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ImageSourceKind {
    InlineBase64,
    RemoteUrl,
    ProviderFileRef,
}

impl ImageSourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InlineBase64 => "inline_base64",
            Self::RemoteUrl => "remote_url",
            Self::ProviderFileRef => "provider_file_ref",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ImageSource {
    InlineBase64 {
        media_type: String,
        data: String,
    },
    RemoteUrl {
        url: String,
    },
    ProviderFileRef {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
        id: String,
    },
}

impl ImageSource {
    pub fn kind(&self) -> ImageSourceKind {
        match self {
            Self::InlineBase64 { .. } => ImageSourceKind::InlineBase64,
            Self::RemoteUrl { .. } => ImageSourceKind::RemoteUrl,
            Self::ProviderFileRef { .. } => ImageSourceKind::ProviderFileRef,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::InlineBase64 { media_type, data } => {
                if media_type.trim().is_empty() {
                    return Err("inline image missing media_type".to_string());
                }
                if !media_type.starts_with("image/") {
                    return Err(format!(
                        "inline image media_type `{media_type}` is not image/*"
                    ));
                }
                if data.trim().is_empty() {
                    return Err("inline image missing base64 data".to_string());
                }
                Ok(())
            }
            Self::RemoteUrl { url } => {
                if url.trim().is_empty() {
                    Err("remote image URL is empty".to_string())
                } else {
                    Ok(())
                }
            }
            Self::ProviderFileRef { provider, id } => {
                if provider.as_deref().is_some_and(str::is_empty) {
                    return Err("provider file image has empty provider scope".to_string());
                }
                if id.trim().is_empty() {
                    Err("provider file image missing id".to_string())
                } else {
                    Ok(())
                }
            }
        }
    }

    pub fn scoped_for(&self, provider: &str) -> bool {
        match self {
            Self::ProviderFileRef {
                provider: Some(owner),
                ..
            } => owner == provider,
            Self::ProviderFileRef { provider: None, .. } => true,
            _ => false,
        }
    }
}

/// One typed piece of message content. Tool calls/results are first-class,
/// not stringly-typed — so translation never has to guess.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text {
        text: String,
    },
    Image {
        source: ImageSource,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<ImageDetail>,
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
    /// Assistant reasoning / chain-of-thought summary. `signature` carries the
    /// provider's opaque verification signature (e.g. Anthropic extended
    /// thinking) so a multi-turn replay can re-submit the thinking block intact.
    Reasoning {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
}

impl ContentPart {
    pub fn text(s: impl Into<String>) -> Self {
        ContentPart::Text { text: s.into() }
    }

    pub fn image_url(url: impl Into<String>, detail: Option<ImageDetail>) -> Self {
        let url = url.into();
        let source = if let Some((media_type, data)) = split_base64_data_url(&url) {
            ImageSource::InlineBase64 { media_type, data }
        } else {
            ImageSource::RemoteUrl { url }
        };
        ContentPart::Image { source, detail }
    }

    pub fn image_base64(media_type: impl Into<String>, data: impl Into<String>) -> Self {
        ContentPart::Image {
            source: ImageSource::InlineBase64 {
                media_type: media_type.into(),
                data: data.into(),
            },
            detail: None,
        }
    }

    pub fn image_file_ref(
        provider: Option<impl Into<String>>,
        id: impl Into<String>,
        detail: Option<ImageDetail>,
    ) -> Self {
        ContentPart::Image {
            source: ImageSource::ProviderFileRef {
                provider: provider.map(Into::into),
                id: id.into(),
            },
            detail,
        }
    }

    pub fn image_source_kind(&self) -> Option<ImageSourceKind> {
        match self {
            ContentPart::Image { source, .. } => Some(source.kind()),
            _ => None,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        match self {
            ContentPart::Image { source, .. } => source.validate(),
            _ => Ok(()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentPart>,
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Message {
            role: Role::User,
            content: vec![ContentPart::text(text)],
        }
    }
    pub fn assistant(text: impl Into<String>) -> Self {
        Message {
            role: Role::Assistant,
            content: vec![ContentPart::text(text)],
        }
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
    /// Gateway attribution (NOT sent upstream): the tenant this request is billed
    /// to + an optional project label, resolved from the API key at the edge.
    /// Drives per-tenant usage attribution and quota enforcement.
    #[serde(default)]
    pub tenant: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    /// OpenAI request params we don't model as typed fields — `top_p`, `stop`,
    /// `seed`, `frequency_penalty`, `presence_penalty`, `n`, `tool_choice`,
    /// `parallel_tool_calls`, `logit_bias`, `logprobs`, `stream_options`,
    /// `user`, … — captured verbatim and forwarded to OpenAI-shaped upstreams
    /// for full API compatibility. Non-OpenAI targets ignore it.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub passthrough: serde_json::Map<String, Json>,
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
            tenant: None,
            project: None,
            passthrough: serde_json::Map::new(),
        }
    }

    pub fn requires_tools(&self) -> bool {
        !self.tools.is_empty()
    }

    pub fn requires_vision(&self) -> bool {
        self.image_count() > 0
    }

    pub fn image_count(&self) -> usize {
        self.messages
            .iter()
            .flat_map(|message| &message.content)
            .filter(|part| matches!(part, ContentPart::Image { .. }))
            .count()
    }

    pub fn required_image_sources(&self) -> BTreeSet<ImageSourceKind> {
        self.messages
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(ContentPart::image_source_kind)
            .collect()
    }

    pub fn validate_canonical(&self) -> Result<(), String> {
        for message in &self.messages {
            for part in &message.content {
                part.validate()?;
            }
        }
        Ok(())
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
    MessageStart {
        id: String,
        model: String,
    },
    TextDelta {
        text: String,
    },
    ReasoningDelta {
        text: String,
    },
    ToolCallStart(ToolCallStart),
    ToolCallArgsDelta {
        index: u32,
        json: String,
    },
    ToolCallEnd {
        index: u32,
    },
    UsageDelta {
        usage: Usage,
    },
    MessageEnd {
        finish_reason: FinishReason,
    },
    Error {
        message: String,
        class: crate::ErrorClass,
    },
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
                ContentPart::ToolUse {
                    id: "1".into(),
                    name: "x".into(),
                    args: Json::Null,
                },
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

    #[test]
    fn image_sources_round_trip_and_report_requirements() {
        let req = AiRequest::new(
            "vision",
            vec![Message {
                role: Role::User,
                content: vec![
                    ContentPart::image_base64("image/png", "abc"),
                    ContentPart::image_url(
                        "https://example.test/image.png",
                        Some(ImageDetail::Low),
                    ),
                    ContentPart::image_file_ref(
                        Some("openai"),
                        "file_123",
                        Some(ImageDetail::Auto),
                    ),
                ],
            }],
        );

        let json = serde_json::to_string(&req).unwrap();
        let back: AiRequest = serde_json::from_str(&json).unwrap();

        assert!(back.requires_vision());
        assert_eq!(back.image_count(), 3);
        assert_eq!(
            back.required_image_sources(),
            BTreeSet::from([
                ImageSourceKind::InlineBase64,
                ImageSourceKind::RemoteUrl,
                ImageSourceKind::ProviderFileRef,
            ])
        );
        assert!(back.validate_canonical().is_ok());
    }

    #[test]
    fn image_validation_rejects_bad_inline_images() {
        let request = AiRequest::new(
            "vision",
            vec![Message {
                role: Role::User,
                content: vec![ContentPart::Image {
                    source: ImageSource::InlineBase64 {
                        media_type: "text/plain".to_string(),
                        data: "abc".to_string(),
                    },
                    detail: None,
                }],
            }],
        );

        let err = request.validate_canonical().unwrap_err();
        assert!(err.contains("not image/*"));
    }
}
