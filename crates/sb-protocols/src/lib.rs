//! Wire-format translation. OpenAI Chat Completions is the canonical hub:
//! every inbound protocol translates `format -> AiRequest`, every outbound
//! response `AiStreamEvent -> format`. Never `format -> other_format`.
//!
//! OpenAI is the v1 hub.

pub mod openai;
