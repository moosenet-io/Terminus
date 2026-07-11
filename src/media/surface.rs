//! Media domain — Lumina surface integration (MEDIA-07).
//!
//! This module does NOT add any mutation logic and does NOT register a new
//! tool. The mutation-safety gates live in MEDIA-03 (`media_request`,
//! `media_organize`) and MEDIA-04 (`media_delete`, `media_cleanup`); this
//! module only *shapes and routes* around those tools so Lumina (the
//! personality-agent surface, running as the Terminus tool-subagent's
//! caller) can:
//!
//! 1. **Resolve fuzzy conversational intent to a tool or tool chain**
//!    ([`resolve_intent`]) -- e.g. "put something on" -> `media_recommend`;
//!    "grab that show" -> the `media_search` -> `media_status` ->
//!    `media_request` chain; "is X on Plex?" -> `media_status`; "clean up
//!    watched" -> `media_cleanup`. This repo has no separate
//!    keyword/intent-hint field on [`crate::tool::RustTool`] and no
//!    in-process subagent matcher (tool selection for the live MCP surface
//!    happens in Chord, out of this repo's scope) -- so this module's
//!    routing table is a standalone, directly-testable reference a
//!    subagent-side matcher (or a future in-repo one) can consult, in the
//!    same spirit as this repo's existing keyword-rich `description()`
//!    strings (BLUEPRINT.md #8).
//! 2. **Narrate a confirmation payload in Lumina's voice**
//!    ([`narrate_request_confirmation`], [`narrate_delete_confirmation`],
//!    [`narrate_cleanup_confirmation`]) -- turning MEDIA-03/04's raw
//!    `structured` JSON (title/year/size/quality for requests; exact target
//!    for deletes; enumerated set for cleanup) into a short first-person
//!    question, instead of Lumina reading raw tool JSON aloud.
//! 3. **Document/support multi-step chain composition**
//!    ([`MediaChain`], [`chain_for`]) while asserting -- see the
//!    `confirm_gate_holds_mid_chain` tests below -- that chaining tool calls
//!    can never bypass MEDIA-03/04's `confirm: true` / typed
//!    `confirm_delete` gate. A chain is just an ordered sequence of
//!    independent tool calls; nothing here (or in MEDIA-03/04) special-cases
//!    "this call is part of a chain" to skip a gate.

use serde_json::Value;

// ── 1. Intent routing (pure, testable) ──────────────────────────────────────

/// What a resolved conversational intent maps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaIntent {
    /// A single tool call fully satisfies the intent.
    Tool(&'static str),
    /// An ordered chain of tool calls. Each step is still an independent
    /// tool call with its own gates (see module docs, point 3) -- this is
    /// routing metadata, not a bypass mechanism.
    Chain(&'static [&'static str]),
    /// The phrase doesn't carry enough information to pick a tool/chain --
    /// Lumina should ask a clarifying question, not guess.
    Clarify(&'static str),
}

/// One entry in the intent-routing table: keywords that, if any is found in
/// the (lowercased) phrase, resolve it to `intent`. Routes are matched in
/// table order -- put more specific routes first so e.g. "clean up" doesn't
/// get eaten by a broader "watch" route.
struct IntentRoute {
    keywords: &'static [&'static str],
    intent: MediaIntent,
}

/// Representative phrase -> tool/chain routing table. Not exhaustive NLU --
/// a small, directly-testable keyword table mirroring the EDGE CASES called
/// out in the S94 spec's MEDIA-07 item.
const INTENT_ROUTES: &[IntentRoute] = &[
    // Destructive bulk (must win over a bare "watch"/"clean" collision).
    IntentRoute {
        keywords: &["clean up", "cleanup", "free up space", "remove what i've watched", "remove watched"],
        intent: MediaIntent::Tool("media_cleanup"),
    },
    // Destructive single-item delete.
    IntentRoute {
        keywords: &["delete", "get rid of", "remove it", "remove this"],
        intent: MediaIntent::Tool("media_delete"),
    },
    // Presence/availability check.
    IntentRoute {
        keywords: &["is on plex", "on plex?", "do i have", "do we have", "already have", "is it in my library", "check if"],
        intent: MediaIntent::Tool("media_status"),
    },
    // Acquisition chain: resolve title -> check presence -> request.
    IntentRoute {
        keywords: &["grab", "download", "get me", "watch that", "get that show", "get that movie"],
        intent: MediaIntent::Chain(&["media_search", "media_status", "media_request"]),
    },
    // Direct request when the target is already fully specified.
    IntentRoute {
        keywords: &["request ", "add to radarr", "add to sonarr"],
        intent: MediaIntent::Tool("media_request"),
    },
    // Non-destructive organize (tag/monitor/collection).
    IntentRoute {
        keywords: &["stop monitoring", "tag it", "add to collection", "mark as monitored"],
        intent: MediaIntent::Tool("media_organize"),
    },
    // Continue-watching / up-next.
    IntentRoute {
        keywords: &["on deck", "continue watching", "what's next", "up next"],
        intent: MediaIntent::Tool("media_on_deck"),
    },
    IntentRoute {
        keywords: &["recently added", "what's new", "new on plex"],
        intent: MediaIntent::Tool("media_recently_added"),
    },
    // Recommendation / passive "put something on" (checked after the more
    // specific acquisition/status routes so it only catches genuinely
    // open-ended requests).
    IntentRoute {
        keywords: &["put something on", "suggest something", "recommend something", "what should i watch", "surprise me"],
        intent: MediaIntent::Tool("media_recommend"),
    },
    // Plain lookup.
    IntentRoute {
        keywords: &["search for", "find me", "look up"],
        intent: MediaIntent::Tool("media_search"),
    },
];

/// Resolve a fuzzy conversational phrase to the media tool/chain it implies.
/// Pure function, keyword/substring matched (case-insensitive) -- not full
/// NLU, but deterministic and directly unit-testable. Returns
/// [`MediaIntent::Clarify`] with a ready-to-ask question when the phrase is
/// too vague to route confidently (EDGE CASE: under-specified intent must
/// surface as a question, never a wrong action).
pub fn resolve_intent(phrase: &str) -> MediaIntent {
    let normalized = phrase.trim().to_lowercase();
    if normalized.is_empty() {
        return MediaIntent::Clarify("What would you like me to do with your media library?");
    }

    for route in INTENT_ROUTES {
        if route.keywords.iter().any(|kw| normalized.contains(kw)) {
            return route.intent;
        }
    }

    // Media-domain-adjacent vocabulary present, but not specific enough to
    // pick a tool -- ask rather than guess.
    let media_words = ["movie", "show", "watch", "media", "plex", "library", "episode", "series"];
    if media_words.iter().any(|w| normalized.contains(w)) {
        return MediaIntent::Clarify(
            "Do you want me to look something up, check if you already have it, request it, or clean something out?",
        );
    }

    MediaIntent::Clarify("I'm not sure what media action you mean -- could you say a bit more?")
}

/// Convenience: the ordered tool names a resolved intent implies, whether it
/// was a single tool or a chain. Empty for [`MediaIntent::Clarify`].
pub fn chain_for(intent: MediaIntent) -> Vec<&'static str> {
    match intent {
        MediaIntent::Tool(name) => vec![name],
        MediaIntent::Chain(names) => names.to_vec(),
        MediaIntent::Clarify(_) => Vec::new(),
    }
}

// ── 3. Multi-step chain description (documentation + composability check) ──

/// A named, ordered multi-step chain the subagent can compose from
/// independent tool calls. Purely descriptive -- executing a chain is just
/// calling each tool in order; nothing here executes anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaChain {
    pub name: &'static str,
    pub steps: &'static [&'static str],
    pub description: &'static str,
}

/// The canonical "grab that show/movie" chain: resolve the fuzzy title,
/// check whether it's already present, then request it. Each step is an
/// independent tool call -- `media_request`'s own tiering/confirm gate
/// (MEDIA-03) still applies at the last step regardless of how it was
/// reached.
pub const SEARCH_STATUS_REQUEST_CHAIN: MediaChain = MediaChain {
    name: "search_status_request",
    steps: &["media_search", "media_status", "media_request"],
    description: "Resolve a fuzzy title, confirm it isn't already in the library, then request it. \
        The final media_request call is still subject to MEDIA-03's tiering: a Confirm-tier request \
        returns a confirmation payload, not an executed result, no matter how it was reached.",
};

// ── 2. Confirmation-prompt shaping (pure, testable) ─────────────────────────

const BYTES_PER_GB: f64 = 1024.0 * 1024.0 * 1024.0;

/// Turn a `media_request` / `media_organize` (MEDIA-03) **unexecuted,
/// Confirm-tier** `structured` payload into a short, first-person question
/// Lumina can say aloud, carrying the concrete specifics (title/size/
/// quality) rather than raw JSON. Returns `None` when there is nothing to
/// confirm -- either the call already executed, or it was never a
/// Confirm-tier response in the first place (both are the correct "no
/// narration needed" outcomes, not an error).
pub fn narrate_request_confirmation(structured: &Value) -> Option<String> {
    if structured.get("tier").and_then(|v| v.as_str()) != Some("confirm") {
        return None;
    }
    if structured.get("executed").and_then(|v| v.as_bool()).unwrap_or(true) {
        return None;
    }

    let title = structured.get("title").and_then(|v| v.as_str()).unwrap_or("that");
    let year = structured.get("year").and_then(|v| v.as_str());
    let quality = structured.get("quality_hint").and_then(|v| v.as_str());
    let size_gb = structured.get("estimated_size_bytes").and_then(|v| v.as_u64()).map(|b| b as f64 / BYTES_PER_GB);

    let mut desc = format!("\"{title}\"");
    if let Some(y) = year {
        desc.push_str(&format!(" ({y})"));
    }
    let mut detail = String::new();
    if let Some(gb) = size_gb {
        detail.push_str(&format!("that's a ~{gb:.0}GB"));
    } else {
        detail.push_str("that's a");
    }
    if let Some(q) = quality {
        detail.push_str(&format!(" {q}"));
    }
    detail.push_str(" grab");

    Some(format!("{desc} -- {detail} -- want me to go ahead?"))
}

/// Turn a `media_delete` (MEDIA-04) unexecuted `structured` payload into an
/// in-voice hard-confirm prompt. `media_delete`'s gate is a typed exact-title
/// match, not a yes/no `confirm: true` -- the narration says so explicitly
/// rather than inviting a casual "yes".
pub fn narrate_delete_confirmation(structured: &Value) -> Option<String> {
    if structured.get("executed").and_then(|v| v.as_bool()).unwrap_or(true) {
        return None;
    }
    if !structured.get("requires_confirmation").and_then(|v| v.as_bool()).unwrap_or(false) {
        return None;
    }
    let title = structured.get("title").and_then(|v| v.as_str())?;
    Some(format!(
        "That'll permanently delete \"{title}\" and its files -- there's no undo. Say \"yes, delete {title}\" and I'll confirm with the exact title to go ahead."
    ))
}

/// Turn a `media_cleanup` (MEDIA-04) unexecuted `structured` payload
/// (enumerated eligible/flagged sets) into an in-voice hard-confirm prompt
/// that names every title that would be removed.
pub fn narrate_cleanup_confirmation(structured: &Value) -> Option<String> {
    if structured.get("executed").and_then(|v| v.as_bool()).unwrap_or(true) {
        return None;
    }
    if !structured.get("requires_confirmation").and_then(|v| v.as_bool()).unwrap_or(false) {
        return None;
    }
    let eligible: Vec<String> = structured
        .get("eligible")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    if eligible.is_empty() {
        return None;
    }
    let flagged_count = structured.get("flagged").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
    let mut prompt = format!(
        "I'd be permanently removing {} thing{}: {}.",
        eligible.len(),
        if eligible.len() == 1 { "" } else { "s" },
        eligible.join(", ")
    );
    if flagged_count > 0 {
        prompt.push_str(&format!(
            " {flagged_count} other item{} not watched by everyone, so I'm leaving {} alone.",
            if flagged_count == 1 { "" } else { "s" },
            if flagged_count == 1 { "it" } else { "them" }
        ));
    }
    prompt.push_str(" Want me to go ahead?");
    Some(prompt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── resolve_intent ───────────────────────────────────────────────────

    #[test]
    fn put_something_on_routes_to_recommend() {
        assert_eq!(resolve_intent("put something on"), MediaIntent::Tool("media_recommend"));
        assert_eq!(resolve_intent("Surprise me tonight"), MediaIntent::Tool("media_recommend"));
    }

    #[test]
    fn grab_that_show_routes_to_search_status_request_chain() {
        assert_eq!(
            resolve_intent("grab that show I was watching"),
            MediaIntent::Chain(&["media_search", "media_status", "media_request"])
        );
    }

    #[test]
    fn is_x_on_plex_routes_to_status() {
        assert_eq!(resolve_intent("is Dune on Plex?"), MediaIntent::Tool("media_status"));
        assert_eq!(resolve_intent("do I already have Arrival"), MediaIntent::Tool("media_status"));
    }

    #[test]
    fn clean_up_watched_routes_to_cleanup() {
        assert_eq!(resolve_intent("clean up what I've watched"), MediaIntent::Tool("media_cleanup"));
    }

    #[test]
    fn delete_it_routes_to_delete_not_cleanup() {
        assert_eq!(resolve_intent("delete that movie"), MediaIntent::Tool("media_delete"));
    }

    #[test]
    fn what_should_i_watch_routes_to_recommend() {
        assert_eq!(resolve_intent("what should I watch tonight?"), MediaIntent::Tool("media_recommend"));
    }

    #[test]
    fn on_deck_routes_to_on_deck() {
        assert_eq!(resolve_intent("what's on deck?"), MediaIntent::Tool("media_on_deck"));
    }

    #[test]
    fn recently_added_routes_correctly() {
        assert_eq!(resolve_intent("what's new on Plex"), MediaIntent::Tool("media_recently_added"));
    }

    #[test]
    fn matching_is_case_insensitive() {
        assert_eq!(resolve_intent("GRAB THAT SHOW"), MediaIntent::Chain(&["media_search", "media_status", "media_request"]));
    }

    #[test]
    fn empty_phrase_is_a_clarifying_question() {
        assert!(matches!(resolve_intent(""), MediaIntent::Clarify(_)));
        assert!(matches!(resolve_intent("   "), MediaIntent::Clarify(_)));
    }

    #[test]
    fn under_specified_media_mention_is_a_question_not_a_wrong_action() {
        // EDGE CASE: contains media vocabulary but no clear verb/action --
        // must surface as a clarifying question, never guess a tool.
        let intent = resolve_intent("something about my movies");
        assert!(matches!(intent, MediaIntent::Clarify(_)));
    }

    #[test]
    fn completely_unrelated_phrase_is_a_clarifying_question_not_a_tool() {
        let intent = resolve_intent("what's the weather like");
        assert!(matches!(intent, MediaIntent::Clarify(_)));
    }

    #[test]
    fn clarify_question_is_non_empty_and_a_question() {
        for phrase in ["", "something about media", "gibberish xyz"] {
            if let MediaIntent::Clarify(q) = resolve_intent(phrase) {
                assert!(!q.is_empty());
                assert!(q.contains('?'), "clarifying prompt should read as a question: {q}");
            }
        }
    }

    // ── chain_for ────────────────────────────────────────────────────────

    #[test]
    fn chain_for_tool_is_single_element() {
        assert_eq!(chain_for(MediaIntent::Tool("media_status")), vec!["media_status"]);
    }

    #[test]
    fn chain_for_chain_preserves_order() {
        let steps = chain_for(MediaIntent::Chain(&["media_search", "media_status", "media_request"]));
        assert_eq!(steps, vec!["media_search", "media_status", "media_request"]);
    }

    #[test]
    fn chain_for_clarify_is_empty() {
        assert!(chain_for(MediaIntent::Clarify("?")).is_empty());
    }

    #[test]
    fn search_status_request_chain_constant_matches_grab_routing() {
        assert_eq!(SEARCH_STATUS_REQUEST_CHAIN.steps.to_vec(), chain_for(resolve_intent("grab that movie")));
    }

    // ── narrate_request_confirmation ────────────────────────────────────

    #[test]
    fn narrates_confirm_tier_request_with_specifics() {
        let structured = json!({
            "title": "Dune",
            "year": "2021",
            "media_type": "movie",
            "quality_hint": "2160p remux",
            "estimated_size_bytes": 60u64 * 1024 * 1024 * 1024,
            "tier": "confirm",
            "executed": false,
        });
        let prompt = narrate_request_confirmation(&structured).expect("must narrate a confirm-tier payload");
        assert!(prompt.contains("Dune"));
        assert!(prompt.contains("2021"));
        assert!(prompt.contains("60GB"));
        assert!(prompt.contains("2160p remux"));
        assert!(prompt.contains('?'), "must read as an in-voice question: {prompt}");
    }

    #[test]
    fn no_narration_when_already_executed() {
        let structured = json!({
            "title": "Arrival", "tier": "light", "executed": true,
        });
        assert!(narrate_request_confirmation(&structured).is_none());
    }

    #[test]
    fn no_narration_when_not_confirm_tier() {
        let structured = json!({
            "title": "Arrival", "tier": "light", "executed": false,
        });
        assert!(narrate_request_confirmation(&structured).is_none());
    }

    #[test]
    fn narration_handles_missing_size_gracefully() {
        let structured = json!({ "title": "Foundation", "tier": "confirm", "executed": false });
        let prompt = narrate_request_confirmation(&structured).unwrap();
        assert!(prompt.contains("Foundation"));
        assert!(!prompt.contains("GB"));
    }

    // ── narrate_delete_confirmation ─────────────────────────────────────

    #[test]
    fn narrates_delete_confirmation_with_exact_title() {
        let structured = json!({
            "id": 5, "media_type": "movie", "title": "Dune",
            "executed": false, "requires_confirmation": true,
        });
        let prompt = narrate_delete_confirmation(&structured).expect("must narrate a pending delete");
        assert!(prompt.contains("Dune"));
        assert!(prompt.to_lowercase().contains("permanently"));
    }

    #[test]
    fn no_delete_narration_once_executed() {
        let structured = json!({ "id": 5, "title": "Dune", "executed": true });
        assert!(narrate_delete_confirmation(&structured).is_none());
    }

    #[test]
    fn no_delete_narration_for_already_absent_no_op() {
        // media_delete's "already_absent" no-op path never requires
        // confirmation -- must not fabricate a prompt for it.
        let structured = json!({
            "id": 5, "title": "Dune", "executed": false, "already_absent": true,
        });
        assert!(narrate_delete_confirmation(&structured).is_none());
    }

    // ── narrate_cleanup_confirmation ─────────────────────────────────────

    #[test]
    fn narrates_cleanup_confirmation_listing_titles() {
        let structured = json!({
            "media_type": "movie", "executed": false, "requires_confirmation": true,
            "eligible": ["Old Movie 1", "Old Movie 2"],
            "flagged": ["Shared Family Favorite"],
        });
        let prompt = narrate_cleanup_confirmation(&structured).expect("must narrate a pending cleanup");
        assert!(prompt.contains("Old Movie 1"));
        assert!(prompt.contains("Old Movie 2"));
        assert!(prompt.contains('2'));
        assert!(prompt.to_lowercase().contains("other item") || prompt.contains('1'));
        assert!(prompt.contains('?'));
    }

    #[test]
    fn no_cleanup_narration_when_nothing_eligible() {
        let structured = json!({
            "media_type": "movie", "executed": false,
            "eligible": [], "flagged": ["Shared Family Favorite"],
        });
        assert!(narrate_cleanup_confirmation(&structured).is_none());
    }

    #[test]
    fn no_cleanup_narration_once_executed() {
        let structured = json!({
            "media_type": "movie", "executed": true,
            "deleted": ["Old Movie 1"], "already_absent": [], "failed": [], "flagged": [],
        });
        assert!(narrate_cleanup_confirmation(&structured).is_none());
    }

    // ── NEGATIVE: the mutation confirm gate holds mid-chain ─────────────
    //
    // These reuse MEDIA-03's own `classify_request`/`MediaRequest` and
    // MEDIA-04's `MediaDelete`/`MediaCleanup` to prove that reaching a
    // mutation tool via a resolved chain (search -> status -> request, or
    // any other ordering the subagent composes) changes NOTHING about
    // whether it executes. There is no "came from a chain" flag anywhere in
    // this domain's tools for a chain to exploit.

    use crate::media::request::{classify_request, MutationTier, RequestKind};
    use crate::registry::ToolRegistry;

    #[tokio::test]
    async fn chained_confirm_tier_request_still_returns_confirmation_not_executed() {
        // Simulate the subagent having just run media_search then
        // media_status (per SEARCH_STATUS_REQUEST_CHAIN) and now calling the
        // final step, media_request, for a whole series (always Confirm-tier
        // per MEDIA-03) -- WITHOUT confirm: true, exactly as a chain that
        // tried to skip the gate would. Goes through the real, public
        // registry/tool-call path (no private-field access, no mocking
        // needed -- the Confirm-tier-and-unconfirmed branch returns before
        // any client is ever touched).
        assert_eq!(chain_for(resolve_intent("grab that show")), SEARCH_STATUS_REQUEST_CHAIN.steps.to_vec());

        let mut registry = ToolRegistry::new();
        crate::media::request::register(&mut registry);
        let result = registry
            .call(
                "media_request",
                serde_json::json!({ "title": "Foundation", "media_type": "series", "tvdb_id": 358903 }),
            )
            .await
            .expect("media_request must be registered")
            .expect("media_request must not error for a well-formed, unconfirmed whole-series request");
        let parsed: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed["structured"]["executed"], false, "a chain must not bypass the confirm gate");
        assert_eq!(parsed["structured"]["tier"], "confirm");

        // And the surface layer narrates that pending confirmation instead
        // of Lumina reporting a false "done".
        let prompt = narrate_request_confirmation(&parsed["structured"]).expect("must narrate the pending confirm");
        assert!(prompt.contains("Foundation"));
        assert!(prompt.contains('?'));
    }

    #[test]
    fn classify_request_confirm_tier_never_downgrades_to_light_for_chain_reasons() {
        // Reinforces MEDIA-03's own guarantee from this surface's point of
        // view: nothing about "this call is step 3 of a chain" is a legal
        // input to classify_request at all -- it only ever sees
        // kind/is_ambiguous/item_count/size, so a chain cannot smuggle in a
        // Light-tier classification for what is otherwise a Confirm case.
        assert_eq!(
            classify_request(RequestKind::Series, false, 1, 1024 * 1024 * 1024),
            MutationTier::Confirm
        );
    }
}
