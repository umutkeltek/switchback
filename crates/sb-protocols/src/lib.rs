//! Wire-format translation. OpenAI Chat Completions is the canonical hub:
//! every inbound protocol translates `format -> AiRequest`, every outbound
//! response `AiStreamEvent -> format`. Never `format -> other_format`.
//!
//! (Implemented by the `openai` module — to be filled in.)
