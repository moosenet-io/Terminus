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

use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use crate::error::ToolError;

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
    /// author/committer ident lines seen (remapped in GHIST-03; passed through here).
    pub idents_seen: usize,
}

/// Replay options. GHIST-03 adds the author-identity remap here; GHIST-01 passes
/// author/committer idents through unchanged.
#[derive(Default)]
pub struct ReplayOpts {
    // GHIST-03: `pub author_map: Option<IdentityMap>` slots in here.
    _private: (),
}

impl ReplayOpts {
    pub fn new() -> Self {
        Self::default()
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

/// Reproduce `source`'s entire commit history into `work_dir` as a PII-scrubbed
/// derivative. `work_dir` MUST be empty/non-existent (a fresh backfill target); the
/// source repo is never modified. Returns metrics on what was replayed.
pub fn replay_full_history(
    source: &Path,
    work_dir: &Path,
    _opts: &ReplayOpts,
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
    let pump = transform_stream(BufReader::new(export_out), BufWriter::new(import_in));

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
fn transform_stream<R: BufRead, W: Write>(mut r: R, mut w: W) -> Result<HistoryReport, ToolError> {
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
            // GHIST-03 remaps the ident here; GHIST-01 passes it through and counts.
            report.idents_seen += 1;
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
}
