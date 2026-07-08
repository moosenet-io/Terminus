//! GHMR-01: Rust PII pre-push / pre-commit gate — the authoritative replacement
//! for the legacy `.githooks/pii_gate.py`.
//!
//! It is a thin CLI around [`terminus_rs::github::pii`]'s tree-sweep engine.
//! All detection logic lives in the library (shared with the runtime GitHub
//! write gate and the mirror engine); this binary only decides *what set of
//! content* to scan and how to report.
//!
//! ## Modes
//! - (default, git pre-push): reads the pre-push protocol on stdin
//!   (`<local_ref> <local_sha> <remote_ref> <remote_sha>` per line) and scans
//!   the **committed blobs** being pushed — for a new branch, every file in the
//!   pushed tip tree; otherwise the files changed in `<remote_sha>..<local_sha>`.
//! - `--staged` (git pre-commit): scans the **staged index** blobs.
//! - `--tree [PATH]`: sweeps an entire working-directory tree (defaults to the
//!   repo root) — used by the mirror engine and for full audits.
//! - `--json`: emit a machine-readable JSON report instead of the human summary.
//!
//! The gate reads git *objects* (commit / index blobs), not the working tree,
//! so a secret that is committed/staged but since deleted or masked by a clean
//! unstaged edit is still caught, and content not actually being pushed is not
//! falsely flagged.
//!
//! Config (optional): a repo-root `pii-gate.toml` (or the path in
//! `TERMINUS_PII_CONFIG`) supplies repo-specific terms, extra patterns, allowed
//! emails, and exclusions. Missing config uses the built-in defaults. The same
//! file/extension exclusions apply in every mode.
//!
//! Exit code: `0` when clean, `1` when any violation is found OR when git
//! enumeration fails (the gate fails **closed** — a git error is never
//! indistinguishable from a clean push).
//!
//! ## Installing as the git hook (replacing the Python gate)
//! ```text
//! cargo build --release --bin pii_gate
//! ln -sf ../../target/release/pii_gate .git/hooks/pre-push
//! # (or copy the binary and point core.hooksPath at it)
//! ```

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use terminus_rs::github::pii::{violations_to_json, PiiRuleSet, TreeViolation};

const NULL_SHA: &str = "0000000000000000000000000000000000000000"; // pii-test-fixture

/// Run a git command in `root`, returning stdout on success or an error string
/// on failure (so callers can fail closed rather than treating an error as an
/// empty — i.e. clean — result).
fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map_err(|e| format!("failed to execute git {args:?}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Read a git blob (`<rev>:<path>`, or `:<path>` for the index) as text.
/// Returns `None` for binary (NUL-containing) or unreadable blobs.
fn read_blob(root: &Path, rev: &str, rel: &str) -> Option<Vec<u8>> {
    let spec = format!("{rev}:{rel}");
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["show", &spec])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(out.stdout)
}

fn names(out: &str) -> Vec<String> {
    out.lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn repo_root() -> PathBuf {
    let out = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output();
    if let Ok(o) = out {
        if o.status.success() {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if !s.is_empty() {
                return PathBuf::from(s);
            }
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn load_ruleset(root: &Path) -> PiiRuleSet {
    if let Ok(p) = std::env::var("TERMINUS_PII_CONFIG") {
        return PiiRuleSet::from_config_file(Path::new(&p));
    }
    let cfg = root.join("pii-gate.toml");
    if cfg.is_file() {
        PiiRuleSet::from_config_file(&cfg)
    } else {
        PiiRuleSet::new()
    }
}

/// Scan a set of `(rev, path)` blobs, honoring exclusions and the
/// `pii-test-fixture` line-exact exemption.
fn scan_blobs(root: &Path, rs: &PiiRuleSet, entries: &[(String, String)]) -> Vec<TreeViolation> {
    let mut out = Vec::new();
    for (rev, rel) in entries {
        if rs.is_excluded(Path::new(rel)) {
            continue;
        }
        let bytes = match read_blob(root, rev, rel) {
            Some(b) => b,
            None => continue,
        };
        if bytes.contains(&0) {
            continue; // binary
        }
        let content = String::from_utf8_lossy(&bytes);
        let scrubbed: String = content
            .lines()
            .map(|l| if l.contains("pii-test-fixture") { "" } else { l })
            .collect::<Vec<_>>()
            .join("\n");
        for v in rs.scan_content(&scrubbed) {
            out.push(TreeViolation {
                file: rel.clone(),
                line: v.line,
                pattern_kind: v.category,
                context: v.context,
            });
        }
    }
    out
}

/// Enumerate `(tip_sha, path)` blobs being pushed. Fails closed on any git error.
fn prepush_entries(root: &Path) -> Result<Vec<(String, String)>, String> {
    let mut stdin = String::new();
    std::io::stdin()
        .read_to_string(&mut stdin)
        .map_err(|e| format!("failed to read pre-push stdin: {e}"))?;

    let mut entries = Vec::new();
    for line in stdin.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 4 {
            continue;
        }
        let (local_sha, remote_sha) = (parts[1], parts[3]);
        if local_sha == NULL_SHA {
            continue; // branch deletion — nothing to scan
        }
        let files = if remote_sha == NULL_SHA {
            // New branch on the remote: no merge base known, so scan every file
            // in the pushed tip tree (fail-safe, over-inclusive).
            names(&git(root, &["ls-tree", "-r", "--name-only", local_sha])?)
        } else {
            names(&git(root, &["diff", "--name-only", remote_sha, local_sha])?)
        };
        for f in files {
            entries.push((local_sha.to_string(), f));
        }
    }
    entries.sort();
    entries.dedup();
    Ok(entries)
}

/// Enumerate `("", path)` staged (index) blobs. Fails closed on git error.
fn staged_entries(root: &Path) -> Result<Vec<(String, String)>, String> {
    let files = names(&git(root, &["diff", "--cached", "--name-only"])?);
    Ok(files.into_iter().map(|f| (String::new(), f)).collect())
}

fn report(violations: &[TreeViolation], json: bool) -> i32 {
    if json {
        println!("{}", violations_to_json(violations));
        return i32::from(!violations.is_empty());
    }
    if violations.is_empty() {
        println!("PII gate: clean (0 violations).");
        return 0;
    }
    eprintln!("{}", "=".repeat(62));
    eprintln!("  PII GATE BLOCKED: secrets/PII detected — push refused");
    eprintln!("{}", "=".repeat(62));
    eprintln!();
    let mut current = "";
    for v in violations {
        if v.file != current {
            eprintln!("File: {}", v.file);
            current = &v.file;
        }
        eprintln!("  Line {}: [{}] {}", v.line, v.pattern_kind, v.context);
    }
    eprintln!();
    eprintln!(
        "Found {} violation(s). Fix the content and amend/rebase, then push again.",
        violations.len()
    );
    1
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let json = args.iter().any(|a| a == "--json");
    let staged = args.iter().any(|a| a == "--staged");
    let tree_mode = args.iter().any(|a| a == "--tree" || a == "--all");

    let root = repo_root();
    let rs = load_ruleset(&root);

    // A git-enumeration failure must fail CLOSED (nonzero exit), never be
    // reported as a clean scan.
    let result: Result<Vec<TreeViolation>, String> = if tree_mode {
        let path = args
            .iter()
            .position(|a| a == "--tree")
            .and_then(|i| args.get(i + 1))
            .filter(|s| !s.starts_with("--"))
            .map(PathBuf::from)
            .unwrap_or_else(|| root.clone());
        Ok(rs.scan_tree(&path))
    } else if staged {
        staged_entries(&root).map(|e| scan_blobs(&root, &rs, &e))
    } else {
        prepush_entries(&root).map(|e| scan_blobs(&root, &rs, &e))
    };

    match result {
        Ok(violations) => std::process::exit(report(&violations, json)),
        Err(e) => {
            eprintln!("PII gate ERROR (failing closed): {e}");
            std::process::exit(1);
        }
    }
}
