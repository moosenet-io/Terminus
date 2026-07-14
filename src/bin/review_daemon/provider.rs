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
    /// The Epic-capstone Fable lens — the `claude` CLI at the `claude-fable-5`
    /// model (Fable OAuth). Wire name is `claude-fable-5` to match `review_run`.
    #[serde(rename = "claude-fable-5")]
    Fable,
}

impl Provider {
    pub fn as_str(&self) -> &'static str {
        match self {
            Provider::Opus => "opus",
            Provider::Codex => "codex",
            Provider::Agy => "agy",
            Provider::Fable => "claude-fable-5",
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
            Provider::Fable => CLAUDE_BIN,
        }
    }
}

// ── Fixed constants (never caller-controlled) ───────────────────────────────

const CLAUDE_BIN: &str = "claude";
const CODEX_BIN: &str = "codex";
const AGY_BIN: &str = "agy";

/// Claude CLI model alias for the "opus" provider slot.
const OPUS_MODEL: &str = "opus";
/// Claude CLI model for the Fable capstone lens (Fable OAuth).
const FABLE_MODEL: &str = "claude-fable-5";
/// Read-ONLY exploration tools pre-approved for the claude slots in explore mode.
/// Deliberately excludes Bash/Write/Edit — a capstone auditor may READ the repo
/// (audit real code) but never execute a command or mutate anything. Passed as
/// pre-approved (`--allowedTools`) so tool use never blocks on a permission prompt
/// (the daemon has no stdin), WITHOUT `--dangerously-skip-permissions` (which the
/// claude CLI refuses to run as root anyway).
const EXPLORE_TOOLS: &[&str] = &["Read", "Grep", "Glob", "LS"];
/// Codex CLI model for the "codex" provider slot.
const CODEX_MODEL: &str = "gpt-5.5";
/// agy (Antigravity CLI) model for the "agy" provider slot.
const AGY_MODEL: &str = "gemini-3.1-pro";

/// REVCAP-01 PART B: the `claude` CLI flag that sets the model's reasoning
/// effort level for the session. CONFIRMED against the installed `claude` CLI's
/// own `--help` (`--effort <level>`, level values low/medium/high, guarded by
/// `config::clamp_reasoning_effort`'s allowlist). Kept behind `Option<&str>`
/// throughout this module so it only affects the NEW intensive path: every
/// pre-existing call site (routine reviews, the Epic capstone) passes `None`
/// and is byte-for-byte unaffected.
const CLAUDE_REASONING_EFFORT_FLAG: &str = "--effort";
/// The specific key codex uses for reasoning effort, emitted via codex's
/// long-form config-override flag (`--config key=value`; the short form is
/// `-c`, but that literal is a `SHELL_MARKERS` member and would trip the
/// `assert_no_shell_markers` anti-`sh -c` invariant, so the long form is used).
/// codex parses the value as TOML (cf. its `-c model="o3"` example).
const CODEX_REASONING_EFFORT_KEY: &str = "model_reasoning_effort";

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
/// `reasoning_effort` is REVCAP-01 PART B's intensity knob: `None` on every
/// pre-PART-B call site (routine reviews, the Epic capstone) reproduces the
/// exact argv this function built before PART B, byte-for-byte. `Some(level)`
/// (e.g. `"high"`) is only ever passed for an intensive-substitute dispatch (a
/// provider standing in for a currently-DOWN frontier reviewer) and appends the
/// provider's own effort flag -- see [`CLAUDE_REASONING_EFFORT_FLAG`] /
/// [`CODEX_REASONING_EFFORT_KEY`] for the assumed/documented flag names. `agy`
/// has no known effort knob, so the parameter is accepted but ignored for it.
pub fn build_command(
    provider: Provider,
    prompt: &str,
    explore: bool,
    reasoning_effort: Option<&str>,
) -> BuiltCommand {
    match provider {
        Provider::Opus => claude_command(OPUS_MODEL, prompt, explore, reasoning_effort),
        Provider::Fable => claude_command(FABLE_MODEL, prompt, explore, reasoning_effort),
        Provider::Codex => {
            let output_path = std::env::temp_dir()
                .join(format!("review-daemon-codex-{}.txt", Uuid::new_v4()))
                .to_string_lossy()
                .to_string();
            let mut args = vec![
                "exec".into(),
                "--skip-git-repo-check".into(),
                "--sandbox".into(), "read-only".into(),
                "-m".into(), CODEX_MODEL.into(),
            ];
            // REVCAP-01 PART B: only appended for an intensive-substitute dispatch
            // -- a routine/epic codex call (`reasoning_effort: None`) produces the
            // exact same argv as before this change. Uses codex's LONG-form
            // `--config <key=value>` (not the `-c` short form): `-c` is a member
            // of this module's `SHELL_MARKERS` security invariant (which proves
            // the daemon never builds an `sh -c` shell invocation), so emitting a
            // literal `-c` argv element would trip `assert_no_shell_markers`.
            // `--config` is codex's own documented equivalent (`-c, --config
            // <key=value>`) and is not a shell marker. The value keeps its TOML
            // quotes -- codex parses `--config` values as TOML (cf. its
            // `-c model="o3"` example).
            if let Some(effort) = reasoning_effort {
                args.push("--config".into());
                args.push(format!("{CODEX_REASONING_EFFORT_KEY}=\"{effort}\""));
            }
            args.push("--output-last-message".into());
            args.push(output_path.clone());
            // "--" is the standard clap argv terminator: without it, a
            // prompt starting with '-' (e.g. "-not-a-flag ...") is
            // parsed as another `codex exec` option rather than the
            // positional prompt -- confirmed live: codex errors with
            // "unexpected argument '-n' found" on such a prompt
            // without this separator. This is not shell injection
            // (argv is still a fixed array, never a shell string),
            // but caller-controlled prompt text could otherwise
            // influence codex's own flag parsing.
            args.push("--".into());
            args.push(prompt.to_string());
            BuiltCommand { binary: CODEX_BIN, args, output_path: Some(output_path) }
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

/// The `claude` CLI command for the Opus/Fable slots. Routine reviews disable
/// tools (`--tools ""`) so a stdin-less subprocess never blocks on a permission
/// prompt. In EXPLORE mode (the Epic capstone) the auditor instead gets the
/// READ-ONLY [`EXPLORE_TOOLS`] pre-approved via `--allowedTools` — so it can read
/// the repo (run in the request's repo cwd) to audit real code, but can never
/// execute a command or mutate anything. Verified live: `--allowedTools Read Grep
/// Glob LS` pre-approves those tools without a prompt and without the root-blocked
/// `--dangerously-skip-permissions`.
fn claude_command(model: &str, prompt: &str, explore: bool, reasoning_effort: Option<&str>) -> BuiltCommand {
    let mut args = vec![
        "--model".into(), model.to_string(),
        "-p".into(), prompt.to_string(),
        "--output-format".into(), "text".into(),
    ];
    if explore {
        args.push("--allowedTools".into());
        for t in EXPLORE_TOOLS {
            args.push((*t).to_string());
        }
    } else {
        args.push("--tools".into());
        args.push("".into());
    }
    // REVCAP-01 PART B: only appended for an intensive-substitute dispatch --
    // routine/Epic calls (`reasoning_effort: None`) produce the exact same argv
    // as before this change. Flag is the CONFIRMED `--effort <level>` (see
    // `CLAUDE_REASONING_EFFORT_FLAG`'s doc).
    if let Some(effort) = reasoning_effort {
        args.push(CLAUDE_REASONING_EFFORT_FLAG.to_string());
        args.push(effort.to_string());
    }
    BuiltCommand { binary: CLAUDE_BIN, args, output_path: None }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fable_slot_uses_claude_cli_at_the_fable_model() {
        let cmd = build_command(Provider::Fable, "audit", false, None);
        assert_eq!(cmd.binary, CLAUDE_BIN);
        assert_eq!(Provider::Fable.as_str(), "claude-fable-5");
        assert!(cmd.args.windows(2).any(|w| w[0] == "--model" && w[1] == FABLE_MODEL));
    }

    #[test]
    fn fable_deserializes_from_the_wire_name() {
        // The daemon receives the review_run provider string "claude-fable-5".
        let p: Provider = serde_json::from_value(serde_json::json!("claude-fable-5")).unwrap();
        assert_eq!(p, Provider::Fable);
    }

    #[test]
    fn explore_mode_enables_readonly_tools_never_bash_or_write() {
        for prov in [Provider::Opus, Provider::Fable] {
            let routine = build_command(prov, "x", false, None);
            assert!(
                routine.args.windows(2).any(|w| w[0] == "--tools" && w[1].is_empty()),
                "routine claude disables tools"
            );
            let explore = build_command(prov, "x", true, None);
            assert!(explore.args.iter().any(|a| a == "--allowedTools"));
            for t in ["Read", "Grep", "Glob", "LS"] {
                assert!(explore.args.iter().any(|a| a == t), "explore allows {t}");
            }
            // NEVER an exec/mutate tool or the root-blocked bypass flag.
            for forbidden in ["Bash", "Write", "Edit", "--dangerously-skip-permissions"] {
                assert!(
                    !explore.args.iter().any(|a| a == forbidden),
                    "explore must not grant {forbidden}"
                );
            }
        }
    }

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
        let cmd = build_command(Provider::Opus, prompt, false, None);
        assert_no_shell_markers(&cmd);
        // The (potentially adversarial) prompt text must appear as exactly ONE
        // argv element, verbatim -- never split/re-tokenized.
        assert_eq!(cmd.args.iter().filter(|a| a.as_str() == prompt).count(), 1);
        assert_eq!(cmd.binary, "claude");
    }

    #[test]
    fn codex_command_has_no_shell_markers_and_prompt_is_single_trailing_arg() {
        let prompt = "$(whoami) `id` && rm -rf ~";
        let cmd = build_command(Provider::Codex, prompt, false, None);
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
        let cmd = build_command(Provider::Codex, prompt, false, None);
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
        let cmd = build_command(Provider::Agy, prompt, false, None);
        assert_no_shell_markers(&cmd);
        assert_eq!(cmd.args.iter().filter(|a| a.as_str() == prompt).count(), 1);
        assert_eq!(cmd.binary, "agy");
    }

    #[test]
    fn model_strings_are_fixed_not_caller_controlled() {
        // build_command's signature takes no model parameter at all -- there is
        // no code path by which request JSON can influence the model string.
        let cmd = build_command(Provider::Opus, "x", false, None);
        assert!(cmd.args.contains(&OPUS_MODEL.to_string()));
    }

    // ── REVCAP-01 PART B: reasoning-effort argv wiring ──────────────────────

    #[test]
    fn claude_command_omits_effort_flag_when_none() {
        for prov in [Provider::Opus, Provider::Fable] {
            let cmd = build_command(prov, "x", false, None);
            // Assert against the confirmed literal flag, not just the constant,
            // so a wrong flag rename can't silently keep this test passing.
            assert!(
                !cmd.args.iter().any(|a| a == "--effort"),
                "routine claude call must not carry the effort flag: {:?}",
                cmd.args
            );
        }
    }

    #[test]
    fn claude_command_appends_effort_flag_when_some() {
        for prov in [Provider::Opus, Provider::Fable] {
            let cmd = build_command(prov, "x", false, Some("high"));
            // The confirmed claude CLI flag is `--effort <level>` (verified
            // against `claude --help`), passed as two argv elements.
            assert!(
                cmd.args.windows(2).any(|w| w[0] == "--effort" && w[1] == "high"),
                "intensive claude call must carry `--effort high`: {:?}",
                cmd.args
            );
            assert_eq!(CLAUDE_REASONING_EFFORT_FLAG, "--effort");
        }
    }

    #[test]
    fn claude_command_effort_flag_coexists_with_explore_mode() {
        // Not a real call shape review_run ever produces (intensive is explore:
        // false), but build_command itself must not silently drop one or the
        // other if ever combined -- both independently toggleable.
        let cmd = build_command(Provider::Opus, "x", true, Some("high"));
        assert!(cmd.args.iter().any(|a| a == "--allowedTools"));
        assert!(cmd.args.windows(2).any(|w| w[0] == "--effort" && w[1] == "high"));
    }

    #[test]
    fn codex_command_omits_effort_override_when_none() {
        let cmd = build_command(Provider::Codex, "x", false, None);
        assert!(
            !cmd.args.iter().any(|a| a.contains(CODEX_REASONING_EFFORT_KEY)),
            "routine codex call must not carry the --config override: {:?}",
            cmd.args
        );
        // And never a bare `--config` flag either.
        assert!(!cmd.args.iter().any(|a| a == "--config"), "{:?}", cmd.args);
    }

    #[test]
    fn codex_command_appends_effort_override_when_some() {
        let cmd = build_command(Provider::Codex, "x", false, Some("high"));
        // Long-form `--config` (not `-c`): `-c` is a SHELL_MARKERS member (the
        // anti-`sh -c` invariant), so the effort override must use codex's
        // documented long form to keep `assert_no_shell_markers` passing.
        let pos = cmd
            .args
            .iter()
            .position(|a| a == "--config")
            .expect("must carry --config flag");
        assert_eq!(
            cmd.args.get(pos + 1).map(String::as_str),
            Some(format!("{CODEX_REASONING_EFFORT_KEY}=\"high\"").as_str())
        );
        // The short-form `-c` must NOT appear (it would trip the shell-marker guard).
        assert!(!cmd.args.iter().any(|a| a == "-c"), "{:?}", cmd.args);
        // The prompt's "--" terminator + trailing prompt arg must still be intact
        // regardless of the extra --config pair inserted before them.
        assert_eq!(cmd.args.last().map(String::as_str), Some("x"));
        assert_eq!(cmd.args.get(cmd.args.len() - 2).map(String::as_str), Some("--"));
        // The load-bearing security invariant must still hold WITH the override present.
        assert_no_shell_markers(&cmd);
    }

    #[test]
    fn agy_command_ignores_effort_param_it_has_no_such_knob() {
        let cmd = build_command(Provider::Agy, "x", false, Some("high"));
        assert!(
            !cmd.args.iter().any(|a| a == "high" || a.contains(CODEX_REASONING_EFFORT_KEY)),
            "agy has no effort knob and must ignore the parameter: {:?}",
            cmd.args
        );
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
