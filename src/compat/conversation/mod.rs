//! Vendored copy of `lumina-core::conversation` — only the [`buffer`] submodule
//! terminus actually uses (the S78 Tier-1 working-memory buffer). The parent
//! lumina-core `conversation` module additionally carries the SQLCipher store,
//! vault, and chord client, none of which terminus references, so only `buffer`
//! is vendored here.
//!
//! `buffer.rs` is copied verbatim from lumina-core save for its single external
//! reference, `crate::chord::ChatMessage`, which is replaced by the minimal
//! [`ChatMessage`] defined here (buffer only needs `role`, `content`, and the
//! `text()` constructor — identical observable behavior).

pub mod buffer;

/// Minimal stand-in for `lumina-core::chord::ChatMessage`, carrying only the
/// surface [`buffer::ConversationBuffer::context_messages`] produces and that
/// terminus reads back (`role`, optional `content`, and the `text()`
/// constructor). Behavior-identical to lumina-core's `ChatMessage` for the
/// user/assistant/system text messages the buffer emits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatMessage {
    pub role: String,
    /// Content may be null when a model returns tool_calls instead; the buffer
    /// only ever produces text messages, so it is always `Some` here.
    pub content: Option<String>,
}

impl ChatMessage {
    /// Convenience constructor for user/assistant/system text messages — matches
    /// `lumina-core::chord::ChatMessage::text`.
    pub fn text(role: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: Some(content.into()),
        }
    }
}
