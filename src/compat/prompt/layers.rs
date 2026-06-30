//! DPROMPT-01: Layer definitions, token budgeting, and section markers.
//!
//! The assembled system prompt is built from a fixed, ordered set of layers.
//! Each layer has a section marker (`[identity]`, `[style]`, …), an optional
//! backing file, a soft per-layer token budget, and a *truncation priority*
//! used when the assembled prompt exceeds the global budget.
//!
//! Later sprints add content to the `[personality]`, `[opinions]`,
//! `[knowledge]`, `[context]`, `[memory]`, and `[proactive]` sections — the
//! layer table here already reserves their markers and ordering so those
//! sprints only supply content, never re-thread the assembler.

/// Global maximum tokens for the whole assembled prompt (excluding per-query
/// retrieval, which has its own budget in DPROMPT-11).  The spec targets
/// ~900 tokens; the S75 ceiling was 1000.
///
/// S77 RESP-01/02 enlarged the protected `[rules]` and `[capabilities]` layers
/// (anti-fabrication + capability-accuracy guardrails), so the global ceiling
/// was raised to 1300 to preserve room for the dynamic personality/knowledge
/// layers rather than starving them under the protected static set.
///
/// The conversational-engagement rules grew the protected `[rules]` layer to a
/// 460-token cap; the ceiling was raised to 1450 to keep the same headroom for
/// the dynamic personality/knowledge layers rather than starving them.
pub const GLOBAL_TOKEN_BUDGET: usize = 1620;

/// Approximate token count for a string.
///
/// We use a words→tokens ratio of ~0.75 words/token (i.e. tokens ≈ words×4/3),
/// which tracks GPT-style tokenisers closely enough for budget enforcement
/// without pulling in a tokenizer dependency.
pub fn estimate_tokens(s: &str) -> usize {
    let words = s.split_whitespace().count();
    (words * 4).div_ceil(3)
}

/// Identifies a layer in the assembled prompt, in render order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerKind {
    Identity,
    Rules,
    Capabilities,
    Style,
    Personality,
    Opinions,
    Knowledge,
    Context,
    Memory,
    Proactive,
    Now,
}

/// Static configuration for one layer.
#[derive(Debug, Clone, Copy)]
pub struct LayerConfig {
    pub kind: LayerKind,
    /// Section marker emitted before the layer body, e.g. `"[identity]"`.
    pub marker: &'static str,
    /// Backing filename within the user's layer dir, if the layer is file-backed.
    /// `None` for computed layers (style, memory, proactive, now).
    pub filename: Option<&'static str>,
    /// Soft per-layer token budget; bodies longer than this are truncated.
    pub max_tokens: usize,
    /// Truncation priority when the whole prompt is over budget — higher is
    /// dropped first.  Identity (0) and Style (0) are never truncated.
    pub truncate_priority: u8,
}

/// The full ordered layer table.
pub const LAYERS: &[LayerConfig] = &[
    LayerConfig { kind: LayerKind::Identity,    marker: "[identity]",     filename: Some("core-identity.txt"),     max_tokens: 120, truncate_priority: 0 },
    LayerConfig { kind: LayerKind::Rules,       marker: "[rules]",        filename: Some("behavioral-rules.txt"),  max_tokens: 600, truncate_priority: 0 },
    LayerConfig { kind: LayerKind::Capabilities, marker: "[capabilities]", filename: Some("capabilities.txt"),     max_tokens: 360, truncate_priority: 0 },
    LayerConfig { kind: LayerKind::Style,       marker: "[style]",        filename: None,                           max_tokens: 40,  truncate_priority: 0 },
    LayerConfig { kind: LayerKind::Personality, marker: "[personality]", filename: Some("personality-vector.txt"),  max_tokens: 220, truncate_priority: 3 },
    LayerConfig { kind: LayerKind::Opinions,    marker: "[opinions]",    filename: Some("opinions.txt"),            max_tokens: 90,  truncate_priority: 4 },
    LayerConfig { kind: LayerKind::Knowledge,   marker: "[knowledge]",   filename: Some("knowledge-digest.txt"),    max_tokens: 320, truncate_priority: 2 },
    LayerConfig { kind: LayerKind::Context,     marker: "[context]",     filename: Some("active-context.txt"),      max_tokens: 160, truncate_priority: 1 },
    LayerConfig { kind: LayerKind::Memory,      marker: "[memory]",      filename: None,                            max_tokens: 220, truncate_priority: 5 },
    LayerConfig { kind: LayerKind::Proactive,   marker: "[proactive]",   filename: None,                            max_tokens: 80,  truncate_priority: 6 },
    LayerConfig { kind: LayerKind::Now,         marker: "[now]",         filename: None,                            max_tokens: 60,  truncate_priority: 0 },
];

/// Look up the config for a layer kind.
pub fn config_for(kind: LayerKind) -> &'static LayerConfig {
    LAYERS
        .iter()
        .find(|l| l.kind == kind)
        .expect("every LayerKind has a LayerConfig entry")
}

/// Truncate `body` to at most `max_tokens` approximate tokens, on a word
/// boundary, appending an ellipsis when content was dropped.
pub fn truncate_to_tokens(body: &str, max_tokens: usize) -> String {
    if estimate_tokens(body) <= max_tokens {
        return body.to_string();
    }
    // tokens ≈ words×4/3  →  words ≈ tokens×3/4.  Reserve one word for the
    // trailing ellipsis so the *result* (content + "…") still estimates at or
    // below `max_tokens`.
    let max_words = (max_tokens * 3 / 4).saturating_sub(1).max(1);
    let mut out: Vec<&str> = body.split_whitespace().take(max_words).collect();
    if out.is_empty() {
        return String::new();
    }
    out.push("…");
    let joined = out.join(" ");
    debug_assert!(estimate_tokens(&joined) <= max_tokens);
    joined
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_estimate_reasonable() {
        assert_eq!(estimate_tokens(""), 0);
        // 3 words → 4 tokens
        assert_eq!(estimate_tokens("one two three"), 4);
    }

    #[test]
    fn every_kind_has_config() {
        for k in [
            LayerKind::Identity, LayerKind::Rules, LayerKind::Capabilities,
            LayerKind::Style, LayerKind::Personality,
            LayerKind::Opinions, LayerKind::Knowledge, LayerKind::Context,
            LayerKind::Memory, LayerKind::Proactive, LayerKind::Now,
        ] {
            let _ = config_for(k);
        }
    }

    #[test]
    fn identity_and_style_never_truncated_priority() {
        assert_eq!(config_for(LayerKind::Identity).truncate_priority, 0);
        assert_eq!(config_for(LayerKind::Style).truncate_priority, 0);
        assert_eq!(config_for(LayerKind::Now).truncate_priority, 0);
    }

    #[test]
    fn truncate_keeps_short_text() {
        let s = "short body here";
        assert_eq!(truncate_to_tokens(s, 100), s);
    }

    #[test]
    fn truncate_long_text_adds_ellipsis() {
        let body = "word ".repeat(500);
        let out = truncate_to_tokens(&body, 50);
        assert!(out.ends_with('…'));
        assert!(estimate_tokens(&out) <= 60);
    }

    #[test]
    fn layer_order_is_canonical() {
        let order: Vec<LayerKind> = LAYERS.iter().map(|l| l.kind).collect();
        assert_eq!(order.first(), Some(&LayerKind::Identity));
        assert_eq!(order.last(), Some(&LayerKind::Now));
        // memory sits between context and now per the addendum spec
        let mem = order.iter().position(|k| *k == LayerKind::Memory).unwrap();
        let ctx = order.iter().position(|k| *k == LayerKind::Context).unwrap();
        let now = order.iter().position(|k| *k == LayerKind::Now).unwrap();
        assert!(ctx < mem && mem < now);
    }
}
