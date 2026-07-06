//! Closed provider enum, fixed per-provider binary/model constants, and pure
//! argv-array command builders for the review-daemon.
//!
//! Security invariants this module exists to uphold:
//!   - `Provider` is a closed Rust enum. An unrecognized `"provider"` string in
//!     the request JSON fails serde deserialization at the parse boundary —
//!     it can never reach [`spawn_args`] or any process-spawn code path.
//!   - Binary name, base command, and model string are hardcoded constants
//!     (never derived from request input).
//!   - Command builders return a `Vec<String>` argv array. The prompt is
//!     always passed as ONE opaque element of that array (never
//!     string-concatenated into a shell command). No builder here ever
//!     constructs a shell invocation ("sh -c", "bash -c", etc).

use serde::Deserialize;
use uuid::Uuid;

/// Closed set of review providers the daemon will dispatch to. Deserializing an
/// unrecognized string (e.g. `"gpt5"`) fails at the serde boundary — see
/// `daemon_client`/`http` for the 400 this produces before any spawn logic runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Opus,
    Codex,
    Agy,
}

impl Provider {
    pub fn as_str(&self) -> &'static str {
        match self {
            Provider::Opus => "opus",
            Provider::Codex => "codex",
            Provider::Agy => "agy",
        }
    }

    /// The fixed CLI binary name this provider spawns. Hardcoded, resolved once
    /// at daemon startup via [`super::resolve::resolve_on_path`] (never
    /// re-resolved per request).
    pub fn binary(&self) -> &'static str {
        match self {
            Provider::Opus => CLAUDE_BIN,
            Provider::Codex => CODEX_BIN,
            Provider::Agy => AGY_BIN,
        }
    }
}

// ── Fixed constants (never caller-controlled) ───────────────────────────────

const CLAUDE_BIN: &str = "claude";
const CODEX_BIN: &str = "codex";
const AGY_BIN: &str = "agy";

/// Claude CLI model alias for the "opus" provider slot.
const OPUS_MODEL: &str = "opus";
/// Codex CLI model for the "codex" provider slot.
const CODEX_MODEL: &str = "gpt-5.5";
/// agy (Antigravity CLI) model for the "agy" provider slot.
const AGY_MODEL: &str = "gemini-3.1-pro";

/// A fully-built, ready-to-spawn command: binary name + argv array. Never a
/// shell string. `output_path` is populated only for providers (codex) that
/// write their clean reply to a file rather than stdout.
pub struct BuiltCommand {
    pub binary: &'static str,
    pub args: Vec<String>,
    pub output_path: Option<String>,
}

/// Build the argv array for `provider` given an opaque `prompt` string. This is
/// the ONLY place command lines are assembled; it never touches a shell.
///
/// The prompt is passed as a single argv element (`claude`/`agy`) or, for
/// `codex`, as the single trailing positional argument — never split, never
/// interpolated into a larger string that a shell would re-parse.
pub fn build_command(provider: Provider, prompt: &str) -> BuiltCommand {
    match provider {
        Provider::Opus => BuiltCommand {
            binary: CLAUDE_BIN,
            // --tools "" disables built-in tool use so a subprocess with no
            // interactive stdin never blocks on a permission prompt.
            args: vec![
                "--model".into(), OPUS_MODEL.into(),
                "-p".into(), prompt.to_string(),
                "--output-format".into(), "text".into(),
                "--tools".into(), "".into(),
            ],
            output_path: None,
        },
        Provider::Codex => {
            let output_path = std::env::temp_dir()
                .join(format!("review-daemon-codex-{}.txt", Uuid::new_v4()))
                .to_string_lossy()
                .to_string();
            BuiltCommand {
                binary: CODEX_BIN,
                args: vec![
                    "exec".into(),
                    "--skip-git-repo-check".into(),
                    "--sandbox".into(), "read-only".into(),
                    "-m".into(), CODEX_MODEL.into(),
                    "--output-last-message".into(), output_path.clone().into(),
                    // "--" is the standard clap argv terminator: without it, a
                    // prompt starting with '-' (e.g. "-not-a-flag ...") is
                    // parsed as another `codex exec` option rather than the
                    // positional prompt -- confirmed live: codex errors with
                    // "unexpected argument '-n' found" on such a prompt
                    // without this separator. This is not shell injection
                    // (argv is still a fixed array, never a shell string),
                    // but caller-controlled prompt text could otherwise
                    // influence codex's own flag parsing.
                    "--".into(),
                    prompt.to_string(),
                ],
                output_path: Some(output_path),
            }
        }
        Provider::Agy => BuiltCommand {
            binary: AGY_BIN,
            args: vec![
                "--model".into(), AGY_MODEL.into(),
                "-p".into(), prompt.to_string(),
                "--dangerously-skip-permissions".into(),
            ],
            output_path: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHELL_MARKERS: &[&str] = &["sh", "-c", "bash"];

    fn assert_no_shell_markers(cmd: &BuiltCommand) {
        assert!(
            !SHELL_MARKERS.contains(&cmd.binary),
            "binary must never be a shell: {}",
            cmd.binary
        );
        for a in &cmd.args {
            assert!(
                !SHELL_MARKERS.contains(&a.as_str()),
                "argv must never contain a shell marker element, found {a:?} in {:?}",
                cmd.args
            );
        }
    }

    #[test]
    fn opus_command_has_no_shell_markers_and_prompt_is_single_arg() {
        let prompt = "review this; rm -rf / && echo pwned";
        let cmd = build_command(Provider::Opus, prompt);
        assert_no_shell_markers(&cmd);
        // The (potentially adversarial) prompt text must appear as exactly ONE
        // argv element, verbatim -- never split/re-tokenized.
        assert_eq!(cmd.args.iter().filter(|a| a.as_str() == prompt).count(), 1);
        assert_eq!(cmd.binary, "claude");
    }

    #[test]
    fn codex_command_has_no_shell_markers_and_prompt_is_single_trailing_arg() {
        let prompt = "$(whoami) `id` && rm -rf ~";
        let cmd = build_command(Provider::Codex, prompt);
        assert_no_shell_markers(&cmd);
        assert_eq!(cmd.args.last().map(String::as_str), Some(prompt));
        assert_eq!(cmd.binary, "codex");
        assert!(cmd.output_path.is_some());
    }

    #[test]
    fn codex_prompt_is_preceded_by_argv_terminator_even_when_flag_like() {
        // A prompt starting with '-' must not be parsed as a codex CLI flag.
        // Confirmed live: without the "--" terminator, codex errors with
        // "unexpected argument '-n' found" on a prompt starting with '-n...'.
        // The second-to-last argv element must be the literal "--" terminator
        // immediately before the prompt.
        let prompt = "-not-a-flag, reply with the word HELLO";
        let cmd = build_command(Provider::Codex, prompt);
        assert_eq!(cmd.args.last().map(String::as_str), Some(prompt));
        assert_eq!(
            cmd.args.get(cmd.args.len() - 2).map(String::as_str),
            Some("--"),
            "expected the argv terminator immediately before the prompt, got {:?}",
            cmd.args
        );
    }

    #[test]
    fn agy_command_has_no_shell_markers_and_prompt_is_single_arg() {
        let prompt = "; cat /etc/passwd #";
        let cmd = build_command(Provider::Agy, prompt);
        assert_no_shell_markers(&cmd);
        assert_eq!(cmd.args.iter().filter(|a| a.as_str() == prompt).count(), 1);
        assert_eq!(cmd.binary, "agy");
    }

    #[test]
    fn model_strings_are_fixed_not_caller_controlled() {
        // build_command's signature takes no model parameter at all -- there is
        // no code path by which request JSON can influence the model string.
        let cmd = build_command(Provider::Opus, "x");
        assert!(cmd.args.contains(&OPUS_MODEL.to_string()));
    }

    #[test]
    fn unrecognized_provider_string_is_rejected_at_deserialize_boundary() {
        // This is the enum-closure guarantee: an unknown provider string must
        // fail to deserialize into `Provider` at all -- there is no `Provider`
        // variant it could produce, so `build_command` (and therefore any
        // spawn code) is structurally unreachable for it.
        let err = serde_json::from_str::<Provider>("\"gpt5\"").unwrap_err();
        let _ = err; // presence of the error is the assertion
        assert!(serde_json::from_str::<Provider>("\"gpt5\"").is_err());
        assert!(serde_json::from_str::<Provider>("\"opus\"").is_ok());
        assert!(serde_json::from_str::<Provider>("\"codex\"").is_ok());
        assert!(serde_json::from_str::<Provider>("\"agy\"").is_ok());
    }
}
