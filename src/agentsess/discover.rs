//! Discovery of live coder CLI agent sessions on a host (AGSS-01).
//!
//! ## Why discovery is process-first, not tmux-first
//! Coder CLI agents are NOT reliably launched one-per-named-tmux-session: a
//! host may run several inside one pane, or run them with no tmux at all. A
//! tmux-driven enumerator therefore observes almost nothing. The unit of
//! discovery here is the agent PROCESS; tmux is an optional ATTACHMENT that
//! tells us which pane a session can be watched through.
//!
//! ## The probes, and what each contributes
//! 1. **processes** — the candidate set, plus pid/ppid/start time.
//! 2. **cwd** — where each agent is working, via `/proc/<pid>/cwd`.
//! 3. **git** — the repo root, branch, and the `PREFIX-NN` item hint parsed
//!    from the branch, which is the join key Harmony uses to reach Plane.
//! 4. **tmux** — pane attachment, matched by pane pid or ancestry.
//! 5. **transcripts** — the live activity signal (see below).
//!
//! Every probe is independent and DEGRADES ALONE: a host with no tmux, no
//! readable transcript root, or no git still yields a useful session list,
//! with the shortfall named in `warnings` rather than swallowed. That is
//! deliberate — an observability tool that silently under-reports is worse
//! than one that says what it could not see.
//!
//! ## Session↔transcript matching, and its one heuristic
//! Claude Code exports `CLAUDE_CODE_SESSION_ID` into its own environment, so
//! the exact session UUID is read from `/proc/<pid>/environ` — an exact match,
//! not a guess. **Only that single variable is ever extracted; no other
//! environment content is read into the result, logged, or returned**, because
//! a process environment routinely holds credentials.
//!
//! When that is unavailable (a non-Claude agent, or an unreadable environ) we
//! fall back to matching the transcript directory slug against the session's
//! cwd. That fallback is a HEURISTIC and is ambiguous when two agents share
//! one cwd; in that case the newest transcript wins and a warning says so.

use std::collections::HashMap;

use chrono::{Duration, Utc};

use crate::error::ToolError;

use super::exec::HostExecutor;
use super::model::{parse_item_hint, AgentKind, AgentSession, RepoContext, SessionAttachment, SessionsSnapshot};

/// Subcommands that mean "this is the CLI's own helper process, not a session".
///
/// A DENY-list rather than an allow-list, deliberately: top-level invocation
/// shapes are open-ended (flags, a bare prompt, a resumed session), so an
/// allow-list would silently drop real sessions — the worse failure for an
/// observability tool. A new helper subcommand shows up as a spurious extra
/// session, which is visible and fixable; a dropped session is invisible.
const HELPER_SUBCOMMANDS: &[&str] = &[
    "daemon",
    "bg-pty-host",
    "bg-spare",
    "bg-worker",
    "mcp",
    "update",
    "install",
    "doctor",
    "migrate-installer",
];

fn max_sessions() -> usize {
    std::env::var("AGENTSESS_MAX_SESSIONS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(50)
}

/// Extra program names to treat as agents, comma-separated. They classify as
/// [`AgentKind::Other`] so an operator can surface a CLI this build predates.
fn extra_agent_programs() -> Vec<String> {
    std::env::var("AGENTSESS_AGENT_PATTERNS")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn transcript_root(is_local: bool) -> Result<String, String> {
    if let Ok(v) = std::env::var("AGENTSESS_TRANSCRIPT_ROOT") {
        if !v.trim().is_empty() {
            return Ok(v);
        }
    }
    if !is_local {
        // Do NOT assume the local HOME applies to a remote host — that would
        // silently probe the wrong path and report "no transcripts" as if it
        // were a fact about the remote box.
        return Err(
            "AGENTSESS_TRANSCRIPT_ROOT is not set — transcript activity is unavailable for a remote host"
                .to_string(),
        );
    }
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() => Ok(format!("{home}/.claude/projects")),
        _ => Err("neither AGENTSESS_TRANSCRIPT_ROOT nor HOME is set — transcript activity unavailable".into()),
    }
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// The directory-slug convention Claude Code uses for a session's cwd:
/// every `/` and `.` becomes `-`, so `/home/u/repos/X` → `-home-u-repos-X`.
pub(crate) fn cwd_slug(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c == '/' || c == '.' { '-' } else { c })
        .collect()
}

#[derive(Debug, Clone)]
struct ProcRow {
    pid: i32,
    ppid: i32,
    etimes: i64,
    args: String,
}

/// Parse `ps -eo pid=,ppid=,etimes=,args=` output.
fn parse_ps(stdout: &str) -> Vec<ProcRow> {
    let mut rows = Vec::new();
    for line in stdout.lines() {
        let line = line.trim_start();
        let mut it = line.splitn(4, char::is_whitespace).filter(|s| !s.is_empty());
        // `splitn` with a whitespace predicate can yield empty fragments when
        // columns are space-padded, so re-split defensively.
        let mut fields = line.split_whitespace();
        let (pid, ppid, etimes) = match (fields.next(), fields.next(), fields.next()) {
            (Some(a), Some(b), Some(c)) => (a, b, c),
            _ => continue,
        };
        let (pid, ppid, etimes) = match (pid.parse(), ppid.parse(), etimes.parse()) {
            (Ok(p), Ok(pp), Ok(e)) => (p, pp, e),
            _ => continue,
        };
        // Everything after the third column is the command line, preserved verbatim.
        let args = {
            let mut seen = 0usize;
            let mut idx = 0usize;
            let bytes = line.as_bytes();
            while idx < bytes.len() && seen < 3 {
                while idx < bytes.len() && bytes[idx].is_ascii_whitespace() {
                    idx += 1;
                }
                while idx < bytes.len() && !bytes[idx].is_ascii_whitespace() {
                    idx += 1;
                }
                seen += 1;
            }
            line[idx..].trim().to_string()
        };
        let _ = it.next();
        rows.push(ProcRow { pid, ppid, etimes, args });
    }
    rows
}

/// Decide whether a process row is a TOP-LEVEL agent session.
///
/// Returns the classified kind, or `None` for "not an agent" / "an agent
/// CLI's own helper process". Tested in both directions — this predicate is
/// the part most likely to drift as the CLIs add subcommands.
pub(crate) fn classify_row(args: &str, extra: &[String]) -> Option<AgentKind> {
    let mut tokens = args.split_whitespace();
    let argv0 = tokens.next()?;
    let program = basename(argv0);

    let kind = AgentKind::classify(program).or_else(|| {
        extra
            .iter()
            .any(|p| p == program)
            .then(|| AgentKind::Other(program.to_string()))
    })?;

    // The first non-flag argument is the subcommand, if any.
    if let Some(sub) = tokens.find(|t| !t.starts_with('-')) {
        if HELPER_SUBCOMMANDS.contains(&sub) {
            return None;
        }
    }
    Some(kind)
}

/// Parse `tmux list-panes -a -F ...` output into `(pane_pid, attachment)`.
pub(crate) fn parse_tmux_panes(stdout: &str) -> Vec<(i32, SessionAttachment)> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 4 {
            continue;
        }
        let pid: i32 = match parts[3].trim().parse() {
            Ok(p) => p,
            Err(_) => continue,
        };
        out.push((pid, SessionAttachment::new(parts[0], parts[1], parts[2])));
    }
    out
}

/// Extract ONLY `CLAUDE_CODE_SESSION_ID` from a NUL-separated environ blob.
///
/// Everything else in the blob is discarded unread — a process environment
/// routinely carries credentials, so nothing but this one value may escape.
pub(crate) fn session_id_from_environ(environ: &str) -> Option<String> {
    environ
        .split('\0')
        .find_map(|kv| kv.strip_prefix("CLAUDE_CODE_SESSION_ID="))
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

/// Parse `find <root> -maxdepth 2 -name '*.jsonl' -printf '%T@ %p\n'` output
/// into `(dir_slug, path, mtime_epoch_secs)`, newest first.
pub(crate) fn parse_transcripts(stdout: &str) -> Vec<(String, String, i64)> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        let (ts, path) = match line.trim().split_once(' ') {
            Some(v) => v,
            None => continue,
        };
        let secs = ts
            .split_once('.')
            .map(|(w, _)| w)
            .unwrap_or(ts)
            .parse::<i64>();
        let secs = match secs {
            Ok(s) => s,
            Err(_) => continue,
        };
        // The slug is the transcript file's PARENT directory name.
        let parent = path.rsplitn(3, '/').nth(1).unwrap_or("").to_string();
        out.push((parent, path.to_string(), secs));
    }
    out.sort_by(|a, b| b.2.cmp(&a.2));
    out
}

/// Run a full discovery pass on `exec`.
pub async fn discover(
    exec: &dyn HostExecutor,
    repo_filter: Option<&str>,
) -> Result<SessionsSnapshot, ToolError> {
    let mut warnings: Vec<String> = Vec::new();
    let host = exec.host_label().to_string();
    let is_local = host == "local";

    // ---- 1. processes (the only probe whose failure is fatal) -------------
    let ps = exec
        .run(&["ps", "-eo", "pid=,ppid=,etimes=,args="])
        .await
        .map_err(|e| ToolError::Execution(format!("process probe failed on {host}: {e}")))?;
    if !ps.ok() {
        return Err(ToolError::Execution(format!(
            "process probe failed on {host}: {}",
            ps.stderr.trim()
        )));
    }
    let rows = parse_ps(&ps.stdout);
    let ppid_of: HashMap<i32, i32> = rows.iter().map(|r| (r.pid, r.ppid)).collect();

    let extra = extra_agent_programs();
    let mut candidates: Vec<(ProcRow, AgentKind)> = rows
        .iter()
        .filter_map(|r| classify_row(&r.args, &extra).map(|k| (r.clone(), k)))
        .collect();
    candidates.sort_by_key(|(r, _)| r.etimes);

    let cap = max_sessions();
    let truncated = candidates.len() > cap;
    if truncated {
        warnings.push(format!(
            "result capped at AGENTSESS_MAX_SESSIONS={cap} ({} agent processes seen)",
            candidates.len()
        ));
        candidates.truncate(cap);
    }

    // ---- 2. tmux panes (optional) ----------------------------------------
    let panes = match exec
        .run(&[
            "tmux",
            "list-panes",
            "-a",
            "-F",
            "#{session_name}\t#{window_index}\t#{pane_index}\t#{pane_pid}",
        ])
        .await
    {
        Ok(o) if o.ok() => parse_tmux_panes(&o.stdout),
        Ok(o) => {
            // A tmux with no server running exits non-zero; that is a normal
            // "no attachments", not a failure worth alarming about.
            warnings.push(format!("tmux unavailable: {}", o.stderr.trim()));
            Vec::new()
        }
        Err(e) => {
            warnings.push(format!("tmux unavailable: {e}"));
            Vec::new()
        }
    };
    let pane_by_pid: HashMap<i32, SessionAttachment> = panes.into_iter().collect();

    // ---- 3. transcripts (optional) ---------------------------------------
    let mut transcripts: Vec<(String, String, i64)> = Vec::new();
    match transcript_root(is_local) {
        Ok(root) => {
            match exec
                .run(&[
                    "find",
                    &root,
                    "-maxdepth",
                    "2",
                    "-name",
                    "*.jsonl",
                    "-printf",
                    "%T@ %p\n",
                ])
                .await
            {
                Ok(o) if o.ok() => transcripts = parse_transcripts(&o.stdout),
                Ok(o) => warnings.push(format!(
                    "transcript root '{root}' unreadable: {}",
                    o.stderr.trim()
                )),
                Err(e) => warnings.push(format!("transcript scan failed: {e}")),
            }
        }
        Err(msg) => warnings.push(msg),
    }
    let by_uuid: HashMap<&str, &(String, String, i64)> = transcripts
        .iter()
        .map(|t| (basename(&t.1).trim_end_matches(".jsonl"), t))
        .collect();

    // ---- 4. per-session enrichment ---------------------------------------
    let now = Utc::now();
    let mut git_cache: HashMap<String, Option<RepoContext>> = HashMap::new();
    let mut sessions = Vec::new();
    let mut ambiguous_cwds: Vec<String> = Vec::new();

    for (row, kind) in candidates {
        // cwd
        let cwd = match exec
            .run(&["readlink", "-f", &format!("/proc/{}/cwd", row.pid)])
            .await
        {
            Ok(o) if o.ok() && !o.stdout.trim().is_empty() => Some(o.stdout.trim().to_string()),
            // A pid that exited between probes, or one we cannot inspect —
            // drop the cwd rather than the session.
            _ => None,
        };

        // repo context (cached per cwd — several agents often share one)
        let repo = match &cwd {
            Some(c) => {
                if let Some(hit) = git_cache.get(c) {
                    hit.clone()
                } else {
                    let ctx = git_context(exec, c).await;
                    git_cache.insert(c.clone(), ctx.clone());
                    ctx
                }
            }
            None => None,
        };

        if let Some(filter) = repo_filter {
            let matches = repo
                .as_ref()
                .and_then(|r| r.repo_name.as_deref())
                .map(|n| n == filter)
                .unwrap_or(false);
            if !matches {
                continue;
            }
        }

        // transcript: exact via the exported session id, else the cwd-slug heuristic
        let exact_id = match exec
            .run(&["cat", &format!("/proc/{}/environ", row.pid)])
            .await
        {
            Ok(o) if o.ok() => session_id_from_environ(&o.stdout),
            _ => None,
        };

        let transcript = match exact_id.as_deref().and_then(|id| by_uuid.get(id)) {
            Some(t) => Some((*t).clone()),
            None => cwd.as_ref().and_then(|c| {
                let slug = cwd_slug(c);
                let mut matches = transcripts.iter().filter(|t| t.0 == slug);
                let first = matches.next().cloned();
                if first.is_some() && matches.next().is_some() && !ambiguous_cwds.contains(c) {
                    ambiguous_cwds.push(c.clone());
                }
                first
            }),
        };

        // tmux attachment: the pane whose pid is this process or an ancestor of it
        let attachment = find_attachment(row.pid, &ppid_of, &pane_by_pid);

        let id = exact_id
            .clone()
            .unwrap_or_else(|| format!("{host}-p{}", row.pid));

        sessions.push(AgentSession {
            id,
            kind,
            pid: row.pid,
            host: host.clone(),
            cwd,
            repo,
            attachment,
            started_at: Some(now - Duration::seconds(row.etimes)),
            last_activity_at: transcript.as_ref().and_then(|t| {
                chrono::DateTime::from_timestamp(t.2, 0).map(|d| {
                    // Clock skew must never surface as a negative age.
                    if d > now {
                        now
                    } else {
                        d
                    }
                })
            }),
            transcript_path: transcript.map(|t| t.1),
        });
    }

    for cwd in ambiguous_cwds {
        warnings.push(format!(
            "more than one transcript matches '{cwd}'; the newest was used — activity for agents sharing a working directory may be misattributed"
        ));
    }

    sessions.sort_by(|a, b| b.last_activity_at.cmp(&a.last_activity_at));

    Ok(SessionsSnapshot {
        sessions,
        host,
        warnings,
        truncated,
    })
}

/// Walk up the process tree from `pid` looking for a pid that owns a tmux pane.
fn find_attachment(
    pid: i32,
    ppid_of: &HashMap<i32, i32>,
    pane_by_pid: &HashMap<i32, SessionAttachment>,
) -> Option<SessionAttachment> {
    let mut cur = pid;
    // Bounded walk — a corrupt ppid map must not loop forever.
    for _ in 0..32 {
        if let Some(a) = pane_by_pid.get(&cur) {
            return Some(a.clone());
        }
        match ppid_of.get(&cur) {
            Some(&next) if next > 1 && next != cur => cur = next,
            _ => break,
        }
    }
    None
}

/// Resolve the git toplevel + branch for a working directory.
async fn git_context(exec: &dyn HostExecutor, cwd: &str) -> Option<RepoContext> {
    let out = exec
        .run(&[
            "git",
            "-C",
            cwd,
            "rev-parse",
            "--show-toplevel",
            "--abbrev-ref",
            "HEAD",
        ])
        .await
        .ok()?;
    if !out.ok() {
        return None;
    }
    let mut lines = out.stdout.lines();
    let root = lines.next()?.trim().to_string();
    if root.is_empty() {
        return None;
    }
    let branch = lines
        .next()
        .map(str::trim)
        .filter(|b| !b.is_empty() && *b != "HEAD") // detached HEAD
        .map(str::to_string);
    Some(RepoContext {
        repo_name: Some(basename(&root).to_string()),
        item_hint: branch.as_deref().and_then(parse_item_hint),
        branch,
        root,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agentsess::exec::test_support::FakeExecutor;

    const PS_SAMPLE: &str = "\
   1402      1 185000 claude --dangerously-skip-permissions
   1538   1402 184990 /home/u/.npm/bin/claude.exe daemon run --origin transient
   1801   1402 184980 claude bg-pty-host --bg-pty-host /tmp/x.sock 200 50
   1824   1402 184970 claude bg-spare --bg-spare /tmp/y.sock
   2100      1  10000 codex exec review
   2200      1   5000 aider --model gpt
   3000      1    100 bash -lc 'claude something'
";

    #[test]
    fn ps_parsing_extracts_all_columns() {
        let rows = parse_ps(PS_SAMPLE);
        assert_eq!(rows.len(), 7);
        assert_eq!(rows[0].pid, 1402);
        assert_eq!(rows[0].ppid, 1);
        assert_eq!(rows[0].etimes, 185000);
        assert_eq!(rows[0].args, "claude --dangerously-skip-permissions");
    }

    #[test]
    fn classify_accepts_top_level_sessions() {
        let extra: Vec<String> = vec![];
        assert_eq!(
            classify_row("claude --dangerously-skip-permissions", &extra),
            Some(AgentKind::ClaudeCode)
        );
        assert_eq!(classify_row("codex exec review", &extra), Some(AgentKind::Codex));
        assert_eq!(classify_row("aider --model gpt", &extra), Some(AgentKind::Aider));
        // A bare invocation with no subcommand at all is still a session.
        assert_eq!(classify_row("claude", &extra), Some(AgentKind::ClaudeCode));
    }

    #[test]
    fn classify_rejects_helper_processes() {
        let extra: Vec<String> = vec![];
        // The daemon binary basename does not classify at all.
        assert_eq!(classify_row("/home/u/.npm/bin/claude.exe daemon run", &extra), None);
        // These share the `claude` basename but are helper subcommands.
        assert_eq!(classify_row("claude bg-pty-host --bg-pty-host /tmp/x", &extra), None);
        assert_eq!(classify_row("claude bg-spare --bg-spare /tmp/y", &extra), None);
        assert_eq!(classify_row("claude mcp list", &extra), None);
        // Unrelated programs never classify.
        assert_eq!(classify_row("bash -lc 'claude something'", &extra), None);
        assert_eq!(classify_row("", &extra), None);
    }

    #[test]
    fn classify_honours_extra_configured_programs() {
        let extra = vec!["mycoder".to_string()];
        assert_eq!(
            classify_row("mycoder --go", &extra),
            Some(AgentKind::Other("mycoder".into()))
        );
        assert_eq!(classify_row("othercoder --go", &extra), None);
    }

    #[test]
    fn tmux_pane_format_parses() {
        let out = "build\t0\t1\t4242\nother\t2\t0\t99\nmalformed-line\n";
        let panes = parse_tmux_panes(out);
        assert_eq!(panes.len(), 2);
        assert_eq!(panes[0].0, 4242);
        assert_eq!(panes[0].1.target, "build:0.1");
        assert_eq!(panes[1].1.target, "other:2.0");
    }

    #[test]
    fn environ_yields_only_the_session_id() {
        // The neighbouring variables stand in for the credentials a real
        // process environment carries; the point of the test is that NONE of
        // them can leave this function. Deliberately not spelled with a
        // credential-shaped value — a fixture that trips the repo's own PII
        // gate would only teach the next author to reach for an exemption.
        let env =
            "PATH=/usr/bin\0A_TOKEN_VAR=placeholder\0CLAUDE_CODE_SESSION_ID=abc-123\0HOME=/root\0";
        assert_eq!(session_id_from_environ(env).as_deref(), Some("abc-123"));
        // No other variable is reachable through this function at all.
        assert_eq!(session_id_from_environ("PATH=/usr/bin\0"), None);
        assert_eq!(session_id_from_environ("CLAUDE_CODE_SESSION_ID=\0"), None);
    }

    #[test]
    fn cwd_slug_matches_the_transcript_directory_convention() {
        assert_eq!(cwd_slug("/home/u"), "-home-u");
        assert_eq!(cwd_slug("/home/u/repos/Terminus"), "-home-u-repos-Terminus");
        // A dot in a path segment also becomes a dash, yielding a double dash.
        assert_eq!(cwd_slug("/home/u/.claude/wt"), "-home-u--claude-wt");
    }

    #[test]
    fn transcript_listing_parses_and_sorts_newest_first() {
        let out = "1785563670.12 /r/-home-u/aaa.jsonl\n1785563999.00 /r/-home-u/bbb.jsonl\ngarbage\n";
        let t = parse_transcripts(out);
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].1, "/r/-home-u/bbb.jsonl");
        assert_eq!(t[0].0, "-home-u");
        assert_eq!(t[0].2, 1785563999);
    }

    #[test]
    fn attachment_matches_through_process_ancestry() {
        let ppid: HashMap<i32, i32> = [(500, 400), (400, 300), (300, 1)].into_iter().collect();
        let panes: HashMap<i32, SessionAttachment> =
            [(300, SessionAttachment::new("s", "0", "0"))].into_iter().collect();
        // The agent is a grandchild of the pane's process.
        assert_eq!(
            find_attachment(500, &ppid, &panes).unwrap().target,
            "s:0.0"
        );
        // An unrelated pid gets no attachment.
        assert!(find_attachment(999, &ppid, &panes).is_none());
    }

    #[test]
    fn attachment_walk_terminates_on_a_cyclic_ppid_map() {
        let ppid: HashMap<i32, i32> = [(10, 20), (20, 10)].into_iter().collect();
        let panes: HashMap<i32, SessionAttachment> = HashMap::new();
        assert!(find_attachment(10, &ppid, &panes).is_none());
    }

    #[tokio::test]
    async fn discovery_degrades_when_tmux_and_transcripts_are_absent() {
        let exec = FakeExecutor::new()
            .with_stdout("ps", PS_SAMPLE)
            .with_stdout("readlink", "/work/repo\n")
            .with_failure("git", "not a repo");
        // tmux / find / cat are unregistered → NotFound, the absent-probe path.
        let snap = discover(&exec, None).await.unwrap();
        // Three real sessions; the four helper/unrelated rows are excluded.
        assert_eq!(snap.sessions.len(), 3, "{:?}", snap.sessions);
        assert!(snap.sessions.iter().all(|s| s.attachment.is_none()));
        assert!(snap.warnings.iter().any(|w| w.contains("tmux unavailable")));
        assert!(!snap.truncated);
    }

    #[tokio::test]
    async fn discovery_fails_only_when_the_process_probe_fails() {
        let exec = FakeExecutor::new().with_failure("ps", "boom");
        assert!(discover(&exec, None).await.is_err());
    }

    #[tokio::test]
    async fn repo_filter_excludes_non_matching_sessions() {
        let exec = FakeExecutor::new()
            .with_stdout("ps", PS_SAMPLE)
            .with_stdout("readlink", "/work/Terminus\n")
            .with_stdout("git", "/work/Terminus\nAGSS-01-core\n");
        let all = discover(&exec, None).await.unwrap();
        assert_eq!(all.sessions.len(), 3);
        let hint = all.sessions[0].repo.as_ref().unwrap().item_hint.clone();
        assert_eq!(hint.as_deref(), Some("AGSS-01"));

        let filtered = discover(&exec, Some("Terminus")).await.unwrap();
        assert_eq!(filtered.sessions.len(), 3);
        let none = discover(&exec, Some("Harmony")).await.unwrap();
        assert_eq!(none.sessions.len(), 0);
    }
}
