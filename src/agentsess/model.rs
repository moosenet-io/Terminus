//! Types describing a live coder CLI agent session (AGSS-01).
//!
//! These are the shapes every `agentsess_*` tool returns and that Harmony's
//! own `agent_sessions` module mirrors into its API. Keep every field
//! ADDITIVE-friendly: a consumer deserializing an older shape must not break
//! when a new field appears, so nothing here is `deny_unknown_fields` and
//! optional data is genuinely `Option`, never a sentinel value.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Which coder CLI is running this session.
///
/// `Other` carries the matched program name so an unrecognised-but-configured
/// agent still reports something useful rather than being dropped.
/// Serializes as a bare string for the known agents (`"claude_code"`) and as
/// `{"other": "<program>"}` for an unrecognised one — externally tagged rather
/// than flattened into the session struct, because `flatten` + a tagged enum
/// is a known-fragile serde combination and this shape needs to stay stable
/// for Harmony's deserializer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentKind {
    ClaudeCode,
    Codex,
    Aider,
    Other(String),
}

impl AgentKind {
    /// Classify a program name (argv[0]'s basename) against the built-in
    /// agent vocabulary. Returns `None` when the name matches no known agent.
    ///
    /// Matching is on the BASENAME only and is exact-or-prefix, never a
    /// substring search: a substring match would classify unrelated programs
    /// (`claude-backup.sh`, `my-aider-wrapper`) as agent sessions.
    pub fn classify(program: &str) -> Option<Self> {
        match program {
            "claude" => Some(Self::ClaudeCode),
            "codex" => Some(Self::Codex),
            "aider" => Some(Self::Aider),
            _ => None,
        }
    }

    /// Stable lowercase label, used in filters and log lines.
    pub fn label(&self) -> &str {
        match self {
            Self::ClaudeCode => "claude_code",
            Self::Codex => "codex",
            Self::Aider => "aider",
            Self::Other(p) => p,
        }
    }
}

/// The git repository a session is working in, when its cwd is inside one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoContext {
    /// Absolute path of the git toplevel.
    pub root: String,
    /// The repository's NAME only — deliberately not the remote URL, which
    /// can carry an internal host/IP (S1). Callers that need the remote must
    /// resolve it from their own config by name.
    pub repo_name: Option<String>,
    /// Current branch, or `None` on a detached HEAD.
    pub branch: Option<String>,
    /// A `PREFIX-NN` parsed out of the branch name when one is present. This
    /// is the join key Harmony uses to attach a session to its Plane item.
    pub item_hint: Option<String>,
}

/// The tmux pane a session can be observed through, when it has one.
///
/// tmux is an ATTACHMENT, not the unit of discovery — a session with no pane
/// is still a real, fully-reported session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionAttachment {
    pub tmux_session: String,
    pub window: String,
    pub pane: String,
    /// The canonical `session:window.pane` target string.
    pub target: String,
}

impl SessionAttachment {
    pub fn new(tmux_session: &str, window: &str, pane: &str) -> Self {
        Self {
            tmux_session: tmux_session.to_string(),
            window: window.to_string(),
            pane: pane.to_string(),
            target: format!("{tmux_session}:{window}.{pane}"),
        }
    }
}

/// One live coder CLI agent session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSession {
    /// Stable-ish identifier: the transcript UUID when the agent has one
    /// (survives across a pane move), else `<host>-p<pid>`.
    pub id: String,
    pub kind: AgentKind,
    pub pid: i32,
    /// Which host this session was observed on (`local`, or the dev host label).
    pub host: String,
    pub cwd: Option<String>,
    pub repo: Option<RepoContext>,
    pub attachment: Option<SessionAttachment>,
    pub started_at: Option<DateTime<Utc>>,
    /// Most recent evidence of activity — the transcript's mtime when there is
    /// one. `None` means "no activity signal available", which a consumer must
    /// render differently from "idle for a long time".
    pub last_activity_at: Option<DateTime<Utc>>,
    pub transcript_path: Option<String>,
}

/// The full result of a discovery pass.
///
/// `warnings` is load-bearing: a probe that could not run (no tmux, unreadable
/// transcript root) degrades to empty AND says so here, so a caller can always
/// distinguish "nothing found" from "could not look".
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionsSnapshot {
    pub sessions: Vec<AgentSession>,
    pub host: String,
    pub warnings: Vec<String>,
    /// True when the result was capped by `AGENTSESS_MAX_SESSIONS`. A silent
    /// cap would read as "that is all of them", which is exactly wrong.
    pub truncated: bool,
}

/// Parse a `PREFIX-NN` work-item hint out of a branch name.
///
/// Accepts the shapes the build pipeline actually produces —
/// `AGSS-01-agentsess-core`, `feat/AGSS-01-thing`, `AGSS-01` — and requires
/// the 2-8 uppercase-leading prefix shape the spec registry enforces, so an
/// ordinary branch like `fix-2-bugs` does not produce a bogus hint.
pub fn parse_item_hint(branch: &str) -> Option<String> {
    let segment = branch.rsplit('/').next().unwrap_or(branch);
    let bytes = segment.as_bytes();
    // Find the first `-` that separates an all-uppercase prefix from digits.
    for (i, _) in segment.match_indices('-') {
        let (prefix, rest) = (&segment[..i], &segment[i + 1..]);
        if !(2..=8).contains(&prefix.len()) {
            continue;
        }
        if !prefix.starts_with(|c: char| c.is_ascii_uppercase()) {
            continue;
        }
        if !prefix
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        {
            continue;
        }
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            continue;
        }
        // The digits must end the segment or be followed by a separator —
        // otherwise `ABC-1x` would wrongly yield `ABC-1`.
        let after = &rest[digits.len()..];
        if after.is_empty() || after.starts_with('-') || after.starts_with('_') {
            let _ = bytes;
            return Some(format!("{prefix}-{digits}"));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_matches_known_agents_only() {
        assert_eq!(AgentKind::classify("claude"), Some(AgentKind::ClaudeCode));
        assert_eq!(AgentKind::classify("codex"), Some(AgentKind::Codex));
        assert_eq!(AgentKind::classify("aider"), Some(AgentKind::Aider));
        // Substring-shaped impostors must NOT classify.
        assert_eq!(AgentKind::classify("claude-backup.sh"), None);
        assert_eq!(AgentKind::classify("my-aider-wrapper"), None);
        assert_eq!(AgentKind::classify("bash"), None);
    }

    #[test]
    fn item_hint_parses_pipeline_branch_shapes() {
        assert_eq!(
            parse_item_hint("AGSS-01-agentsess-core").as_deref(),
            Some("AGSS-01")
        );
        assert_eq!(
            parse_item_hint("feat/HAGS-12-sessions-view").as_deref(),
            Some("HAGS-12")
        );
        assert_eq!(parse_item_hint("TERM-7").as_deref(), Some("TERM-7"));
        assert_eq!(parse_item_hint("COND2-03-thing").as_deref(), Some("COND2-03"));
    }

    #[test]
    fn item_hint_rejects_ordinary_branches() {
        assert_eq!(parse_item_hint("main"), None);
        assert_eq!(parse_item_hint("fix-2-bugs"), None);
        assert_eq!(parse_item_hint("feature/add-thing"), None);
        // Lowercase prefix is not a work-item prefix.
        assert_eq!(parse_item_hint("abc-01-thing"), None);
        // Digits immediately followed by a letter are not an item number.
        assert_eq!(parse_item_hint("ABC-1x-thing"), None);
        // Over-long prefix is outside the registry's 2-8 char rule.
        assert_eq!(parse_item_hint("TOOLONGPREFIX-01"), None);
    }

    #[test]
    fn attachment_builds_canonical_target() {
        let a = SessionAttachment::new("build", "0", "1");
        assert_eq!(a.target, "build:0.1");
    }

    #[test]
    fn session_serializes_kind_as_bare_string() {
        let s = AgentSession {
            id: "abc".into(),
            kind: AgentKind::ClaudeCode,
            pid: 42,
            host: "local".into(),
            cwd: None,
            repo: None,
            attachment: None,
            started_at: None,
            last_activity_at: None,
            transcript_path: None,
        };
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["kind"], "claude_code");
        assert_eq!(v["pid"], 42);
    }
}
