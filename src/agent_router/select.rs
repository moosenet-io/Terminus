//! TRTR-02 — identity-scoped tool SELECTION for the relocated router.
//!
//! The router moved out of Chord because Chord has no caller identity: it held one
//! flat catalog for every client, so tool exposure could not be scoped per user and a
//! family member would have been offered the operator's whole fleet surface.
//! Selection happens HERE, at the Terminus egress, where the principal is already
//! resolved by `mesh::principal`.
//!
//! Three filters compose, in this order, and each can only ever REMOVE:
//! 1. **Authorization** — `gateway_framework::AllowlistPolicy` (per-principal).
//! 2. **Availability** — `crate::availability` (is the tool alive at all, TAVAIL-01).
//! 3. **Relevance** — lexical scoring against the user's query, so the model sees a
//!    handful of plausible tools rather than 400.
//!
//! Because 1 and 2 run BEFORE relevance, an unauthorized or parked tool is never even
//! a candidate — the model cannot be tempted into calling something it would then be
//! refused, which is the failure mode that produced the `deep_research` phantom.

use crate::availability;
use crate::gateway_framework::GatewayFramework;
use crate::mesh::principal::Principal;
use crate::registry::ToolInfo;

/// How many discovered tools to offer the model. Enough to cover a plausible request,
/// small enough to keep the prompt cheap and the model decisive — the whole reason the
/// catalog is narrowed at all.
pub const MAX_SELECTED: usize = 12;

/// Tools always offered when the caller is allowed them: without a clock and a search
/// the assistant cannot ground even trivial questions, and it will confabulate instead.
pub const ESSENTIALS: &[&str] = &["utc_now", "health", "searxng_search"];

/// Score `tool` against `query` tokens. Name matches dominate description matches —
/// a tool literally named `weather` should win the query "weather" outright.
fn score(tool: &ToolInfo, q_tokens: &[String]) -> i32 {
    let name_l = tool.name.to_lowercase();
    let desc_l = tool.description.to_lowercase();
    let mut s = 0;
    for t in q_tokens {
        if name_l == *t {
            s += 10; // exact name hit
        } else if name_l.contains(t.as_str()) {
            s += 3;
        } else if desc_l.contains(t.as_str()) {
            s += 1;
        }
    }
    s
}

/// Very small stopword set — enough that "what is the weather" scores on `weather`
/// rather than matching every tool whose description contains "the".
const STOPWORDS: &[&str] = &[
    "the", "a", "an", "is", "are", "was", "were", "what", "whats", "how", "do", "does", "did",
    "my", "me", "i", "you", "please", "can", "could", "would", "to", "of", "in", "on", "for",
    "and", "or", "it", "this", "that", "now", "right", "tell",
];

fn tokenize(q: &str) -> Vec<String> {
    q.to_lowercase()
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|w| w.len() >= 2 && !STOPWORDS.contains(w))
        .map(|w| w.to_string())
        .collect()
}

/// Whether `principal` may use `tool`, per the authorization gate.
///
/// Fail-closed: when a gateway is configured but the caller has NO principal, nothing
/// is selectable. That mirrors `filter_catalog_for_principal`, which returns an empty
/// catalog for an unidentified caller.
fn authorized(gateway: Option<&GatewayFramework>, principal: Option<&Principal>, tool: &str) -> bool {
    match gateway {
        Some(gw) => match principal {
            Some(p) => gw.permits_tool(p.name(), tool),
            None => false,
        },
        // No gateway configured (e.g. terminus_personal): unchanged, ungated behaviour.
        None => true,
    }
}

/// Select the tools to offer the model for `query`.
///
/// `catalog` is the full merged tool list; the caller supplies it so this stays a pure
/// function over data (and therefore straightforwardly testable without a live
/// registry, a gateway, or a network).
pub fn select_tools(
    catalog: &[ToolInfo],
    query: &str,
    gateway: Option<&GatewayFramework>,
    principal: Option<&Principal>,
) -> Vec<ToolInfo> {
    let avail = availability::policy();
    let q_tokens = tokenize(query);

    // Filters 1 + 2 first: an unauthorized or parked tool is never a candidate.
    let eligible: Vec<&ToolInfo> = catalog
        .iter()
        .filter(|t| avail.agent_usable(&t.name))
        .filter(|t| authorized(gateway, principal, &t.name))
        .collect();

    let mut selected: Vec<ToolInfo> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Filter 3: relevance. Skipped entirely for an empty query, where every tool
    // scores 0 and an arbitrary alphabetical dozen would be actively misleading.
    if !q_tokens.is_empty() {
        let mut scored: Vec<(i32, &&ToolInfo)> = eligible
            .iter()
            .map(|t| (score(t, &q_tokens), t))
            .filter(|(s, _)| *s > 0)
            .collect();
        // Deterministic: score desc, then name asc so the same query yields the same
        // offer set every time (a model that sees a different tool order each turn
        // behaves inconsistently for no reason).
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
        for (_, t) in scored.into_iter().take(MAX_SELECTED) {
            if seen.insert(t.name.clone()) {
                selected.push((*t).clone());
            }
        }
    }

    // Essentials last, and only if the caller is actually allowed them — an essential
    // is a convenience, never an authorization bypass.
    for name in ESSENTIALS {
        if seen.contains(*name) {
            continue;
        }
        if let Some(t) = eligible.iter().find(|t| t.name == *name) {
            seen.insert((*name).to_string());
            selected.push((*t).clone());
        }
    }

    selected
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(name: &str, desc: &str) -> ToolInfo {
        ToolInfo {
            name: name.to_string(),
            description: desc.to_string(),
            parameters: json!({"type": "object", "properties": {}}),
        }
    }

    fn catalog() -> Vec<ToolInfo> {
        vec![
            tool("weather", "Current conditions and forecast for a location"),
            tool("news_headlines", "Top news headlines by category or country"),
            tool("news_search", "Search news articles by keyword"),
            tool("pve__get_nodes", "List Proxmox cluster nodes and their status"),
            tool("utc_now", "The current UTC time"),
            tool("health", "Service health"),
            tool("searxng_search", "Web search"),
            tool("ledger_recent", "Recent financial transactions"),
        ]
    }

    #[test]
    fn a_weather_question_selects_the_weather_tool() {
        let sel = select_tools(&catalog(), "what is the weather right now?", None, None);
        assert_eq!(sel[0].name, "weather", "the exact-name match must rank first");
    }

    #[test]
    fn a_news_question_selects_the_news_family() {
        let sel = select_tools(&catalog(), "what's the latest news?", None, None);
        let names: Vec<&str> = sel.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"news_headlines"), "got {names:?}");
    }

    #[test]
    fn a_proxmox_question_selects_the_pve_tool() {
        // The operator's actual failing question.
        let sel = select_tools(&catalog(), "are all my proxmox nodes running?", None, None);
        let names: Vec<&str> = sel.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"pve__get_nodes"), "got {names:?}");
    }

    #[test]
    fn essentials_are_always_offered() {
        let sel = select_tools(&catalog(), "tell me a joke", None, None);
        let names: Vec<&str> = sel.iter().map(|t| t.name.as_str()).collect();
        for e in ESSENTIALS {
            assert!(names.contains(e), "essential {e} missing from {names:?}");
        }
    }

    #[test]
    fn selection_is_bounded_and_deterministic() {
        let c = catalog();
        let a = select_tools(&c, "news weather nodes search time", None, None);
        let b = select_tools(&c, "news weather nodes search time", None, None);
        assert_eq!(
            a.iter().map(|t| &t.name).collect::<Vec<_>>(),
            b.iter().map(|t| &t.name).collect::<Vec<_>>(),
            "the same query must yield the same offer set every turn"
        );
        assert!(a.len() <= MAX_SELECTED + ESSENTIALS.len());
    }

    #[test]
    fn an_empty_query_offers_only_essentials() {
        // Every tool scores 0, so an arbitrary alphabetical dozen would be misleading.
        let sel = select_tools(&catalog(), "", None, None);
        let names: Vec<&str> = sel.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names.len(), ESSENTIALS.len(), "got {names:?}");
    }

    #[test]
    fn stopwords_do_not_drive_selection() {
        // "what is the" must not match every tool whose description contains "the".
        let sel = select_tools(&catalog(), "what is the", None, None);
        let names: Vec<&str> = sel.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names.len(), ESSENTIALS.len(), "stopwords should score nothing: {names:?}");
    }

    #[test]
    fn tokenizer_keeps_underscored_tool_names_intact() {
        let toks = tokenize("check news_headlines please");
        assert!(toks.contains(&"news_headlines".to_string()), "got {toks:?}");
    }
}
