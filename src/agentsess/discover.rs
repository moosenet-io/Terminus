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
//! not a guess.
//!
//! **The filter is applied AT THE SOURCE, not after the fact.** The probe is a
//! NUL-delimited `grep` for that one variable, so the other environment
//! entries are never materialised into this process's memory — and, on the
//! remote path, never cross the SSH connection at all. Reading the whole
//! environ blob and discarding the rest afterwards would look equivalent and
//! is not: a process environment routinely holds credentials, and "we threw
//! them away after copying them over the network" is not a privacy property.
//!
//! When that is unavailable (a non-Claude agent, or an unreadable environ) we
//! fall back to matching the transcript directory slug against the session's
//! cwd. That fallback is a HEURISTIC and is ambiguous when two agents share
//! one cwd; in that case the newest transcript wins and a warning says so.

use std::collections::HashMap;

use chrono::{DateTime, Duration, Timelike, Utc};

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

/// Derive a process's start time from `ps -o etimes=` (elapsed seconds).
///
/// TERM #605 — **the result must be no more precise than its input justifies.**
/// `etimes` has WHOLE-SECOND resolution. Subtracting it from a full-precision
/// `Utc::now()` produced a value that carried the SUBSECOND component of
/// whichever instant the poll happened to fire at — so an entirely unchanged
/// process reported a different `started_at` on every discovery pass, drifting
/// by up to a second. The extra digits were noise from the OBSERVER, not
/// information about the OBSERVED, and they read as real precision downstream:
/// any consumer diffing snapshots to detect change saw every session as
/// "changed" every poll. Harmony's websocket dedup had to exclude the field
/// entirely to stop an idle fleet broadcasting continuously (HARM #445) — a
/// workaround at the consumer for a defect at the producer, which every future
/// consumer would have hit in turn. Fixing it here means no consumer has to.
///
/// So: truncate `now` to a whole second BEFORE subtracting.
///
/// **This is the FALLBACK, and its residual is stated rather than hidden.**
/// `etimes` is `floor(now - start)`, so the derived value is
/// `floor(now) - floor(now - start)`, which for a process whose true start has a
/// fractional part still ALTERNATES between two adjacent seconds depending on
/// where the poll lands relative to that fraction. Truncation therefore reduces
/// the jitter from arbitrary-subsecond to at most ONE SECOND — a real
/// improvement, but not stability. (gpt56 review, TERM #605.)
///
/// The stable path is [`start_epoch_from_proc`], which reads the process's own
/// `starttime` from `/proc` and depends on nothing that changes between polls.
/// This helper is used only when that is unavailable (an unreadable `/proc`, a
/// remote host that answered `ps` but not `cat`), where a bounded 1s wobble
/// still beats an unbounded subsecond one.
///
/// (`last_activity_at`, the other timestamp on a session, is NOT in this class:
/// it comes from a transcript file's mtime read as whole seconds
/// (`DateTime::from_timestamp(secs, 0)`), so it is already whole-second and
/// stable for an unchanged file. Reviewed, not changed.)
/// Fallback clock-tick rate when the observed host will not tell us its own.
///
/// `/proc/<pid>/stat`'s `starttime` is in the host's USER_HZ
/// (`sysconf(_SC_CLK_TCK)`), which is 100 on every Linux the fleet runs but is
/// NOT universally 100 — alpha uses 1024, and a wrong divisor silently yields a
/// wrong start time rather than an error. So the rate is READ from the host
/// (`getconf CLK_TCK`, which works identically over the local and ssh
/// executors) and this constant is only the fallback when that probe fails.
/// (gpt56/opus review, TERM #605.)
const DEFAULT_USER_HZ: u64 = 100;

/// Parse `getconf CLK_TCK` output into a usable tick rate.
///
/// Fails CLOSED to [`DEFAULT_USER_HZ`] on anything unparseable or non-positive:
/// a zero would divide-by-zero and a negative is meaningless, and neither is
/// worth failing a whole listing over when the fallback is right on every host
/// the fleet actually has.
fn parse_clock_ticks(stdout: &str) -> u64 {
    stdout
        .trim()
        .parse::<u64>()
        .ok()
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_USER_HZ)
}

/// Parse a batched `cat /proc/stat /proc/<pid>/stat ...` into the boot time and
/// each pid's `starttime` (field 22, in the host's clock ticks since boot).
///
/// The lines are distinguishable without separators: `/proc/stat`'s lines all
/// begin with a WORD (`cpu`, `btime`, `intr`, …) while a `/proc/<pid>/stat` line
/// begins with the numeric pid.
///
/// The `comm` field (2nd) is the process name IN PARENTHESES and may itself
/// contain spaces and parentheses, so fields are taken from after the LAST `)` —
/// the standard way to parse this file. A naive whitespace split silently reads
/// the wrong field for a process whose name contains a space.
fn parse_proc_starttimes(stdout: &str) -> (Option<i64>, HashMap<i32, u64>) {
    let mut btime = None;
    let mut starts: HashMap<i32, u64> = HashMap::new();
    for line in stdout.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("btime ") {
            btime = rest.trim().parse::<i64>().ok();
            continue;
        }
        let Some(pid) = line.split_whitespace().next().and_then(|t| t.parse::<i32>().ok()) else {
            continue;
        };
        let Some(close) = line.rfind(')') else { continue };
        // After `) ` the fields are 3.. of the file, so `starttime` (field 22)
        // is index 19 here.
        if let Some(ticks) = line[close + 1..]
            .split_whitespace()
            .nth(19)
            .and_then(|t| t.parse::<u64>().ok())
        {
            starts.insert(pid, ticks);
        }
    }
    (btime, starts)
}

/// The process's start time to WHOLE-SECOND resolution: boot time plus its
/// `starttime` ticks, floored to a second.
///
/// Not "exact" to the tick — the sub-second remainder is deliberately dropped,
/// because a whole second is the resolution this field is contracted to report
/// (see [`started_at_from_etimes`]) and keeping the fraction would put back
/// precision consumers must not diff on. What matters is that it depends on
/// NOTHING that changes between polls: boot time and the process's own start
/// tick are both fixed for the life of the process, so the value is stable by
/// construction rather than by rounding. (gpt56 review corrected an earlier
/// "exact" framing here.)
///
/// `hz` is the host's clock-tick rate; a zero can never reach here
/// ([`parse_clock_ticks`] floors it), but it is guarded anyway so a future
/// caller cannot divide by zero.
fn start_epoch_from_proc(btime: i64, ticks: u64, hz: u64) -> i64 {
    let hz = hz.max(1);
    btime.saturating_add((ticks / hz) as i64)
}

fn started_at_from_etimes(now: DateTime<Utc>, etimes: i64) -> DateTime<Utc> {
    // `with_nanosecond(0)` only returns None for an out-of-range value, which 0
    // never is; the fallback keeps this total rather than panicking.
    let whole_second_now = now.with_nanosecond(0).unwrap_or(now);
    // TOTAL for any i64 (gpt56 review): `Duration::seconds` PANICS outside its
    // range and `-` PANICS on date overflow. `etimes` is parsed from `ps`
    // output, so a hostile or corrupt line must degrade, never abort the whole
    // listing. A nonsensical value falls back to `now` — visibly wrong for that
    // one session rather than fatal for every session on the host.
    // A NEGATIVE elapsed time is nonsense — `ps` never emits one, so it means a
    // corrupt or hostile line. Subtracting it would place the process's start in
    // the FUTURE, which is worse than useless: it reads as a real fact. Degrade
    // to `now` like any other unusable value (gpt56 review).
    if etimes < 0 {
        return whole_second_now;
    }
    Duration::try_seconds(etimes)
        .and_then(|d| whole_second_now.checked_sub_signed(d))
        .unwrap_or(whole_second_now)
}

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

/// Validate a transcript root before it is ever handed to `find`.
///
/// argv form stops SHELL injection but not OPTION injection: `find` parses
/// leading-dash arguments as options, and with no starting path GNU `find`
/// silently defaults to `.` — so a root of `-delete` would be read as an
/// action against the current directory rather than a path. Requiring an
/// absolute path closes that off entirely, and is true of every real root.
fn validate_transcript_root(root: &str) -> Result<(), String> {
    if !root.starts_with('/') {
        return Err(format!(
            "AGENTSESS_TRANSCRIPT_ROOT must be an absolute path (got '{root}') — a relative or \
             option-shaped value is refused because `find` would interpret it as an option"
        ));
    }
    Ok(())
}

/// The transcript root for whichever host `exec` observes — the same
/// resolution [`discover`] uses, exposed so the transcript tool jails an
/// explicit caller-supplied path against the SAME root rather than deriving
/// its own (two roots would mean two jails, and the weaker one would win).
pub(crate) fn transcript_root_for(exec: &dyn HostExecutor) -> Result<String, String> {
    transcript_root(exec.host_label() == "local")
}

fn transcript_root(is_local: bool) -> Result<String, String> {
    if let Ok(v) = std::env::var("AGENTSESS_TRANSCRIPT_ROOT") {
        if !v.trim().is_empty() {
            validate_transcript_root(&v)?;
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
        Ok(home) if !home.is_empty() => {
            let root = format!("{home}/.claude/projects");
            validate_transcript_root(&root)?;
            Ok(root)
        }
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

/// Parse the session id out of the environ probe's output.
///
/// The probe (see [`session_id_probe_argv`]) already filters to the single
/// matching NUL-delimited entry at the source, so this normally sees exactly
/// one record. It still splits on NUL and matches the prefix explicitly, so a
/// grep that returned more than expected cannot smuggle another variable's
/// value through — nothing but `CLAUDE_CODE_SESSION_ID` can ever be returned.
pub(crate) fn session_id_from_environ(environ: &str) -> Option<String> {
    environ
        .split('\0')
        .find_map(|kv| kv.strip_prefix("CLAUDE_CODE_SESSION_ID="))
        .map(|v| v.trim_end_matches('\n'))
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

/// The argv that reads ONLY the session-id entry out of a process environ.
///
/// `/proc/<pid>/environ` is NUL-delimited, so `grep -z` treats each `KEY=VALUE`
/// as its own record and `-m1` stops at the first match. `-a` forces text mode
/// on what grep would otherwise call a binary file. The result is that a single
/// short record leaves the target host — not the whole environment.
fn session_id_probe_argv(pid: i32) -> [String; 6] {
    [
        "grep".into(),
        "-a".into(),
        "-z".into(),
        "-m1".into(),
        "^CLAUDE_CODE_SESSION_ID=".into(),
        format!("/proc/{pid}/environ"),
    ]
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
pub(crate) async fn discover(
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

    // NOTE the ordering: the result cap is applied at the END, AFTER the repo
    // filter. Capping the candidate list here instead would let
    // `agentsess_list({repo: "X"})` return zero sessions while sessions in X
    // were live, simply because they sorted beyond the cap — a filter that
    // silently hides matches is worse than no filter.

    // ---- 1b. EXACT process start times (optional, TERM #605) --------------
    // `ps -o etimes=` is whole-second ELAPSED time, so a start time derived from
    // it is a function of WHEN THE POLL RAN and wobbles by up to a second for an
    // unchanged process. `/proc/<pid>/stat`'s `starttime` plus `/proc/stat`'s
    // `btime` is the process's OWN start, identical on every pass — the value
    // consumers can safely diff (HARM #445).
    //
    // ONE batched `cat` for every candidate, not one probe per pid: this runs on
    // every listing, including over ssh. A failure here is NOT fatal — it falls
    // back to the etimes derivation, whose residual is documented on
    // `started_at_from_etimes`.
    // The host's own clock-tick rate; `getconf` is present on every Linux and
    // the probe degrades to the fleet-correct default if it is not.
    let user_hz = match exec.run(&["getconf", "CLK_TCK"]).await {
        Ok(o) if o.ok() => parse_clock_ticks(&o.stdout),
        _ => DEFAULT_USER_HZ,
    };
    let (boot_time, proc_starts) = {
        let mut paths: Vec<String> = vec!["/proc/stat".to_string()];
        paths.extend(candidates.iter().map(|(r, _)| format!("/proc/{}/stat", r.pid)));
        let mut argv: Vec<&str> = vec!["cat"];
        argv.extend(paths.iter().map(String::as_str));
        match exec.run(&argv).await {
            // `cat` exits non-zero when ANY file is missing (a process that
            // exited between the two probes — routine), while still printing the
            // rest. So the OUTPUT is used regardless of the exit status; only a
            // hard exec failure gives up.
            Ok(o) => parse_proc_starttimes(&o.stdout),
            Err(_) => (None, HashMap::new()),
        }
    };

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
    let mut git_probe_failed = false;

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
                    let (ctx, probe_ok) = git_context(exec, c).await;
                    git_probe_failed |= !probe_ok;
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

        // transcript: exact via the exported session id, else the cwd-slug
        // heuristic. The probe greps for that ONE variable inside the target
        // host, so the rest of the environment never reaches this process (and
        // on the remote path never crosses the wire).
        let probe = session_id_probe_argv(row.pid);
        let probe_argv: Vec<&str> = probe.iter().map(String::as_str).collect();
        let exact_id = match exec.run(&probe_argv).await {
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
            // TERM #605: an EXACT, poll-independent start time when /proc gave
            // us one; otherwise the whole-second etimes derivation (see
            // `started_at_from_etimes` for that path's stated residual).
            started_at: Some(
                boot_time
                    .zip(proc_starts.get(&row.pid).copied())
                    .and_then(|(b, ticks)| {
                        chrono::DateTime::from_timestamp(
                            start_epoch_from_proc(b, ticks, user_hz),
                            0,
                        )
                    })
                    .unwrap_or_else(|| started_at_from_etimes(now, row.etimes)),
            ),
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

    if git_probe_failed {
        // "not a repository" and "the git probe could not run" both produce
        // `repo: None`, which would otherwise be indistinguishable. Say which
        // happened rather than letting a missing git read as "no repos here".
        warnings.push(
            "the git probe failed for at least one working directory — some sessions may show no \
             repository/branch even though they are inside one"
                .to_string(),
        );
    }

    sessions.sort_by(|a, b| b.last_activity_at.cmp(&a.last_activity_at));

    // Cap LAST, so the repo filter above can never be starved by the cap.
    let cap = max_sessions();
    let truncated = sessions.len() > cap;
    if truncated {
        warnings.push(format!(
            "result capped at AGENTSESS_MAX_SESSIONS={cap} ({} sessions matched)",
            sessions.len()
        ));
        sessions.truncate(cap);
    }

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
///
/// Returns `(context, probe_ran)`. The second element distinguishes "this
/// directory is not in a repository" (probe ran, answered no — `(None, true)`)
/// from "the probe itself could not run" (`(None, false)`), which the caller
/// surfaces as a warning. Collapsing both into a bare `None` would let a
/// missing git binary read as "none of these sessions are in a repo".
async fn git_context(exec: &dyn HostExecutor, cwd: &str) -> (Option<RepoContext>, bool) {
    let out = match exec
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
    {
        Ok(o) => o,
        // git missing / unrunnable — a probe failure, not an answer.
        Err(_) => return (None, false),
    };
    if !out.ok() {
        // Only ONE non-zero outcome is git actually ANSWERING: "not a git
        // repository". Everything else — a missing binary (which the SSH path
        // surfaces as exit 127 rather than an Err), permission denied, dubious
        // ownership, a corrupt repo — means the probe could not answer, and
        // must be reported rather than silently rendered as "not in a repo".
        let stderr = out.stderr.to_ascii_lowercase();
        let answered_not_a_repo = stderr.contains("not a git repository");
        return (None, answered_not_a_repo);
    }
    let mut lines = out.stdout.lines();
    let root = match lines.next() {
        Some(r) if !r.trim().is_empty() => r.trim().to_string(),
        _ => return (None, true),
    };
    let branch = lines
        .next()
        .map(str::trim)
        .filter(|b| !b.is_empty() && *b != "HEAD") // detached HEAD
        .map(str::to_string);
    (
        Some(RepoContext {
            repo_name: Some(basename(&root).to_string()),
            item_hint: branch.as_deref().and_then(parse_item_hint),
            branch,
            root,
        }),
        true,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agentsess::exec::test_support::FakeExecutor;
    use serial_test::serial;

    const PS_SAMPLE: &str = "\
   1402      1 185000 claude --dangerously-skip-permissions
   1538   1402 184990 /home/u/.npm/bin/claude.exe daemon run --origin transient
   1801   1402 184980 claude bg-pty-host --bg-pty-host /tmp/x.sock 200 50
   1824   1402 184970 claude bg-spare --bg-spare /tmp/y.sock
   2100      1  10000 codex exec review
   2200      1   5000 aider --model gpt
   3000      1    100 bash -lc 'claude something'
";

    /// TERM #605 — the producer-side property: an UNCHANGED process must report
    /// the SAME `started_at` on every poll.
    ///
    /// The two polls below land in the same wall-clock second (…:10.100 and
    /// …:10.900) and read the same whole-second `etimes`, which is exactly what
    /// happens in the field. Before the fix the derived start times differed by
    /// 800 ms — enough to make every consumer diffing snapshots see a change
    /// that did not happen.
    #[test]
    fn started_at_is_stable_across_polls_within_one_second() {
        let poll_a = "2026-08-02T04:00:10.100Z".parse::<DateTime<Utc>>().unwrap();
        let poll_b = "2026-08-02T04:00:10.900Z".parse::<DateTime<Utc>>().unwrap();
        let etimes = 3_600; // ps reports whole seconds; unchanged between polls

        assert_eq!(
            started_at_from_etimes(poll_a, etimes),
            started_at_from_etimes(poll_b, etimes),
            "two polls in the SAME second must derive the SAME start time — the \
             subsecond part of now() is observer noise, not information"
        );
    }

    /// A second later, `etimes` has advanced by one — so the derived start time
    /// must still be the SAME instant, not one second earlier/later. This is the
    /// property that makes the value usable as a stable identity across polls.
    #[test]
    fn started_at_is_stable_as_the_clock_and_etimes_advance_together() {
        let base = "2026-08-02T04:00:10.100Z".parse::<DateTime<Utc>>().unwrap();
        let first = started_at_from_etimes(base, 3_600);
        for tick in 1..=5i64 {
            // Polls do NOT fire on an exact subsecond boundary, so jitter the
            // subsecond part too — otherwise every poll would share one offset
            // and the test would pass even against the un-truncated producer.
            let later = base + Duration::seconds(tick) + Duration::milliseconds(tick * 37);
            assert_eq!(
                started_at_from_etimes(later, 3_600 + tick),
                first,
                "an unchanged process must keep one start time as now and etimes advance together"
            );
        }
    }

    /// The value must not CLAIM precision it does not have: `etimes` is whole
    /// seconds, so the derived timestamp carries no subsecond component at all.
    #[test]
    fn started_at_carries_no_subsecond_component() {
        for nanos in [0u32, 1, 500_000_000, 999_999_999] {
            let now = "2026-08-02T04:00:10Z"
                .parse::<DateTime<Utc>>()
                .unwrap()
                .with_nanosecond(nanos)
                .unwrap();
            let started = started_at_from_etimes(now, 42);
            assert_eq!(
                started.nanosecond(),
                0,
                "derived start time must be whole-second (now had {nanos}ns)"
            );
            // And it is still the CORRECT second, not merely a round one.
            assert_eq!(started.timestamp(), now.timestamp() - 42);
        }
    }

    /// The STABLE path (gpt56 review): the exact start time comes from the
    /// process's own `/proc` record, so it is not a function of when the poll
    /// ran at all. Parsing must survive a `comm` containing spaces AND
    /// parentheses — a naive whitespace split silently reads the wrong field.
    #[test]
    fn proc_starttime_parsing_survives_a_hostile_comm_and_yields_a_stable_epoch() {
        let sample = "\
cpu  1 2 3 4
btime 1700000000
intr 99 1 2
1402 (claude --dangerously) S 1 1402 1402 0 -1 4194304 1 2 3 4 5 6 7 8 20 0 1 0 360000 1 2 3
2100 (weird (name) here) S 1 2100 2100 0 -1 4194304 1 2 3 4 5 6 7 8 20 0 1 0 720050 1 2 3
";
        let (btime, starts) = parse_proc_starttimes(sample);
        assert_eq!(btime, Some(1_700_000_000));
        assert_eq!(starts.get(&1402).copied(), Some(360_000));
        assert_eq!(
            starts.get(&2100).copied(),
            Some(720_050),
            "a comm with spaces and nested parens must not shift the field index"
        );

        // 360000 ticks / 100 Hz = 3600s after boot.
        assert_eq!(
            start_epoch_from_proc(btime.unwrap(), starts[&1402], 100),
            1_700_003_600
        );
        // Poll-independent by construction: the same inputs on any later pass
        // give the same answer, because `now` is not an input.
        assert_eq!(
            start_epoch_from_proc(btime.unwrap(), starts[&1402], 100),
            start_epoch_from_proc(btime.unwrap(), starts[&1402], 100)
        );
    }

    /// gpt56/opus review: the tick rate is the HOST's, not a constant. A host
    /// with a different USER_HZ must not be silently misconverted, and an
    /// unusable `getconf` answer must fall back rather than divide by zero.
    #[test]
    fn the_clock_tick_rate_comes_from_the_host_and_fails_closed() {
        assert_eq!(parse_clock_ticks("100\n"), 100);
        assert_eq!(parse_clock_ticks(" 1024 "), 1024);
        for bad in ["", "0", "-5", "not-a-number", "100 200"] {
            assert_eq!(
                parse_clock_ticks(bad),
                DEFAULT_USER_HZ,
                "unusable getconf output {bad:?} must fall back, never poison the divisor"
            );
        }
        // The SAME ticks under a different host rate is a different instant —
        // which is exactly why hardcoding 100 was wrong.
        assert_eq!(start_epoch_from_proc(1_000, 1024, 1024), 1_001);
        assert_eq!(start_epoch_from_proc(1_000, 1024, 100), 1_010);
        // And a zero can never divide.
        assert_eq!(start_epoch_from_proc(1_000, 1024, 0), 1_000 + 1024);
    }

    /// A negative `etimes` must never place a process's start in the FUTURE —
    /// that reads as a real fact rather than as the corrupt input it is.
    #[test]
    fn a_negative_etimes_never_produces_a_future_start_time() {
        let now = "2026-08-02T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
        for etimes in [-1i64, -3600, i64::MIN] {
            assert!(
                started_at_from_etimes(now, etimes) <= now,
                "etimes {etimes} produced a start time in the future"
            );
        }
    }

    /// The documented residual of the FALLBACK, pinned rather than hidden:
    /// truncation bounds the wobble at one second, it does not remove it. A
    /// process whose true start has a fractional part still alternates between
    /// two adjacent seconds — which is exactly why `/proc` is the primary path.
    #[test]
    fn the_etimes_fallback_is_bounded_at_one_second_not_stable() {
        // True start at t=100.4; etimes = floor(now - 100.4).
        let base = "2026-08-02T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let start_frac_ms = 100_400i64;
        let mut seen = std::collections::BTreeSet::new();
        for poll_ms in [200_100i64, 200_600, 201_100, 201_600, 202_100] {
            let now = base + Duration::milliseconds(poll_ms);
            let etimes = (poll_ms - start_frac_ms) / 1000; // floor, as ps does
            seen.insert(started_at_from_etimes(now, etimes));
        }
        assert!(
            seen.len() > 1,
            "if this ever becomes stable the doc comment is wrong and should be corrected"
        );
        let spread = *seen.iter().next_back().unwrap() - *seen.iter().next().unwrap();
        assert!(
            spread <= Duration::seconds(1),
            "the fallback's wobble must stay bounded at one second, got {spread}"
        );
    }

    /// The fallback must be TOTAL: a corrupt or hostile `etimes` degrades that
    /// one session, never aborts the whole host listing (gpt56 review).
    #[test]
    fn the_etimes_fallback_never_panics_on_an_absurd_value() {
        let now = "2026-08-02T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
        for etimes in [i64::MAX, i64::MIN, -1, 0] {
            let _ = started_at_from_etimes(now, etimes);
        }
    }

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

    // Reads AGENTSESS_MAX_SESSIONS, which a sibling test mutates — every
    // test that observes that cap must be serialised with it (PCON-08).
    #[tokio::test]
    #[serial]
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

    #[test]
    fn transcript_root_refuses_option_shaped_and_relative_values() {
        // argv form stops shell injection but NOT option injection: a root of
        // `-delete` would be parsed by `find` as an action, and GNU find with
        // no starting path defaults to `.`.
        assert!(validate_transcript_root("-delete").is_err());
        assert!(validate_transcript_root("--help").is_err());
        assert!(validate_transcript_root("relative/path").is_err());
        assert!(validate_transcript_root("").is_err());
        assert!(validate_transcript_root("/home/u/.claude/projects").is_ok());
    }

    #[test]
    fn session_id_probe_reads_only_the_one_variable() {
        let argv = session_id_probe_argv(4242);
        assert_eq!(argv[0], "grep");
        // -z: NUL-delimited records (that is the environ format);
        // -m1: stop at the first match, so nothing else is even scanned out.
        assert!(argv.contains(&"-z".to_string()));
        assert!(argv.contains(&"-m1".to_string()));
        assert_eq!(argv[4], "^CLAUDE_CODE_SESSION_ID=");
        assert_eq!(argv[5], "/proc/4242/environ");
        // The whole-file read must NOT be how this is done.
        assert!(!argv.iter().any(|a| a == "cat"));
    }

    // Reads AGENTSESS_MAX_SESSIONS, which a sibling test mutates — every
    // test that observes that cap must be serialised with it (PCON-08).
    #[tokio::test]
    #[serial]
    async fn git_probe_failure_is_reported_not_silently_read_as_no_repo() {
        // `git` unregistered on the fake executor => the probe cannot RUN,
        // which must not look the same as "these sessions are not in a repo".
        let exec = FakeExecutor::new()
            .with_stdout("ps", PS_SAMPLE)
            .with_stdout("readlink", "/work/thing\n");
        let snap = discover(&exec, None).await.unwrap();
        assert!(snap.sessions.iter().all(|s| s.repo.is_none()));
        assert!(
            snap.warnings.iter().any(|w| w.contains("git probe failed")),
            "expected a git-probe warning, got {:?}",
            snap.warnings
        );
    }

    // Mutates a shared env var, so it must not race other tests (PCON-08).
    #[tokio::test]
    #[serial]
    async fn the_repo_filter_is_applied_before_the_result_cap() {
        // The starvation case this guards: candidates are enriched in ascending
        // etimes order, so the two NON-matching sessions (pids 2200 @5000s and
        // 2100 @10000s) come first and the only MATCHING one (pid 1402 @185000s)
        // comes last. With a cap of 1 applied BEFORE filtering, the matching
        // session is truncated away and the caller sees zero — even though it is
        // live. The fixture must therefore give different pids different repos;
        // a fixture where everything matches would pass either way.
        std::env::set_var("AGENTSESS_MAX_SESSIONS", "1");
        let exec = FakeExecutor::new()
            .with_stdout("ps", PS_SAMPLE)
            .with_stdout_matching("/proc/1402/cwd", "/work/Terminus\n")
            .with_stdout_matching("/proc/2100/cwd", "/work/Other\n")
            .with_stdout_matching("/proc/2200/cwd", "/work/Other\n")
            .with_stdout_matching("git -C /work/Terminus", "/work/Terminus\nAGSS-01-core\n")
            .with_stdout_matching("git -C /work/Other", "/work/Other\nmain\n");
        let snap = discover(&exec, Some("Terminus")).await.unwrap();
        std::env::remove_var("AGENTSESS_MAX_SESSIONS");

        assert_eq!(
            snap.sessions.len(),
            1,
            "the matching session sorted last and must survive the cap: {:?}",
            snap.sessions
        );
        assert_eq!(snap.sessions[0].pid, 1402);
        assert_eq!(
            snap.sessions[0].repo.as_ref().unwrap().repo_name.as_deref(),
            Some("Terminus")
        );
        assert!(!snap.truncated, "one match, cap 1 — nothing was dropped");
    }

    #[tokio::test]
    #[serial]
    async fn a_git_that_cannot_run_remotely_is_a_probe_failure_not_a_verdict() {
        // The remote path surfaces a missing binary as exit 127, NOT as an Err,
        // so an exit-code-agnostic "non-zero means not a repo" would hide it —
        // along with permission denied, dubious ownership and corrupt repos.
        let exec = FakeExecutor::new()
            .with_stdout("ps", PS_SAMPLE)
            .with_stdout("readlink", "/work/thing\n")
            .with_exit("git", 127, "bash: git: command not found");
        let snap = discover(&exec, None).await.unwrap();
        assert!(snap.sessions.iter().all(|s| s.repo.is_none()));
        assert!(
            snap.warnings.iter().any(|w| w.contains("git probe failed")),
            "exit 127 must be reported, got {:?}",
            snap.warnings
        );
    }

    #[tokio::test]
    #[serial]
    async fn git_answering_not_a_repository_is_a_verdict_not_a_failure() {
        // The one non-zero outcome that IS an answer must stay silent — a
        // session legitimately running outside a repo is not a shortfall.
        let exec = FakeExecutor::new()
            .with_stdout("ps", PS_SAMPLE)
            .with_stdout("readlink", "/work/outside-any-repo\n")
            .with_exit("git", 128, "fatal: not a git repository (or any parent up to /)");
        let snap = discover(&exec, None).await.unwrap();
        assert!(snap.sessions.iter().all(|s| s.repo.is_none()));
        assert!(
            !snap.warnings.iter().any(|w| w.contains("git probe failed")),
            "a genuine not-a-repo answer must not raise a probe warning: {:?}",
            snap.warnings
        );
    }

    // Reads AGENTSESS_MAX_SESSIONS, which a sibling test mutates — every
    // test that observes that cap must be serialised with it (PCON-08).
    #[tokio::test]
    #[serial]
    async fn discovery_fails_only_when_the_process_probe_fails() {
        let exec = FakeExecutor::new().with_failure("ps", "boom");
        assert!(discover(&exec, None).await.is_err());
    }

    // Reads AGENTSESS_MAX_SESSIONS, which a sibling test mutates — every
    // test that observes that cap must be serialised with it (PCON-08).
    #[tokio::test]
    #[serial]
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
