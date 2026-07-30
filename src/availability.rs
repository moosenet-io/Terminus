//! TAVAIL-01 — tool AVAILABILITY state: registry-visible, agent-unavailable.
//!
//! Operator requirement (S128, 2026-07-30): *"I don't want to remove tools, it's
//! helpful to see them in the registry, but they should be in an off position not
//! available to the agents."*
//!
//! Before this module a tool was binary — registered and offered to every agent, or
//! absent entirely. There was no way to keep a tool VISIBLE for humans while
//! withholding it from agent selection, so Lumina was offered tools whose backends
//! were provably dead (e.g. `crucible_*`/`odyssey_*`, whose backing host was
//! decommissioned), tried them, and reported confusing failures to the operator.
//!
//! ## The two gates are DIFFERENT concerns and must COMPOSE
//! - **Authorization** (`crate::gateway_framework`) — *may this principal use it?*
//!   Per-identity, already enforced on `tools/list` (MESH-08) and `tools/call`.
//! - **Availability** (this module) — *does this tool work at all, for anyone?*
//!   Principal-independent.
//!
//! A tool is offered to an agent only if BOTH allow it. Availability NEVER widens
//! access: it can only remove a tool an identity would otherwise have been granted.
//!
//! ## Fail-closed
//! A malformed availability entry resolves to [`Availability::Off`], never
//! `Available` — an operator typo must not silently re-expose a dead tool. An
//! ABSENT config, by contrast, means every tool is `Available`: that is the
//! pre-TAVAIL-01 behaviour, preserved byte-for-byte for every deployment that
//! never sets the variable.

use std::collections::BTreeMap;

use serde::Deserialize;

/// Env var holding the availability map. Shape:
/// `{"crucible_": {"state": "off", "reason": "retired 2026-07-30"}, ...}`
pub const AVAILABILITY_ENV: &str = "TERMINUS_TOOL_AVAILABILITY_JSON";

/// Whether a tool may be offered to, and invoked by, an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Availability {
    /// Normal: advertised and callable (subject to authorization).
    Available,
    /// Deliberately switched off by the operator — still listed in the ADMIN view
    /// with its reason, but never advertised to or callable by an agent.
    Off,
    /// Known-broken backend. Same agent-facing effect as `Off`; kept distinct so the
    /// admin view can tell "we turned this off" from "this is failing".
    Broken,
}

impl Availability {
    /// Whether an AGENT may see and call this tool.
    pub fn agent_usable(self) -> bool {
        matches!(self, Availability::Available)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Availability::Available => "available",
            Availability::Off => "off",
            Availability::Broken => "broken",
        }
    }
}

/// One operator-authored entry. `state` is parsed leniently at the string level so a
/// typo becomes `Off` (fail-closed) rather than a hard config error that would take
/// the whole map — and therefore every tool's state — down with it.
#[derive(Debug, Clone, Deserialize)]
struct RawEntry {
    state: String,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Clone)]
struct Entry {
    state: Availability,
    reason: Option<String>,
}

/// Resolved availability policy. Keys are matched as EXACT names first, then as
/// PREFIXES (longest match wins), mirroring how
/// `gateway_framework::DEFAULT_SENSITIVE_DENY_PREFIXES` lets an operator express a
/// whole family (`"crucible_"`) without enumerating its ten tool names.
#[derive(Debug, Clone, Default)]
pub struct AvailabilityPolicy {
    entries: BTreeMap<String, Entry>,
}

impl AvailabilityPolicy {
    /// Build from the process environment. An unset/blank var yields an EMPTY policy
    /// (everything `Available`) — the exact pre-TAVAIL-01 behaviour.
    pub fn from_env() -> Self {
        match std::env::var(AVAILABILITY_ENV) {
            Ok(raw) if !raw.trim().is_empty() => Self::from_json(&raw),
            _ => Self::default(),
        }
    }

    /// Parse a policy from JSON. A wholly unparseable document yields an empty
    /// policy plus a loud error: refusing to serve ANY tool because one operator
    /// typo broke the JSON would be a far worse outage than ignoring the map, and
    /// the per-entry fail-closed rule below still protects the entries that DID
    /// parse.
    pub fn from_json(raw: &str) -> Self {
        let parsed: BTreeMap<String, RawEntry> = match serde_json::from_str(raw) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(
                    "availability: {AVAILABILITY_ENV} is not valid JSON ({e}) — \
                     ignoring it; every tool stays available. Fix the map."
                );
                return Self::default();
            }
        };

        let mut entries = BTreeMap::new();
        for (key, raw_entry) in parsed {
            let state = match raw_entry.state.trim().to_ascii_lowercase().as_str() {
                "available" => Availability::Available,
                "off" => Availability::Off,
                "broken" => Availability::Broken,
                other => {
                    // FAIL CLOSED: an unrecognised state must never resolve to
                    // Available, or a typo silently re-exposes a dead tool.
                    tracing::error!(
                        "availability: unrecognised state {other:?} for {key:?} — \
                         treating as 'off' (fail-closed)"
                    );
                    Availability::Off
                }
            };
            entries.insert(key, Entry { state, reason: raw_entry.reason });
        }
        Self { entries }
    }

    /// The resolved state for `tool_name`, plus the operator's reason if any.
    ///
    /// Exact name wins over a prefix; among prefixes the LONGEST wins, so a
    /// family-wide `"crucible_" => off` can still be overridden by a specific
    /// `"crucible_status" => available`.
    pub fn state_of(&self, tool_name: &str) -> (Availability, Option<&str>) {
        if let Some(e) = self.entries.get(tool_name) {
            return (e.state, e.reason.as_deref());
        }
        let mut best: Option<(&String, &Entry)> = None;
        for (key, entry) in &self.entries {
            if tool_name.starts_with(key.as_str())
                && best.map_or(true, |(bk, _)| key.len() > bk.len())
            {
                best = Some((key, entry));
            }
        }
        match best {
            Some((_, e)) => (e.state, e.reason.as_deref()),
            None => (Availability::Available, None),
        }
    }

    /// Whether an AGENT may see/call `tool_name`.
    pub fn agent_usable(&self, tool_name: &str) -> bool {
        self.state_of(tool_name).0.agent_usable()
    }

    /// The denial message handed back on a `tools/call` for an unavailable tool.
    /// Names the state and the operator's reason — never a bare "not found", which
    /// would send the model hunting for a tool that is deliberately parked.
    pub fn denial_message(&self, tool_name: &str) -> String {
        let (state, reason) = self.state_of(tool_name);
        match reason {
            Some(r) => format!(
                "`{tool_name}` is currently {} and cannot be called: {r}",
                state.as_str()
            ),
            None => format!("`{tool_name}` is currently {} and cannot be called.", state.as_str()),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Process-wide policy, resolved once from the environment on first use.
///
/// A `OnceLock` rather than a field on `McpServerState` because the policy is
/// read-only env config identical for the whole process — threading it through the
/// state struct would churn every test constructor for no behavioural gain. The
/// trade-off is deliberate: changing availability requires a service restart, which
/// matches how every other `Environment=`-driven knob on this service behaves.
pub fn policy() -> &'static AvailabilityPolicy {
    static POLICY: std::sync::OnceLock<AvailabilityPolicy> = std::sync::OnceLock::new();
    POLICY.get_or_init(|| {
        let p = AvailabilityPolicy::from_env();
        if p.is_empty() {
            tracing::debug!("availability: no {AVAILABILITY_ENV} configured — all tools available");
        } else {
            tracing::info!(
                "availability: {} rule(s) loaded from {AVAILABILITY_ENV}",
                p.entries.len()
            );
        }
        p
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(json: &str) -> AvailabilityPolicy {
        AvailabilityPolicy::from_json(json)
    }

    #[test]
    fn absent_config_leaves_everything_available() {
        let p = AvailabilityPolicy::default();
        assert!(p.is_empty());
        assert!(p.agent_usable("crucible_status"));
        assert!(p.agent_usable("anything_at_all"));
    }

    #[test]
    fn prefix_entry_switches_a_whole_family_off() {
        let p = policy(r#"{"crucible_": {"state":"off","reason":"retired 2026-07-30"}}"#);
        assert!(!p.agent_usable("crucible_status"));
        assert!(!p.agent_usable("crucible_reading_list"));
        // Unrelated families are untouched.
        assert!(p.agent_usable("weather"));
        assert!(p.agent_usable("news_headlines"));
    }

    #[test]
    fn exact_name_wins_over_prefix() {
        let p = policy(
            r#"{"crucible_": {"state":"off"}, "crucible_status": {"state":"available"}}"#,
        );
        assert!(p.agent_usable("crucible_status"), "exact entry must override the family prefix");
        assert!(!p.agent_usable("crucible_reading_list"));
    }

    #[test]
    fn longest_prefix_wins() {
        let p = policy(r#"{"a_": {"state":"off"}, "a_b_": {"state":"available"}}"#);
        assert!(!p.agent_usable("a_x"));
        assert!(p.agent_usable("a_b_c"), "the more specific prefix must win");
    }

    #[test]
    fn unrecognised_state_fails_closed_to_off() {
        // The load-bearing safety property: a typo must NEVER read as available.
        let p = policy(r#"{"soma_": {"state":"disabled"}}"#);
        assert!(!p.agent_usable("soma_status"));
        assert_eq!(p.state_of("soma_status").0, Availability::Off);
    }

    #[test]
    fn empty_state_string_fails_closed_to_off() {
        let p = policy(r#"{"soma_": {"state":""}}"#);
        assert!(!p.agent_usable("soma_status"));
    }

    #[test]
    fn malformed_json_is_ignored_not_a_global_outage() {
        // One typo must not take every tool down; per-entry fail-closed still applies
        // to entries that DO parse.
        let p = policy("{not json");
        assert!(p.is_empty());
        assert!(p.agent_usable("crucible_status"));
    }

    #[test]
    fn broken_is_distinct_from_off_but_equally_unusable() {
        let p = policy(
            r#"{"hearth_shopping_list": {"state":"broken","reason":"backend fault"},
                "soma_": {"state":"off","reason":"retired"}}"#,
        );
        assert!(!p.agent_usable("hearth_shopping_list"));
        assert!(!p.agent_usable("soma_status"));
        assert_eq!(p.state_of("hearth_shopping_list").0, Availability::Broken);
        assert_eq!(p.state_of("soma_status").0, Availability::Off);
    }

    #[test]
    fn denial_message_names_state_and_reason_never_not_found() {
        let p = policy(r#"{"crucible_": {"state":"off","reason":"retired 2026-07-30"}}"#);
        let msg = p.denial_message("crucible_status");
        assert!(msg.contains("off"));
        assert!(msg.contains("retired 2026-07-30"));
        // Must not look like a missing tool — that sends the model hunting.
        assert!(!msg.to_lowercase().contains("not found"));
    }

    #[test]
    fn denial_message_without_reason_still_names_the_state() {
        let p = policy(r#"{"soma_": {"state":"off"}}"#);
        let msg = p.denial_message("soma_status");
        assert!(msg.contains("off"));
        assert!(!msg.to_lowercase().contains("not found"));
    }

    #[test]
    fn explicit_available_is_honoured() {
        let p = policy(r#"{"weather": {"state":"available"}}"#);
        assert!(p.agent_usable("weather"));
    }

    #[test]
    fn availability_never_widens_access_only_narrows() {
        // Availability is principal-INDEPENDENT and can only remove. This test
        // documents the composition contract the caller must honour: authorization
        // is checked separately, and both must allow.
        let p = policy(r#"{"infisical_": {"state":"available"}}"#);
        // Marking a sensitive family "available" here does NOT grant it — the
        // gateway allowlist is a separate gate that still applies.
        assert!(p.agent_usable("infisical_get_secret"));
    }
}
