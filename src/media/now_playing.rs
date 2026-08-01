//! MUSEL-LIVE (MUSE #111 phase 1) — **live** Plex session data.
//!
//! Everything else the media domain reads from Plex is historical: library
//! sections, watch history, on-deck, recently-added. `/status/sessions` — what
//! is playing RIGHT NOW — was used nowhere in this tree. This module is that
//! missing read, and the `media_now_playing` tool that exposes it.
//!
//! Phase 2 renders this in the MUSE web GUI, so the STRUCTURED payload emitted
//! here is a contract, not an implementation detail. See
//! [`build_response`] for the field-by-field shape.
//!
//! ## Three outcomes that must never render identically
//!
//! "I could not reach Plex", "Plex refused my token" and "nobody is watching"
//! mean opposite things to somebody reading a dashboard, and collapsing them is
//! the ambiguity this item exists to remove. They are kept apart at the type
//! level from the transport up: [`PlexSessionsError`] in the client, and the
//! `status` discriminant here. `idle` is an `ok` answer with an empty list;
//! `unreachable` and `unauthorized` are not answers at all and carry no
//! session data — not even a count.
//!
//! The same collapse can arrive through the PARSER rather than the transport,
//! and that direction is the more dangerous one, because "nobody is watching"
//! reads as a confident, correct answer. So `idle` is granted only to the one
//! empty shape the live server actually emits, and every other unreadable body
//! is `malformed` — see [`session_items`]. That line is drawn finely enough to
//! separate an ABSENT `Metadata` key (idle at `size: 0`) from an explicitly
//! `null` one (always malformed), because Plex omits keys and never nulls them.
//! By the same evidence `size` itself is REQUIRED and must be a non-negative
//! whole number: Plex states a size on every container it emits, so an absent
//! one means the response was altered, and `0.5` sessions is not an imprecise
//! count but an impossible one — neither is ever floored into `idle`.
//!
//! The same `null` rule reaches one level down: a `TranscodeSession` that is
//! present but is not an OBJECT fails the whole response, because that block is
//! the only thing a playback decision is derived from — left unchecked a
//! `"TranscodeSession": null` rendered as a confident `direct_stream` with an
//! invented "remuxed" reason rather than as an unreadable answer.
//!
//! ## Never cached
//!
//! Live session data is worthless stale, so `media_now_playing` is named in
//! [`crate::tool_cache::NEVER_CACHED_TOOLS`]. That is a DELIBERATE extension of
//! the never-cache rule rather than a naming trick: the pre-existing rule keyed
//! on `alert`/`severe`/`warning`, which no honest name for this tool contains.
//!
//! ## Entitlement — the sensitive part
//!
//! Now-playing reveals which household member is home and what they are
//! watching, in real time. It is gated on the caller via the TRTR-05
//! [`CallerContext`] mechanism: an unentitled caller (guest, unknown, absent
//! principal, or the un-threaded `execute()` path) gets `status: "forbidden"`
//! and NOTHING else — no titles, no usernames, no device names, and no session
//! count, since a count alone discloses occupancy. The gate runs BEFORE the
//! client is touched, so an unentitled call issues no Plex request at all.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::instrument;

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::{CallerContext, RustTool, ToolOutput};

use super::clients::plex::{PlexClient, PlexSessionsError};

/// The tool name. Also the key the never-cache rule matches on.
pub const TOOL_NAME: &str = "media_now_playing";

// ── entitlement ─────────────────────────────────────────────────────────────

/// May this caller see who is watching what, right now?
///
/// **Why this reuses the two existing TRTR-05 signals instead of adding a
/// third flag.** `CallerContext`'s flags are minted by
/// `GatewayFramework::caller_context` from PROBE TOOLS — "may a tool fold in a
/// source of operator context on this caller's behalf" is answered by asking
/// whether the caller could already read that source directly. Both existing
/// probes (`google_calendar_today`, `commute_estimate`) are operator-tier and
/// neither is in `GUEST_BASELINE_ALLOW`, which is a CEILING a guest grant is
/// clamped to — so requiring BOTH is exactly the predicate "this principal is
/// operator-tier, not a guest and not an unknown". Requiring both rather than
/// either is the fail-closed choice: a partial grant yields nothing.
///
/// The cleaner long-term shape is a dedicated `may_see_household_activity`
/// flag minted from a `NOW_PLAYING_CONTEXT_PROBE`, which would make the gate
/// self-describing instead of inferred. That is a `gateway_framework` change
/// and is deliberately NOT made here (see the report accompanying this item):
/// this module is additive, and reusing the existing signal is strictly no
/// weaker than a new flag probed off an operator-tier tool. Recorded here so
/// the next reader knows it was a decision, not an oversight.
pub fn may_see_now_playing(caller: CallerContext) -> bool {
    caller.may_infer_from_calendar() && caller.may_infer_from_routine()
}

// ── the source seam (so "no request was issued" is testable) ────────────────

/// The live reads `media_now_playing` needs, behind a trait so a test can
/// COUNT calls and prove the entitlement gate short-circuits before any I/O.
#[async_trait]
pub trait SessionSource: Send + Sync {
    async fn sessions(&self) -> Result<Value, PlexSessionsError>;
    /// Best-effort server header. An `Err` here must never change the session
    /// outcome — it only means the GUI header is unavailable this tick.
    async fn identity(&self) -> Result<Value, PlexSessionsError>;
}

#[async_trait]
impl SessionSource for PlexClient {
    async fn sessions(&self) -> Result<Value, PlexSessionsError> {
        PlexClient::sessions(self).await
    }
    async fn identity(&self) -> Result<Value, PlexSessionsError> {
        PlexClient::identity(self).await
    }
}

// ── parsing: model what the server actually returns ─────────────────────────

/// How Plex is delivering one session's media.
///
/// Plex does NOT hand this over as a single field — it has to be DERIVED, and
/// the derivation was checked against a live server rather than the docs:
/// - no `TranscodeSession` at all           => direct play
/// - `TranscodeSession` with video AND audio `decision == "copy"`  => direct
///   stream (a container remux; nothing is re-encoded)
/// - anything else                          => transcode
///
/// The live session observed while building this had `videoDecision: "copy"`
/// with `audioDecision: "transcode"`, which is a TRANSCODE — treating any
/// `copy` as direct stream would have mislabelled it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackDecision {
    DirectPlay,
    DirectStream,
    Transcode,
}

impl PlaybackDecision {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DirectPlay => "direct_play",
            Self::DirectStream => "direct_stream",
            Self::Transcode => "transcode",
        }
    }
}

fn str_at<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}

/// A JSON number read as an INTEGER, and only when it genuinely is one.
///
/// `serde_json::Number::as_i64` already fails on a float, so the interesting
/// case is the fallback: `2.0` is the integer 2 and is accepted, while `2.5`
/// is not an integer at all and is rejected rather than floored. The bounds
/// check exists because `f64 as i64` SATURATES in Rust, so an out-of-range
/// float would otherwise silently become `i64::MAX`.
fn integral(n: &serde_json::Number) -> Option<i64> {
    if let Some(i) = n.as_i64() {
        return Some(i);
    }
    let f = n.as_f64()?;
    (f.is_finite() && f.fract() == 0.0 && f >= i64::MIN as f64 && f <= i64::MAX as f64)
        .then(|| f as i64)
}

/// A COUNT or an ORDINAL — a whole number of things. Plex is inconsistent
/// about numeric-vs-string JSON (`sessionKey` is a string, `duration` an int,
/// `Genre[].count` a string), so both forms are accepted.
///
/// **A fractional value is rejected, never floored.** This is the accessor for
/// `MediaContainer.size`, `parentIndex` (season), `index` (episode) and `year`
/// — fields where a fraction has no meaning at all, so a fraction is evidence
/// the value is not what we think it is rather than a value to round off. The
/// earlier `n.as_f64().map(|f| f as i64)` fallback truncated: `size: 0.5`
/// became `0` and rendered a house full of viewers as `idle`, and `index: 12.7`
/// would have rendered as episode 12 — a specific, wrong, confident claim.
/// Rejecting yields `None`, which for `size` is [`container_size`]'s malformed
/// (below) and for a season/episode ordinal simply drops that component from
/// the title, so `full_title` says "Show - Title" instead of inventing an
/// `S04E12` that the payload never stated.
///
/// Contrast [`quantity_at`], which rounds — see the justification there.
fn num_at(v: &Value, key: &str) -> Option<i64> {
    match v.get(key) {
        Some(Value::Number(n)) => integral(n),
        Some(Value::String(s)) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

/// A measured QUANTITY with a unit — milliseconds (`duration`, `viewOffset`)
/// or kilobits per second (`Session.bandwidth`) — as opposed to a count.
///
/// **These ROUND rather than reject, and the asymmetry with [`num_at`] is the
/// point.** A count of 0.5 sessions is not a slightly-imprecise count, it is a
/// statement that cannot be true, and acting on it means claiming an empty
/// house. A duration of `2400000.5` ms is an ordinary measurement carrying
/// sub-millisecond precision that nothing downstream renders: `progress_ms` /
/// `duration_ms` feed a percentage rounded to one decimal place, and
/// `bandwidth_kbps` is summed for a dashboard total. Rejecting those would
/// blank a progress bar and drop a stream out of the bandwidth total over a
/// half-millisecond — trading a harmless rounding for a visible hole. So the
/// rule is per-field and follows the meaning: fractional COUNT ⇒ malformed,
/// fractional MEASUREMENT ⇒ rounded to its nearest whole unit.
///
/// (`sessionKey` needs no decision here: Plex sends it as a string and the
/// payload carries it through as an opaque string via `str_at`, so it is never
/// interpreted as a number in the first place.)
fn quantity_at(v: &Value, key: &str) -> Option<i64> {
    fn round_in_range(f: f64) -> Option<i64> {
        (f.is_finite() && f >= i64::MIN as f64 && f <= i64::MAX as f64).then(|| f.round() as i64)
    }
    match v.get(key) {
        Some(Value::Number(n)) => integral(n).or_else(|| round_in_range(n.as_f64()?)),
        Some(Value::String(s)) => {
            let s = s.trim();
            s.parse::<i64>().ok().or_else(|| round_in_range(s.parse::<f64>().ok()?))
        }
        _ => None,
    }
}

fn f64_at(v: &Value, key: &str) -> Option<f64> {
    match v.get(key) {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

fn bool_at(v: &Value, key: &str) -> Option<bool> {
    match v.get(key) {
        Some(Value::Bool(b)) => Some(*b),
        Some(Value::Number(n)) => n.as_i64().map(|i| i != 0),
        Some(Value::String(s)) => match s.trim() {
            "1" | "true" => Some(true),
            "0" | "false" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// `MediaContainer.Metadata[]` — or a description of why the payload could not
/// be read at all.
///
/// **Why this returns a `Result` and not a `Vec`.** An earlier version returned
/// an empty `Vec` for anything it could not walk, which meant `{}`, a missing
/// `MediaContainer`, and a container claiming sessions it did not carry all
/// rendered as `status: "idle"` — *"nobody is watching"*. That is the exact
/// collapse this module exists to prevent, arriving through the parser instead
/// of the transport, and it is the more dangerous direction: on a dashboard a
/// broken read that says "nobody is watching" reads as a confident, correct
/// answer, whereas `malformed` visibly says "ask again".
///
/// The one legitimately-empty shape is narrow and was verified against the
/// live server (Plex Media Server 1.42.2): at rest `/status/sessions` returns
/// exactly `{"MediaContainer":{"size":0}}` — `size` is an integer `0` and the
/// `Metadata` key is absent entirely. So *absent `Metadata` with `size == 0`*
/// is idle, and everything else that cannot be walked is malformed. Leaning on
/// the observed shape rather than on defensiveness matters in both directions:
/// calling a genuine idle response malformed would be its own false alarm.
///
/// **Absent and explicitly `null` are NOT the same thing here.** An explicit
/// `"Metadata": null` is malformed at any `size`, including `size: 0`. That is
/// a decision made on evidence, not on defensiveness: Plex's JSON serializer
/// OMITS keys it has no value for, and emits no JSON `null` anywhere — re-probed
/// read-only against the live server (1.42.2) across `/status/sessions`,
/// `/identity`, `/library/sections`, `/library/onDeck` and
/// `/status/sessions/history/all`, none of which contains a single `null`. The
/// client between here and the wire is a plain `serde_json::from_str` of the
/// response body ([`super::clients::plex::PlexClient::sessions`]), so it cannot
/// introduce one either. A null therefore means something in the path REWROTE
/// the response, which makes the `size` sitting next to it no more trustworthy
/// than the key it replaced — exactly the "we cannot read this" case, not the
/// "nobody is watching" one. `idle` stays narrow: the absent key, or an empty
/// array, each with `size == 0`.
///
/// **`size` is REQUIRED, and must be a non-negative whole number.** Same
/// evidence, same reasoning as the `null` rule, and re-verified read-only
/// against the live server (Plex Media Server 1.42.2) rather than assumed:
/// `size` was present, and an integer, on every endpoint probed —
/// `/status/sessions` (`{"MediaContainer":{"size":0}}` at rest),
/// `/transcode/sessions`, `/clients`, `/identity`, `/library/sections`,
/// `/library/onDeck`, `/library/recentlyAdded` and
/// `/status/sessions/history/all`. Plex states the count on every container it
/// emits, including containers that carry nothing else. So an absent `size` is
/// not a legitimate state we must tolerate — it means something altered the
/// response, and the fields still standing next to it are no more trustworthy
/// than the one that vanished. A missing `size` is therefore `malformed` at
/// every shape, including alongside an empty `Metadata` list, which previously
/// slipped through as `idle` because the cross-check below was skipped when
/// there was no count to check against.
///
/// Cross-checks applied, each because it would otherwise become a silent
/// UNDERCOUNT — a quiet lie about who is watching:
/// - `Metadata` explicitly `null` ⇒ malformed, at ANY `size` (above).
/// - `size` absent ⇒ malformed, whatever `Metadata` does (above).
/// - `size` fractional, negative, or not a number at all ⇒ malformed. A
///   collection size is a count of things: `0.5` is not an imprecise count but
///   an impossible one, and flooring it to `0` would render a full house as an
///   empty one. See [`num_at`] for why counts reject and measurements round.
/// - `Metadata` absent but `size` nonzero ⇒ malformed.
/// - `Metadata` present but not a list ⇒ malformed.
/// - a `Metadata` entry that is not an object ⇒ malformed (see below).
/// - an entry whose `TranscodeSession` is present but is **not an object** ⇒
///   malformed, whole response (see below).
/// - `size` disagreeing with the number of entries ⇒ malformed.
///   `/status/sessions` is not a paginated collection as this client calls it
///   (it grows `offset`/`totalSize` only when asked for a page, and
///   [`super::clients::plex::PlexClient::sessions`] sends no pagination
///   parameters — verified live), so `size` and the list length are two
///   statements of the same fact; when they disagree, one of them is wrong and
///   we do not know which.
///
/// **An unparseable entry fails the WHOLE response rather than being dropped.**
/// [`session_json`] is total over any JSON *object* — every field is optional
/// and tolerates a missing/odd type — so the only way a single entry is
/// genuinely unreadable is that it is not an object at all. That is not one bad
/// viewer, it is evidence that the thing we are talking to does not speak this
/// schema, which makes the rest of the list untrustworthy too. Dropping it
/// would return a list that is short by one with a count to match: precisely
/// the undercount to avoid. It would also need a fourth, partially-known state
/// the contract deliberately does not have — today every non-`ok` status
/// carries NO count, so a consumer either gets a complete count or none at all.
///
/// **A non-object `TranscodeSession` fails the whole response too — the
/// `Metadata: null` fault one level down.** `TranscodeSession` is the ONLY
/// nested block a playback decision is derived from: [`decide`] reads
/// `videoDecision`/`audioDecision` off it and every derived
/// [`transcode_reason`] comes out of it. A `"TranscodeSession": null` is the
/// same evidence as a null `Metadata` — Plex omits keys rather than nulling
/// them and emits no JSON `null` on any of the eight endpoints probed
/// read-only against the live server (1.42.2) — so it means something rewrote
/// the response. Left unchecked that null did not degrade to "unknown": it read
/// as a session with nothing re-encoded, i.e. `decision: "direct_stream"` with
/// the derived reason `remuxed to a different container` — a confident,
/// specific, unobserved claim about the household, manufactured entirely out of
/// a value the server never sent. The unparseable-entry argument above decides
/// the same scope here: rejecting just that entry returns a list short by one
/// with a count to match (the undercount), and keeping the entry with its
/// transcode block ignored still states a playback decision derived from a
/// payload we have just concluded was altered. So the WHOLE response is
/// `malformed`, at every `size`, for a `TranscodeSession` that is `null`, a
/// scalar, or a list.
///
/// `MediaContainer.size`, insisted upon as a non-negative whole number.
///
/// Absent, fractional, negative or non-numeric are all `Err` — see
/// [`session_items`] for the live evidence that Plex always emits it, and
/// [`num_at`] for why a count rejects a fraction instead of flooring it.
/// Plex sends it as an integer on this endpoint; `num_at` also accepts the
/// string form some other Plex endpoints use, so a stringly-typed server is
/// read rather than declared broken.
fn container_size(mc: &Value) -> Result<i64, String> {
    let Some(raw) = mc.get("size") else {
        return Err(
            "MediaContainer carried no size, and Plex states a size on every container it \
             emits (at rest /status/sessions is exactly {\"MediaContainer\":{\"size\":0}}), \
             so the response was altered somewhere in the path"
                .to_string(),
        );
    };
    let n = num_at(mc, "size").ok_or_else(|| {
        format!("MediaContainer.size was {raw}, which is not a whole number of sessions")
    })?;
    if n < 0 {
        return Err(format!(
            "MediaContainer.size was {n}, and a number of sessions cannot be negative"
        ));
    }
    Ok(n)
}

/// The NAME of a JSON shape, for a malformed detail that says what arrived
/// without quoting the payload back (a rewritten body is not something to echo,
/// and a large array would swamp the message).
fn json_kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "a list",
        Value::Object(_) => "an object",
    }
}

fn session_items(raw: &Value) -> Result<Vec<&Value>, String> {
    let Some(mc) = raw.get("MediaContainer") else {
        return Err("the response from Plex had no MediaContainer".to_string());
    };
    if !mc.is_object() {
        return Err("MediaContainer was not an object".to_string());
    }

    // Explicit null is NOT absence. Plex omits keys rather than nulling them
    // (verified live, see the doc comment), so a null is evidence the response
    // was rewritten in transit — and a rewritten response makes the `size`
    // beside it untrustworthy too, at 0 as much as at 3. Checked BEFORE the
    // size so the detail names the more specific fault when both are wrong.
    if let Some(Value::Null) = mc.get("Metadata") {
        return Err(
            "MediaContainer.Metadata was explicitly null, which is not a shape Plex emits \
             (it omits the key entirely when nothing is playing)"
                .to_string(),
        );
    }

    // A missing/invalid size fails EVERY shape, including an empty Metadata
    // list. Nothing below may fall back to "no size to check against".
    let size = container_size(mc)?;

    match mc.get("Metadata") {
        None => {
            if size == 0 {
                Ok(Vec::new())
            } else {
                Err(format!(
                    "MediaContainer reported {size} session(s) but carried no Metadata list"
                ))
            }
        }
        Some(Value::Array(items)) => {
            for (i, item) in items.iter().enumerate() {
                if !item.is_object() {
                    return Err(format!(
                        "MediaContainer.Metadata entry {i} was not a session object"
                    ));
                }
                // A `TranscodeSession` that is PRESENT but is not an object —
                // `null` above all — is the `Metadata: null` fault one level
                // down, and is rejected for the same reason and on the same
                // evidence. See the doc comment above.
                if let Some(ts) = item.get("TranscodeSession") {
                    if !ts.is_object() {
                        return Err(format!(
                            "MediaContainer.Metadata entry {i} carried a TranscodeSession that \
                             was {}, not an object, and Plex emits neither a JSON null nor a \
                             scalar there",
                            json_kind(ts)
                        ));
                    }
                }
            }
            if size != items.len() as i64 {
                return Err(format!(
                    "MediaContainer reported {size} session(s) but carried {}",
                    items.len()
                ));
            }
            Ok(items.iter().collect())
        }
        Some(_) => Err("MediaContainer.Metadata was not a list".to_string()),
    }
}

/// `Media[0].Part[0].decision` — where Plex states the per-part decision
/// explicitly (`directplay` / `copy` / `transcode`).
fn part_decision(item: &Value) -> Option<&str> {
    let part = item
        .get("Media")?
        .as_array()?
        .first()?
        .get("Part")?
        .as_array()?
        .first()?;
    str_at(part, "decision")
}

fn decide(item: &Value) -> PlaybackDecision {
    // `.filter(is_object)` is the second half of the non-object-TranscodeSession
    // rule and is deliberately redundant: [`session_items`] has already failed
    // the whole response before any such entry reaches here. It stays because it
    // is what makes the `transcode_reason` invariant true BY CONSTRUCTION rather
    // than by upstream sequencing — these two functions are the only things that
    // read the object, and neither may be reachable from a `null` and still
    // produce a confident answer. A non-object is treated as ABSENT, so the
    // decision falls to `Part.decision` and no reason can be derived.
    let Some(ts) = item.get("TranscodeSession").filter(|ts| ts.is_object()) else {
        // No transcode session at all. Normally that is a direct play, but
        // Plex states the decision on the Part too, so cross-check it rather
        // than assuming: a payload that says `transcode` without a
        // TranscodeSession must not be reported as a direct play.
        return match part_decision(item) {
            Some("transcode") => PlaybackDecision::Transcode,
            Some("copy") => PlaybackDecision::DirectStream,
            _ => PlaybackDecision::DirectPlay,
        };
    };
    let video = str_at(ts, "videoDecision").unwrap_or("copy");
    let audio = str_at(ts, "audioDecision").unwrap_or("copy");
    if video == "copy" && audio == "copy" {
        PlaybackDecision::DirectStream
    } else {
        PlaybackDecision::Transcode
    }
}

/// Plex's own reason string when it gives one, else a derived one.
///
/// `TranscodeSession.transcodeReason` is documented but was **absent** on the
/// live server checked for this item (Plex Media Server 1.42.x), so a
/// reason-or-nothing implementation would have shown nothing in practice.
/// When it is missing we say which streams are being re-encoded and from what
/// — the operationally useful part of the answer.
///
/// # When this is `None` (contract-relevant — read [`build_response`])
///
/// Every derived reason is derived FROM the `TranscodeSession` object: which
/// streams are re-encoded, from what codec to what codec, which protocol the
/// remux targets. So a reason exists only when Plex supplied that object.
///
/// The exact rule, and the one a consumer may rely on:
///
/// > `transcode_reason` is non-null **iff** `decision != "direct_play"` **and**
/// > the session carried a `TranscodeSession` **object**. A `TranscodeSession`
/// > that is present but is not an object — `null` above all — is never read as
/// > one: it fails the WHOLE response as `malformed` ([`session_items`]), so it
/// > can never surface as a confident decision with an invented reason.
///
/// So it is always `None` for `direct_play`, and it is also `None` for the two
/// payloads where Plex states a non-direct-play decision on `Media[0].Part[0]`
/// but supplies no `TranscodeSession` to explain it. `decision` — never the
/// presence of a reason — is the discriminant for playback mode.
///
/// This is why the Phase 2 contract permits null for `direct_stream` and
/// `transcode` instead of saying "null iff direct play". The alternative was to
/// synthesise a reason for those cases, and that was rejected deliberately:
/// with no `TranscodeSession` the server has told us *that* it is not direct
/// playing and nothing at all about *why* or *into what*. A field whose entire
/// job is to state a reason must not carry a guess — an invented "remuxed to a
/// different container" is a claim about the server we did not observe, and it
/// is worse than an honest blank because a dashboard renders it as fact.
fn transcode_reason(item: &Value, decision: PlaybackDecision) -> Option<String> {
    if decision == PlaybackDecision::DirectPlay {
        return None;
    }
    // Only an OBJECT can carry a reason (see [`decide`] for why this guard is
    // deliberately redundant with the `session_items` rejection). Without it a
    // `"TranscodeSession": null` fell through every branch below to the
    // direct-stream fallback and manufactured `remuxed to a different
    // container` — a non-null reason from a session that carried no session.
    let ts = item.get("TranscodeSession").filter(|ts| ts.is_object())?;
    if let Some(reason) = str_at(ts, "transcodeReason") {
        return Some(reason.to_string());
    }

    let mut parts: Vec<String> = Vec::new();
    for (label, decision_key, src_key, dst_key) in [
        ("video", "videoDecision", "sourceVideoCodec", "videoCodec"),
        ("audio", "audioDecision", "sourceAudioCodec", "audioCodec"),
    ] {
        if str_at(ts, decision_key) == Some("transcode") {
            match (str_at(ts, src_key), str_at(ts, dst_key)) {
                (Some(src), Some(dst)) => parts.push(format!("{label} {src} -> {dst}")),
                _ => parts.push(format!("{label} re-encoded")),
            }
        }
    }
    if str_at(ts, "subtitleDecision") == Some("transcode") {
        parts.push("subtitles burned in or converted".to_string());
    }
    if parts.is_empty() {
        // Direct stream: nothing re-encoded, the container is being remuxed.
        let container = str_at(ts, "protocol").unwrap_or("a different container");
        parts.push(format!("remuxed to {container}"));
    }
    Some(parts.join("; "))
}

/// A display title that reads correctly for BOTH shapes Plex nests
/// differently (verified against live payloads):
/// - movie:   `title` + `year`
/// - episode: `grandparentTitle` (show) + `parentIndex` (season) +
///            `index` (episode) + `title` (episode title)
fn full_title(item: &Value) -> String {
    let title = str_at(item, "title").unwrap_or("Unknown");
    if str_at(item, "type") == Some("episode") {
        let show = str_at(item, "grandparentTitle").unwrap_or("Unknown show");
        return match (num_at(item, "parentIndex"), num_at(item, "index")) {
            (Some(s), Some(e)) => format!("{show} - S{s:02}E{e:02} - {title}"),
            _ => format!("{show} - {title}"),
        };
    }
    match num_at(item, "year") {
        Some(y) => format!("{title} ({y})"),
        None => title.to_string(),
    }
}

/// One session, flattened into the shape the GUI consumes.
///
/// Deliberately NOT included: `Player.address` and `Player.remotePublicAddress`
/// (household IP addresses, of no use to the dashboard) and the whole
/// `Media`/`Part`/`Stream` metadata tail (cast, genres, artwork, summaries).
/// A live-activity view needs who/what/where/how, not the catalogue record.
fn session_json(item: &Value) -> Value {
    let decision = decide(item);
    let user = item.get("User").cloned().unwrap_or(Value::Null);
    let player = item.get("Player").cloned().unwrap_or(Value::Null);
    let session = item.get("Session").cloned().unwrap_or(Value::Null);
    let ts = item.get("TranscodeSession").cloned().unwrap_or(Value::Null);
    let media = item
        .get("Media")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .cloned()
        .unwrap_or(Value::Null);

    // MEASUREMENTS, not counts: `quantity_at` rounds a fractional millisecond
    // rather than dropping the field. See `quantity_at` for the split.
    let duration_ms = quantity_at(item, "duration");
    let progress_ms = quantity_at(item, "viewOffset").unwrap_or(0);
    let progress_percent = match duration_ms {
        Some(d) if d > 0 => Some(((progress_ms as f64 / d as f64) * 1000.0).round() / 10.0),
        _ => None,
    };

    json!({
        "session_key": str_at(item, "sessionKey"),
        "media_type": str_at(item, "type"),
        "title": str_at(item, "title"),
        "show_title": str_at(item, "grandparentTitle"),
        "season": num_at(item, "parentIndex"),
        "episode": num_at(item, "index"),
        "year": num_at(item, "year"),
        "full_title": full_title(item),
        "library_section": str_at(item, "librarySectionTitle"),
        "user": str_at(&user, "title"),
        "user_id": user.get("id").map(|v| v.to_string().trim_matches('"').to_string()),
        "player": str_at(&player, "title"),
        "player_product": str_at(&player, "product"),
        "player_platform": str_at(&player, "platform"),
        "player_state": str_at(&player, "state"),
        "player_local": bool_at(&player, "local"),
        "progress_ms": progress_ms,
        "duration_ms": duration_ms,
        "progress_percent": progress_percent,
        "decision": decision.as_str(),
        "video_decision": str_at(&ts, "videoDecision"),
        "audio_decision": str_at(&ts, "audioDecision"),
        "subtitle_decision": str_at(&ts, "subtitleDecision"),
        "transcode_reason": transcode_reason(item, decision),
        "transcode_progress_percent": f64_at(&ts, "progress"),
        "transcode_throttled": bool_at(&ts, "throttled"),
        "transcode_hw": bool_at(&ts, "transcodeHwRequested"),
        "bandwidth_kbps": quantity_at(&session, "bandwidth"),
        "stream_location": str_at(&session, "location"),
        "container": str_at(&media, "container"),
        "video_resolution": str_at(&media, "videoResolution"),
    })
}

/// `MediaContainer` from `/identity`, reduced to the GUI-useful fields.
fn server_json(raw: &Value) -> Value {
    let mc = raw.get("MediaContainer").cloned().unwrap_or(Value::Null);
    json!({
        "version": str_at(&mc, "version"),
        "api_version": str_at(&mc, "apiVersion"),
        "machine_identifier": str_at(&mc, "machineIdentifier"),
    })
}

// ── the response contract (Phase 2 consumes this verbatim) ──────────────────

/// Build the structured payload.
///
/// `status` is the ONLY discriminant a consumer needs, and it is exhaustive:
///
/// | `status`        | `ok`  | has `sessions`/`session_count` | means |
/// |-----------------|-------|-------------------------------|-------|
/// | `playing`       | true  | yes (non-empty)               | somebody is watching |
/// | `idle`          | true  | yes (empty, count 0)          | Plex answered: nobody is watching |
/// | `unreachable`   | false | **no**                        | no answer from Plex — we do not know |
/// | `unauthorized`  | false | **no**                        | Plex refused the token |
/// | `malformed`     | false | **no**                        | Plex answered something we cannot read |
/// | `not_configured`| false | **no**                        | `PLEX_URL`/`PLEX_TOKEN` unset |
/// | `forbidden`     | false | **no**                        | caller is not entitled; nothing was fetched |
///
/// The absence of `session_count` on every non-`ok` status is deliberate and
/// load-bearing in two directions: a failed read must not be mistakable for an
/// empty house, and an unentitled caller must not learn occupancy from a count.
///
/// `malformed` covers BOTH a body that was not JSON at all (raised in the
/// client, [`PlexSessionsError::Malformed`]) and a well-formed JSON body whose
/// session container could not be read ([`session_items`]). `idle` is reserved
/// for the one shape the live server actually emits when nothing is playing —
/// see [`session_items`] for exactly where that line falls and why.
///
/// ### `idle` vs `malformed`: absent `Metadata` vs explicit `null`
///
/// Stated here because it is the one place the shape is genuinely ambiguous to
/// a consumer, and Phase 2 must not have to guess:
///
/// > `status` is `idle` only when `MediaContainer.size` is **present** and is
/// > the whole number `0`, **and** `MediaContainer.Metadata` is either
/// > **absent** or an **empty array**. Everything else is `malformed`, never
/// > `idle`:
/// >
/// > - an explicitly `null` `Metadata`, at every `size` including `0` — Plex
/// >   omits the key when nothing is playing (`{"MediaContainer":{"size":0}}`)
/// >   and emits no JSON `null` on any endpoint, so a null means the response
/// >   was rewritten in transit and neither it nor the `size` beside it can be
/// >   trusted;
/// > - an **absent** `size`, whatever `Metadata` does — Plex states a size on
/// >   every container it emits, so a missing one is the same evidence of an
/// >   altered response;
/// > - a `size` that is **fractional, negative, or not a number** — a count of
/// >   things is a whole non-negative number, so `0.5` is not an imprecise
/// >   count but an impossible one and is never floored to `0`;
/// > - an entry whose `TranscodeSession` is present but is **not an object**
/// >   (`null`, a scalar, a list) — same evidence, same verdict, one level
/// >   down, and the whole response fails rather than that one entry.
///
/// ### Per-session field contract, where it is not self-evident
///
/// - `decision` is one of `direct_play` | `direct_stream` | `transcode`, and is
///   the ONLY discriminant for playback mode.
/// - `transcode_reason` is non-null **iff** `decision != "direct_play"` **and**
///   the session carried a `TranscodeSession` **object**. It is therefore
///   permitted to be null for `direct_stream` and `transcode` — a consumer must
///   render a null reason as "no reason given" and must NOT infer direct play
///   from it. See [`transcode_reason`] for why no reason is invented in that
///   case. A `TranscodeSession` that is present but is **not an object** —
///   `null` above all — is never read as one: it fails the whole response as
///   `malformed` (above), so no session ever renders a decision or a reason
///   derived from it.
/// - Numeric fields split by MEANING, and the split is part of the contract.
///   `season`, `episode` and `year` are ordinals: a fractional value is dropped
///   to `null` rather than truncated, so a consumer never sees an episode
///   number the payload did not state and `full_title` omits the `SxxEyy`
///   component instead of inventing one. `progress_ms`, `duration_ms` and
///   `bandwidth_kbps` are measurements: a fractional value is rounded to the
///   nearest whole unit, because a half-millisecond is not worth a blank
///   progress bar or a stream missing from the bandwidth total. `session_key`
///   is carried through as an opaque string and never read as a number.
fn build_response(status: &str, message: &str, extra: Value) -> Value {
    let mut out = json!({
        "status": status,
        "ok": matches!(status, "playing" | "idle"),
        "message": message,
    });
    if let (Some(map), Some(more)) = (out.as_object_mut(), extra.as_object()) {
        for (k, v) in more {
            map.insert(k.clone(), v.clone());
        }
    }
    out
}

/// The refusal. Constructed WITHOUT consulting Plex or any argument, so there
/// is nothing in it to leak.
fn forbidden_response() -> Value {
    build_response(
        "forbidden",
        "Live viewing activity is private to the household operator; this caller is not entitled to it.",
        json!({}),
    )
}

fn ok_response(sessions: Vec<Value>, server: Option<Value>) -> Value {
    let total_bandwidth: Option<i64> = {
        let sum: i64 = sessions.iter().filter_map(|s| s["bandwidth_kbps"].as_i64()).sum();
        let any = sessions.iter().any(|s| s["bandwidth_kbps"].is_i64());
        any.then_some(sum)
    };
    let count = sessions.len();
    let (status, message) = if count == 0 {
        ("idle", "Plex answered: nothing is playing right now.".to_string())
    } else {
        ("playing", format!("{count} stream(s) playing right now."))
    };
    build_response(
        status,
        &message,
        json!({
            "session_count": count,
            "total_bandwidth_kbps": total_bandwidth,
            "sessions": sessions,
            "server": server,
        }),
    )
}

fn error_response(err: &PlexSessionsError) -> Value {
    let message = match err {
        PlexSessionsError::Unreachable(_) => {
            "Could not reach Plex, so what is playing is UNKNOWN - this is not the same as nothing playing."
        }
        PlexSessionsError::TokenRejected(_) => {
            "Plex is up but rejected the configured token, so live activity could not be read."
        }
        PlexSessionsError::Malformed(_) => {
            "Plex answered with something this client could not read, so live activity is unknown."
        }
    };
    build_response(err.kind(), message, json!({ "detail": err.detail() }))
}

// ── the tool ────────────────────────────────────────────────────────────────

pub struct MediaNowPlaying {
    source: Option<Arc<dyn SessionSource>>,
}

impl MediaNowPlaying {
    pub fn from_env() -> Self {
        Self {
            source: PlexClient::from_env().ok().map(|c| Arc::new(c) as Arc<dyn SessionSource>),
        }
    }

    pub fn with_source(source: Arc<dyn SessionSource>) -> Self {
        Self { source: Some(source) }
    }

    /// The whole decision, in one place.
    ///
    /// Order matters: the entitlement gate is FIRST, before `self.source` is
    /// even inspected, so an unentitled call cannot issue a Plex request and
    /// cannot distinguish "not entitled" from "not configured" either.
    async fn run(&self, caller: CallerContext) -> Value {
        if !may_see_now_playing(caller) {
            return forbidden_response();
        }
        let Some(source) = self.source.as_ref() else {
            return build_response(
                "not_configured",
                "Plex is not configured here (PLEX_URL / PLEX_TOKEN are unset).",
                json!({}),
            );
        };

        match source.sessions().await {
            Ok(raw) => match session_items(&raw) {
                Ok(items) => {
                    let sessions: Vec<Value> = items.into_iter().map(session_json).collect();
                    // Best-effort only: the server header must never turn a
                    // good session read into a failure.
                    let server = source.identity().await.ok().map(|v| server_json(&v));
                    ok_response(sessions, server)
                }
                // A 200 whose BODY we cannot read is the same class of problem
                // as a body that was not JSON — and must never become "idle".
                Err(detail) => error_response(&PlexSessionsError::Malformed(detail)),
            },
            Err(e) => error_response(&e),
        }
    }
}

#[async_trait]
impl RustTool for MediaNowPlaying {
    fn name(&self) -> &str {
        TOOL_NAME
    }

    fn description(&self) -> &str {
        "Show what is playing on Plex RIGHT NOW: title, who is watching, on which player, how far in, and whether it is direct play, direct stream or a transcode (with the reason). Live and never cached. Private: only an entitled operator-tier caller receives any of it." // pii-test-fixture
    }

    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    /// The UN-THREADED path. It gets [`CallerContext::untrusted`], which is
    /// unentitled, so it returns the refusal — never household activity.
    #[instrument(skip(self, _args), fields(tool = "media_now_playing"))]
    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        Ok(self.run(CallerContext::untrusted()).await.to_string())
    }

    /// The authorized path: the gateway derived this caller's entitlement from
    /// the same server-verified principal it authorized the call with.
    async fn execute_with_caller(
        &self,
        _args: Value,
        caller: CallerContext,
    ) -> Result<ToolOutput, ToolError> {
        let payload = self.run(caller).await;
        let text = payload["message"].as_str().unwrap_or_default().to_string();
        Ok(ToolOutput::with_structured(text, payload))
    }
}

pub fn register(registry: &mut ToolRegistry) {
    registry.register_or_replace(Box::new(MediaNowPlaying::from_env()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // ── fixtures ───────────────────────────────────────────────────────────
    //
    // Every title, username and device name below is INVENTED. Nothing here
    // came off the live library that was probed to learn the payload shape.

    /// A transcoding EPISODE, shaped exactly as a live server returns it:
    /// `grandparentTitle`/`parentIndex`/`index` nesting, a `TranscodeSession`
    /// with per-stream decisions, `Session.bandwidth`, `User`, `Player`.
    fn transcoding_episode() -> Value {
        json!({
            "type": "episode",
            "sessionKey": "101",
            "title": "The Placeholder Episode", // pii-test-fixture: invented title, not from the live library
            "grandparentTitle": "Placeholder Show", // pii-test-fixture: invented title, not from the live library
            "parentIndex": 4,
            "index": 12,
            "year": 1996,
            "duration": 2_400_000,
            "viewOffset": 600_000,
            "librarySectionTitle": "TV Shows",
            "Media": [{ "container": "mkv", "videoResolution": "1080p",
                        "Part": [{ "decision": "transcode" }] }],
            "User": { "id": "42", "title": "household-member-a" }, // pii-test-fixture: invented user, not a real household account
            "Player": { "title": "Placeholder Player", "product": "Plex for Placeholder", // pii-test-fixture: invented device name
                        "platform": "Android", "state": "playing", "local": false },
            "Session": { "id": "sess-a", "bandwidth": 8906, "location": "wan" },
            "TranscodeSession": {
                "throttled": true, "complete": false, "progress": 36.5,
                "videoDecision": "copy", "audioDecision": "transcode",
                "subtitleDecision": "transcode",
                "sourceVideoCodec": "h264", "sourceAudioCodec": "eac3",
                "videoCodec": "h264", "audioCodec": "opus",
                "protocol": "hls", "transcodeHwRequested": true
            }
        })
    }

    /// A direct-play MOVIE: no `TranscodeSession` at all, `Part.decision` is
    /// `directplay`, and the title/year live at the top level.
    fn direct_play_movie() -> Value {
        json!({
            "type": "movie",
            "sessionKey": "102",
            "title": "A Placeholder Motion Picture", // pii-test-fixture: invented title, not from the live library
            "year": 2025,
            "duration": 6_000_000,
            "viewOffset": 3_000_000,
            "librarySectionTitle": "Movies",
            "Media": [{ "container": "mkv", "videoResolution": "2160p",
                        "Part": [{ "decision": "directplay" }] }],
            "User": { "id": 7, "title": "household-member-b" }, // pii-test-fixture: invented user, not a real household account
            "Player": { "title": "Placeholder TV", "product": "Plex for Placeholder TV", // pii-test-fixture: invented device name
                        "platform": "tvOS", "state": "paused", "local": true },
            "Session": { "id": "sess-b", "bandwidth": 40_000, "location": "lan" }
        })
    }

    /// Direct STREAM: a `TranscodeSession` exists but nothing is re-encoded.
    fn direct_stream_movie() -> Value {
        json!({
            "type": "movie",
            "sessionKey": "103",
            "title": "Another Placeholder Picture", // pii-test-fixture: invented title, not from the live library
            "duration": 5_000_000,
            "viewOffset": 0,
            "User": { "id": "9", "title": "household-member-c" }, // pii-test-fixture: invented user, not a real household account
            "Player": { "title": "Placeholder Browser", "state": "buffering" }, // pii-test-fixture: invented device name
            "TranscodeSession": {
                "videoDecision": "copy", "audioDecision": "copy",
                "protocol": "dash", "progress": 4.0
            }
        })
    }

    fn container(items: Vec<Value>) -> Value {
        json!({ "MediaContainer": { "size": items.len(), "Metadata": items } })
    }

    fn identity_payload() -> Value {
        json!({ "MediaContainer": {
            "machineIdentifier": "MACHINE_ID_PLACEHOLDER",
            "version": "1.42.0.0", "apiVersion": "1.1.1" } })
    }

    /// A source that COUNTS how many times it was asked. The entitlement test
    /// asserts this stays at zero — a gate that returned an empty result after
    /// fetching would still have leaked timing/load, and would pass a weaker
    /// assertion.
    struct CountingSource {
        sessions: AtomicUsize,
        identity: AtomicUsize,
        payload: Result<Value, PlexSessionsError>,
    }

    impl CountingSource {
        fn ok(payload: Value) -> Arc<Self> {
            Arc::new(Self {
                sessions: AtomicUsize::new(0),
                identity: AtomicUsize::new(0),
                payload: Ok(payload),
            })
        }
        fn failing(err: PlexSessionsError) -> Arc<Self> {
            Arc::new(Self {
                sessions: AtomicUsize::new(0),
                identity: AtomicUsize::new(0),
                payload: Err(err),
            })
        }
        fn calls(&self) -> usize {
            self.sessions.load(Ordering::SeqCst) + self.identity.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl SessionSource for CountingSource {
        async fn sessions(&self) -> Result<Value, PlexSessionsError> {
            self.sessions.fetch_add(1, Ordering::SeqCst);
            self.payload.clone()
        }
        async fn identity(&self) -> Result<Value, PlexSessionsError> {
            self.identity.fetch_add(1, Ordering::SeqCst);
            Ok(identity_payload())
        }
    }

    /// An entitled caller. Uses the `cfg(test)`-only constructor rather than
    /// standing up a gateway; the PRODUCTION boundary (only the gateway can
    /// mint one) is proven by the `compile_fail` doctest on `CallerContext`.
    fn entitled() -> CallerContext {
        CallerContext::entitled_for_test_only(true, true)
    }

    // ── decision parsing ───────────────────────────────────────────────────

    #[test]
    fn transcode_and_direct_play_are_both_parsed_from_one_container() {
        let raw = container(vec![transcoding_episode(), direct_play_movie()]);
        let parsed: Vec<Value> = session_items(&raw)
            .expect("a well-formed container parses")
            .into_iter()
            .map(session_json)
            .collect();
        assert_eq!(parsed.len(), 2);

        assert_eq!(parsed[0]["decision"], "transcode");
        assert_eq!(parsed[0]["video_decision"], "copy");
        assert_eq!(parsed[0]["audio_decision"], "transcode");
        let reason = parsed[0]["transcode_reason"].as_str().expect("a transcode has a reason");
        assert!(reason.contains("audio eac3 -> opus"), "reason was {reason}");
        assert!(reason.contains("subtitles"), "reason was {reason}");

        assert_eq!(parsed[1]["decision"], "direct_play");
        assert_eq!(parsed[1]["transcode_reason"], Value::Null);
    }

    #[test]
    fn plexs_own_transcode_reason_wins_over_the_derived_one() {
        let mut item = transcoding_episode();
        item["TranscodeSession"]["transcodeReason"] =
            json!("Conversion of the source audio is required");
        let parsed = session_json(&item);
        assert_eq!(parsed["transcode_reason"], "Conversion of the source audio is required");
    }

    #[test]
    fn a_part_decision_without_a_transcode_session_is_still_believed() {
        // Defensive: absence of TranscodeSession normally means direct play,
        // but Plex states the decision on the Part too. If the two ever
        // disagree, an explicit "transcode" must not be flattened to
        // "direct play" — the optimistic answer is the misleading one.
        let mut item = direct_play_movie();
        item["Media"][0]["Part"][0]["decision"] = json!("transcode");
        assert_eq!(session_json(&item)["decision"], "transcode");

        item["Media"][0]["Part"][0]["decision"] = json!("copy");
        assert_eq!(session_json(&item)["decision"], "direct_stream");

        item["Media"][0]["Part"][0]["decision"] = json!("directplay");
        assert_eq!(session_json(&item)["decision"], "direct_play");
    }

    #[test]
    fn copy_on_both_streams_is_direct_stream_not_transcode() {
        let parsed = session_json(&direct_stream_movie());
        assert_eq!(parsed["decision"], "direct_stream");
        // The live server's session had videoDecision=copy WITH
        // audioDecision=transcode; that must stay a transcode.
        assert_eq!(session_json(&transcoding_episode())["decision"], "transcode");
    }

    #[test]
    fn direct_stream_without_a_transcode_session_has_no_reason_and_that_is_the_contract() {
        // Plex states `copy` on the Part but supplies no TranscodeSession, so
        // it has told us THAT it is remuxing and nothing about why or into
        // what. The contract permits null here; nothing is invented.
        let mut item = direct_play_movie();
        item["Media"][0]["Part"][0]["decision"] = json!("copy");
        let parsed = session_json(&item);
        assert_eq!(parsed["decision"], "direct_stream");
        assert_eq!(parsed["transcode_reason"], Value::Null);

        // Same for the transcode-by-Part case: no session object, no reason.
        item["Media"][0]["Part"][0]["decision"] = json!("transcode");
        let parsed = session_json(&item);
        assert_eq!(parsed["decision"], "transcode");
        assert_eq!(parsed["transcode_reason"], Value::Null);

        // And the contract stated positively: a reason appears exactly when a
        // TranscodeSession is present and the decision is not direct play.
        for (item, expect_reason) in [
            (transcoding_episode(), true),   // transcode, has TranscodeSession
            (direct_stream_movie(), true),   // direct_stream, has TranscodeSession
            (direct_play_movie(), false),    // direct_play
        ] {
            let has_ts = item.get("TranscodeSession").is_some();
            let p = session_json(&item);
            let non_direct_play = p["decision"] != "direct_play";
            assert_eq!(
                p["transcode_reason"].is_string(),
                has_ts && non_direct_play,
                "reason presence must follow the stated iff, payload was {p}"
            );
            assert_eq!(p["transcode_reason"].is_string(), expect_reason);
        }
    }

    // ── malformed vs idle: a broken read must never read as an empty house ──

    #[test]
    fn structurally_invalid_payloads_are_malformed_and_never_empty() {
        // Each of these previously produced an empty Vec, i.e. "idle".
        for (label, raw) in [
            ("an empty object", json!({})),
            ("no MediaContainer", json!({ "size": 0, "Metadata": [] })),
            ("a non-object response", json!("nothing is playing")),
            ("a null response", Value::Null),
            ("a MediaContainer that is not an object", json!({ "MediaContainer": 0 })),
            (
                "size nonzero with no Metadata",
                json!({ "MediaContainer": { "size": 2 } }),
            ),
            (
                "neither size nor Metadata",
                json!({ "MediaContainer": { "identifier": "com.plexapp.plugins.library" } }),
            ),
            (
                "Metadata that is not a list",
                json!({ "MediaContainer": { "size": 1, "Metadata": { "title": "x" } } }),
            ),
        ] {
            let err = session_items(&raw)
                .expect_err(&format!("{label} must not parse as a session list"));
            assert!(!err.is_empty(), "{label}: malformed needs a detail");
        }
    }

    #[test]
    fn the_live_servers_empty_shape_is_idle_not_malformed() {
        // POSITIVE CONTROL, and the asymmetry that matters: calling a genuine
        // idle response malformed would be its own false alarm. This is the
        // exact body the live server returns at rest (Plex 1.42.2): size is an
        // integer 0 and the Metadata key is absent entirely.
        let raw = json!({ "MediaContainer": { "size": 0 } });
        assert_eq!(session_items(&raw).expect("idle is a legitimate answer").len(), 0);

        // An explicitly empty list, and the stringly-typed size some Plex
        // endpoints use, are idle too.
        assert!(session_items(&json!({ "MediaContainer": { "size": 0, "Metadata": [] } }))
            .expect("an empty list is idle")
            .is_empty());
        assert!(session_items(&json!({ "MediaContainer": { "size": "0" } }))
            .expect("a stringly-typed zero is still zero")
            .is_empty());
    }

    #[tokio::test]
    async fn an_explicitly_null_metadata_is_malformed_while_absent_and_empty_stay_idle() {
        // DECISION, on evidence rather than on which option is more defensive:
        // an explicit `"Metadata": null` is MALFORMED at every size, including
        // 0. Plex's serializer omits keys instead of nulling them — re-probed
        // read-only against the live server across /status/sessions,
        // /identity, /library/sections, /library/onDeck and
        // /status/sessions/history/all, none of which contains a single JSON
        // null — and the client is a plain from_str of the body, so it cannot
        // introduce one. A null means the response was rewritten in transit,
        // which makes the size beside it no more trustworthy than the key it
        // replaced. Without this, `{"size":0,"Metadata":null}` rendered as
        // idle: the malformed/idle collapse this module exists to prevent.
        for (label, raw) in [
            ("null Metadata at size 0", json!({ "MediaContainer": { "size": 0, "Metadata": null } })),
            ("null Metadata at size 3", json!({ "MediaContainer": { "size": 3, "Metadata": null } })),
            ("null Metadata with no size", json!({ "MediaContainer": { "Metadata": null } })),
        ] {
            let err = session_items(&raw).expect_err(&format!("{label} must not parse as idle"));
            assert!(err.contains("null"), "{label}: the detail should name it: {err}");
        }

        // POSITIVE CONTROLS — the asymmetry that shaped this module: calling a
        // LEGITIMATE idle response malformed would be its own false alarm.
        // Absent and empty-array must both still be idle at size 0.
        assert!(
            session_items(&json!({ "MediaContainer": { "size": 0 } }))
                .expect("an ABSENT Metadata key at size 0 is the live server's idle shape")
                .is_empty()
        );
        assert!(
            session_items(&json!({ "MediaContainer": { "size": 0, "Metadata": [] } }))
                .expect("an EMPTY Metadata list at size 0 is idle")
                .is_empty()
        );

        // And end to end, the shape Phase 2 renders: a null never reaches the
        // GUI as an empty house, and carries no count to be misread as one.
        let p = MediaNowPlaying::with_source(CountingSource::ok(
            json!({ "MediaContainer": { "size": 0, "Metadata": null } }),
        ))
        .run(entitled())
        .await;
        assert_eq!(p["status"], "malformed");
        assert_eq!(p["ok"], false);
        assert!(p.get("session_count").is_none());
        assert!(p.get("sessions").is_none());
    }

    #[tokio::test]
    async fn an_absent_size_is_malformed_whatever_metadata_does() {
        // DECISION, on evidence rather than defensiveness — re-probed
        // READ-ONLY against the live server for this review round. `size` was
        // present, and a JSON integer, on every endpoint checked:
        // /status/sessions (exactly {"MediaContainer":{"size":0}} at rest),
        // /transcode/sessions, /clients, /identity, /library/sections,
        // /library/onDeck, /library/recentlyAdded, /status/sessions/history/all
        // — and not one JSON null anywhere in any of them. Plex states a count
        // on every container it emits, including containers carrying nothing
        // else, so an ABSENT size is not a legitimate state to tolerate: it
        // means the response was altered, and the fields still standing next to
        // it are no more trustworthy than the one that vanished.
        //
        // The empty-list case is the one that used to slip through: with no
        // size there was no count to cross-check against, so `{"Metadata": []}`
        // rendered as a confident "nobody is watching".
        for (label, raw) in [
            ("no size, no Metadata", json!({ "MediaContainer": {} })),
            (
                "no size, empty Metadata",
                json!({ "MediaContainer": { "Metadata": [] } }),
            ),
            (
                "no size, populated Metadata",
                json!({ "MediaContainer": { "Metadata": [transcoding_episode()] } }),
            ),
            (
                "no size, other keys present",
                json!({ "MediaContainer": { "identifier": "com.plexapp.plugins.library" } }),
            ),
        ] {
            let err = session_items(&raw).expect_err(&format!("{label} must not parse as idle"));
            assert!(err.contains("no size"), "{label}: the detail should name it: {err}");
        }

        // End to end: an absent size never reaches the GUI as an empty house,
        // and carries no count that could be misread as one.
        let p = MediaNowPlaying::with_source(CountingSource::ok(
            json!({ "MediaContainer": { "Metadata": [] } }),
        ))
        .run(entitled())
        .await;
        assert_eq!(p["status"], "malformed");
        assert_eq!(p["ok"], false);
        assert!(p.get("session_count").is_none());
        assert!(p.get("sessions").is_none());

        // POSITIVE CONTROL — the live at-rest body is still idle. A rule that
        // rejected a genuine idle response would be its own false alarm.
        assert!(session_items(&json!({ "MediaContainer": { "size": 0 } }))
            .expect("the live server's at-rest body is idle")
            .is_empty());
    }

    #[tokio::test]
    async fn a_size_that_is_not_a_whole_non_negative_count_is_malformed_never_floored() {
        // A collection size counts things. `0.5` is not an imprecise count but
        // an impossible one, and the old `as i64` fallback TRUNCATED it to 0 —
        // rendering a house full of viewers as `idle`, the exact collapse this
        // module exists to prevent. Negative and non-numeric are the same class.
        for (label, raw) in [
            ("fractional zero", json!({ "MediaContainer": { "size": 0.5 } })),
            (
                "fractional zero with an empty list",
                json!({ "MediaContainer": { "size": 0.5, "Metadata": [] } }),
            ),
            (
                "fractional count with sessions",
                json!({ "MediaContainer": { "size": 1.5, "Metadata": [transcoding_episode()] } }),
            ),
            ("negative", json!({ "MediaContainer": { "size": -1 } })),
            (
                "negative with an empty list",
                json!({ "MediaContainer": { "size": -1, "Metadata": [] } }),
            ),
            ("non-numeric string", json!({ "MediaContainer": { "size": "none" } })),
            ("fractional string", json!({ "MediaContainer": { "size": "0.5" } })),
            ("boolean", json!({ "MediaContainer": { "size": false } })),
            ("a nested object", json!({ "MediaContainer": { "size": { "n": 0 } } })),
        ] {
            let err = session_items(&raw).expect_err(&format!("{label} must not parse as a list"));
            assert!(!err.is_empty(), "{label}: malformed needs a detail");
            assert!(
                !err.contains("no size"),
                "{label}: it HAS a size, it is just not a count: {err}"
            );
        }

        // The NEGATIVE guard specifically. Recorded honestly: it has no unique
        // kill on the VERDICT — mutating it away leaves every negative case
        // still malformed, because `-1` also fails the size-vs-length
        // cross-check (`-1 != 0`) and the "nonzero size with no Metadata"
        // branch. What it uniquely contributes is the DIAGNOSIS, and that is
        // worth keeping: without it an operator reads "reported -1 session(s)
        // but carried 0", which describes the symptom as a miscount rather
        // than naming the impossible value. So the assertion is on the detail,
        // which is the thing the guard actually decides.
        for raw in [
            json!({ "MediaContainer": { "size": -1 } }),
            json!({ "MediaContainer": { "size": -1, "Metadata": [] } }),
            json!({ "MediaContainer": { "size": "-3" } }),
        ] {
            let err = session_items(&raw).expect_err("a negative size is not a count");
            assert!(
                err.contains("cannot be negative"),
                "a negative count should be named as one, not reported as a miscount: {err}"
            );
        }

        // Nothing is floored on the way to the GUI either.
        let p = MediaNowPlaying::with_source(CountingSource::ok(
            json!({ "MediaContainer": { "size": 0.5 } }),
        ))
        .run(entitled())
        .await;
        assert_eq!(p["status"], "malformed");
        assert!(p.get("session_count").is_none());

        // POSITIVE CONTROLS. An integral float IS the integer it spells, and
        // the stringly-typed form other Plex endpoints use still reads.
        assert!(session_items(&json!({ "MediaContainer": { "size": 0.0 } }))
            .expect("0.0 is the whole number zero")
            .is_empty());
        assert!(session_items(&json!({ "MediaContainer": { "size": "0" } }))
            .expect("a stringly-typed zero is still zero")
            .is_empty());
        assert_eq!(
            session_items(&container(vec![transcoding_episode(), direct_play_movie()]))
                .expect("a normal populated response parses")
                .len(),
            2,
            "a populated container must still count its sessions"
        );
    }

    #[test]
    fn ordinals_reject_a_fraction_while_measurements_round_it() {
        // The per-field split, asserted. A COUNT/ORDINAL that is fractional is
        // dropped rather than truncated, so nothing is claimed that the payload
        // did not state; a MEASUREMENT is rounded, because a half-millisecond
        // is not worth blanking a progress bar.
        let mut item = transcoding_episode();
        item["parentIndex"] = json!(4.7);
        item["index"] = json!(12.3);
        item["year"] = json!(1996.5);
        let p = session_json(&item);
        assert_eq!(p["season"], Value::Null, "a fractional season is not season 4");
        assert_eq!(p["episode"], Value::Null, "a fractional episode is not episode 12");
        assert_eq!(p["year"], Value::Null);
        assert_eq!(
            p["full_title"], "Placeholder Show - The Placeholder Episode",
            "no SxxEyy may be invented from a fraction"
        );

        // Measurements: rounded to the nearest whole unit, never dropped.
        let mut item = transcoding_episode();
        item["duration"] = json!(2_400_000.4);
        item["viewOffset"] = json!(599_999.6);
        item["Session"]["bandwidth"] = json!(8905.5);
        let p = session_json(&item);
        assert_eq!(p["duration_ms"], 2_400_000);
        assert_eq!(p["progress_ms"], 600_000);
        assert_eq!(p["bandwidth_kbps"], 8906);
        assert_eq!(p["progress_percent"], 25.0);

        // And an integral float is still just the integer, on both sides.
        let mut item = transcoding_episode();
        item["index"] = json!(12.0);
        item["duration"] = json!(2_400_000.0);
        let p = session_json(&item);
        assert_eq!(p["episode"], 12);
        assert_eq!(p["duration_ms"], 2_400_000);
    }

    #[test]
    fn a_count_that_disagrees_with_the_list_is_malformed_never_an_undercount() {
        let mut raw = container(vec![transcoding_episode()]);
        raw["MediaContainer"]["size"] = json!(3);
        assert!(
            session_items(&raw).is_err(),
            "reporting 1 of 3 sessions would be a quiet lie about who is watching"
        );
    }

    #[test]
    fn an_unparseable_entry_fails_the_whole_response_rather_than_shortening_it() {
        // DECISION (see `session_items`): the whole response is malformed. The
        // entry cannot be dropped, because dropping it returns a list short by
        // one WITH a matching count — an undercount that reads as authoritative.
        let raw = json!({ "MediaContainer": { "size": 2, "Metadata": [
            transcoding_episode(),
            "not a session object"
        ] } });
        let err = session_items(&raw).expect_err("a non-object entry is not readable");
        assert!(err.contains("entry 1"), "the detail should locate it: {err}");
    }

    /// Build a one-session container whose `TranscodeSession` is `ts`, keeping
    /// `size` honest so nothing but the transcode block can be the fault.
    fn one_session_with_transcode_session(ts: Value) -> Value {
        let mut item = transcoding_episode();
        item["TranscodeSession"] = ts;
        container(vec![item])
    }

    #[tokio::test]
    async fn a_non_object_transcode_session_is_malformed_never_a_confident_decision() {
        // DECISION (see `session_items`): REJECT, whole response, consistent
        // with the `Metadata: null` precedent and with the unparseable-entry
        // scope already settled above. TranscodeSession is the only nested
        // block a playback decision is derived from, so a rewritten one is
        // exactly where a confident-but-wrong decision comes from: before this,
        // `"TranscodeSession": null` rendered as `direct_stream` with the
        // manufactured reason `remuxed to a different container`.
        for (label, ts) in [
            ("null", Value::Null),
            ("a number", json!(0)),
            ("a string", json!("transcoding")),
            ("a list", json!([{ "videoDecision": "copy" }])),
            ("a boolean", json!(false)),
        ] {
            let raw = one_session_with_transcode_session(ts);
            let err = session_items(&raw).expect_err(&format!(
                "a TranscodeSession that is {label} must not parse as a session list"
            ));
            assert!(
                err.contains("TranscodeSession"),
                "{label}: the detail should name the block: {err}"
            );
            assert!(err.contains("entry 0"), "{label}: the detail should locate it: {err}");

            // End to end, the shape Phase 2 renders: malformed, no count, no
            // sessions — never a decision, and never an empty house either.
            let p = MediaNowPlaying::with_source(CountingSource::ok(raw))
                .run(entitled())
                .await;
            assert_eq!(p["status"], "malformed", "{label}");
            assert_eq!(p["ok"], false, "{label}");
            assert!(p.get("session_count").is_none(), "{label}");
            assert!(p.get("sessions").is_none(), "{label}");
            assert!(
                !p.to_string().contains("remux"),
                "{label}: a rewritten transcode block must never yield a reason: {p}"
            );
        }

        // POSITIVE CONTROL 1 — a REAL TranscodeSession object still decides and
        // still explains itself. The guard must reject only what is not an
        // object.
        let p = MediaNowPlaying::with_source(CountingSource::ok(container(vec![
            transcoding_episode(),
            direct_stream_movie(),
        ])))
        .run(entitled())
        .await;
        assert_eq!(p["status"], "playing");
        assert_eq!(p["session_count"], 2);
        assert_eq!(p["sessions"][0]["decision"], "transcode");
        assert!(p["sessions"][0]["transcode_reason"]
            .as_str()
            .expect("a real transcode session still explains itself")
            .contains("audio eac3 -> opus"));
        assert_eq!(p["sessions"][1]["decision"], "direct_stream");
        assert_eq!(p["sessions"][1]["transcode_reason"], "remuxed to dash");

        // POSITIVE CONTROL 2 — an ABSENT TranscodeSession is still direct play
        // with a null reason, exactly as before. Absence is not the fault; a
        // rewritten value is.
        let p = MediaNowPlaying::with_source(CountingSource::ok(container(vec![
            direct_play_movie(),
        ])))
        .run(entitled())
        .await;
        assert_eq!(p["status"], "playing");
        assert_eq!(p["sessions"][0]["decision"], "direct_play");
        assert_eq!(p["sessions"][0]["transcode_reason"], Value::Null);
    }

    #[test]
    fn a_non_object_transcode_session_never_derives_a_decision_if_it_reaches_the_derivation() {
        // The invariant held BY CONSTRUCTION, not merely by upstream ordering:
        // `decide` and `transcode_reason` are called here directly, bypassing
        // the `session_items` rejection that would normally have failed the
        // response first. `decide` must treat a non-object as ABSENT and fall
        // to `Part.decision` (the payload says `transcode`), rather than
        // reading its default `copy`/`copy` off a null and calling it a direct
        // stream.
        for ts in [Value::Null, json!(0), json!("x"), json!([])] {
            let mut item = transcoding_episode();
            item["TranscodeSession"] = ts.clone();
            assert_eq!(
                decide(&item),
                PlaybackDecision::Transcode,
                "a {ts} TranscodeSession must not be read as copy/copy"
            );
            assert_eq!(
                session_json(&item)["decision"],
                "transcode",
                "payload decision for a {ts} TranscodeSession"
            );
        }
    }

    #[test]
    fn a_non_object_transcode_session_never_yields_a_reason_if_it_reaches_the_derivation() {
        // Second half of the same by-construction guarantee, and the exact
        // regression: a null TranscodeSession fell through every branch of
        // `transcode_reason` to the direct-stream fallback and manufactured
        // `remuxed to a different container` — a non-null reason from a session
        // that carried no session object, violating the documented iff.
        for ts in [Value::Null, json!(0), json!("x"), json!([])] {
            let mut item = transcoding_episode();
            item["TranscodeSession"] = ts.clone();
            for decision in [
                PlaybackDecision::Transcode,
                PlaybackDecision::DirectStream,
                PlaybackDecision::DirectPlay,
            ] {
                assert_eq!(
                    transcode_reason(&item, decision),
                    None,
                    "a {ts} TranscodeSession must explain nothing, at {}",
                    decision.as_str()
                );
            }
            assert_eq!(
                session_json(&item)["transcode_reason"],
                Value::Null,
                "payload reason for a {ts} TranscodeSession"
            );
        }

        // POSITIVE CONTROL — the derivation itself is untouched for a real
        // object, including the `remuxed to …` fallback the null was stealing.
        assert_eq!(
            transcode_reason(&direct_stream_movie(), PlaybackDecision::DirectStream).as_deref(),
            Some("remuxed to dash")
        );
    }

    #[tokio::test]
    async fn malformed_and_idle_do_not_render_identically_end_to_end() {
        let idle = MediaNowPlaying::with_source(CountingSource::ok(json!({
            "MediaContainer": { "size": 0 }
        })))
        .run(entitled())
        .await;

        for broken in [
            json!({}),
            json!({ "MediaContainer": { "size": 2 } }),
            json!({ "size": 0 }),
        ] {
            let p = MediaNowPlaying::with_source(CountingSource::ok(broken.clone()))
                .run(entitled())
                .await;

            assert_eq!(p["status"], "malformed", "payload was {broken}");
            assert_eq!(p["ok"], false, "payload was {broken}");
            assert_ne!(p["status"], idle["status"]);
            assert_ne!(p["message"], idle["message"]);
            assert_ne!(p.to_string(), idle.to_string());

            // The same invariant the transport failures carry: a read we could
            // not complete reports NO count, so it can never be misread as an
            // empty house.
            assert!(p.get("session_count").is_none(), "payload was {broken}");
            assert!(p.get("sessions").is_none(), "payload was {broken}");
            assert!(p["detail"].is_string(), "payload was {broken}");
        }

        assert_eq!(idle["status"], "idle");
        assert_eq!(idle["session_count"], 0);
    }

    // ── episode vs movie shapes ────────────────────────────────────────────

    #[test]
    fn episode_and_movie_titles_both_render_from_their_own_nesting() {
        let ep = session_json(&transcoding_episode());
        assert_eq!(ep["media_type"], "episode");
        assert_eq!(ep["show_title"], "Placeholder Show");
        assert_eq!(ep["season"], 4);
        assert_eq!(ep["episode"], 12);
        assert_eq!(ep["full_title"], "Placeholder Show - S04E12 - The Placeholder Episode");

        let mv = session_json(&direct_play_movie());
        assert_eq!(mv["media_type"], "movie");
        assert_eq!(mv["show_title"], Value::Null);
        assert_eq!(mv["season"], Value::Null);
        assert_eq!(mv["full_title"], "A Placeholder Motion Picture (2025)");
    }

    #[test]
    fn progress_user_and_player_are_carried_through() {
        let ep = session_json(&transcoding_episode());
        assert_eq!(ep["progress_ms"], 600_000);
        assert_eq!(ep["duration_ms"], 2_400_000);
        assert_eq!(ep["progress_percent"], 25.0);
        assert_eq!(ep["user"], "household-member-a");
        assert_eq!(ep["user_id"], "42");
        assert_eq!(ep["player"], "Placeholder Player");
        assert_eq!(ep["player_state"], "playing");
        assert_eq!(ep["bandwidth_kbps"], 8906);
        assert_eq!(ep["stream_location"], "wan");

        // Plex sends User.id as a bare number in some payloads and a string in
        // others; both must land as the same contract type.
        assert_eq!(session_json(&direct_play_movie())["user_id"], "7");
    }

    #[test]
    fn a_household_ip_address_is_never_carried_into_the_payload() {
        let mut item = transcoding_episode();
        item["Player"]["address"] = json!("<internal-ip>"); // pii-test-fixture: a synthetic RFC1918 address, present so the test can prove it is DROPPED
        item["Player"]["remotePublicAddress"] = json!("203.0.113.9");
        let rendered = session_json(&item).to_string();
        assert!(!rendered.contains("<internal-ip>")); // pii-test-fixture: same synthetic address, asserted absent from the payload
        assert!(!rendered.contains("203.0.113.9"));
    }

    // ── the three outcomes, end to end through the tool ────────────────────

    #[tokio::test]
    async fn entitled_caller_gets_full_session_detail() {
        // POSITIVE CONTROL. A version that silently returns nothing for
        // everybody cannot pass this.
        let src = CountingSource::ok(container(vec![transcoding_episode()]));
        let tool = MediaNowPlaying::with_source(src.clone());

        let out = tool.execute_with_caller(json!({}), entitled()).await.unwrap();
        let p = out.structured.expect("entitled callers get structured detail");

        assert_eq!(p["status"], "playing");
        assert_eq!(p["ok"], true);
        assert_eq!(p["session_count"], 1);
        assert_eq!(p["total_bandwidth_kbps"], 8906);
        assert_eq!(p["sessions"][0]["full_title"], "Placeholder Show - S04E12 - The Placeholder Episode");
        assert_eq!(p["sessions"][0]["user"], "household-member-a");
        assert_eq!(p["sessions"][0]["player"], "Placeholder Player");
        assert_eq!(p["sessions"][0]["decision"], "transcode");
        assert!(p["sessions"][0]["transcode_reason"].is_string());
        assert_eq!(p["server"]["version"], "1.42.0.0");
        assert!(!out.text.is_empty());
    }

    #[tokio::test]
    async fn zero_sessions_is_idle_and_is_not_a_failure() {
        let src = CountingSource::ok(json!({ "MediaContainer": { "size": 0 } }));
        let tool = MediaNowPlaying::with_source(src);

        let p = tool
            .execute_with_caller(json!({}), entitled())
            .await
            .unwrap()
            .structured
            .unwrap();

        assert_eq!(p["status"], "idle");
        assert_eq!(p["ok"], true);
        assert_eq!(p["session_count"], 0);
        assert_eq!(p["sessions"], json!([]));
        assert_eq!(p["total_bandwidth_kbps"], Value::Null);
    }

    #[tokio::test]
    async fn unreachable_token_rejected_and_idle_never_render_identically() {
        let idle = MediaNowPlaying::with_source(CountingSource::ok(json!({
            "MediaContainer": { "size": 0 }
        })))
        .run(entitled())
        .await;
        let down = MediaNowPlaying::with_source(CountingSource::failing(
            PlexSessionsError::Unreachable("could not connect to Plex".into()),
        ))
        .run(entitled())
        .await;
        let refused = MediaNowPlaying::with_source(CountingSource::failing(
            PlexSessionsError::TokenRejected("Plex rejected the configured token (HTTP 401)".into()),
        ))
        .run(entitled())
        .await;

        assert_eq!(idle["status"], "idle");
        assert_eq!(down["status"], "unreachable");
        assert_eq!(refused["status"], "unauthorized");

        // Distinct on every axis a consumer could key on.
        for (a, b) in [(&idle, &down), (&idle, &refused), (&down, &refused)] {
            assert_ne!(a["status"], b["status"]);
            assert_ne!(a["message"], b["message"]);
            assert_ne!(a.to_string(), b.to_string());
        }

        // A failed read must NEVER be mistakable for an empty house.
        assert_eq!(down["ok"], false);
        assert_eq!(refused["ok"], false);
        assert!(down.get("session_count").is_none());
        assert!(refused.get("session_count").is_none());
        assert!(down.get("sessions").is_none());
        assert!(refused.get("sessions").is_none());
    }

    #[tokio::test]
    async fn a_failed_identity_lookup_never_spoils_a_good_session_read() {
        struct IdentityFails(Value);
        #[async_trait]
        impl SessionSource for IdentityFails {
            async fn sessions(&self) -> Result<Value, PlexSessionsError> {
                Ok(self.0.clone())
            }
            async fn identity(&self) -> Result<Value, PlexSessionsError> {
                Err(PlexSessionsError::Unreachable("no answer from Plex".into()))
            }
        }

        let tool = MediaNowPlaying::with_source(Arc::new(IdentityFails(container(vec![
            direct_play_movie(),
        ]))));
        let p = tool.run(entitled()).await;
        assert_eq!(p["status"], "playing");
        assert_eq!(p["server"], Value::Null);
    }

    #[tokio::test]
    async fn not_configured_is_its_own_outcome() {
        let tool = MediaNowPlaying { source: None };
        let p = tool.run(entitled()).await;
        assert_eq!(p["status"], "not_configured");
        assert_eq!(p["ok"], false);
        assert!(p.get("session_count").is_none());
    }

    // ── entitlement ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn unentitled_caller_gets_nothing_and_no_plex_request_is_issued() {
        let cases = [
            ("untrusted", CallerContext::untrusted()),
            ("default", CallerContext::default()),
            ("calendar only", CallerContext::entitled_for_test_only(true, false)),
            ("routine only", CallerContext::entitled_for_test_only(false, true)),
        ];

        for (label, caller) in cases {
            let src = CountingSource::ok(container(vec![transcoding_episode()]));
            let tool = MediaNowPlaying::with_source(src.clone());

            let out = tool.execute_with_caller(json!({}), caller).await.unwrap();
            let p = out.structured.expect("even a refusal is structured");

            assert_eq!(p["status"], "forbidden", "{label}");
            assert_eq!(p["ok"], false, "{label}");
            assert_eq!(src.calls(), 0, "{label}: an unentitled call must not touch Plex");

            // Nothing about the household leaks — not a title, a username, a
            // device name, and not even a count, which alone reveals occupancy.
            let rendered = format!("{}{}", out.text, p);
            for secret in [
                "Placeholder Show",
                "The Placeholder Episode",
                "household-member-a",
                "Placeholder Player",
            ] {
                assert!(!rendered.contains(secret), "{label}: leaked {secret}");
            }
            assert!(p.get("session_count").is_none(), "{label}: leaked a count");
            assert!(p.get("sessions").is_none(), "{label}: leaked sessions");
            assert!(p.get("server").is_none(), "{label}: leaked the server header");
        }
    }

    #[tokio::test]
    async fn the_unthreaded_execute_path_is_unentitled() {
        let src = CountingSource::ok(container(vec![transcoding_episode()]));
        let tool = MediaNowPlaying::with_source(src.clone());

        let text = tool.execute(json!({})).await.unwrap();
        let p: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(p["status"], "forbidden");
        assert_eq!(src.calls(), 0);
    }

    #[test]
    fn the_gate_predicate_requires_both_operator_signals() {
        assert!(may_see_now_playing(CallerContext::entitled_for_test_only(true, true)));
        assert!(!may_see_now_playing(CallerContext::entitled_for_test_only(true, false)));
        assert!(!may_see_now_playing(CallerContext::entitled_for_test_only(false, true)));
        assert!(!may_see_now_playing(CallerContext::untrusted()));
    }

    // ── caching ────────────────────────────────────────────────────────────

    #[test]
    fn the_live_tool_is_never_served_from_cache() {
        assert!(crate::tool_cache::policy_for(TOOL_NAME).is_none());
        assert!(
            crate::tool_cache::is_never_cached(TOOL_NAME),
            "must be caught by the RULE, not merely by having no matching prefix"
        );
    }

    // ── registration ───────────────────────────────────────────────────────

    #[test]
    fn tool_metadata_and_registration() {
        let mut reg = ToolRegistry::new();
        register(&mut reg);
        assert!(reg.contains(TOOL_NAME));

        let tool = MediaNowPlaying::from_env();
        assert_eq!(tool.name(), TOOL_NAME);
        assert!(!tool.description().is_empty());
        assert_eq!(tool.parameters()["type"], "object");
    }
}
