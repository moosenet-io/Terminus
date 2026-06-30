//! Decoupling compat layer — vendored copies of the small lumina-core surfaces
//! terminus-rs depends on, so terminus builds as a standalone crate with NO
//! `lumina-core` dependency (clean 2-crate chain chord → terminus).
//!
//! These are byte-for-byte copies of the canonical lumina-core types terminus
//! actually references (the S84 assistant-sweep dimensions use them):
//!
//!   - [`prompt::PromptAssembler`] — the real 5-layer Lumina system-prompt
//!     assembler (dim5_prompted), copied verbatim from
//!     `lumina-core::prompt` (mod + `layers` + `pulse` + `traits`); the 13
//!     S75 sprint submodules that the `assemble()` path never touches are NOT
//!     vendored.
//!   - [`conversation::buffer::ConversationBuffer`] — the S78 Tier-1 working
//!     memory buffer (dim3_memory), copied verbatim from
//!     `lumina-core::conversation::buffer`. Its one external reference,
//!     `lumina_core::chord::ChatMessage`, is replaced by the minimal local
//!     [`conversation::ChatMessage`] below (buffer only uses `role`, `content`
//!     and the `text()` constructor — the chord HTTP/JWT machinery is not
//!     needed here).
//!
//! Behavior is identical to lumina-core; this is a structural decouple (copy,
//! not rewrite), so the assistant-sweep measures the same production surfaces.

pub mod conversation;
pub mod prompt;
