//! REVCAP-01 PART A — reviewer-provider capacity core.
//!
//! Review providers (opus/codex/agy/free/nemotron/qwen_coder) periodically hit
//! rate limits or subscription/quota cliffs on their upstream. Before this
//! module, `review_run` just degraded that one provider's entry to
//! `"unavailable: ..."` per-call with no memory -- so a genuinely-shelved
//! provider (e.g. agy mid multi-day quota exhaustion) got re-dispatched (and
//! re-failed) on every single review, and a caller had no way to see "N
//! frontier providers are down right now" before spending a dispatch.
//!
//! This module is a process-global, self-populating STATE MACHINE
//! ([`ReviewerRegistry`]) that:
//!   - classifies a dispatch error into a TWO-TIER cap ([`CapStatus`]): a
//!     short rolling per-minute limit is a [`CapStatus::Cooldown`] (re-probe
//!     after the horizon elapses); a long subscription/quota cliff (the
//!     agy-style "Resets in 76h4m" cap) is a [`CapStatus::Shelved`] (route
//!     around it until the absolute recovery time -- never early-probe a
//!     multi-day shelf);
//!   - stores an ABSOLUTE recovery instant computed ONCE per cap (never a
//!     re-decremented countdown, which is what produces the "the timer looks
//!     like it's drifting" confusion this design explicitly avoids);
//!   - backs off exponentially on repeat caps (mirrors Harmony's
//!     `providers/status.rs` pattern);
//!   - is consulted by a capacity GATE in `review::mod` before dispatch: if
//!     two or more of the FRONTIER providers (codex/agy/opus) are down, a
//!     panel must not silently return a degraded verdict.
//!
//! Detection (`is_rate_limit_error` / `parse_horizon_secs`) generalizes the
//! existing OpenRouter-429-only `dispatch::is_openrouter_rate_limited` to the
//! full set of cap phrasings the daemon-backed CLI providers (opus/codex/agy)
//! and OpenRouter both actually produce.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use serde::Serialize;

/// A short rolling limit resets within this many seconds -> [`CapStatus::Cooldown`].
/// Above [`SHELVE_THRESHOLD_SECS`] -> [`CapStatus::Shelved`]. Strictly between the
/// two (301..=1800s) still counts as a cooldown -- the two-tier split only cares
/// about "re-probe soon" vs "route around for a long while", so the boundary is
/// drawn once, at the shelve threshold, rather than adding a third tier.
const COOLDOWN_THRESHOLD_SECS: u64 = 300;
/// Above this, a cap is a subscription/quota cliff -> [`CapStatus::Shelved`].
const SHELVE_THRESHOLD_SECS: u64 = 1800;
/// Default cooldown applied when a rate-limit is detected but no horizon could be
/// parsed out of the error text -- deliberately short (not a multi-day shelve) so
/// an unparseable cap message never strands a provider for days.
const DEFAULT_UNPARSED_COOLDOWN_SECS: u64 = 600;
/// Base backoff applied on the SECOND consecutive cap (the first cap uses the
/// horizon as-is); doubles per additional consecutive cap up to the ceiling.
const BACKOFF_BASE_SECS: u64 = 60;
/// Backoff ceiling: ~120 minutes, mirroring Harmony's `providers/status.rs`.
const BACKOFF_CEILING_SECS: u64 = 120 * 60;

/// The three CLI-backed, daemon-routed providers that a capacity-starved panel
/// (>=2 down) must not silently degrade through. Kept here (not in `dispatch.rs`)
/// since this is a capacity-gate concept, not a transport-routing one.
pub const FRONTIER: &[&str] = &["codex", "agy", "opus"];

/// Two-tier + non-cap classification for a reviewer provider's current state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CapStatus {
    /// No known cap; free to dispatch.
    Available,
    /// A short rolling per-minute limit; re-probe once `cooldown_until` passes.
    Cooldown,
    /// A long subscription/quota cliff; route around until the absolute time --
    /// do NOT early-probe.
    Shelved,
    /// The provider was slow (a wall-clock dispatch timeout), NOT capped. Still
    /// available; recorded only as a diagnostic signal, never blocks dispatch.
    Latency,
    /// A non-rate-limit dispatch error (auth, malformed response, etc). Still
    /// available for the next attempt; distinct from a genuine cap.
    Error,
}

impl Default for CapStatus {
    fn default() -> Self {
        CapStatus::Available
    }
}

/// One reviewer provider's tracked capacity state.
#[derive(Debug, Clone, Serialize)]
pub struct ReviewerStatus {
    pub name: String,
    pub available: bool,
    #[serde(with = "option_systemtime_unix")]
    pub cooldown_until: Option<SystemTime>,
    pub consecutive_caps: u32,
    pub backoff_secs: u64,
    /// The raw reset horizon (seconds) that produced the current cap, BEFORE
    /// backoff was added -- i.e. what the provider's own message reported
    /// (or [`DEFAULT_UNPARSED_COOLDOWN_SECS`] if unparseable). `None` when
    /// there is no current cap. Distinct from `cooldown_until - now`, which
    /// also includes the backoff widening.
    pub last_horizon_secs: Option<u64>,
    pub last_status: CapStatus,
    /// Human-readable provenance of the last state change (e.g. the dispatch
    /// error text that produced it, or `"dispatch success"`).
    pub source: String,
}

impl ReviewerStatus {
    fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            available: true,
            cooldown_until: None,
            consecutive_caps: 0,
            backoff_secs: 0,
            last_horizon_secs: None,
            last_status: CapStatus::Available,
            source: "no dispatch yet".to_string(),
        }
    }

    /// Classify + record a rate-limit cap. `horizon_secs` is the parsed reset
    /// horizon from the error text, if any (`None` -> [`DEFAULT_UNPARSED_COOLDOWN_SECS`]).
    /// Computes `cooldown_until` as an ABSOLUTE instant ONCE (`now + effective_secs`),
    /// applies exponential backoff on repeat consecutive caps, and increments
    /// `consecutive_caps`. Does not itself inspect the error text -- callers pass
    /// the already-parsed horizon (from [`parse_horizon_secs`]) plus the raw text
    /// as `source` for provenance.
    pub fn mark_rate_limited(
        &mut self,
        horizon_secs: Option<u64>,
        now: SystemTime,
        source: impl Into<String>,
    ) {
        let base = horizon_secs.unwrap_or(DEFAULT_UNPARSED_COOLDOWN_SECS);
        self.consecutive_caps = self.consecutive_caps.saturating_add(1);

        // Exponential backoff on repeat caps: the FIRST cap uses the horizon
        // as-is (respect what the provider actually told us); the second and
        // later consecutive caps widen the effective wait so a provider that
        // keeps getting capped right after recovery backs off harder each time,
        // capped at BACKOFF_CEILING_SECS.
        let backoff_secs = if self.consecutive_caps <= 1 {
            0
        } else {
            let exp = self.consecutive_caps.saturating_sub(2).min(20); // guard against overflow in the shift
            let doubled = BACKOFF_BASE_SECS.saturating_mul(1u64 << exp);
            doubled.min(BACKOFF_CEILING_SECS)
        };
        self.backoff_secs = backoff_secs;

        let effective_secs = base.saturating_add(backoff_secs);
        let status = classify_horizon(base);
        self.last_status = status;
        self.available = false;
        self.cooldown_until = Some(now + Duration::from_secs(effective_secs));
        self.last_horizon_secs = Some(base);
        self.source = source.into();
    }

    /// Whether `now` is at/after `cooldown_until` (i.e. the cap has expired).
    /// `true` when there is no cap at all (`cooldown_until` is `None`).
    pub fn check_recovery(&self, now: SystemTime) -> bool {
        match self.cooldown_until {
            None => true,
            Some(until) => now >= until,
        }
    }

    /// Flip back to available once [`check_recovery`] is true. Does NOT reset
    /// `consecutive_caps` (that only resets on an actual dispatch success, via
    /// [`mark_success`]) -- a provider that recovers and gets capped again
    /// immediately should still see the backoff escalate.
    pub fn mark_recovered(&mut self) {
        self.available = true;
        self.cooldown_until = None;
        self.last_horizon_secs = None;
        self.last_status = CapStatus::Available;
    }

    /// Record a genuine dispatch success: clears any cap state and resets the
    /// consecutive-cap counter/backoff (the provider has demonstrably recovered).
    pub fn mark_success(&mut self) {
        self.available = true;
        self.cooldown_until = None;
        self.consecutive_caps = 0;
        self.backoff_secs = 0;
        self.last_horizon_secs = None;
        self.last_status = CapStatus::Available;
        self.source = "dispatch success".to_string();
    }

    /// Record a wall-clock timeout: NOT a cap (still available), but tracked as
    /// a distinct diagnostic status.
    pub fn mark_latency(&mut self, source: impl Into<String>) {
        self.last_status = CapStatus::Latency;
        self.source = source.into();
        // Latency never blocks dispatch and never touches cooldown_until/
        // consecutive_caps -- `available` stays whatever it already was.
    }

    /// Record a non-cap dispatch error (auth failure, malformed response, ...).
    /// Still available for the next attempt; tracked only for diagnostics.
    pub fn mark_error(&mut self, source: impl Into<String>) {
        self.last_status = CapStatus::Error;
        self.source = source.into();
    }

    /// Whether this provider currently counts as "down" for the capacity gate:
    /// Shelved (always, until `check_recovery`), or Cooldown that hasn't yet
    /// elapsed. Latency/Error/Available never count as down.
    pub fn is_down(&self, now: SystemTime) -> bool {
        matches!(self.last_status, CapStatus::Shelved | CapStatus::Cooldown)
            && !self.check_recovery(now)
    }
}

/// Two-tier split: `horizon_secs <= COOLDOWN_THRESHOLD_SECS` -> Cooldown;
/// `> SHELVE_THRESHOLD_SECS` -> Shelved; the gap between the two thresholds
/// (301..=1800s) also resolves to Cooldown (see the module-level doc on why
/// there are only two tiers).
fn classify_horizon(horizon_secs: u64) -> CapStatus {
    if horizon_secs > SHELVE_THRESHOLD_SECS {
        CapStatus::Shelved
    } else {
        CapStatus::Cooldown
    }
}

/// `serde` support for `Option<SystemTime>` as an RFC3339 string (for a human
/// reader / MCP client convenience) is intentionally NOT what this does --
/// simpler and dependency-free to serialize as Unix seconds, which is what
/// `review_provider_status`'s consumers actually need to compute "how long
/// until recovery". Named module (not `#[serde(with = "...")]` inline) so
/// both the serialize path here and any future deserialize need share one
/// definition.
mod option_systemtime_unix {
    use serde::Serializer;
    use std::time::SystemTime;

    pub fn serialize<S: Serializer>(v: &Option<SystemTime>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            None => s.serialize_none(),
            Some(t) => {
                let secs = t
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(0.0);
                s.serialize_some(&secs)
            }
        }
    }
}

/// Thread-safe process-global registry of every provider's [`ReviewerStatus`],
/// keyed by provider name. `std::sync::Mutex` (this crate has no `parking_lot`
/// dependency) around a `HashMap`, mirroring `review::in_flight()`'s
/// `OnceLock<Mutex<..>>` process-wide-cache pattern.
pub struct ReviewerRegistry {
    inner: Mutex<HashMap<String, ReviewerStatus>>,
}

impl ReviewerRegistry {
    fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Current status for `name`, materializing a fresh `Available` entry if
    /// this provider has never been recorded -- so a caller never has to
    /// special-case "unknown provider" vs. "known and available".
    pub fn get(&self, name: &str) -> ReviewerStatus {
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        map.entry(name.to_string())
            .or_insert_with(|| ReviewerStatus::new(name))
            .clone()
    }

    /// Mutate `name`'s entry in place via `f`, creating a fresh `Available`
    /// entry first if this provider has never been recorded.
    pub fn update<F: FnOnce(&mut ReviewerStatus)>(&self, name: &str, f: F) {
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let entry = map
            .entry(name.to_string())
            .or_insert_with(|| ReviewerStatus::new(name));
        f(entry);
    }

    /// Snapshot of every currently-tracked provider (does not include
    /// providers that have never been dispatched at least once).
    pub fn snapshot(&self) -> Vec<ReviewerStatus> {
        let map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut out: Vec<ReviewerStatus> = map.values().cloned().collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Whether `provider` currently counts as "down" for the capacity gate
    /// (see [`ReviewerStatus::is_down`]). A never-recorded provider (no entry
    /// yet) is available -- this is the backward-compat path: an empty/fresh
    /// registry behaves exactly like "nothing is capped".
    pub fn is_down(&self, provider: &str, now: SystemTime) -> bool {
        let map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        map.get(provider).map(|s| s.is_down(now)).unwrap_or(false)
    }
}

/// Process-global registry accessor. `OnceLock` (mirrors `free_pool::global_pool`
/// / `review::in_flight`) so the cap/recovery state actually persists across
/// `review_run` calls within one process.
pub fn registry() -> &'static ReviewerRegistry {
    static REGISTRY: OnceLock<ReviewerRegistry> = OnceLock::new();
    REGISTRY.get_or_init(ReviewerRegistry::new)
}

/// Whether a dispatch error string indicates a rate-limit / quota cap, across
/// every phrasing this codebase's providers actually produce: OpenRouter's
/// HTTP 429 (generalizes `dispatch::is_openrouter_rate_limited`), the
/// daemon-backed CLI providers' own quota/subscription language (agy's
/// "quota reached" / "Resets in", a generic "usage limit" / "limit reached" /
/// "upgrade your subscription"), and the generic "resource exhausted" /
/// "too many requests" phrasings. Matched case-insensitively against the
/// whole message; a genuine wall-clock timeout string ("timeout") does NOT
/// match here -- callers should classify a timeout via `mark_latency`
/// instead, never as a rate limit.
pub fn is_rate_limit_error(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    const NEEDLES: &[&str] = &[
        "quota",
        "rate limit",
        "rate-limit",
        "ratelimit",
        "http 429",
        " 429 ",
        "resource exhausted",
        "usage limit",
        "limit reached",
        "too many requests",
        "upgrade your subscription",
        "resets in",
    ];
    NEEDLES.iter().any(|n| m.contains(n))
}

/// Whether a dispatch error string indicates a wall-clock TIMEOUT rather than a
/// cap -- distinct classification so a slow-but-not-capped provider is recorded
/// as [`CapStatus::Latency`], never mistaken for a rate limit even if a timeout
/// message happens to also mention a duration.
pub fn is_timeout_error(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("timeout") || m.contains("timed out") || m.contains("stalled")
}

/// Parse a reset/retry horizon (seconds) out of a dispatch error message.
/// Recognizes, in order:
///   - `Resets in 76h4m15s` / `76h4m` / `4m15s` / `15s` (agy's exact phrasing,
///     and the more general `\d+h\d+m(\d+s)?` / `\d+m\d+s` shapes)
///   - `retry-after: 30` / `retry after 30` (case/spacing-insensitive,
///     optional colon)
///   - `reset(s) in 30 (h|m|s)` / `reset(s) in 30` (bare seconds if no unit)
/// Returns `None` if no horizon can be extracted (caller then falls back to
/// [`DEFAULT_UNPARSED_COOLDOWN_SECS`] in `mark_rate_limited`).
pub fn parse_horizon_secs(msg: &str) -> Option<u64> {
    let m = msg.to_ascii_lowercase();

    if let Some(secs) = parse_hms(&m) {
        return Some(secs);
    }
    if let Some(secs) = parse_retry_after(&m) {
        return Some(secs);
    }
    if let Some(secs) = parse_resets_in(&m) {
        return Some(secs);
    }
    None
}

/// `(\d+)h(\d+)m(\d+)?s?` or `(\d+)m(\d+)s` -- scans for the FIRST match of
/// either shape anywhere in the (already-lowercased) string. Hand-rolled
/// (no `regex` dependency in this crate) scanning digit-run/unit-letter pairs
/// left to right.
fn parse_hms(m: &str) -> Option<u64> {
    let bytes = m.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            let num1: u64 = m[start..i].parse().ok()?;
            if i < bytes.len() && bytes[i] == b'h' {
                let after_h = i + 1;
                if let Some((mins, secs_opt, next)) = parse_m_then_optional_s(m, after_h) {
                    return Some(num1 * 3600 + mins * 60 + secs_opt.unwrap_or(0));
                }
                // `\d+h` alone (no following m/s) still counts as hours-only.
                return Some(num1 * 3600);
            }
            if i < bytes.len() && bytes[i] == b'm' {
                let after_m = i + 1;
                if let Some(secs) = parse_leading_seconds(m, after_m) {
                    return Some(num1 * 60 + secs);
                }
                return Some(num1 * 60);
            }
            // A bare trailing `\d+s` on its own (no preceding h/m) is handled
            // by the `resets in`/`retry-after` parsers instead, to avoid this
            // generic scanner treating every random number+s as a horizon.
        } else {
            i += 1;
        }
    }
    None
}

/// From position `start` (just past an `h`), try to parse `(\d+)m(\d+)?s?`.
/// Returns `(minutes, seconds_opt, next_pos)` on success.
fn parse_m_then_optional_s(m: &str, start: usize) -> Option<(u64, Option<u64>, usize)> {
    let bytes = m.as_bytes();
    let mut i = start;
    let digit_start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == digit_start {
        return None;
    }
    let mins: u64 = m[digit_start..i].parse().ok()?;
    if i >= bytes.len() || bytes[i] != b'm' {
        return None;
    }
    i += 1;
    let secs = parse_leading_seconds(m, i);
    Some((mins, secs, i))
}

/// From position `start`, try to parse a leading `(\d+)s`.
fn parse_leading_seconds(m: &str, start: usize) -> Option<u64> {
    let bytes = m.as_bytes();
    let mut i = start;
    let digit_start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == digit_start || i >= bytes.len() || bytes[i] != b's' {
        return None;
    }
    m[digit_start..i].parse().ok()
}

/// `retry-after: 30` / `retry after 30` / `retry.after 30` (with or without a
/// unit suffix h/m/s; bare number defaults to seconds, matching HTTP's
/// `Retry-After` header semantics).
fn parse_retry_after(m: &str) -> Option<u64> {
    let idx = m
        .find("retry-after")
        .or_else(|| m.find("retry after"))
        .or_else(|| m.find("retry.after"))?;
    let rest = &m[idx..];
    let after_marker = rest.find(|c: char| c.is_ascii_digit())?;
    let digits_start = after_marker;
    let bytes = rest.as_bytes();
    let mut i = digits_start;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == digits_start {
        return None;
    }
    let num: u64 = rest[digits_start..i].parse().ok()?;
    match rest.as_bytes().get(i) {
        Some(b'h') => Some(num * 3600),
        Some(b'm') => Some(num * 60),
        _ => Some(num), // bare number or trailing 's' -> seconds
    }
}

/// `reset(s) in 30 (h|m|s)?` -- bare number defaults to seconds.
fn parse_resets_in(m: &str) -> Option<u64> {
    let idx = m.find("resets in").or_else(|| m.find("reset in"))?;
    let rest = &m[idx..];
    let after_marker = rest.find(|c: char| c.is_ascii_digit())?;
    let digits_start = after_marker;
    let bytes = rest.as_bytes();
    let mut i = digits_start;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == digits_start {
        return None;
    }
    let num: u64 = rest[digits_start..i].parse().ok()?;
    match rest.as_bytes().get(i) {
        Some(b'h') => Some(num * 3600),
        Some(b'm') => Some(num * 60),
        _ => Some(num),
    }
}

/// Whether `>= 2` of the [`FRONTIER`] providers currently count as "down"
/// (Shelved, or Cooldown not yet elapsed) in `reg`. This is the exact
/// predicate the `review_run` capacity gate consults before dispatch; kept
/// here (pure over a registry snapshot + a clock) so it's unit-testable
/// without touching the process-global singleton.
pub fn frontier_capacity_paused(reg: &ReviewerRegistry, now: SystemTime) -> (bool, Vec<String>) {
    let down: Vec<String> = FRONTIER
        .iter()
        .filter(|p| reg.is_down(p, now))
        .map(|s| s.to_string())
        .collect();
    (down.len() >= 2, down)
}

/// Earliest recovery time among `providers` (the ones reported down by
/// [`frontier_capacity_paused`]), for reporting alongside a paused outcome.
/// `None` if none of the given providers has a recorded `cooldown_until`
/// (shouldn't happen for genuinely-down providers, but never panics).
pub fn earliest_recovery(reg: &ReviewerRegistry, providers: &[String]) -> Option<SystemTime> {
    providers
        .iter()
        .filter_map(|p| reg.get(p).cooldown_until)
        .min()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs_from_now(now: SystemTime, secs: u64) -> SystemTime {
        now + Duration::from_secs(secs)
    }

    // ── detection: is_rate_limit_error / is_timeout_error ──────────────────

    #[test]
    fn detects_agy_quota_message() {
        let msg = "Individual quota reached. Please wait or upgrade. Resets in 76h4m15s.";
        assert!(is_rate_limit_error(msg));
        assert!(!is_timeout_error(msg));
    }

    #[test]
    fn detects_codex_retry_after() {
        let msg = "unavailable: codex http 429: retry-after: 30";
        assert!(is_rate_limit_error(msg));
    }

    #[test]
    fn detects_openrouter_429_phrasings() {
        assert!(is_rate_limit_error(
            "unavailable: openrouter http 429 Too Many Requests: Provider returned error"
        ));
        assert!(is_rate_limit_error("Too Many Requests"));
    }

    #[test]
    fn wall_clock_timeout_is_not_a_rate_limit() {
        let msg = "unavailable: daemon dispatch timeout after 120s";
        assert!(is_timeout_error(msg));
        assert!(!is_rate_limit_error(msg));
    }

    #[test]
    fn non_cap_errors_match_neither() {
        let msg = "unavailable: openrouter returned empty content";
        assert!(!is_rate_limit_error(msg));
        assert!(!is_timeout_error(msg));
    }

    // ── parse_horizon_secs ──────────────────────────────────────────────────

    #[test]
    fn parses_agy_hms_horizon() {
        let secs = parse_horizon_secs("Individual quota reached. ... Resets in 76h4m15s.").unwrap();
        assert_eq!(secs, 76 * 3600 + 4 * 60 + 15);
    }

    #[test]
    fn parses_codex_retry_after_seconds() {
        let secs = parse_horizon_secs("unavailable: codex http 429: retry-after: 30").unwrap();
        assert_eq!(secs, 30);
    }

    #[test]
    fn parses_minutes_seconds_shape() {
        let secs = parse_horizon_secs("rate limited, retry in 4m30s").unwrap();
        assert_eq!(secs, 4 * 60 + 30);
    }

    #[test]
    fn parses_resets_in_with_unit() {
        let secs = parse_horizon_secs("quota exceeded, resets in 2h").unwrap();
        assert_eq!(secs, 2 * 3600);
    }

    #[test]
    fn unparseable_horizon_yields_none() {
        assert_eq!(parse_horizon_secs("quota reached, try again later"), None);
    }

    // ── two-tier classification + absolute time + backoff ──────────────────

    #[test]
    fn short_horizon_classifies_as_cooldown_with_absolute_time() {
        let now = SystemTime::now();
        let mut s = ReviewerStatus::new("codex");
        s.mark_rate_limited(Some(30), now, "retry-after: 30");
        assert_eq!(s.last_status, CapStatus::Cooldown);
        assert!(!s.available);
        assert_eq!(s.cooldown_until, Some(secs_from_now(now, 30)));
        assert_eq!(s.consecutive_caps, 1);
    }

    #[test]
    fn long_horizon_classifies_as_shelved_with_absolute_time_far_out() {
        let now = SystemTime::now();
        let mut s = ReviewerStatus::new("agy");
        let horizon = 76 * 3600 + 4 * 60 + 15u64; // ~274255s
        s.mark_rate_limited(Some(horizon), now, "Resets in 76h4m15s.");
        assert_eq!(s.last_status, CapStatus::Shelved);
        assert!(!s.available);
        let until = s.cooldown_until.unwrap();
        let delta = until.duration_since(now).unwrap().as_secs();
        // First cap: no backoff added, so this should equal the horizon exactly.
        assert_eq!(delta, horizon);
        assert!(delta > 270_000, "expected ~76h out, got {delta}s");
    }

    #[test]
    fn unparsed_horizon_falls_back_to_default_cooldown_not_a_multiday_shelve() {
        let now = SystemTime::now();
        let mut s = ReviewerStatus::new("codex");
        s.mark_rate_limited(None, now, "quota reached, try again later");
        assert_eq!(s.last_status, CapStatus::Cooldown);
        let delta = s
            .cooldown_until
            .unwrap()
            .duration_since(now)
            .unwrap()
            .as_secs();
        assert_eq!(delta, DEFAULT_UNPARSED_COOLDOWN_SECS);
    }

    #[test]
    fn repeat_caps_back_off_exponentially() {
        let now = SystemTime::now();
        let mut s = ReviewerStatus::new("codex");
        s.mark_rate_limited(Some(30), now, "cap 1");
        let first_delta = s
            .cooldown_until
            .unwrap()
            .duration_since(now)
            .unwrap()
            .as_secs();
        assert_eq!(first_delta, 30); // no backoff on the first cap

        s.mark_rate_limited(Some(30), now, "cap 2");
        let second_delta = s
            .cooldown_until
            .unwrap()
            .duration_since(now)
            .unwrap()
            .as_secs();
        assert!(
            second_delta > first_delta,
            "second cap should back off harder: {second_delta} vs {first_delta}"
        );

        s.mark_rate_limited(Some(30), now, "cap 3");
        let third_delta = s
            .cooldown_until
            .unwrap()
            .duration_since(now)
            .unwrap()
            .as_secs();
        assert!(
            third_delta > second_delta,
            "third cap should back off even harder: {third_delta} vs {second_delta}"
        );
        assert_eq!(s.consecutive_caps, 3);
    }

    #[test]
    fn backoff_is_capped_at_ceiling() {
        let now = SystemTime::now();
        let mut s = ReviewerStatus::new("agy");
        for i in 0..30 {
            s.mark_rate_limited(Some(10), now, format!("cap {i}"));
        }
        assert!(s.backoff_secs <= BACKOFF_CEILING_SECS);
    }

    #[test]
    fn check_recovery_flips_true_only_after_the_time_passes() {
        let now = SystemTime::now();
        let mut s = ReviewerStatus::new("codex");
        s.mark_rate_limited(Some(30), now, "cap");
        assert!(!s.check_recovery(now));
        assert!(!s.check_recovery(secs_from_now(now, 29)));
        assert!(s.check_recovery(secs_from_now(now, 30)));
        assert!(s.check_recovery(secs_from_now(now, 31)));
    }

    #[test]
    fn mark_recovered_clears_cap_but_keeps_consecutive_count() {
        let now = SystemTime::now();
        let mut s = ReviewerStatus::new("codex");
        s.mark_rate_limited(Some(30), now, "cap");
        s.mark_recovered();
        assert!(s.available);
        assert_eq!(s.cooldown_until, None);
        assert_eq!(s.last_status, CapStatus::Available);
        assert_eq!(
            s.consecutive_caps, 1,
            "mark_recovered must not reset consecutive_caps"
        );
    }

    #[test]
    fn mark_success_fully_resets_state() {
        let now = SystemTime::now();
        let mut s = ReviewerStatus::new("codex");
        s.mark_rate_limited(Some(30), now, "cap");
        s.mark_success();
        assert!(s.available);
        assert_eq!(s.cooldown_until, None);
        assert_eq!(s.consecutive_caps, 0);
        assert_eq!(s.backoff_secs, 0);
        assert_eq!(s.last_status, CapStatus::Available);
    }

    #[test]
    fn mark_latency_never_caps_the_provider() {
        let mut s = ReviewerStatus::new("codex");
        s.mark_latency("daemon dispatch timeout after 120s");
        assert_eq!(s.last_status, CapStatus::Latency);
        assert!(s.available, "a timeout must never count as a cap");
        assert_eq!(s.cooldown_until, None);
    }

    // ── ReviewerRegistry ─────────────────────────────────────────────────

    #[test]
    fn registry_get_materializes_a_fresh_available_entry() {
        let reg = ReviewerRegistry::new();
        let s = reg.get("codex");
        assert_eq!(s.last_status, CapStatus::Available);
        assert!(s.available);
    }

    #[test]
    fn registry_update_mutates_in_place() {
        let now = SystemTime::now();
        let reg = ReviewerRegistry::new();
        reg.update("codex", |s| s.mark_rate_limited(Some(30), now, "cap"));
        assert!(reg.is_down("codex", now));
        assert!(!reg.is_down("codex", secs_from_now(now, 31)));
    }

    #[test]
    fn registry_snapshot_is_sorted_and_only_includes_tracked_providers() {
        let reg = ReviewerRegistry::new();
        reg.update("opus", |s| s.mark_success());
        reg.update("agy", |s| s.mark_success());
        let names: Vec<String> = reg.snapshot().into_iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["agy".to_string(), "opus".to_string()]);
    }

    // ── the >=2-frontier-down capacity gate ─────────────────────────────

    #[test]
    fn gate_pauses_when_two_frontier_providers_are_shelved() {
        let now = SystemTime::now();
        let reg = ReviewerRegistry::new();
        reg.update("codex", |s| s.mark_rate_limited(Some(3000), now, "shelved"));
        reg.update("agy", |s| s.mark_rate_limited(Some(3000), now, "shelved"));
        let (paused, down) = frontier_capacity_paused(&reg, now);
        assert!(paused);
        assert_eq!(down.len(), 2);
        assert!(down.contains(&"codex".to_string()));
        assert!(down.contains(&"agy".to_string()));
    }

    #[test]
    fn gate_proceeds_when_only_one_frontier_provider_is_down() {
        let now = SystemTime::now();
        let reg = ReviewerRegistry::new();
        reg.update("codex", |s| s.mark_rate_limited(Some(3000), now, "shelved"));
        let (paused, down) = frontier_capacity_paused(&reg, now);
        assert!(!paused);
        assert_eq!(down, vec!["codex".to_string()]);
    }

    #[test]
    fn gate_proceeds_on_an_empty_fresh_registry_backward_compat() {
        let now = SystemTime::now();
        let reg = ReviewerRegistry::new();
        let (paused, down) = frontier_capacity_paused(&reg, now);
        assert!(!paused);
        assert!(down.is_empty());
    }

    #[test]
    fn gate_ignores_a_recovered_cooldown() {
        let now = SystemTime::now();
        let reg = ReviewerRegistry::new();
        reg.update("codex", |s| s.mark_rate_limited(Some(30), now, "short cap"));
        reg.update("agy", |s| s.mark_rate_limited(Some(30), now, "short cap"));
        // Both would be down right now...
        assert!(frontier_capacity_paused(&reg, now).0);
        // ...but not once their cooldown has elapsed.
        let later = secs_from_now(now, 31);
        let (paused, _) = frontier_capacity_paused(&reg, later);
        assert!(!paused);
    }

    #[test]
    fn earliest_recovery_reports_the_soonest_of_the_down_providers() {
        let now = SystemTime::now();
        let reg = ReviewerRegistry::new();
        reg.update("codex", |s| s.mark_rate_limited(Some(3000), now, "cap"));
        reg.update("agy", |s| s.mark_rate_limited(Some(9000), now, "cap"));
        let down = vec!["codex".to_string(), "agy".to_string()];
        let earliest = earliest_recovery(&reg, &down).unwrap();
        assert_eq!(earliest, secs_from_now(now, 3000));
    }
}
