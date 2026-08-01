//! Reading a coder CLI agent's live transcript into a summarised activity
//! stream (AGSS-02).
//!
//! ## Why summarise rather than stream raw
//! A transcript line carries the full text of a message, a tool's entire input,
//! or a command's entire stdout. Handing that to a dashboard would be both
//! useless (a wall of text) and unsafe (arbitrary content, including anything
//! the session happened to print). What an observer actually wants is "what has
//! this agent been *doing*" — so each record collapses to a one-line summary:
//! a tool name plus its primary argument, or a message's first line.
//!
//! ## Three properties that are load-bearing
//!
//! 1. **Bounded read.** Transcripts reach tens of megabytes. Only the last
//!    [`tail_bytes`] are read, and the first (probably partial) line of that
//!    window is discarded. Nothing here ever reads a whole transcript.
//!
//! 2. **Redaction before return.** Every summary and detail passes through the
//!    crate's existing [`DeterministicCleaner`] — the same scrubber the public
//!    mirror uses — because a transcript legitimately contains whatever the
//!    session handled, which can include credentials. Reusing that engine
//!    rather than writing a second one means this path inherits every
//!    hard-won pattern fix it has accumulated.
//!
//! 3. **Defensive parsing.** The transcript is an internal format that may add
//!    fields or change shape between CLI releases. A line that is not JSON is
//!    skipped and counted into `skipped_lines`; a record that IS JSON but whose
//!    shape we do not recognise is skipped and counted into `unknown_records`.
//!    The two are kept apart because they have different causes — a truncated
//!    write versus a CLI format change — and either being non-zero turns a
//!    format drift into a visible number rather than a silently shorter list.
//!    Neither is ever fatal, and neither pads the activity feed with
//!    "unrecognised record" noise.
//!
//! ## What is deliberately NOT surfaced
//! Assistant `thinking` blocks are skipped entirely. They are the model's
//! private reasoning, they carry an opaque signature blob, and an observability
//! surface has no business republishing them.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::ToolError;
use crate::forge::mirror::native_clean::DeterministicCleaner;

use super::exec::HostExecutor;

/// How much of the transcript tail to read. Default 256 KiB — enough for a
/// long recent stretch, small enough that a 50 MB transcript costs nothing.
pub(crate) fn tail_bytes() -> u64 {
    std::env::var("AGENTSESS_TAIL_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(256 * 1024)
}

/// Width a single summary is truncated to. A summary is a glance, not content.
const SUMMARY_WIDTH: usize = 160;
/// Width a detail field is truncated to.
const DETAIL_WIDTH: usize = 400;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    UserMessage,
    AssistantMessage,
    ToolCall,
    ToolResult,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityEvent {
    pub at: Option<DateTime<Utc>>,
    pub kind: EventKind,
    pub summary: String,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TranscriptTail {
    pub events: Vec<ActivityEvent>,
    /// Lines that could not be parsed as JSON at all.
    pub skipped_lines: usize,
    /// Records that parsed as JSON but whose SHAPE we do not recognise.
    ///
    /// Kept separate from `skipped_lines` on purpose: "this is not JSON" and
    /// "this is JSON we do not understand" are different failures with
    /// different causes (a truncated write vs. a CLI format change), and
    /// collapsing them would hide which one is happening. Either being
    /// non-zero is how a format drift becomes a visible number instead of a
    /// silently shorter activity list.
    pub unknown_records: usize,
    /// True when the read started mid-file (i.e. earlier activity exists).
    pub truncated: bool,
    pub path: String,
}

/// Record types that are session bookkeeping, not activity.
///
/// These are UNDERSTOOD and deliberately produce no activity, which is why
/// they are not counted as `unknown_records` — counting a record we recognise
/// as drift would make the drift signal meaningless.
const BOOKKEEPING_TYPES: &[&str] = &[
    "agent-setting",
    "mode",
    "permission-mode",
    "file-history-snapshot",
    "file-history-delta",
    "last-prompt",
    "attachment",
];

fn truncate(s: &str, width: usize) -> String {
    let cleaned = s.replace(['\n', '\r', '\t'], " ");
    let trimmed = cleaned.trim();
    if trimmed.chars().count() <= width {
        return trimmed.to_string();
    }
    let cut: String = trimmed.chars().take(width).collect();
    format!("{cut}…")
}

/// Unquoted `NAME=VALUE` secret assignments.
///
/// The shared [`DeterministicCleaner`] targets SOURCE files, where a secret
/// appears as `field: "value"` — so its field rule requires the value to be
/// QUOTED. A shell transcript produces the unquoted form (for example
/// an exported token, a database password on a command line), which that rule
/// cannot match. This pattern covers exactly that gap and nothing else.
///
/// It follows the same discipline the cleaner's module doc lays out, because
/// those rules were learned by breaking them: the value class is TOKEN-BOUNDED
/// (`[A-Za-z0-9._+/\-]`, never `\S+`, which once ate a closing quote and
/// corrupted a mirror) and contains no newline, so a match can never span a
/// line or swallow the next statement. The `{8,}` floor keeps `TOKEN=` and
/// `TOKEN=$VAR` from matching.
fn env_assign_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        // `_PAT_` / `_PAT` are included because that is THIS fleet's own
        // credential naming convention (`GITEA_PAT_MOOSE`, `PLANE_PAT_CLAUDE`,
        // `GITHUB_PAT_HARMONY`), which none of the generic keywords match.
        // They are underscore-delimited on purpose: a bare `PAT` substring
        // would redact `PATH=` and `PATTERN=`, two of the most common
        // assignments in any shell transcript.
        regex::Regex::new(
            r"(?i)\b([A-Z0-9_]*(?:SECRET|TOKEN|PASSWORD|PASSWD|API[_-]?KEY|ACCESS[_-]?KEY|PRIVATE[_-]?KEY|_PAT_|_PAT\b)[A-Z0-9_]*)[ \t]*=[ \t]*([A-Za-z0-9._+/\-]{8,})",
        )
        .expect("agentsess env-assignment redaction regex")
    })
}

/// Scrub every string that leaves this module.
///
/// Two layers, deliberately in this order:
/// 1. The crate's existing [`DeterministicCleaner`] — reused rather than
///    reimplemented, so this path inherits every pattern fix that engine has
///    accumulated (private IPs, hostnames, JWTs, prefixed API keys, quoted
///    secret fields).
/// 2. [`env_assign_re`] for the unquoted shell-assignment shape layer 1 does
///    not target.
///
/// **This is best-effort defence in depth, not a guarantee.** A transcript can
/// contain arbitrary text, and no pattern set recognises every secret shape.
/// The properties that actually bound exposure here are structural, not
/// pattern-based: summaries are truncated to one short line, tool *results* are
/// summarised rather than echoed in full, and `thinking` blocks are never
/// surfaced at all. Redaction reduces what slips through the remainder; do not
/// treat it as a promise that a transcript is safe to publish.
fn redact(s: &str) -> String {
    let first = DeterministicCleaner::scrub_text(s);
    env_assign_re()
        .replace_all(&first, "$1=<REDACTED-SECRET>")
        .into_owned()
}

/// Pick the argument that best identifies what a tool call was doing.
///
/// Ordered by how much it tells an observer: an explicit path or command beats
/// a pattern, which beats a free-text query. Falling back to "the first string
/// value" keeps an unfamiliar tool informative rather than blank.
fn primary_arg(input: &Value) -> Option<String> {
    const PREFERRED: &[&str] = &[
        "file_path",
        "path",
        "notebook_path",
        "command",
        "pattern",
        "query",
        "url",
        "prompt",
        "description",
    ];
    let obj = input.as_object()?;
    for key in PREFERRED {
        if let Some(v) = obj.get(*key).and_then(Value::as_str) {
            if !v.trim().is_empty() {
                return Some(v.to_string());
            }
        }
    }
    obj.values()
        .find_map(Value::as_str)
        .filter(|v| !v.trim().is_empty())
        .map(str::to_string)
}

fn timestamp_of(rec: &Value) -> Option<DateTime<Utc>> {
    rec.get("timestamp")
        .and_then(Value::as_str)
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc))
}

/// What one transcript record yielded, and whether we understood its shape.
///
/// These two answers MUST come from one place. An earlier version computed
/// them in two parallel functions that both switched on the top-level `type`,
/// which looked equivalent and was not: a record of a KNOWN type whose NESTED
/// shape had drifted (a `user` whose content field was renamed, an `assistant`
/// whose content blocks are all a future type) produced no events and was
/// still counted as understood — silently shortening the feed, which is
/// exactly the failure `unknown_records` exists to make visible.
pub(crate) struct RecordOutcome {
    pub events: Vec<ActivityEvent>,
    /// False when the record's shape — at ANY level — is one we do not
    /// recognise. Counted into `unknown_records`.
    pub understood: bool,
}

/// Convenience for callers that only want the events.
#[cfg(test)]
pub(crate) fn events_from_record(rec: &Value) -> Vec<ActivityEvent> {
    classify_record(rec).events
}

/// Turn one transcript record into zero or more activity events, and say
/// whether its shape was recognised.
///
/// Zero events is a legitimate outcome for an UNDERSTOOD record (bookkeeping,
/// a turn containing only a `thinking` block); it is a drift signal for one we
/// do not understand. The distinction is the whole point of this function.
pub(crate) fn classify_record(rec: &Value) -> RecordOutcome {
    let at = timestamp_of(rec);
    let ty = rec.get("type").and_then(Value::as_str).unwrap_or("");

    if BOOKKEEPING_TYPES.contains(&ty) {
        // Understood and deliberately silent.
        return RecordOutcome { events: Vec::new(), understood: true };
    }

    let mut out = Vec::new();
    let mut understood = true;
    match ty {
        "user" => {
            // A `user` record is either a real human turn or the delivery of a
            // tool's result back to the model. They read very differently to an
            // observer, so they are not conflated. A `user` record carrying
            // NEITHER is a shape we no longer recognise, not an empty turn.
            if rec.get("toolUseResult").is_none()
                && rec.get("message").and_then(|m| m.get("content")).is_none()
            {
                understood = false;
            }
            if let Some(result) = rec.get("toolUseResult") {
                let text = result
                    .get("stdout")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| result.to_string());
                out.push(ActivityEvent {
                    at,
                    kind: EventKind::ToolResult,
                    summary: redact(&truncate(&text, SUMMARY_WIDTH)),
                    detail: None,
                });
            } else if let Some(content) = rec.get("message").and_then(|m| m.get("content")) {
                let text = match content {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                out.push(ActivityEvent {
                    at,
                    kind: EventKind::UserMessage,
                    summary: redact(&truncate(&text, SUMMARY_WIDTH)),
                    detail: None,
                });
            }
        }
        "assistant" => {
            let blocks = rec
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(Value::as_array);
            let Some(blocks) = blocks else {
                // `content` absent or not an array — drifted shape.
                return RecordOutcome { events: out, understood: false };
            };
            // An assistant turn whose blocks are ALL unfamiliar has drifted.
            // An EMPTY block list is legitimately empty, not drift.
            let mut recognised_any = blocks.is_empty();
            for b in blocks {
                match b.get("type").and_then(Value::as_str) {
                    // Private reasoning — never republished (see module doc),
                    // but a RECOGNISED block: its presence is not drift.
                    Some("thinking") => recognised_any = true,
                    Some("text") => {
                        recognised_any = true;
                        if let Some(t) = b.get("text").and_then(Value::as_str) {
                            if !t.trim().is_empty() {
                                out.push(ActivityEvent {
                                    at,
                                    kind: EventKind::AssistantMessage,
                                    summary: redact(&truncate(t, SUMMARY_WIDTH)),
                                    detail: None,
                                });
                            }
                        }
                    }
                    Some("tool_use") => {
                        recognised_any = true;
                        let name = b.get("name").and_then(Value::as_str).unwrap_or("tool");
                        let arg = b.get("input").and_then(primary_arg);
                        let summary = match &arg {
                            Some(a) => format!("{name}: {a}"),
                            None => name.to_string(),
                        };
                        out.push(ActivityEvent {
                            at,
                            kind: EventKind::ToolCall,
                            summary: redact(&truncate(&summary, SUMMARY_WIDTH)),
                            detail: arg
                                .map(|a| redact(&truncate(&a, DETAIL_WIDTH)))
                                .filter(|d| !d.is_empty()),
                        });
                    }
                    _ => {}
                }
            }
            if !recognised_any {
                understood = false;
            }
        }
        other if other.contains("error") => {
            // An error record IS activity an observer wants to see.
            out.push(ActivityEvent {
                at,
                kind: EventKind::Error,
                summary: redact(&truncate(other, SUMMARY_WIDTH)),
                detail: None,
            });
        }
        // Everything else — an unknown `type`, or no `type` at all — is a
        // shape we do not understand. It is NOT surfaced as activity: a feed
        // padded with "unrecognised record" lines is noise that crowds out the
        // real work. The caller learns about it through `unknown_records`
        // instead, which is a number that can be alerted on.
        _ => understood = false,
    }
    RecordOutcome { events: out, understood }
}

/// Parse a raw tail window into events.
///
/// `started_mid_file` tells the parser to drop the first line, which is almost
/// certainly a fragment of a record that began before the window.
pub(crate) fn parse_tail(window: &str, started_mid_file: bool, limit: usize) -> TranscriptTail {
    let mut lines: Vec<&str> = window.lines().collect();
    if started_mid_file && !lines.is_empty() {
        lines.remove(0);
    }

    let mut events = Vec::new();
    let mut skipped = 0usize;
    let mut unknown = 0usize;
    for line in &lines {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(rec) => {
                let outcome = classify_record(&rec);
                if !outcome.understood {
                    unknown += 1;
                }
                events.extend(outcome.events);
            }
            // A truncated final line is normal on a file being appended to.
            Err(_) => skipped += 1,
        }
    }

    // Newest first, then cap. Capping before reversing would return the OLDEST
    // `limit` events, which is the opposite of what "recent activity" means.
    events.reverse();
    events.truncate(limit);

    TranscriptTail {
        events,
        skipped_lines: skipped,
        unknown_records: unknown,
        truncated: started_mid_file,
        path: String::new(),
    }
}

/// Resolve and validate a caller-supplied transcript path.
///
/// Jailed to `root` the same way `crate::dev` jails its workspace roots: `..`
/// is rejected outright and the path must lie under the configured root. A
/// leading-dash path is refused too — `tail` would read it as an option, the
/// same class of hazard as the `find` root in AGSS-01.
pub(crate) fn resolve_transcript_path(root: &str, requested: &str) -> Result<String, ToolError> {
    if requested.contains("..") {
        return Err(ToolError::InvalidArgument(
            "transcript path must not contain '..'".into(),
        ));
    }
    if !requested.starts_with('/') {
        return Err(ToolError::InvalidArgument(
            "transcript path must be absolute".into(),
        ));
    }
    let root_trimmed = root.trim_end_matches('/');
    if !requested.starts_with(&format!("{root_trimmed}/")) {
        return Err(ToolError::InvalidArgument(format!(
            "transcript path must be inside the configured transcript root ({root_trimmed})"
        )));
    }
    Ok(requested.to_string())
}

/// Read the bounded tail of a transcript through `exec`.
pub(crate) async fn read_tail(
    exec: &dyn HostExecutor,
    path: &str,
    limit: usize,
) -> Result<TranscriptTail, ToolError> {
    let bytes = tail_bytes();
    // Request ONE byte more than the window. `len == bytes + 1` then means the
    // file is genuinely larger than the window (so the first line is a
    // fragment and must be dropped); `len <= bytes` means we have the whole
    // file and every line is complete.
    //
    // Reading exactly `bytes` cannot distinguish those two cases: a file of
    // EXACTLY the window size comes back full, and treating "full" as
    // "truncated" silently discards its first complete record. The extra byte
    // is what makes the boundary decidable while keeping the read bounded.
    let request = bytes.saturating_add(1);
    let out = exec
        .run(&["tail", "-c", &request.to_string(), path])
        .await
        .map_err(|e| ToolError::Execution(format!("could not read transcript: {e}")))?;
    if !out.ok() {
        return Err(ToolError::NotFound(format!(
            "transcript unreadable: {}",
            truncate(&out.stderr, 200)
        )));
    }
    let started_mid_file = out.stdout.len() as u64 > bytes;
    let mut tail = parse_tail(&out.stdout, started_mid_file, limit);
    tail.path = path.to_string();
    Ok(tail)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev(rec: Value) -> Vec<ActivityEvent> {
        events_from_record(&rec)
    }

    #[test]
    fn a_human_turn_becomes_a_user_message() {
        let e = ev(json!({
            "type": "user",
            "timestamp": "2026-08-01T05:51:43.357Z",
            "message": {"role": "user", "content": "please fix the build"}
        }));
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].kind, EventKind::UserMessage);
        assert_eq!(e[0].summary, "please fix the build");
        assert!(e[0].at.is_some());
    }

    #[test]
    fn a_tool_result_is_not_conflated_with_a_human_turn() {
        // Both arrive as type=user; only one is a person speaking.
        let e = ev(json!({
            "type": "user",
            "toolUseResult": {"stdout": "Chord\nTerminus\nharmony"}
        }));
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].kind, EventKind::ToolResult);
        assert!(e[0].summary.contains("Terminus"));
    }

    #[test]
    fn a_tool_call_summarises_as_name_plus_primary_arg() {
        let e = ev(json!({
            "type": "assistant",
            "message": {"content": [
                {"type": "tool_use", "name": "Edit",
                 "input": {"file_path": "src/agentsess/mod.rs", "old_string": "a", "new_string": "b"}}
            ]}
        }));
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].kind, EventKind::ToolCall);
        assert_eq!(e[0].summary, "Edit: src/agentsess/mod.rs");
        assert_eq!(e[0].detail.as_deref(), Some("src/agentsess/mod.rs"));
    }

    #[test]
    fn primary_arg_prefers_the_identifying_field_over_an_arbitrary_one() {
        assert_eq!(
            primary_arg(&json!({"description": "d", "command": "ls -la"})).as_deref(),
            Some("ls -la")
        );
        // An unfamiliar tool still yields something rather than nothing.
        assert_eq!(
            primary_arg(&json!({"unknown_field": "value"})).as_deref(),
            Some("value")
        );
        assert_eq!(primary_arg(&json!({})), None);
        // Blank values are not informative and must not win.
        assert_eq!(
            primary_arg(&json!({"file_path": "   ", "command": "real"})).as_deref(),
            Some("real")
        );
    }

    #[test]
    fn thinking_blocks_are_never_surfaced() {
        let e = ev(json!({
            "type": "assistant",
            "message": {"content": [
                {"type": "thinking", "thinking": "private chain of reasoning", "signature": "abc"},
                {"type": "text", "text": "Here is the plan."}
            ]}
        }));
        assert_eq!(e.len(), 1, "only the visible text should survive");
        assert_eq!(e[0].kind, EventKind::AssistantMessage);
        assert!(!e.iter().any(|x| x.summary.contains("private")));
    }

    #[test]
    fn one_assistant_turn_can_yield_several_events() {
        let e = ev(json!({
            "type": "assistant",
            "message": {"content": [
                {"type": "text", "text": "Running two things."},
                {"type": "tool_use", "name": "Bash", "input": {"command": "ls"}},
                {"type": "tool_use", "name": "Read", "input": {"file_path": "/x/y.rs"}}
            ]}
        }));
        assert_eq!(e.len(), 3);
        assert_eq!(e[1].summary, "Bash: ls");
        assert_eq!(e[2].summary, "Read: /x/y.rs");
    }

    #[test]
    fn bookkeeping_and_unknown_records_both_produce_no_activity() {
        for ty in ["agent-setting", "mode", "permission-mode", "file-history-snapshot"] {
            assert!(ev(json!({"type": ty})).is_empty(), "{ty} should be skipped");
        }
        // An unknown shape is NOT surfaced as an activity line — a feed padded
        // with "unrecognised record" crowds out the real work. It is counted
        // instead (see the counting test below).
        assert!(ev(json!({"type": "some-future-record"})).is_empty());
        assert!(ev(json!({"foo": "bar"})).is_empty());
        // An error record IS activity and must still come through.
        let e = ev(json!({"type": "api-error"}));
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].kind, EventKind::Error);
    }

    #[test]
    fn unknown_shapes_are_counted_separately_from_unparseable_lines() {
        // The two failures have different causes — a truncated write vs. a CLI
        // format change — so collapsing them would hide which is happening.
        let window = concat!(
            "{\"type\":\"user\",\"message\":{\"content\":\"hi\"}}\n",
            "{\"type\":\"some-future-record\"}\n",
            "{\"type\":\"mode\"}\n",
            "not json at all\n"
        );
        let t = parse_tail(window, false, 100);
        assert_eq!(t.events.len(), 1, "only the real turn is activity");
        assert_eq!(t.skipped_lines, 1, "the non-JSON line");
        assert_eq!(t.unknown_records, 1, "the unknown shape, NOT the bookkeeping one");
    }

    #[test]
    fn understood_and_unknown_are_decided_in_one_place() {
        let u = |v: serde_json::Value| classify_record(&v).understood;

        // Understood and deliberately silent.
        for ty in ["agent-setting", "mode", "permission-mode", "attachment"] {
            assert!(u(json!({"type": ty})), "{ty} is understood bookkeeping");
        }
        // Understood activity.
        assert!(u(json!({"type": "user", "message": {"content": "hi"}})));
        assert!(u(json!({"type": "user", "toolUseResult": {"stdout": "x"}})));
        assert!(u(json!({"type": "assistant", "message": {"content": [
            {"type": "text", "text": "hello"}]}})));
        // A turn carrying ONLY private reasoning is understood, not drift.
        assert!(u(json!({"type": "assistant", "message": {"content": [
            {"type": "thinking", "thinking": "x"}]}})));
        // An empty block list is legitimately empty.
        assert!(u(json!({"type": "assistant", "message": {"content": []}})));

        // Unknown top-level shapes.
        assert!(!u(json!({"type": "brand-new"})));
        assert!(!u(json!({"no_type": 1})));
    }

    #[test]
    fn nested_drift_inside_a_known_type_is_counted_not_silently_dropped() {
        // The failure this guards: a record of a type we recognise whose INNER
        // shape has changed produces no events. Counting it as understood
        // would silently shorten the feed — precisely what unknown_records
        // exists to expose.
        let u = |v: serde_json::Value| classify_record(&v).understood;

        // `user` carrying neither a message body nor a tool result.
        assert!(!u(json!({"type": "user", "renamed_message": {"content": "hi"}})));
        // `assistant` whose content is not an array.
        assert!(!u(json!({"type": "assistant", "message": {"content": "a string now"}})));
        // `assistant` whose blocks are ALL a future block type.
        assert!(!u(json!({"type": "assistant", "message": {"content": [
            {"type": "future_block", "data": 1}]}})));
        // But a MIX containing one recognised block is still understood.
        assert!(u(json!({"type": "assistant", "message": {"content": [
            {"type": "future_block"}, {"type": "text", "text": "hi"}]}})));
    }

    #[test]
    fn nested_drift_shows_up_in_the_unknown_count() {
        let window = concat!(
            "{\"type\":\"user\",\"message\":{\"content\":\"real\"}}\n",
            "{\"type\":\"user\",\"renamed_message\":{\"content\":\"drifted\"}}\n",
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"future_block\"}]}}\n"
        );
        let t = parse_tail(window, false, 100);
        assert_eq!(t.events.len(), 1, "only the intact record is activity");
        assert_eq!(t.unknown_records, 2, "both drifted records are counted");
        assert_eq!(t.skipped_lines, 0, "they parsed fine as JSON");
    }

    #[test]
    fn malformed_lines_are_skipped_and_counted() {
        let window = "{\"type\":\"user\",\"message\":{\"content\":\"hi\"}}\nnot json at all\n{oops\n";
        let t = parse_tail(window, false, 100);
        assert_eq!(t.events.len(), 1);
        assert_eq!(t.skipped_lines, 2);
    }

    #[test]
    fn an_all_malformed_window_is_distinguishable_from_an_empty_one() {
        let garbage = parse_tail("nope\nalso nope\n", false, 100);
        assert!(garbage.events.is_empty());
        assert_eq!(garbage.skipped_lines, 2, "the caller must be able to tell");

        let empty = parse_tail("", false, 100);
        assert!(empty.events.is_empty());
        assert_eq!(empty.skipped_lines, 0);
    }

    #[test]
    fn a_partial_first_line_is_discarded_only_when_starting_mid_file() {
        let window = "ent\":\"fragment\"}}\n{\"type\":\"user\",\"message\":{\"content\":\"real\"}}\n";
        let mid = parse_tail(window, true, 100);
        assert_eq!(mid.events.len(), 1);
        assert_eq!(mid.skipped_lines, 0, "the fragment was dropped, not counted");
        assert!(mid.truncated);

        // Reading from the very start keeps every line, so the bad one counts.
        let whole = parse_tail(window, false, 100);
        assert_eq!(whole.skipped_lines, 1);
        assert!(!whole.truncated);
    }

    #[test]
    fn events_come_back_newest_first_and_the_limit_keeps_the_newest() {
        let mut w = String::new();
        for i in 0..10 {
            w.push_str(&format!(
                "{{\"type\":\"user\",\"message\":{{\"content\":\"msg{i}\"}}}}\n"
            ));
        }
        let t = parse_tail(&w, false, 3);
        assert_eq!(t.events.len(), 3);
        // Newest first: msg9, msg8, msg7 — NOT msg0..2.
        assert_eq!(t.events[0].summary, "msg9");
        assert_eq!(t.events[2].summary, "msg7");
    }

    #[test]
    fn long_content_is_truncated() {
        let long = "x".repeat(5000);
        let e = ev(json!({"type": "user", "message": {"content": long}}));
        assert!(e[0].summary.chars().count() <= SUMMARY_WIDTH + 1);
        assert!(e[0].summary.ends_with('…'));
    }

    #[test]
    fn newlines_never_break_a_one_line_summary() {
        let e = ev(json!({"type": "user", "message": {"content": "line one\nline two"}}));
        assert!(!e[0].summary.contains('\n'));
        assert_eq!(e[0].summary, "line one line two");
    }

    #[test]
    fn the_shared_cleaner_layer_is_applied() {
        // A prefixed API key is the shared cleaner's territory — asserting it
        // here proves layer 1 is actually wired, not just imported.
        // The literal below is a synthetic, never-valid token. It has to be
        // token-SHAPED or the test proves nothing — this is precisely the
        // "legitimate security-test literal" the fixture tag exists for.
        let raw = "token is <REDACTED-SECRET> in the log"; // pii-test-fixture
        let e = ev(json!({"type": "user", "message": {"content": raw}}));
        assert!(
            !e[0].summary.contains("<REDACTED-SECRET>"), // pii-test-fixture
            "shared cleaner not applied: {}",
            e[0].summary
        );
    }

    #[test]
    fn unquoted_shell_assignments_are_redacted_too() {
        // This is the shape a TRANSCRIPT produces and the shared cleaner does
        // NOT catch (its field rule requires a quoted value). Regression guard
        // for the gap that made this second layer necessary.
        for raw in [
            "export SOME_API_KEY=abcdefghijklmnop0123456789", // pii-test-fixture
            "PGPASSWORD=hunter2hunter2 psql -h db", // pii-test-fixture
            "GITEA_PAT_MOOSE=abcd1234efgh5678",
            "my_secret = averylongsecretvalue",
        ] {
            let e = ev(json!({"type": "user", "message": {"content": raw}}));
            assert!(
                e[0].summary.contains("<REDACTED-SECRET>"),
                "not redacted: {} -> {}",
                raw,
                e[0].summary
            );
        }
    }

    #[test]
    fn redaction_does_not_over_match_ordinary_text() {
        // The name must still be visible (an observer needs to know WHICH
        // variable was set), and non-secret assignments must survive intact.
        let e = ev(json!({"type": "user", "message": {"content": "export API_KEY=abcdefghijklmnop"}})); // pii-test-fixture
        assert!(e[0].summary.contains("API_KEY="), "{}", e[0].summary); // pii-test-fixture

        for benign in [
            "CARGO_TARGET_DIR=/mnt/build-target",
            "TOKEN=",
            "let token_count = 5",
            // The two assignments a bare `PAT` substring would wrongly eat.
            // Every shell transcript contains at least one of them.
            "PATH=/usr/local/bin:/usr/bin:/bin",
            "PATTERN=somelongvaluehere",
            "MY_PATH=/some/long/directory/path",
        ] {
            let e = ev(json!({"type": "user", "message": {"content": benign}}));
            assert!(
                !e[0].summary.contains("<REDACTED-SECRET>"),
                "over-matched benign text: {} -> {}",
                benign,
                e[0].summary
            );
        }
    }

    #[test]
    fn redaction_never_spans_a_line() {
        // The value class excludes newlines, so a match cannot swallow the
        // following line — the failure mode that once corrupted a mirror.
        let two_lines = "TOKEN=\nnext_line_value_here";
        let out = redact(two_lines);
        assert!(out.contains("next_line_value_here"), "{out}");
        assert_eq!(out.lines().count(), two_lines.lines().count());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn a_file_exactly_the_window_size_is_not_treated_as_truncated() {
        // The boundary case: reading exactly `bytes` cannot tell "file is
        // exactly the window" from "file is larger", and mistaking the former
        // for the latter silently discards a complete first record. The reader
        // asks for one byte more so the distinction is decidable.
        use crate::agentsess::exec::test_support::FakeExecutor;
        std::env::set_var("AGENTSESS_TAIL_BYTES", "64");

        // 64 bytes exactly => whole file, first line must survive.
        let first = "{\"type\":\"user\",\"message\":{\"content\":\"keep-me\"}}";
        let mut exact = String::from(first);
        while exact.len() < 64 {
            exact.push(' ');
        }
        assert_eq!(exact.len(), 64);
        let exec = FakeExecutor::new().with_stdout("tail", &exact);
        let t = read_tail(&exec, "/root/x.jsonl", 50).await.unwrap();
        std::env::remove_var("AGENTSESS_TAIL_BYTES");

        assert!(!t.truncated, "a file exactly the window size is not truncated");
        assert_eq!(t.events.len(), 1, "its first complete line must not be dropped");
        assert_eq!(t.events[0].summary, "keep-me");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn a_file_larger_than_the_window_drops_its_fragment_first_line() {
        use crate::agentsess::exec::test_support::FakeExecutor;
        std::env::set_var("AGENTSESS_TAIL_BYTES", "64");
        // More than 64 bytes back => the file is genuinely larger than the
        // window. The fragment is padded to length so the JSON line after it
        // stays COMPLETE — truncating the JSON instead would test the
        // malformed-line path, not the fragment-drop path.
        let mut over = String::from("ragment-of-an-earlier-record-that-was-cut");
        over.push('\n');
        over.push_str("{\"type\":\"user\",\"message\":{\"content\":\"real\"}}");
        assert!(over.len() > 64, "fixture must exceed the window: {}", over.len());
        let exec = FakeExecutor::new().with_stdout("tail", &over);
        let t = read_tail(&exec, "/root/x.jsonl", 50).await.unwrap();
        std::env::remove_var("AGENTSESS_TAIL_BYTES");

        assert!(t.truncated);
        assert_eq!(t.skipped_lines, 0, "the fragment was dropped, not counted");
    }

    #[test]
    fn path_jail_rejects_traversal_and_escapes() {
        let root = "/home/u/.claude/projects";
        assert!(resolve_transcript_path(root, "/home/u/.claude/projects/../../etc/shadow").is_err());
        assert!(resolve_transcript_path(root, "/etc/shadow").is_err());
        assert!(resolve_transcript_path(root, "relative/x.jsonl").is_err());
        // A path that merely shares a prefix is not inside the root.
        assert!(resolve_transcript_path(root, "/home/u/.claude/projects-evil/x.jsonl").is_err());
        assert!(resolve_transcript_path(root, "/home/u/.claude/projects/p/x.jsonl").is_ok());
        // A trailing slash on the root must not change the verdict.
        assert!(resolve_transcript_path("/home/u/.claude/projects/", "/home/u/.claude/projects/p/x.jsonl").is_ok());
    }
}
