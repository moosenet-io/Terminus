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

/// Is this top-level type an error record?
///
/// A substring test is too loose: `"terror"` contains `"error"` and would be
/// emitted as an error event AND counted as understood, hiding a genuinely
/// unknown type. Match on segment boundaries instead.
fn is_error_type(ty: &str) -> bool {
    ty == "error"
        || ty.ends_with("-error")
        || ty.ends_with("_error")
        || ty.starts_with("error-")
        || ty.starts_with("error_")
}

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

/// Redact FIRST, then truncate.
///
/// The order is load-bearing and was originally wrong. Truncating first can
/// cut a secret's value below the redaction pattern's `{8,}` length floor —
/// `TOKEN=abcdefghijklmnop` becomes `TOKEN=abcdefg…`, which no longer matches,
/// so the surviving prefix of a real credential is emitted in clear. Shortening
/// a secret does not make it safe; it makes it invisible to the scrubber.
///
/// Redacting the full string first means the pattern sees the value at its
/// true length, and the placeholder (not the secret) is what gets truncated.
fn redact_then_truncate(s: &str, width: usize) -> String {
    truncate(&redact(s), width)
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
/// Zero events is a legitimate outcome for an UNDERSTOOD record. There are
/// exactly four such deliberate silent cases, and no others:
/// 1. a bookkeeping record,
/// 2. an assistant turn with an empty content array,
/// 3. an assistant turn whose only blocks are `thinking` (recognised, never
///    republished),
/// 4. a usable `text` block whose text is empty or whitespace.
///
/// Every other zero-event outcome is drift and is counted.
///
/// ## `events` and `understood` are INDEPENDENT axes, on purpose
/// They answer different questions — "what could we read?" and "did we
/// understand all of it?" — so a record may legitimately do BOTH: an assistant
/// turn carrying one valid `text` block and one block of a future type emits
/// the text event AND reports `understood: false`.
///
/// That is deliberate, and the alternatives are both worse. Suppressing the
/// valid event would discard real activity an observer needs, to satisfy a
/// tidier partition. Reporting `understood: true` because *something* parsed
/// would hide the drift, which is the entire failure this counter exists to
/// expose. Emitting what we could read while flagging that we could not read
/// all of it is the only option that loses no information.
///
/// A reviewer has now raised this twice as a partition violation; it is not a
/// bug, and this note exists so the next reader sees the choice was made
/// knowingly.
pub(crate) fn classify_record(rec: &Value) -> RecordOutcome {
    let at = timestamp_of(rec);
    let ty = rec.get("type").and_then(Value::as_str).unwrap_or("");
    // A `timestamp` that is PRESENT but unparseable is a format change we
    // should surface, not silently drop to None.
    let timestamp_drifted = rec.get("timestamp").is_some() && at.is_none();

    if BOOKKEEPING_TYPES.contains(&ty) {
        // Understood and deliberately silent.
        return RecordOutcome { events: Vec::new(), understood: !timestamp_drifted };
    }

    let mut out = Vec::new();
    let mut understood = !timestamp_drifted;
    match ty {
        "user" => {
            // A `user` record is either a real human turn or the delivery of a
            // tool's result back to the model. They read very differently to an
            // observer, so they are not conflated. A `user` record carrying
            // NEITHER is a shape we no longer recognise, not an empty turn.
            let content = rec.get("message").and_then(|m| m.get("content"));
            // Presence is not recognition: a `content` that is a number or an
            // object is a drifted shape, not a message we can read.
            let usable_content = matches!(content, Some(Value::String(_)) | Some(Value::Array(_)));
            if rec.get("toolUseResult").is_none() && !usable_content {
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
                    summary: redact_then_truncate(&text, SUMMARY_WIDTH),
                    detail: None,
                });
            } else if let (true, Some(content)) = (usable_content, content) {
                let text = match content {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                out.push(ActivityEvent {
                    at,
                    kind: EventKind::UserMessage,
                    summary: redact_then_truncate(&text, SUMMARY_WIDTH),
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
            // EVERY block must be recognised and usable, not merely one of
            // them. Under an "any" rule a single familiar block — a `thinking`
            // block especially, which is always present — would MASK drifted
            // siblings, which is the failure this counter exists to catch. An
            // EMPTY block list has nothing to fail and is legitimately empty.
            let mut all_recognised = true;
            for b in blocks {
                match b.get("type").and_then(Value::as_str) {
                    // Private reasoning — recognised, and never republished
                    // (see module doc). It cannot mask a drifted sibling
                    // because recognition is now all-of, not any-of.
                    Some("thinking") => {}
                    // A recognised block TYPE whose required field is missing
                    // or wrongly typed is still drift: `{"type":"text","text":42}`
                    // would otherwise count as understood while yielding
                    // nothing. Recognition therefore requires the block to be
                    // USABLE, not merely to carry a familiar type tag.
                    Some("text") => {
                        if let Some(t) = b.get("text").and_then(Value::as_str) {
                            if !t.trim().is_empty() {
                                out.push(ActivityEvent {
                                    at,
                                    kind: EventKind::AssistantMessage,
                                    summary: redact_then_truncate(t, SUMMARY_WIDTH),
                                    detail: None,
                                });
                            }
                        } else {
                            // `text` missing or not a string — familiar tag,
                            // unusable payload.
                            all_recognised = false;
                        }
                    }
                    Some("tool_use") => {
                        let Some(name) = b
                            .get("name")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|n| !n.is_empty())
                        else {
                            // A tool call with no usable name tells an observer
                            // nothing — an empty-summary line is worse than a
                            // counted drift signal. Blank and whitespace-only
                            // names are as unusable as a missing one.
                            all_recognised = false;
                            continue;
                        };
                        let arg = b.get("input").and_then(primary_arg);
                        let summary = match &arg {
                            Some(a) => format!("{name}: {a}"),
                            None => name.to_string(),
                        };
                        out.push(ActivityEvent {
                            at,
                            kind: EventKind::ToolCall,
                            summary: redact_then_truncate(&summary, SUMMARY_WIDTH),
                            detail: arg
                                .map(|a| redact_then_truncate(&a, DETAIL_WIDTH))
                                .filter(|d| !d.is_empty()),
                        });
                    }
                    _ => all_recognised = false,
                }
            }
            if !all_recognised {
                understood = false;
            }
        }
        other if is_error_type(other) => {
            // An error record IS activity an observer wants to see.
            out.push(ActivityEvent {
                at,
                kind: EventKind::Error,
                summary: redact_then_truncate(other, SUMMARY_WIDTH),
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
///
/// This is the LEXICAL half of the jail. It is not sufficient on its own: a
/// symlink beneath the root can point outside it, and `tail` follows symlinks,
/// so a purely textual prefix check can be walked straight through. Callers
/// must follow this with [`resolve_transcript_path_on_host`], which resolves
/// the real path on the target host and re-checks it.
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

/// Resolve a transcript path to its REAL location on the target host and
/// re-apply the jail.
///
/// The lexical check in [`resolve_transcript_path`] cannot see symlinks, and
/// `tail` follows them — so a link under the root pointing at, say, a key file
/// elsewhere would pass a textual prefix test and then be read. This resolves
/// the path with `readlink -f` ON THE HOST THAT WILL READ IT (so the local
/// filesystem's view is never mistaken for a remote one) and requires the
/// canonical result to still be inside the root.
///
/// A TOCTOU window remains between resolving and reading — the link could be
/// re-pointed in between. Closing it properly needs an openat/O_NOFOLLOW-style
/// primitive that is not available through a shell probe, and the residual is
/// small here because the root is an agent-owned directory rather than an
/// attacker-writable one. It is recorded rather than silently accepted.
pub(crate) async fn resolve_transcript_path_on_host(
    exec: &dyn HostExecutor,
    root: &str,
    lexically_valid: &str,
) -> Result<String, ToolError> {
    let out = exec
        .run(&["readlink", "-f", lexically_valid])
        .await
        .map_err(|e| ToolError::Execution(format!("could not resolve transcript path: {e}")))?;
    let real = out.stdout.trim();
    if !out.ok() || real.is_empty() {
        return Err(ToolError::NotFound(
            "transcript path could not be resolved on the target host".into(),
        ));
    }
    // Re-apply the SAME jail to the resolved path.
    resolve_transcript_path(root, real).map_err(|_| {
        ToolError::InvalidArgument(
            "transcript path resolves outside the configured transcript root (symlink escape)"
                .into(),
        )
    })
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
        // A MIX is drift: one familiar block must not MASK an unfamiliar
        // sibling. Under an any-of rule a `thinking` block — present in nearly
        // every turn — would hide every drifted block behind it.
        assert!(!u(json!({"type": "assistant", "message": {"content": [
            {"type": "future_block"}, {"type": "text", "text": "hi"}]}})));
        assert!(!u(json!({"type": "assistant", "message": {"content": [
            {"type": "thinking", "thinking": "x"}, {"type": "future_block"}]}})));
    }

    #[test]
    fn a_familiar_block_type_with_an_unusable_field_is_drift_not_understood() {
        // Recognition requires the block to be USABLE, not merely to carry a
        // familiar type tag: these yield nothing, so counting them understood
        // would silently shorten the feed.
        let u = |v: serde_json::Value| classify_record(&v).understood;

        // `text` present but not a string.
        assert!(!u(json!({"type": "assistant", "message": {"content": [
            {"type": "text", "text": 42}]}})));
        // `text` missing entirely.
        assert!(!u(json!({"type": "assistant", "message": {"content": [
            {"type": "text"}]}})));
        // `tool_use` with no usable name — a placeholder line would tell an
        // observer nothing, so it is drift rather than a "tool" event.
        assert!(!u(json!({"type": "assistant", "message": {"content": [
            {"type": "tool_use", "input": {"file_path": "/x"}}]}})));
        assert!(!u(json!({"type": "assistant", "message": {"content": [
            {"type": "tool_use", "name": 7}]}})));
        // A blank or whitespace-only name is as unusable as a missing one —
        // it would otherwise emit an empty-summary activity line.
        assert!(!u(json!({"type": "assistant", "message": {"content": [
            {"type": "tool_use", "name": ""}]}})));
        assert!(!u(json!({"type": "assistant", "message": {"content": [
            {"type": "tool_use", "name": "   "}]}})));

        // An EMPTY string is a usable text block — recognised, just silent.
        assert!(u(json!({"type": "assistant", "message": {"content": [
            {"type": "text", "text": "   "}]}})));
        // A well-formed tool_use with no input is still usable.
        assert!(u(json!({"type": "assistant", "message": {"content": [
            {"type": "tool_use", "name": "Bash"}]}})));
    }

    #[test]
    fn a_record_can_both_emit_activity_and_be_flagged_as_drifted() {
        // events and understood answer different questions, so a mixed record
        // does BOTH — see the classify_record doc. Suppressing the valid event
        // would discard real activity; reporting understood would hide drift.
        let outcome = classify_record(&json!({"type": "assistant", "message": {"content": [
            {"type": "text", "text": "visible work"},
            {"type": "future_block"}
        ]}}));
        assert_eq!(outcome.events.len(), 1, "the readable block is still reported");
        assert_eq!(outcome.events[0].summary, "visible work");
        assert!(!outcome.understood, "and the unreadable sibling is still flagged");
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
    fn a_secret_straddling_the_truncation_boundary_still_redacts() {
        // The order bug: truncating first could cut the value below the
        // pattern's {8,} floor, so the surviving PREFIX of a real credential
        // was emitted in clear. Shortening a secret does not make it safe.
        // The name must END just before the cut so that only a FEW value
        // characters survive it — under the old order those survivors fell
        // below the pattern's {8,} floor, so nothing matched and they leaked.
        // A secret positioned entirely past the cut would be removed by
        // truncation alone and would pass even the buggy implementation.
        const NAME: &str = "MY_API_TOKEN=";
        let pad = "x".repeat(SUMMARY_WIDTH - NAME.len() - 5);
        let raw = format!("{pad}{NAME}abcdefghijklmnopqrstuvwxyz012345");
        // Exactly 5 value chars sit inside the truncation window.
        assert_eq!(pad.len() + NAME.len(), SUMMARY_WIDTH - 5);

        let e = ev(json!({"type": "user", "message": {"content": raw}}));
        assert!(
            !e[0].summary.contains("abcde"),
            "the surviving prefix of a straddling secret leaked: {}",
            e[0].summary
        );
        // The placeholder itself is what gets truncated now — which is
        // precisely the evidence we want: redaction ran on the full string
        // first, so the cut lands in the placeholder rather than in a secret.
        assert!(
            e[0].summary.contains("<REDA"),
            "expected a redaction placeholder at the cut: {}",
            e[0].summary
        );
    }

    #[test]
    fn an_error_type_is_matched_on_boundaries_not_as_a_substring() {
        // "terror" contains "error" — a substring test would emit it as an
        // error event AND count it understood, hiding an unknown type.
        assert!(is_error_type("error"));
        assert!(is_error_type("api-error"));
        assert!(is_error_type("api_error"));
        assert!(!is_error_type("terror"));
        assert!(!is_error_type("errors-summary"));
        let outcome = classify_record(&json!({"type": "terror"}));
        assert!(outcome.events.is_empty());
        assert!(!outcome.understood, "an unknown type must be counted as drift");
    }

    #[test]
    fn a_user_content_of_the_wrong_type_is_drift_not_a_message() {
        let u = |v: serde_json::Value| classify_record(&v).understood;
        assert!(u(json!({"type": "user", "message": {"content": "text"}})));
        assert!(u(json!({"type": "user", "message": {"content": [{"type": "text"}]}})));
        // Presence is not recognition.
        assert!(!u(json!({"type": "user", "message": {"content": 42}})));
        assert!(!u(json!({"type": "user", "message": {"content": {"nested": 1}}})));
        assert!(!u(json!({"type": "user", "message": {"content": null}})));
    }

    #[test]
    fn a_present_but_unparseable_timestamp_is_counted_as_drift() {
        let ok = classify_record(&json!({"type": "user", "timestamp": "2026-08-01T05:51:43.357Z",
            "message": {"content": "hi"}}));
        assert!(ok.understood);
        // Present but not a valid RFC3339 value — a format change worth seeing.
        let bad = classify_record(&json!({"type": "user", "timestamp": "last tuesday",
            "message": {"content": "hi"}}));
        assert!(!bad.understood);
        assert_eq!(bad.events.len(), 1, "the message is still reported");
        // Absent entirely is fine — not every record carries one.
        let none = classify_record(&json!({"type": "user", "message": {"content": "hi"}}));
        assert!(none.understood);
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

    #[tokio::test]
    async fn the_jail_also_rejects_a_symlink_that_escapes_the_root() {
        use crate::agentsess::exec::test_support::FakeExecutor;
        let root = "/home/u/.claude/projects";

        // A path that is lexically inside the root but RESOLVES outside it —
        // `tail` would follow the link, so a textual prefix check alone is not
        // a jail.
        let escaping = FakeExecutor::new().with_stdout("readlink", "/etc/shadow\n");
        let err = resolve_transcript_path_on_host(&escaping, root, "/home/u/.claude/projects/p/x.jsonl")
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)), "got {err:?}");

        // A path that resolves to a real location inside the root is accepted.
        let ok = FakeExecutor::new()
            .with_stdout("readlink", "/home/u/.claude/projects/p/real.jsonl\n");
        let got = resolve_transcript_path_on_host(&ok, root, "/home/u/.claude/projects/p/x.jsonl")
            .await
            .unwrap();
        assert_eq!(got, "/home/u/.claude/projects/p/real.jsonl");
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
