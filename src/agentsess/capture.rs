//! Read-only capture of the tmux pane a session is attached to (AGSS-03).
//!
//! This is what lets an operator *watch* an agent rather than only read its
//! summarised activity. It captures; it never sends. See the module doc on
//! [`super`] for why the send half is a separate, gated change.
//!
//! ## The target string is the security boundary here
//! A pane target is caller-supplied and reaches `tmux` as an argument. argv
//! form already stops shell injection, but not OPTION injection: `tmux` parses
//! a leading-dash argument as a flag, so an unvalidated target could smuggle
//! one. [`validate_pane_target`] therefore fails CLOSED against a conservative
//! shape rather than trying to reject bad input — anything that is not
//! recognisably `session:window.pane` is refused before a command is built.
//!
//! ## Output is bounded twice, and redacted
//! A pane can hold a very large scrollback, and a wide one can hold long
//! lines, so the result is capped by BOTH line count and total bytes. It is
//! then scrubbed through the same redaction path the transcript reader uses —
//! a terminal can display a credential just as easily as a transcript can.

use serde::{Deserialize, Serialize};

use crate::error::ToolError;

use super::exec::HostExecutor;

/// Default lines of scrollback — about a screenful and a half.
const DEFAULT_LINES: u32 = 200;

/// Line cap, floored at 1.
///
/// A configured `0` would otherwise make every capture return nothing at all —
/// a silent empty result that reads as "the pane is blank" rather than "the
/// cap is misconfigured". A cap of zero is never a meaningful request, so it is
/// treated as the minimum rather than honoured literally.
fn max_lines() -> u32 {
    std::env::var("AGENTSESS_CAPTURE_MAX_LINES")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(2000)
        .max(1)
}

/// Byte cap, floored at 1 for the same reason as [`max_lines`]: a configured
/// `0` would return an empty string on every capture. The floor is deliberately
/// the minimum rather than something "sensible" — picking a larger floor would
/// silently override an operator who really did want a tiny cap, and would make
/// small caps untestable.
fn max_bytes() -> usize {
    std::env::var("AGENTSESS_CAPTURE_MAX_BYTES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(256 * 1024)
        .max(1)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PaneCapture {
    pub target: String,
    /// The captured text, bounded and redacted. ANSI escapes are preserved —
    /// the consumer renders them; see the field note below.
    pub content: String,
    pub lines_requested: u32,
    /// True when the requested line count exceeded `AGENTSESS_CAPTURE_MAX_LINES`
    /// and was clamped down to it.
    pub lines_clamped: bool,
    /// True when the returned output was trimmed to the effective line bound
    /// because the capture came back with more lines than were asked for. This
    /// is separate from `lines_clamped`: one is about the REQUEST being too
    /// large, the other about the RESULT being too large.
    pub lines_trimmed: bool,
    /// True when the BYTE cap truncated the content. A wide pane can blow the
    /// byte budget well before the line budget, so both are reported.
    pub bytes_truncated: bool,
}

/// Validate a `session:window.pane` target, failing CLOSED.
///
/// The shape is deliberately narrow: a session name of word characters, dots
/// and dashes, then a numeric window and pane. Real tmux allows far more in a
/// session name, and that is exactly why this does not try to mirror tmux's
/// own rules — a permissive validator here would be a place for an option or a
/// separator to hide. A session named outside this shape simply cannot be
/// captured by explicit target; it is still reachable via `session_id`, whose
/// target this module CONSTRUCTS rather than parses.
pub(crate) fn validate_pane_target(target: &str) -> Result<(), ToolError> {
    let reject = |why: &str| {
        Err(ToolError::InvalidArgument(format!(
            "invalid pane target {target:?}: {why}"
        )))
    };

    if target.is_empty() || target.len() > 128 {
        return reject("must be 1-128 characters");
    }
    // A leading dash would be read by tmux as an option, not a target.
    if target.starts_with('-') {
        return reject("must not start with '-'");
    }
    let Some((session, rest)) = target.split_once(':') else {
        return reject("expected 'session:window.pane'");
    };
    let Some((window, pane)) = rest.split_once('.') else {
        return reject("expected 'session:window.pane'");
    };
    if session.is_empty()
        || !session
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
    {
        return reject("session must be alphanumeric, '_', '-' or '.'");
    }
    if window.is_empty() || !window.chars().all(|c| c.is_ascii_digit()) {
        return reject("window index must be numeric");
    }
    if pane.is_empty() || !pane.chars().all(|c| c.is_ascii_digit()) {
        return reject("pane index must be numeric");
    }
    Ok(())
}

/// Capture a pane's recent scrollback.
pub(crate) async fn capture(
    exec: &dyn HostExecutor,
    target: &str,
    lines: Option<u32>,
) -> Result<PaneCapture, ToolError> {
    validate_pane_target(target)?;

    let cap = max_lines();
    let requested = lines.unwrap_or(DEFAULT_LINES).max(1);
    let effective = requested.min(cap);
    let lines_clamped = requested > cap;

    // `-S -N` starts N lines above the visible pane; `-p` writes to stdout.
    // `-J` is deliberately NOT used: joining wrapped lines would misrepresent
    // what the operator would actually see on the pane.
    let start = format!("-{effective}");
    let out = exec
        .run(&["tmux", "capture-pane", "-p", "-t", target, "-S", &start])
        .await
        .map_err(|e| ToolError::Execution(format!("pane capture failed: {e}")))?;

    if !out.ok() {
        let err: String = out.stderr.trim().chars().take(200).collect();
        return Err(ToolError::NotFound(format!(
            "could not capture pane {target}: {err}"
        )));
    }

    // Redact BEFORE bounding, for the same reason the transcript reader does:
    // truncating first can cut a secret below the pattern's length floor, so
    // the scrubber stops matching and the surviving prefix is emitted in clear.
    let redacted = super::transcript::redact(&out.stdout);

    // Enforce the LINE bound ourselves rather than trusting `-S -N` to have
    // done it. tmux is being asked for a starting offset, not a hard limit,
    // and a bound we merely REQUESTED is not a bound we can promise a caller.
    // Keep the NEWEST lines: the tail is what an observer is watching for.
    let line_count = redacted.lines().count();
    let (redacted, lines_dropped) = if line_count > effective as usize {
        let keep = line_count - effective as usize;
        let kept: Vec<&str> = redacted.lines().skip(keep).collect();
        (kept.join("\n"), true)
    } else {
        (redacted, false)
    };

    let byte_cap = max_bytes();
    let (content, bytes_truncated) = if redacted.len() > byte_cap {
        // Cut on a char boundary so the result is still valid UTF-8.
        let mut end = byte_cap;
        while end > 0 && !redacted.is_char_boundary(end) {
            end -= 1;
        }
        (redacted[..end].to_string(), true)
    } else {
        (redacted, false)
    };

    Ok(PaneCapture {
        target: target.to_string(),
        content,
        lines_requested: effective,
        lines_clamped,
        lines_trimmed: lines_dropped,
        bytes_truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agentsess::exec::test_support::FakeExecutor;
    use serial_test::serial;

    // Mutates the shared cap env (TERM #588).
    #[tokio::test]
    #[serial]
    async fn a_misconfigured_zero_cap_does_not_silently_return_nothing() {
        // A cap of 0 is never a meaningful request. Honouring it literally
        // would return an empty capture that reads as "the pane is blank"
        // rather than "the cap is misconfigured".
        std::env::set_var("AGENTSESS_CAPTURE_MAX_LINES", "0");
        std::env::set_var("AGENTSESS_CAPTURE_MAX_BYTES", "0");
        let exec = FakeExecutor::new().with_stdout("tmux", "visible line\n");
        let c = capture(&exec, "build:0.1", Some(10)).await.unwrap();
        std::env::remove_var("AGENTSESS_CAPTURE_MAX_LINES");
        std::env::remove_var("AGENTSESS_CAPTURE_MAX_BYTES");
        assert!(!c.content.is_empty(), "a zero cap must not blank the capture");
        assert_eq!(c.lines_requested, 1, "floored to the minimum, not zero");
    }

    // A non-numeric cap falls back to the default rather than to zero.
    #[tokio::test]
    #[serial]
    async fn a_malformed_cap_falls_back_to_the_default() {
        std::env::set_var("AGENTSESS_CAPTURE_MAX_LINES", "not-a-number");
        let exec = FakeExecutor::new().with_stdout("tmux", "x\n");
        let c = capture(&exec, "build:0.1", Some(50)).await.unwrap();
        std::env::remove_var("AGENTSESS_CAPTURE_MAX_LINES");
        assert_eq!(c.lines_requested, 50, "default cap 2000 leaves 50 untouched");
    }

    #[test]
    fn valid_targets_are_accepted() {
        for t in ["0:0.0", "build:1.2", "my-session:0.0", "a_b.c:10.3"] {
            assert!(validate_pane_target(t).is_ok(), "{t} should be valid");
        }
    }

    #[test]
    fn injection_shaped_targets_are_refused_before_a_command_is_built() {
        // argv form stops SHELL injection; this is about OPTION injection and
        // separator smuggling. Each must fail closed.
        for t in [
            "-X",                    // a bare option
            "-kill-session",         // an option that would DO something
            "0:0.0 -X kill-session", // a separator inside the target
            "0:0.0;whoami",
            "0:0.0`whoami`",
            "0:0.0$(whoami)",
            "0:0.0\nkill",
            "0:0.0'",
            "0:0.0\"",
            "../../etc:0.0",
            "sess:x.0",  // non-numeric window
            "sess:0.y",  // non-numeric pane
            "sess:0",    // missing pane
            "sess",      // missing everything
            "",
        ] {
            assert!(
                validate_pane_target(t).is_err(),
                "{t:?} must be rejected"
            );
        }
        // Over-long input is refused rather than passed to tmux.
        assert!(validate_pane_target(&"a".repeat(200)).is_err());
    }

    // Reads (or mutates) the shared cap env — see TERM #588: the READERS
    // must be serialised too, not only the writers.
    #[tokio::test]
    #[serial]
    async fn capture_returns_pane_text() {
        let exec = FakeExecutor::new().with_stdout("tmux", "line one\nline two\n");
        let c = capture(&exec, "build:0.1", Some(50)).await.unwrap();
        assert_eq!(c.target, "build:0.1");
        assert!(c.content.contains("line two"));
        assert_eq!(c.lines_requested, 50);
        assert!(!c.lines_clamped);
        assert!(!c.bytes_truncated);
    }

    // Reads (or mutates) the shared cap env — see TERM #588: the READERS
    // must be serialised too, not only the writers.
    #[tokio::test]
    #[serial]
    async fn a_line_request_over_the_cap_is_clamped_and_flagged() {
        std::env::set_var("AGENTSESS_CAPTURE_MAX_LINES", "100");
        let exec = FakeExecutor::new().with_stdout("tmux", "x\n");
        let c = capture(&exec, "build:0.1", Some(5000)).await.unwrap();
        std::env::remove_var("AGENTSESS_CAPTURE_MAX_LINES");
        assert_eq!(c.lines_requested, 100, "clamped to the cap");
        assert!(c.lines_clamped, "and the clamp is reported, not silent");
    }

    // Reads (or mutates) the shared cap env — see TERM #588: the READERS
    // must be serialised too, not only the writers.
    // The line bound is enforced HERE, not merely requested of tmux.
    #[tokio::test]
    #[serial]
    async fn more_lines_than_requested_are_trimmed_to_the_bound() {
        let many = (0..50).map(|i| format!("line{i}")).collect::<Vec<_>>().join("\n");
        let exec = FakeExecutor::new().with_stdout("tmux", &many);
        let c = capture(&exec, "build:0.1", Some(5)).await.unwrap();
        assert_eq!(c.content.lines().count(), 5, "the bound is enforced, not assumed");
        assert!(c.lines_trimmed, "and the trim is reported");
        // The NEWEST lines are kept — the tail is what an observer watches.
        assert!(c.content.contains("line49"), "got: {}", c.content);
        assert!(!c.content.contains("line0\n"), "oldest lines dropped");
    }

    #[tokio::test]
    #[serial]
    async fn output_within_the_bound_is_not_trimmed() {
        let exec = FakeExecutor::new().with_stdout("tmux", "a\nb\nc");
        let c = capture(&exec, "build:0.1", Some(10)).await.unwrap();
        assert!(!c.lines_trimmed);
        assert_eq!(c.content.lines().count(), 3);
    }

    #[tokio::test]
    #[serial]
    async fn a_wide_pane_is_bounded_by_bytes_as_well_as_lines() {
        // A few very long lines blow the byte budget well before the line
        // budget, which is why both caps exist.
        std::env::set_var("AGENTSESS_CAPTURE_MAX_BYTES", "64");
        let exec = FakeExecutor::new().with_stdout("tmux", &"y".repeat(500));
        let c = capture(&exec, "build:0.1", Some(10)).await.unwrap();
        std::env::remove_var("AGENTSESS_CAPTURE_MAX_BYTES");
        assert!(c.bytes_truncated, "byte truncation must be flagged");
        assert!(c.content.len() <= 64);
    }

    // Reads (or mutates) the shared cap env — see TERM #588: the READERS
    // must be serialised too, not only the writers.
    #[tokio::test]
    #[serial]
    async fn multibyte_content_is_cut_on_a_char_boundary() {
        std::env::set_var("AGENTSESS_CAPTURE_MAX_BYTES", "10");
        // 3-byte characters straddling a 10-byte cut.
        let exec = FakeExecutor::new().with_stdout("tmux", &"€".repeat(20));
        let c = capture(&exec, "build:0.1", Some(10)).await.unwrap();
        std::env::remove_var("AGENTSESS_CAPTURE_MAX_BYTES");
        assert!(c.bytes_truncated);
        // The assertion that matters: the result is still valid UTF-8 and did
        // not panic on a mid-character slice.
        assert!(c.content.chars().all(|ch| ch == '€'));
    }

    /// TERM #594 (4th test-gate residual, found while working the other three).
    /// This was NOT flaky — it lost a PROCESS-GLOBAL env race, deterministically.
    /// It reads the capture caps (`AGENTSESS_CAPTURE_MAX_LINES`/`_MAX_BYTES`)
    /// that its `#[serial]` siblings above set to tiny values; interleaved, the
    /// tiny byte cap truncated the output before `<REDACTED-SECRET>` and the
    /// assertion failed for a reason that had nothing to do with redaction.
    /// Every test in this module that reads those caps must be `#[serial]` —
    /// the sibling that MUTATES being serial is only half of the contract.
    #[tokio::test]
    #[serial]
    async fn captured_text_is_redacted() {
        let exec = FakeExecutor::new()
            .with_stdout("tmux", "$ export SOME_API_TOKEN=abcdefghijklmnop0123\n"); // pii-test-fixture
        let c = capture(&exec, "build:0.1", None).await.unwrap();
        assert!(
            !c.content.contains("abcdefghijklmnop0123"), // pii-test-fixture
            "a credential shown on the pane leaked: {}",
            c.content
        );
        assert!(c.content.contains("<REDACTED-SECRET>"));
    }

    #[tokio::test]
    #[serial]
    async fn a_missing_pane_is_a_clear_error_not_empty_success() {
        let exec = FakeExecutor::new().with_exit("tmux", 1, "can't find pane: build:0.9");
        let err = capture(&exec, "build:0.9", None).await.unwrap_err();
        match err {
            ToolError::NotFound(m) => assert!(m.contains("build:0.9"), "{m}"),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    #[serial]
    async fn tmux_absent_is_reported_rather_than_read_as_an_empty_pane() {
        let exec = FakeExecutor::new(); // `tmux` unregistered => NotFound
        assert!(capture(&exec, "build:0.1", None).await.is_err());
    }
}
