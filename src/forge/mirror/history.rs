//! GHIST-01 — full-history scrubbed replay for the git-public mirror engine.
//!
//! The per-sync [`MirrorWorkDir::run`](super::workdir::MirrorWorkDir) publishes ONE
//! swept commit per sync — a clean SNAPSHOT, which cannot reproduce a repo's real
//! commit history. This module is the backfill counterpart: it reproduces a source
//! repo's ENTIRE commit graph as a scrubbed derivative, so the public mirror can
//! carry the operator's real contribution history (dates, messages, graph shape)
//! with every historical blob run through the native [`DeterministicCleaner`].
//!
//! ## Mechanism — git-native, streaming, no external tool
//! `git fast-export` the source → transform the stream in-process → `git fast-import`
//! into a fresh work-dir. The transform rewrites only `blob` payloads (and inline
//! `M … inline` blob data) through [`DeterministicCleaner::scrub_bytes`]; commit
//! messages, author/committer idents (remapped in GHIST-03), marks, and the graph
//! structure (`from`/`merge`) pass through unchanged, so commit COUNT and author
//! DATES — what the GitHub contribution calendar keys on — are byte-preserved.
//!
//! The source repo is READ-ONLY throughout (`fast-export` only reads); the scrubbed
//! history lands in a separate work-dir. A full-history PII GATE (GHIST-02) scans
//! every replayed commit's tree before anything is pushed — a secret hidden in an
//! old, later-"removed" commit is caught there, never shipped.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::error::ToolError;
use crate::github::pii::{ruleset_from_config, TreeViolation};

use super::native_clean::DeterministicCleaner;
use super::workdir::run_git;

/// `core.hooksPath=/dev/null` before every git subcommand — the source repo and the
/// fresh work-dir must never run a repo hook during replay (same posture as the rest
/// of the mirror engine).
const HOOKS_OFF: &[&str] = &["-c", "core.hooksPath=/dev/null"];

/// Outcome of a full-history replay (metrics only — no PII, no shas).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct HistoryReport {
    /// `commit` records reproduced.
    pub commits: usize,
    /// blob payloads seen (separate `blob` records + inline `M … inline` data).
    pub blobs_total: usize,
    /// blob payloads the cleaner actually changed.
    pub blobs_rewritten: usize,
    /// author/committer ident lines seen.
    pub idents_seen: usize,
    /// author/committer idents actually rewritten by the GHIST-03 attribution map.
    pub idents_remapped: usize,
}

/// Replay options.
#[derive(Default)]
pub struct ReplayOpts {
    /// GHIST-03 author/committer identity remap. `None` passes idents through
    /// unchanged (GHIST-01 behavior).
    pub author_map: Option<IdentityMap>,
}

impl ReplayOpts {
    pub fn new() -> Self {
        Self::default()
    }

    /// With the given identity remap applied to every author/committer.
    pub fn with_author_map(map: IdentityMap) -> Self {
        Self { author_map: Some(map) }
    }
}

// ── GHIST-03: contribution attribution remap ────────────────────────────────

/// One remap rule: match an internal ident by exact email, email DOMAIN suffix,
/// or exact display name (all case-insensitive), and rewrite it to a public
/// identity. The FIRST matching rule (in file order) wins.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct IdentityRule {
    #[serde(default)]
    pub match_email: Option<String>,
    /// Bare domain, e.g. `example.com` — matches an email ending `@example.com`.
    #[serde(default)]
    pub match_email_domain: Option<String>,
    #[serde(default)]
    pub match_name: Option<String>,
    pub public_name: String,
    pub public_email: String,
}

/// Author-identity remap for the history replay. Loaded from a deployment-config
/// TOML file (path in `TERMINUS_MIRROR_AUTHOR_MAP`) so NO email literal ever lives
/// in source. Rewriting only the ident's name+email (not the timestamp) both
/// attributes commits to the right public account AND scrubs the internal author
/// emails (`*.local`/`*.online`/personal) for free. An email matched by no rule
/// falls through to `default_*` — never left as the raw internal address.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct IdentityMap {
    #[serde(default)]
    pub rules: Vec<IdentityRule>,
    pub default_name: String,
    pub default_email: String,
}

impl IdentityMap {
    /// Load from a TOML file. Errors if the file is missing/unreadable/malformed
    /// (a backfill must NOT silently run with no attribution map).
    pub fn from_toml_file(path: &Path) -> Result<Self, ToolError> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            ToolError::Execution(format!("read author-map {}: {e}", path.display()))
        })?;
        toml::from_str(&text)
            .map_err(|e| ToolError::Execution(format!("parse author-map {}: {e}", path.display())))
    }

    /// Load from `TERMINUS_MIRROR_AUTHOR_MAP` if set, else `None` (no remap).
    pub fn from_env() -> Result<Option<Self>, ToolError> {
        match std::env::var("TERMINUS_MIRROR_AUTHOR_MAP") {
            Ok(p) if !p.trim().is_empty() => Ok(Some(Self::from_toml_file(Path::new(p.trim()))?)),
            _ => Ok(None),
        }
    }

    /// Remap `(name, email)` → the public `(name, email)`. Case-insensitive
    /// matching; first matching rule wins; unmatched → the configured default.
    pub fn remap(&self, name: &str, email: &str) -> (String, String) {
        let email_lc = email.to_ascii_lowercase();
        let name_lc = name.to_ascii_lowercase();
        for r in &self.rules {
            let hit = r
                .match_email
                .as_deref()
                .is_some_and(|m| m.eq_ignore_ascii_case(email))
                || r.match_email_domain
                    .as_deref()
                    .is_some_and(|d| email_lc.ends_with(&format!("@{}", d.to_ascii_lowercase())))
                || r.match_name
                    .as_deref()
                    .is_some_and(|n| n.to_ascii_lowercase() == name_lc);
            if hit {
                return (r.public_name.clone(), r.public_email.clone());
            }
        }
        (self.default_name.clone(), self.default_email.clone())
    }
}

/// Run a git subcommand in `cwd` with hooks disabled, capturing stdout. Unlike
/// [`run_git`] this does NOT assert-against `-f`/`--hard` — the replay legitimately
/// needs `reset --hard` to populate a FRESH throwaway work-dir from the just-imported
/// HEAD (nothing to clobber). Never used for a push and never on the source repo's
/// work-tree.
fn git_capture(cwd: &Path, args: &[&str]) -> Result<String, ToolError> {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(HOOKS_OFF)
        .args(args)
        .output()
        .map_err(|e| ToolError::Execution(format!("git {args:?}: {e}")))?;
    if !out.status.success() {
        return Err(ToolError::Execution(format!(
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Whether `haystack` contains the contiguous byte slice `needle`.
fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || needle.len() > haystack.len() {
        return needle.is_empty();
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Rewrite a fast-export `author`/`committer` ident line's NAME + EMAIL per the
/// attribution map, PRESERVING the trailing ` <timestamp> <tz>` (so the
/// contribution date is unchanged). Line format: `author NAME <EMAIL> TS TZ\n`.
/// Returns the original bytes on any parse failure — a malformed ident is passed
/// through unchanged rather than dropped or panicking.
fn rewrite_ident_line(line: &[u8], map: &IdentityMap) -> Vec<u8> {
    let prefix_len = if line.starts_with(b"author ") {
        "author ".len()
    } else if line.starts_with(b"committer ") {
        "committer ".len()
    } else {
        return line.to_vec();
    };
    let (prefix, rest) = line.split_at(prefix_len);
    // git idents put the email in an angle-bracket pair; names cannot contain
    // '<'/'>', so the first '<' opens the email.
    let lt = match rest.iter().position(|&b| b == b'<') {
        Some(i) => i,
        None => return line.to_vec(),
    };
    let gt = match rest[lt..].iter().position(|&b| b == b'>') {
        Some(i) => lt + i,
        None => return line.to_vec(),
    };
    let name = match std::str::from_utf8(&rest[..lt]) {
        Ok(s) => s.trim(),
        Err(_) => return line.to_vec(),
    };
    let email = match std::str::from_utf8(&rest[lt + 1..gt]) {
        Ok(s) => s.trim(),
        Err(_) => return line.to_vec(),
    };
    let suffix = &rest[gt + 1..]; // ` <TS> <TZ>\n` — preserved verbatim
    let (public_name, public_email) = map.remap(name, email);
    let mut out = Vec::with_capacity(prefix.len() + public_name.len() + public_email.len() + suffix.len() + 4);
    out.extend_from_slice(prefix);
    out.extend_from_slice(public_name.as_bytes());
    out.extend_from_slice(b" <");
    out.extend_from_slice(public_email.as_bytes());
    out.push(b'>');
    out.extend_from_slice(suffix);
    out
}

/// Reproduce `source`'s entire commit history into `work_dir` as a PII-scrubbed
/// derivative. `work_dir` MUST be empty/non-existent (a fresh backfill target); the
/// source repo is never modified. Returns metrics on what was replayed.
pub fn replay_full_history(
    source: &Path,
    work_dir: &Path,
    opts: &ReplayOpts,
) -> Result<HistoryReport, ToolError> {
    // Confirm the source is a git repo before doing anything.
    run_git(source, &["rev-parse", "--git-dir"])
        .map_err(|e| ToolError::Execution(format!("source is not a git repo ({}): {e}", source.display())))?;
    let default_branch = git_capture(source, &["symbolic-ref", "--short", "HEAD"])
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "main".to_string());

    // Fresh work-dir git repo (must be empty — a backfill starts a new lineage).
    std::fs::create_dir_all(work_dir)
        .map_err(|e| ToolError::Execution(format!("create work_dir {}: {e}", work_dir.display())))?;
    git_capture(work_dir, &["init", "-q", "-b", &default_branch])?;

    // Exporter on the source (READ-ONLY). --reencode normalizes to UTF-8; signed
    // tags are stripped (no gpg in the replay); a tag of a filtered object is dropped.
    let mut exporter = Command::new("git")
        .arg("-C")
        .arg(source)
        .args(HOOKS_OFF)
        .args([
            "fast-export",
            "--all",
            "--reencode=yes",
            "--signed-tags=strip",
            "--tag-of-filtered-object=drop",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| ToolError::Execution(format!("spawn git fast-export: {e}")))?;

    // Importer into the work-dir. --force lets it write the refs into the fresh repo;
    // this is fast-import's ref-update flag, NOT a dangerous push-force (it never
    // touches a remote), so it is spawned directly rather than through `run_git`.
    let mut importer = Command::new("git")
        .arg("-C")
        .arg(work_dir)
        .args(HOOKS_OFF)
        .args(["fast-import", "--quiet", "--force"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| ToolError::Execution(format!("spawn git fast-import: {e}")))?;

    let export_out = exporter.stdout.take().expect("piped");
    let export_err = exporter.stderr.take().expect("piped");
    let import_in = importer.stdin.take().expect("piped");
    let import_err = importer.stderr.take().expect("piped");

    // Drain both stderrs on threads so neither pipe can fill and deadlock the pump.
    let e_err = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = BufReader::new(export_err).read_to_string(&mut s);
        s
    });
    let i_err = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = BufReader::new(import_err).read_to_string(&mut s);
        s
    });

    // Pump export → transform → import. The BufWriter owns import_in and closes it
    // (EOF to fast-import) when it is dropped at the end of this call.
    let pump = transform_stream(
        BufReader::new(export_out),
        BufWriter::new(import_in),
        opts.author_map.as_ref(),
    );

    let e_status = exporter
        .wait()
        .map_err(|e| ToolError::Execution(format!("wait fast-export: {e}")))?;
    let i_status = importer
        .wait()
        .map_err(|e| ToolError::Execution(format!("wait fast-import: {e}")))?;
    let e_stderr = e_err.join().unwrap_or_default();
    let i_stderr = i_err.join().unwrap_or_default();

    // Surface a stream/transform error first (it usually explains a downstream import
    // failure), then non-zero child exits.
    let report = pump?;
    if !e_status.success() {
        return Err(ToolError::Execution(format!(
            "git fast-export exited non-zero: {}",
            e_stderr.trim()
        )));
    }
    if !i_status.success() {
        return Err(ToolError::Execution(format!(
            "git fast-import exited non-zero: {}",
            i_stderr.trim()
        )));
    }

    // Populate the work-dir's index + work-tree from the imported HEAD. The repo is
    // freshly init'd (empty index/tree), so this only fills it in — nothing to lose.
    git_capture(work_dir, &["reset", "--hard"])?;

    Ok(report)
}

// ── GHIST-02: full-history PII gate ──────────────────────────────────────────

/// One residual PII violation found in a HISTORICAL commit's tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryViolation {
    /// A commit whose tree carries the violation (the representative commit for
    /// the tree — many commits can share one tree; see [`gate_full_history`]).
    pub commit: String,
    pub file: String,
    pub line: usize,
    pub pattern_kind: String,
    /// Redacted context (the gate never stores the full secret).
    pub context: String,
}

/// Result of scanning EVERY commit's tree in a replayed history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FullHistoryGateReport {
    /// True iff zero residual violations across all commits.
    pub clean: bool,
    pub commits_scanned: usize,
    /// Distinct tree objects actually scanned (commits sharing a tree scan once).
    pub unique_trees: usize,
    pub violations: Vec<HistoryViolation>,
}

/// Scan the tree of EVERY commit reachable in `work_dir` (not just the tip) with
/// the authoritative PII gate, and report any residual `commit:file:line`. This is
/// the safety spine of the backfill: a secret committed once and later "removed"
/// still lives in that historical commit's tree, and a full-history push would ship
/// it — so the caller MUST refuse to push when `clean` is false.
///
/// Cost is bounded by de-duplicating on the TREE object: commits that share a tree
/// (a no-op commit, a revert to an identical tree) are scanned once. Each unique
/// tree is materialized read-only via `git archive | tar -x` into a throwaway temp
/// dir, scanned, and removed. Progress is logged for large histories.
pub fn gate_full_history(work_dir: &Path) -> Result<FullHistoryGateReport, ToolError> {
    let rev_list = git_capture(work_dir, &["rev-list", "--all"])?;
    let commits: Vec<String> = rev_list.split_whitespace().map(|s| s.to_string()).collect();
    // Same ruleset resolution every other gate surface uses (repo pii-gate.toml /
    // TERMINUS_PII_CONFIG / built-in default), so history is gated identically to
    // the tip.
    let ruleset = ruleset_from_config(Some(work_dir));

    let scan_root = temp_dir_unique("ghist02-gate");
    std::fs::create_dir_all(&scan_root)
        .map_err(|e| ToolError::Execution(format!("create gate scan root: {e}")))?;

    let mut seen_trees: HashSet<String> = HashSet::new();
    let mut violations: Vec<HistoryViolation> = Vec::new();
    let total = commits.len();

    for (i, commit) in commits.iter().enumerate() {
        let tree = git_capture(work_dir, &["rev-parse", &format!("{commit}^{{tree}}")])?
            .trim()
            .to_string();
        if !seen_trees.insert(tree.clone()) {
            continue; // tree already scanned via an earlier (representative) commit
        }
        let dir = scan_root.join(&tree);
        if let Err(e) = extract_tree(work_dir, &tree, &dir) {
            let _ = std::fs::remove_dir_all(&scan_root);
            return Err(e);
        }
        for TreeViolation { file, line, pattern_kind, context } in ruleset.scan_tree(&dir) {
            violations.push(HistoryViolation {
                commit: commit.clone(),
                file,
                line,
                pattern_kind,
                context,
            });
        }
        let _ = std::fs::remove_dir_all(&dir);
        if i > 0 && i % 200 == 0 {
            tracing::info!(
                target: "forge.mirror",
                scanned = i,
                total,
                unique_trees = seen_trees.len(),
                residuals = violations.len(),
                "full-history gate progress"
            );
        }
    }
    let _ = std::fs::remove_dir_all(&scan_root);

    Ok(FullHistoryGateReport {
        clean: violations.is_empty(),
        commits_scanned: total,
        unique_trees: seen_trees.len(),
        violations,
    })
}

/// Materialize a git TREE object read-only into `dest` via `git archive | tar -x`.
/// The two children are connected by an OS pipe (git stdout → tar stdin), so there
/// is no manual pump and no deadlock. Neither touches the source work-tree.
fn extract_tree(repo: &Path, tree: &str, dest: &Path) -> Result<(), ToolError> {
    std::fs::create_dir_all(dest)
        .map_err(|e| ToolError::Execution(format!("create tree dir {}: {e}", dest.display())))?;
    let mut archive = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(HOOKS_OFF)
        .args(["archive", "--format=tar", tree])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| ToolError::Execution(format!("spawn git archive: {e}")))?;
    let archive_out: Stdio = archive.stdout.take().expect("piped").into();
    let tar = Command::new("tar")
        .arg("-x")
        .arg("-C")
        .arg(dest)
        .stdin(archive_out)
        .output()
        .map_err(|e| ToolError::Execution(format!("run tar -x: {e}")))?;
    let a_status = archive
        .wait()
        .map_err(|e| ToolError::Execution(format!("wait git archive: {e}")))?;
    let mut a_err = String::new();
    if let Some(mut es) = archive.stderr.take() {
        let _ = es.read_to_string(&mut a_err);
    }
    if !a_status.success() {
        return Err(ToolError::Execution(format!("git archive {tree} failed: {}", a_err.trim())));
    }
    if !tar.status.success() {
        return Err(ToolError::Execution(format!(
            "tar -x failed for {tree}: {}",
            String::from_utf8_lossy(&tar.stderr).trim()
        )));
    }
    Ok(())
}

/// A process-unique temp dir path (no `Date`/`rand` — uses pid + monotonic-ish
/// system time, unique enough for a serialized backfill).
fn temp_dir_unique(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ))
}

/// What the NEXT `data <n>` block belongs to, set by the command line preceding it.
#[derive(PartialEq)]
enum Pending {
    None,
    /// A `blob` record or an inline `M … inline` payload — scrub it.
    Blob,
    /// A `commit` message — pass through unchanged.
    Message,
}

/// Stream-transform a `git fast-export` byte stream into a `git fast-import` stream,
/// scrubbing only blob payloads. Command lines are read with `read_until(b'\n')`;
/// a `data <count>` header is followed by EXACTLY `count` raw bytes (which may
/// contain newlines/binary), read with `read_exact`. Only blob payloads are passed
/// through [`DeterministicCleaner::scrub_bytes`]; commit messages and every
/// structural line (`mark`/`from`/`merge`/`M`/`D`/`author`/…) pass through verbatim,
/// preserving the graph, dates, and commit count.
fn transform_stream<R: BufRead, W: Write>(
    mut r: R,
    mut w: W,
    author_map: Option<&IdentityMap>,
) -> Result<HistoryReport, ToolError> {
    let mut report = HistoryReport::default();
    let mut pending = Pending::None;
    let ioerr = |e: std::io::Error| ToolError::Execution(format!("history stream io: {e}"));

    loop {
        let mut line: Vec<u8> = Vec::new();
        let n = r.read_until(b'\n', &mut line).map_err(ioerr)?;
        if n == 0 {
            break; // EOF
        }

        if line.starts_with(b"data ") {
            // Counted-data header: `data <n>\n` then exactly n raw bytes.
            let hdr = std::str::from_utf8(&line[5..])
                .map_err(|_| ToolError::Execution("non-utf8 fast-export data header".into()))?
                .trim();
            if hdr.starts_with("<<") {
                return Err(ToolError::Execution(
                    "fast-export emitted delimited data (data <<EOF); expected counted data".into(),
                ));
            }
            let len: usize = hdr
                .parse()
                .map_err(|_| ToolError::Execution(format!("bad fast-export data length: {hdr:?}")))?;
            let mut data = vec![0u8; len];
            r.read_exact(&mut data).map_err(ioerr)?;

            let out = match pending {
                Pending::Blob => {
                    report.blobs_total += 1;
                    let scrubbed = DeterministicCleaner::scrub_bytes(&data);
                    if scrubbed != data {
                        report.blobs_rewritten += 1;
                    }
                    scrubbed
                }
                // Commit message (or a stray data with no preceding blob/commit) —
                // never scrubbed here (messages are prose; graph fidelity first).
                _ => data,
            };
            pending = Pending::None;
            w.write_all(format!("data {}\n", out.len()).as_bytes())
                .map_err(ioerr)?;
            w.write_all(&out).map_err(ioerr)?;
            continue;
        }

        // Classify a command line to know what its upcoming `data` belongs to.
        if line == b"blob\n" || line == b"blob" {
            pending = Pending::Blob;
        } else if line.starts_with(b"commit ") {
            pending = Pending::Message;
            report.commits += 1;
        } else if line.starts_with(b"M ") && contains_subslice(&line, b" inline ") {
            pending = Pending::Blob;
        } else if line.starts_with(b"author ") || line.starts_with(b"committer ") {
            report.idents_seen += 1;
            // GHIST-03: remap the ident's name+email to the public identity,
            // preserving the timestamp. Only when a map is configured; on any
            // parse failure the original line is written unchanged.
            if let Some(map) = author_map {
                let rewritten = rewrite_ident_line(&line, map);
                if rewritten != line {
                    report.idents_remapped += 1;
                }
                w.write_all(&rewritten).map_err(ioerr)?;
                continue;
            }
        }

        w.write_all(&line).map_err(ioerr)?;
    }

    w.flush().map_err(ioerr)?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn unique(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "ghist01-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        git_capture(dir, args).unwrap_or_else(|e| panic!("git {args:?}: {e}"))
    }

    fn write(dir: &Path, rel: &str, bytes: &[u8]) {
        let p = dir.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, bytes).unwrap();
    }

    /// A source repo with three commits: an IP in text, a binary blob, and a plain
    /// change — with a fixed author/date so we can assert date fidelity.
    fn init_source() -> PathBuf {
        let dir = unique("src");
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q", "-b", "main"]);
        let commit = |dir: &Path, msg: &str, date: &str| {
            git(dir, &["add", "-A"]);
            // Fixed author + date so the replay's date-preservation is checkable.
            let out = Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(HOOKS_OFF)
                .args([
                    "-c",
                    "user.name=Src Author",
                    "-c",
                    "user.email=<email>", // pii-test-fixture
                    "commit",
                    "-q",
                    "-m",
                    msg,
                    "--date",
                    date,
                ])
                .env("GIT_COMMITTER_DATE", date)
                .env("GIT_COMMITTER_NAME", "Src Author")
                .env("GIT_COMMITTER_EMAIL", "<email>") // pii-test-fixture
                .output()
                .unwrap();
            assert!(out.status.success(), "commit: {}", String::from_utf8_lossy(&out.stderr));
        };
        write(&dir, "config.txt", b"internal host <internal-ip> here\n"); // pii-test-fixture
        commit(&dir, "add config with an internal ip", "2021-01-02T03:04:05");
        // A binary blob (contains a NUL) that must pass through untouched.
        write(&dir, "asset.bin", &[0u8, 1, 2, 3, 0xff, b'1', b'9', b'2', 0, 0xfe]);
        commit(&dir, "add a binary asset", "2021-06-07T08:09:10");
        write(&dir, "notes.md", b"a plain note, nothing sensitive\n");
        commit(&dir, "add plain notes", "2021-12-24T11:12:13");
        dir
    }

    #[test]
    fn replay_preserves_graph_dates_and_scrubs_history() {
        let src = init_source();
        let wd = unique("wd");

        let src_commits = git(&src, &["rev-list", "--count", "--all"]).trim().to_string();
        let src_tree = git(&src, &["rev-parse", "HEAD^{tree}"]).trim().to_string();
        let src_dates: String = git(&src, &["log", "--all", "--format=%ad", "--date=iso-strict"]);

        let report = replay_full_history(&src, &wd, &ReplayOpts::new()).unwrap();

        // Commit count preserved.
        assert_eq!(report.commits, 3, "three commits replayed: {report:?}");
        assert_eq!(
            git(&wd, &["rev-list", "--count", "--all"]).trim(),
            src_commits,
            "commit count preserved"
        );
        // Author DATES preserved (contribution-calendar fidelity).
        assert_eq!(
            git(&wd, &["log", "--all", "--format=%ad", "--date=iso-strict"]),
            src_dates,
            "author dates preserved"
        );
        // The internal IP is scrubbed in the FIRST (historical) commit's blob.
        let first_config = git(&wd, &["show", "HEAD~2:config.txt"]);
        assert!(first_config.contains("<internal-ip>"), "historical IP scrubbed: {first_config:?}");
        assert!(!first_config.contains("<internal-ip>"), "raw IP gone"); // pii-test-fixture
        // The binary blob passed through byte-identical.
        let wd_bin = std::fs::read(wd.join("asset.bin")).unwrap();
        assert_eq!(wd_bin, vec![0u8, 1, 2, 3, 0xff, b'1', b'9', b'2', 0, 0xfe], "binary untouched");
        assert!(report.blobs_rewritten >= 1 && report.blobs_total >= 3, "metrics: {report:?}");

        // SOURCE untouched (read-only guarantee).
        assert_eq!(git(&src, &["rev-parse", "HEAD^{tree}"]).trim(), src_tree, "source tree unchanged");
        assert!(git(&src, &["status", "--porcelain"]).trim().is_empty(), "source clean");

        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&wd);
    }

    #[test]
    fn empty_source_replays_cleanly() {
        let src = unique("empty-src");
        std::fs::create_dir_all(&src).unwrap();
        git(&src, &["init", "-q", "-b", "main"]);
        let wd = unique("empty-wd");
        let report = replay_full_history(&src, &wd, &ReplayOpts::new()).unwrap();
        assert_eq!(report.commits, 0, "no commits");
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&wd);
    }

    #[test]
    fn contains_subslice_works() {
        assert!(contains_subslice(b"M 100644 inline path", b" inline "));
        assert!(!contains_subslice(b"M 100644 :5 path", b" inline "));
        assert!(contains_subslice(b"anything", b""));
    }

    /// Commit `dir` with a fixed identity (helper for the gate tests).
    fn commit_all(dir: &Path, msg: &str) {
        git(dir, &["add", "-A"]);
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(HOOKS_OFF)
            .args([
                "-c",
                "user.name=Gate Test",
                "-c",
                "user.email=<email>", // pii-test-fixture
                "commit",
                "-q",
                "-m",
                msg,
            ])
            .output()
            .unwrap();
        assert!(out.status.success(), "commit: {}", String::from_utf8_lossy(&out.stderr));
    }

    // ── GHIST-02: a secret present ONLY in a historical (non-tip) commit is caught ──
    #[test]
    fn gate_flags_secret_in_historical_commit() {
        std::env::remove_var("TERMINUS_PII_CONFIG");
        let dir = unique("gate-hist");
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q", "-b", "main"]);
        // Commit 1 introduces an internal IP...
        write(&dir, "leak.txt", b"internal host <internal-ip> in an old commit\n"); // pii-test-fixture
        commit_all(&dir, "add a file with an internal ip");
        // Commit 2 DELETES the file — so the TIP tree is clean, but the IP still
        // lives in commit 1's tree.
        std::fs::remove_file(dir.join("leak.txt")).unwrap();
        write(&dir, "readme.txt", b"nothing sensitive now\n");
        commit_all(&dir, "remove the leaky file");

        // Tip is clean...
        let tip_dir = unique("gate-tip");
        let tip_tree = git(&dir, &["rev-parse", "HEAD^{tree}"]).trim().to_string();
        extract_tree(&dir, &tip_tree, &tip_dir).unwrap();
        assert!(
            crate::github::pii::ruleset_from_config(None).scan_tree(&tip_dir).is_empty(),
            "tip tree is clean"
        );

        // ...but the FULL-HISTORY gate flags commit 1.
        let report = gate_full_history(&dir).unwrap();
        assert!(!report.clean, "gate must flag the historical secret: {report:?}");
        assert!(report.commits_scanned >= 2);
        assert!(
            report.violations.iter().any(|v| v.file == "leak.txt" && v.pattern_kind == "private_ip"),
            "the historical leak.txt private_ip is flagged: {:?}",
            report.violations
        );
        // Context is redacted — the raw IP is never echoed in the report.
        assert!(
            report.violations.iter().all(|v| !v.context.contains("<internal-ip>")), // pii-test-fixture
            "context must be redacted"
        );

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&tip_dir);
    }

    // ── GHIST-02: a fully-clean history passes the gate ──
    #[test]
    fn gate_passes_clean_history() {
        std::env::remove_var("TERMINUS_PII_CONFIG");
        let dir = unique("gate-clean");
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q", "-b", "main"]);
        write(&dir, "a.txt", b"just some ordinary content\n");
        commit_all(&dir, "c1");
        write(&dir, "b.txt", b"more ordinary content, no secrets\n");
        commit_all(&dir, "c2");

        let report = gate_full_history(&dir).unwrap();
        assert!(report.clean, "clean history passes: {report:?}");
        assert_eq!(report.violations.len(), 0);
        assert!(report.commits_scanned >= 2 && report.unique_trees >= 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── GHIST-01 + GHIST-02 together: replay scrubs history so the gate then passes ──
    #[test]
    fn replayed_history_passes_the_full_gate() {
        std::env::remove_var("TERMINUS_PII_CONFIG");
        let src = init_source(); // has an internal IP in its first commit
        let wd = unique("replay-gate-wd");
        replay_full_history(&src, &wd, &ReplayOpts::new()).unwrap();
        // After GHIST-01 scrubbed every blob, the full-history gate is clean.
        let report = gate_full_history(&wd).unwrap();
        assert!(report.clean, "replayed (scrubbed) history passes the full gate: {report:?}");
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&wd);
    }

    // ── GHIST-03: attribution remap ──
    fn rule(email: Option<&str>, domain: Option<&str>, name: Option<&str>, pn: &str, pe: &str) -> IdentityRule {
        IdentityRule {
            match_email: email.map(String::from),
            match_email_domain: domain.map(String::from),
            match_name: name.map(String::from),
            public_name: pn.into(),
            public_email: pe.into(),
        }
    }

    #[test]
    fn identity_map_remaps_by_email_domain_and_name() {
        let map = IdentityMap {
            rules: vec![
                rule(Some("<email>"), None, None, "PubMe", "<email>"), // pii-test-fixture
                rule(None, Some("agents.example"), None, "MoosenetBot", "<email>"),
                rule(None, None, Some("Legacy Human"), "PubMe", "<email>"),
            ],
            default_name: "fallback-bot".into(),
            default_email: "<email>".into(),
        };
        // exact email (case-insensitive)
        assert_eq!(map.remap("Whatever", "<email>"), ("PubMe".into(), "<email>".into())); // pii-test-fixture
        // domain suffix
        assert_eq!(map.remap("Agent", "<email>"), ("MoosenetBot".into(), "<email>".into()));
        // display name
        assert_eq!(map.remap("Legacy Human", "<email>"), ("PubMe".into(), "<email>".into()));
        // unmatched → default (never the raw internal email)
        assert_eq!(map.remap("Nobody", "<email>"), ("fallback-bot".into(), "<email>".into()));
    }

    #[test]
    fn rewrite_ident_preserves_timestamp_and_scrubs_email() {
        let map = IdentityMap {
            rules: vec![rule(Some("<email>"), None, None, "PubMe", "<email>")], // pii-test-fixture
            default_name: "bot".into(),
            default_email: "<email>".into(),
        };
        let line = b"author Real Name <<email>> 1609556645 +0000\n"; // pii-test-fixture
        let out = rewrite_ident_line(line, &map);
        let s = String::from_utf8(out).unwrap();
        assert_eq!(s, "author PubMe <<email>> 1609556645 +0000\n", "remapped + ts preserved: {s}");
        assert!(!s.contains("<email>"), "internal email scrubbed"); // pii-test-fixture
        // committer prefix handled too; a malformed line is passed through.
        assert_eq!(rewrite_ident_line(b"committer X <<email>> 1 +0000\n", &map), // pii-test-fixture
                   b"committer PubMe <<email>> 1 +0000\n".to_vec());
        assert_eq!(rewrite_ident_line(b"author malformed line\n", &map), b"author malformed line\n".to_vec());
    }

    #[test]
    fn replay_with_author_map_remaps_all_idents_and_preserves_dates() {
        std::env::remove_var("TERMINUS_PII_CONFIG");
        let src = init_source(); // authored by <email>
        let wd = unique("replay-attr-wd");
        let src_dates: String = git(&src, &["log", "--all", "--format=%ad", "--date=iso-strict"]);
        let map = IdentityMap {
            rules: vec![],
            default_name: "MoosenetBot".into(),
            default_email: "<email>".into(),
        };
        let report = replay_full_history(&src, &wd, &ReplayOpts::with_author_map(map)).unwrap();
        assert!(report.idents_remapped >= 2, "author+committer remapped: {report:?}");
        // NO internal author email survives; all attributed to the mapped identity.
        let authors = git(&wd, &["log", "--all", "--format=%ae|%ce"]);
        assert!(!authors.contains("<email>"), "internal email gone: {authors}"); // pii-test-fixture
        assert!(authors.contains("<email>"), "remapped: {authors}");
        // Dates unchanged (contribution fidelity).
        assert_eq!(git(&wd, &["log", "--all", "--format=%ad", "--date=iso-strict"]), src_dates, "dates preserved");
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&wd);
    }
}
