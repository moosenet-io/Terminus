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

/// Reason reported for every tool when the configured map failed to parse. Stated
/// plainly so an operator reading the admin view sees WHY everything is parked
/// rather than concluding the fleet died.
const MALFORMED_REASON: &str =
    "the tool-availability map failed to parse — every tool is parked until it is fixed";

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
    /// When the operator last CONFIRMED this state (free-form, e.g. "2026-07-30").
    /// Availability is deliberately operator-confirmed rather than auto-inferred from
    /// a failed probe — a flaky upstream would otherwise flap tools off — so the human
    /// reading the admin view needs to know how stale that confirmation is.
    #[serde(default)]
    last_verified: Option<String>,
}

#[derive(Debug, Clone)]
struct Entry {
    state: Availability,
    reason: Option<String>,
    last_verified: Option<String>,
}

/// Resolved availability policy. Keys are matched as EXACT names first, then as
/// PREFIXES (longest match wins), mirroring how
/// `gateway_framework::DEFAULT_SENSITIVE_DENY_PREFIXES` lets an operator express a
/// whole family (`"crucible_"`) without enumerating its ten tool names.
#[derive(Debug, Clone, Default)]
pub struct AvailabilityPolicy {
    entries: BTreeMap<String, Entry>,
    /// True when the configured map FAILED TO PARSE. Such a policy denies EVERY tool
    /// — the literal fail-closed reading of "malformed config fails closed to off".
    ///
    /// Review (S128 r2) was right to reject relying on startup validation alone:
    /// `validate_env()` is only called by a binary that opts in, so any other binary
    /// linking this crate and serving `handle_mcp` (e.g. `terminus_personal`) would
    /// have silently un-parked every tool. The flag makes the guarantee a property of
    /// the POLICY itself, not of one caller remembering to validate.
    malformed: bool,
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

    /// Parse a policy from JSON, or `Err` describing why it could not be parsed.
    ///
    /// **Whole-document failure is NOT silently tolerated.** An earlier revision
    /// returned an empty policy here, which failed OPEN — one operator typo and every
    /// parked tool silently came back. Review (S128) correctly rejected that.
    ///
    /// (Correcting an inverted rationale in the first revision, caught in review: an
    /// EMPTY policy makes every tool *Available* — it fails OPEN, not closed. The
    /// earlier comment claimed the opposite and reasoned from it.)
    ///
    /// Two things now provide the guarantee, belt and braces:
    /// 1. [`validate_env`] — a binary calls it at STARTUP and refuses to boot, so the
    ///    operator who just edited the map sees the error at deploy. Preferred.
    /// 2. [`AvailabilityPolicy::malformed`] — if a binary somehow serves without
    ///    validating, the resulting policy denies EVERY tool. Loud and safe rather
    ///    than silently re-parking nothing.
    pub fn try_from_json(raw: &str) -> Result<Self, String> {
        let parsed: BTreeMap<String, RawEntry> = serde_json::from_str(raw)
            .map_err(|e| format!("{AVAILABILITY_ENV} is not valid JSON: {e}"))?;

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
            entries.insert(
                key,
                Entry { state, reason: raw_entry.reason, last_verified: raw_entry.last_verified },
            );
        }
        Ok(Self { entries, malformed: false })
    }

    /// Lenient parse used by [`policy`]. A malformed document here yields a policy
    /// flagged [`AvailabilityPolicy::malformed`], which denies EVERY tool — so a
    /// binary that never calls [`validate_env`] still fails closed rather than
    /// silently un-parking everything.
    pub fn from_json(raw: &str) -> Self {
        Self::try_from_json(raw).unwrap_or_else(|e| {
            tracing::error!(
                "availability: {e} — FAILING CLOSED: every tool is unavailable until the \
                 map is fixed. (A binary that calls validate_env() at startup refuses to \
                 boot instead, which is the preferred way to hit this.)"
            );
            Self { entries: BTreeMap::new(), malformed: true }
        })
    }

    /// The resolved state for `tool_name`, plus the operator's reason if any.
    ///
    /// Exact name wins over a prefix; among prefixes the LONGEST wins, so a
    /// family-wide `"crucible_" => off` can still be overridden by a specific
    /// `"crucible_status" => available`.
    pub fn state_of(&self, tool_name: &str) -> (Availability, Option<&str>) {
        // A MALFORMED policy resolves to Off for EVERY tool, consistently.
        // Round-3 review caught a real inconsistency here: `agent_usable` denied the
        // call while `state_of` still reported `Available`, so `denial_message` could
        // emit the nonsense "`weather` is currently available and cannot be called."
        // Deny and report must agree.
        if self.malformed {
            return (Availability::Off, Some(MALFORMED_REASON));
        }
        self.entry_for(tool_name)
            .map(|e| (e.state, e.reason.as_deref()))
            .unwrap_or((Availability::Available, None))
    }

    /// Full record for `tool_name`, including `last_verified`.
    pub fn record_of(&self, tool_name: &str) -> (Availability, Option<&str>, Option<&str>) {
        if self.malformed {
            return (Availability::Off, Some(MALFORMED_REASON), None);
        }
        match self.entry_for(tool_name) {
            Some(e) => (e.state, e.reason.as_deref(), e.last_verified.as_deref()),
            None => (Availability::Available, None, None),
        }
    }

    /// Resolve the governing entry, if any.
    ///
    /// A MESH-federated tool is advertised namespaced (`<namespace>__<tool>`), so a
    /// family rule authored against the BARE name (`"crucible_"`) must still govern
    /// `"ct322__crucible_status"` — otherwise a parked tool re-appears the moment it
    /// is reached through an upstream. This mirrors
    /// `gateway_framework::deny_matches`, which closes the same hole for deny
    /// prefixes. Matching order: exact raw name, exact bare name, then longest
    /// prefix over either form.
    fn entry_for(&self, tool_name: &str) -> Option<&Entry> {
        if let Some(e) = self.entries.get(tool_name) {
            return Some(e);
        }
        let bare = crate::mesh::merge::split_namespaced(tool_name).map(|(_, b)| b);
        if let Some(b) = bare {
            if let Some(e) = self.entries.get(b) {
                return Some(e);
            }
        }
        let mut best: Option<(&String, &Entry)> = None;
        for (key, entry) in &self.entries {
            let hit = tool_name.starts_with(key.as_str())
                || bare.map_or(false, |b| b.starts_with(key.as_str()));
            if hit && best.map_or(true, |(bk, _)| key.len() > bk.len()) {
                best = Some((key, entry));
            }
        }
        best.map(|(_, e)| e)
    }

    /// Whether an AGENT may see/call `tool_name`.
    ///
    /// A MALFORMED policy denies everything — fail closed, per the acceptance
    /// criterion. This is deliberately NOT "ignore the map and allow everything":
    /// that direction re-exposes exactly the dead tools the feature exists to park.
    pub fn agent_usable(&self, tool_name: &str) -> bool {
        if self.malformed {
            return false;
        }
        self.state_of(tool_name).0.agent_usable()
    }

    /// Whether the configured map failed to parse.
    pub fn is_malformed(&self) -> bool {
        self.malformed
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

    /// Whether this policy has no effect. A MALFORMED policy is never "empty" — it
    /// denies everything, so the caller must NOT short-circuit past it.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty() && !self.malformed
    }

    /// Number of loaded rules — the admin view reports this so an operator can tell
    /// "my 12 rules loaded" from "my map silently did not parse".
    pub fn rule_count(&self) -> usize {
        self.entries.len()
    }
}

/// Validate the availability map at STARTUP, before serving anything.
///
/// This is where "malformed config fails closed" actually lives: the server refuses
/// to start rather than run under a policy that did not parse. Catching it here — at
/// deploy, in front of the operator who just edited the map — is strictly better than
/// either silently ignoring it (fails open: parked tools come back) or emptying it
/// (fails closed so hard the assistant loses every tool).
///
/// An UNSET or blank variable is valid and means "no rules" — that is the untouched
/// default for every deployment that never opts in.
pub fn validate_env() -> Result<usize, String> {
    match std::env::var(AVAILABILITY_ENV) {
        Ok(raw) if !raw.trim().is_empty() => {
            AvailabilityPolicy::try_from_json(&raw).map(|p| p.rule_count())
        }
        _ => Ok(0),
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
    fn malformed_json_is_a_hard_error_not_a_silent_empty_policy() {
        // Review (S128) correctly rejected the original behaviour here: returning an
        // empty policy on a parse failure fails OPEN — one typo and every parked tool
        // silently comes back. `try_from_json` now reports the error so STARTUP can
        // refuse to boot on it (see `validate_env`).
        let err = AvailabilityPolicy::try_from_json("{not json").unwrap_err();
        assert!(err.contains(AVAILABILITY_ENV), "error must name the variable: {err}");
    }

    #[test]
    fn validate_env_rejects_a_malformed_map() {
        // The startup gate itself: a malformed document is an Err, which
        // terminus_primary turns into a refusal to boot.
        assert!(AvailabilityPolicy::try_from_json("{\"a\": }").is_err());
        // ...while a well-formed one reports its rule count.
        let n = AvailabilityPolicy::try_from_json(r#"{"a_":{"state":"off"},"b_":{"state":"broken"}}"#)
            .unwrap()
            .rule_count();
        assert_eq!(n, 2);
    }

    #[test]
    fn namespaced_federated_names_are_governed_by_a_bare_family_rule() {
        // A mesh upstream advertises `<namespace>__<tool>`. A family rule authored
        // against the BARE name must still park it, or a tool switched off locally
        // silently reappears the moment it is reached through an upstream.
        let p = policy(r#"{"crucible_": {"state":"off","reason":"retired"}}"#);
        assert!(!p.agent_usable("crucible_status"), "bare name must be parked");
        assert!(
            !p.agent_usable("ct322__crucible_status"),
            "the namespaced form must be parked by the same bare family rule"
        );
        // An unrelated namespaced tool is untouched.
        assert!(p.agent_usable("ct322__weather"));
    }

    #[test]
    fn last_verified_round_trips_into_the_record() {
        let p = policy(
            r#"{"soma_": {"state":"off","reason":"retired","last_verified":"2026-07-30"}}"#,
        );
        let (state, reason, last) = p.record_of("soma_status");
        assert_eq!(state, Availability::Off);
        assert_eq!(reason, Some("retired"));
        assert_eq!(last, Some("2026-07-30"));
    }

    #[test]
    fn a_malformed_policy_denies_every_tool_fail_closed() {
        // The literal acceptance criterion. Review r2 correctly rejected relying on
        // startup validation alone: another binary (terminus_personal) serves
        // handle_mcp without calling validate_env, and would have silently un-parked
        // everything.
        let p = AvailabilityPolicy::from_json("{not json");
        assert!(p.is_malformed());
        assert!(!p.agent_usable("weather"), "a malformed map must deny everything");
        assert!(!p.agent_usable("crucible_status"));
        assert!(!p.agent_usable("literally_anything"));
    }

    #[test]
    fn a_malformed_policy_reports_off_consistently_not_available() {
        // Round-3 review: deny and REPORT must agree. Previously agent_usable() said
        // "no" while state_of() said "available", so the denial message read
        // "`weather` is currently available and cannot be called."
        let p = AvailabilityPolicy::from_json("{not json");
        let (state, reason) = p.state_of("weather");
        assert_eq!(state, Availability::Off, "a malformed policy must REPORT off, not available");
        assert!(reason.unwrap().contains("failed to parse"));
        let msg = p.denial_message("weather");
        assert!(msg.contains("off"), "denial must say off: {msg}");
        assert!(!msg.contains("currently available"), "must never say 'available ... cannot be called': {msg}");
        let (rstate, rreason, _) = p.record_of("weather");
        assert_eq!(rstate, Availability::Off);
        assert!(rreason.is_some());
    }

    #[test]
    fn a_malformed_policy_is_not_reported_as_empty() {
        // `is_empty()` short-circuits the tools/list filter. If a malformed policy
        // reported itself empty, the filter would be skipped and the fail-closed
        // guarantee would evaporate at exactly the moment it matters.
        let p = AvailabilityPolicy::from_json("{not json");
        assert!(!p.is_empty(), "a malformed policy must not be short-circuited as a no-op");
    }

    #[test]
    fn rule_count_reports_the_real_number_not_a_boolean() {
        let p = policy(r#"{"a_":{"state":"off"},"b_":{"state":"off"},"c_":{"state":"broken"}}"#);
        assert_eq!(p.rule_count(), 3, "admin view must show how many rules actually loaded");
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
