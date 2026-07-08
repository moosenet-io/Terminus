//! GHMR-01: Rust PII pre-push / pre-commit gate — the authoritative replacement
//! for the legacy `.githooks/pii_gate.py`.
//!
//! It is a thin CLI around [`terminus_rs::github::pii`]'s tree-sweep engine.
//! All detection logic lives in the library (shared with the runtime GitHub
//! write gate and the mirror engine); this binary only decides *what set of
//! files* to scan and how to report.
//!
//! ## Modes
//! - (default, git pre-push): reads the pre-push protocol on stdin
//!   (`<local_ref> <local_sha> <remote_ref> <remote_sha>` per line), scans the
//!   files changed in each pushed commit range.
//! - `--staged` (git pre-commit): scans `git diff --cached` files.
//! - `--tree [PATH]`: sweeps an entire directory tree (defaults to the repo
//!   root / cwd) — used by the mirror engine and for full audits.
//! - `--json`: emit a machine-readable JSON report instead of the human summary.
//!
//! Config (optional): a repo-root `pii-gate.toml` (or the path in
//! `TERMINUS_PII_CONFIG`) supplies repo-specific terms, extra patterns, allowed
//! emails, and exclusions. Missing config uses the built-in defaults.
//!
//! Exit code: `0` when clean, `1` when any violation is found (hard block).
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

use terminus_rs::github::pii::{
    violations_to_json, PiiRuleSet, TreeViolation,
};

const NULL_SHA: &str = "0000000000000000000000000000000000000000"; // pii-test-fixture

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

/// Scan an explicit list of repo-relative files (that still exist on disk),
/// honoring the `pii-test-fixture` line exemption.
fn scan_files(rs: &PiiRuleSet, root: &Path, files: &[String]) -> Vec<TreeViolation> {
    let mut out = Vec::new();
    for rel in files {
        let path = root.join(rel);
        if !path.is_file() {
            continue;
        }
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(_) => continue,
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

fn git_names(args: &[&str]) -> Vec<String> {
    let out = Command::new("git").args(args).output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

fn changed_files_prepush() -> Vec<String> {
    let mut stdin = String::new();
    if std::io::stdin().read_to_string(&mut stdin).is_err() {
        return Vec::new();
    }
    let mut files = Vec::new();
    for line in stdin.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 4 {
            continue;
        }
        let (local_sha, remote_sha) = (parts[1], parts[3]);
        if local_sha == NULL_SHA {
            continue; // branch deletion
        }
        let range = if remote_sha == NULL_SHA {
            local_sha.to_string() // new branch: scan its tip commit
        } else {
            format!("{remote_sha}..{local_sha}")
        };
        files.extend(git_names(&["diff", "--name-only", &range]));
    }
    files.sort();
    files.dedup();
    files
}

fn report(violations: &[TreeViolation], json: bool) -> i32 {
    if json {
        println!("{}", violations_to_json(violations));
        return if violations.is_empty() { 0 } else { 1 };
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

    let violations = if tree_mode {
        // Optional explicit path after --tree.
        let path = args
            .iter()
            .position(|a| a == "--tree")
            .and_then(|i| args.get(i + 1))
            .filter(|s| !s.starts_with("--"))
            .map(PathBuf::from)
            .unwrap_or_else(|| root.clone());
        rs.scan_tree(&path)
    } else if staged {
        let files = git_names(&["diff", "--cached", "--name-only"]);
        scan_files(&rs, &root, &files)
    } else {
        let files = changed_files_prepush();
        scan_files(&rs, &root, &files)
    };

    std::process::exit(report(&violations, json));
}
