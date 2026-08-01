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
//! - `--visibility <internal|public>`: override the repo's declared posture.
//! - `--posture`: print the resolved posture and why, then exit 0.
//!
//! ## Posture (internal vs public)
//! The scan is always full-strength; posture decides only which categories this
//! CLI *reports*. At `internal` — declared via `[repository] visibility` in
//! `.moosenet-repo.toml` — the fleet's own infrastructure IDENTIFIERS
//! ([`INTERNAL_SUPPRESSED`]: container ids, internal hostnames/domains/paths,
//! uuids, phones, operator name, infra services) are not reported, because an
//! internal repo legitimately documents them. Real credentials — private IPs,
//! API keys, JWTs, PEM keys, quoted secrets — are NEVER posture-gated and fire
//! at every posture. This mirrors the Python gate's `EXTENDED_PATTERNS` split,
//! which it replaces. An absent or malformed declaration fails CLOSED to
//! `public`.
//!
//! The posture filter is deliberately confined to this binary: the library seam
//! [`ruleset_from_config`] is shared with the runtime write gate and the
//! git-public mirror engine, which must stay unconditionally full-strength.
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

use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use terminus_rs::github::pii::{ruleset_from_config, violations_to_json, PiiRuleSet, TreeViolation};

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

/// Run a git command in `root`, returning raw stdout bytes. Used for `-z`
/// (NUL-delimited) path listings, where a filename may contain a newline, tab,
/// quote, backslash, or non-UTF-8 byte that a line-based / UTF-8-lossy parse
/// would corrupt — silently dropping the file and creating a detection bypass.
fn git_bytes(root: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
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
    Ok(out.stdout)
}

/// Read a git blob (`<rev>:<path>`, or `:<path>` for the index) as raw bytes.
/// `rel` is the exact path bytes as git reported them (`-z` output), so
/// filenames containing shell/UTF-8-hostile bytes resolve correctly instead of
/// failing `git show` and being skipped. Returns `None` for unreadable blobs.
fn read_blob(root: &Path, rev: &str, rel: &[u8]) -> Option<Vec<u8>> {
    // Build the `<rev>:<path>` pathspec as an OsString so non-UTF-8 path bytes
    // round-trip exactly, rather than going through a lossy String.
    let mut spec = OsString::from(format!("{rev}:"));
    spec.push(OsStr::from_bytes(rel));
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("show")
        .arg(&spec)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(out.stdout)
}

/// Split line-based git output (SHAs — always ASCII-safe) into trimmed,
/// non-empty entries.
fn names(out: &str) -> Vec<String> {
    out.lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Split NUL-delimited (`-z`) git output into raw path byte-strings, dropping
/// empties. Paths are kept as bytes (never line-split, never UTF-8-lossied) so
/// no filename can smuggle content past the gate.
fn paths_z(bytes: &[u8]) -> Vec<Vec<u8>> {
    bytes
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_vec())
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
    // Shared resolver: TERMINUS_PII_CONFIG, else <root>/pii-gate.toml, else default.
    ruleset_from_config(Some(root))
}

/// Repository posture, mirroring the `visibility` declaration the Python gate
/// read from `.moosenet-repo.toml`.
///
/// The scan is ALWAYS full-strength; posture only decides which categories this
/// CLI *reports*. An internal repo legitimately carries its own infrastructure
/// identifiers (container ids, hostnames, operator paths) in docs and comments —
/// a public export must not.
///
/// This filter lives in the BINARY on purpose. [`ruleset_from_config`] is the
/// single seam shared with the runtime write gate, the `github_pii_scan` tool,
/// and the git-public mirror engine; teaching it about posture would weaken the
/// mirror, which must stay unconditionally full-strength. Filtering at the CLI
/// boundary leaves every library surface byte-for-byte unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Visibility {
    Internal,
    Public,
}

impl Visibility {
    fn as_str(self) -> &'static str {
        match self {
            Visibility::Internal => "internal",
            Visibility::Public => "public",
        }
    }

    /// Case-insensitive parse. Returns `None` for anything unrecognized so the
    /// caller can fail closed rather than guess.
    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "internal" => Some(Visibility::Internal),
            "public" => Some(Visibility::Public),
            _ => None,
        }
    }
}

/// Categories suppressed at `Internal` posture: the fleet's own infrastructure
/// IDENTIFIERS. This is the exact analogue of the Python gate's
/// `EXTENDED_PATTERNS` (container number, internal hostname/url/path, uuid,
/// phone) plus the two identifier detectors only the Rust engine has
/// (`operator_name`, `infra_service`).
///
/// These strings are the categories the engine actually EMITS, which are not
/// always its struct field names — `internal_hostname` (field `internal_host`)
/// and `uuid_secret` (field `uuid`) are the two that differ. A typo here would
/// silently suppress nothing and look like a working filter, so
/// `internal_posture_suppresses_every_listed_category` pins each name against a
/// live scan.
///
/// Everything NOT listed fires at every posture: `private_ip`, `api_key`,
/// `email`, `jwt`, `ssh_key`, `aws_access_key`, `google_api_key`,
/// `slack_user_token`, `generic_secret`, and any operator-configured
/// `config_term` / `config_pattern`. A real credential is never posture-gated.
const INTERNAL_SUPPRESSED: &[&str] = &[
    "container_id",
    "internal_hostname",
    "internal_domain",
    "internal_path",
    "uuid_secret",
    "phone",
    "operator_name",
    "infra_service",
];

/// Resolve the repo posture from `<root>/.moosenet-repo.toml`.
///
/// FAILS CLOSED: a missing, unreadable, unparseable, or unrecognized-value
/// declaration yields [`Visibility::Public`] (the strictest posture). A
/// malformed config must never silently weaken the gate.
///
/// The third element is `true` when the posture came from a FALLBACK rather
/// than a valid declaration. [`main`] logs the reason to stderr in that case, in
/// every mode — otherwise a repo whose posture config is broken would be scanned
/// at a posture nobody asked for, and the only hint would be flags an operator
/// has no reason to run (`--posture` / `--json`). Silent is the one thing a
/// fail-closed path must not be.
fn repo_visibility(root: &Path) -> (Visibility, String, bool) {
    let path = root.join(".moosenet-repo.toml");
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            return (
                Visibility::Public,
                format!(".moosenet-repo.toml unreadable ({e}) — failing closed to public"),
                true,
            )
        }
    };
    let parsed: toml::Value = match toml::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            return (
                Visibility::Public,
                format!(".moosenet-repo.toml is not valid TOML ({e}) — failing closed to public"),
                true,
            )
        }
    };
    match parsed
        .get("repository")
        .and_then(|r| r.get("visibility"))
        .and_then(|v| v.as_str())
    {
        Some(s) => match Visibility::parse(s) {
            Some(v) => (v, format!("declared in .moosenet-repo.toml as {s:?}"), false),
            None => (
                Visibility::Public,
                format!("unrecognized visibility {s:?} — failing closed to public"),
                true,
            ),
        },
        None => (
            Visibility::Public,
            "no [repository].visibility declared — failing closed to public".to_string(),
            true,
        ),
    }
}

/// Drop the internal-identifier categories when the repo is internal. Returns
/// the retained violations and the number suppressed.
///
/// At `Public` posture this is an exact no-op, so public/mirror-facing behavior
/// is unchanged from before posture existed.
fn filter_by_posture(
    violations: Vec<TreeViolation>,
    vis: Visibility,
) -> (Vec<TreeViolation>, usize) {
    if vis == Visibility::Public {
        return (violations, 0);
    }
    let before = violations.len();
    let kept: Vec<TreeViolation> = violations
        .into_iter()
        .filter(|v| !INTERNAL_SUPPRESSED.contains(&v.pattern_kind.as_str()))
        .collect();
    let suppressed = before - kept.len();
    (kept, suppressed)
}

/// Parse `--visibility <internal|public>`. `Some(Err(..))` means the flag was
/// given with a bad or missing value — a hard error, never a silent fallback.
fn visibility_override(args: &[String]) -> Option<Result<Visibility, String>> {
    let i = args.iter().position(|a| a == "--visibility")?;
    match args.get(i + 1) {
        None => Some(Err("--visibility requires a value (internal|public)".into())),
        Some(v) => Some(
            Visibility::parse(v)
                .ok_or_else(|| format!("invalid --visibility {v:?} (expected internal|public)")),
        ),
    }
}

/// Scan a set of `(rev, path-bytes)` blobs, honoring exclusions and the
/// `pii-test-fixture` line-exact exemption. Paths are raw bytes so no filename
/// can evade the scan.
fn scan_blobs(root: &Path, rs: &PiiRuleSet, entries: &[(String, Vec<u8>)]) -> Vec<TreeViolation> {
    let mut out = Vec::new();
    for (rev, rel) in entries {
        let rel_path = Path::new(OsStr::from_bytes(rel));
        if rs.is_excluded(rel_path) {
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
        let file = String::from_utf8_lossy(rel).into_owned();
        for v in rs.scan_content(&scrubbed) {
            out.push(TreeViolation {
                file: file.clone(),
                line: v.line,
                pattern_kind: v.category,
                context: v.context,
            });
        }
    }
    out
}

/// Enumerate `(commit_sha, path)` blobs being pushed, across EVERY commit the
/// push introduces — not just the tip — so a secret added in an intermediate
/// commit and removed by the tip is still caught (it would otherwise enter
/// permanent remote history). Fails closed on any git error.
fn prepush_entries(root: &Path) -> Result<Vec<(String, Vec<u8>)>, String> {
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

        // Commits introduced by this push. For an existing remote ref that is an
        // exact range; for a new branch, everything reachable from the tip that
        // is not already on a remote-tracking branch (fail-safe: if no remotes
        // are tracked this scans full history rather than nothing). rev-list
        // emits SHAs (ASCII), so line parsing is safe here.
        let commits = if remote_sha == NULL_SHA {
            let listed = names(&git(root, &["rev-list", local_sha, "--not", "--remotes"])?);
            if listed.is_empty() {
                // Nothing unique found — fall back to the full tip tree so we
                // never scan an empty set on a first push. `-z` keeps paths raw.
                for f in paths_z(&git_bytes(
                    root,
                    &["ls-tree", "-r", "--name-only", "-z", local_sha],
                )?) {
                    entries.push((local_sha.to_string(), f));
                }
                continue;
            }
            listed
        } else {
            names(&git(root, &["rev-list", &format!("{remote_sha}..{local_sha}")])?)
        };

        for c in commits {
            // Files changed by commit `c` (vs its parent; `--root` so the repo's
            // first commit lists all its files). `-z` emits raw NUL-delimited
            // paths so no filename can smuggle a blob past the gate. Blob is read
            // at `c` in scan_blobs.
            let files = paths_z(&git_bytes(
                root,
                &["diff-tree", "--root", "--no-commit-id", "--name-only", "-r", "-z", &c],
            )?);
            for f in files {
                entries.push((c.clone(), f));
            }
        }
    }
    entries.sort();
    entries.dedup();
    Ok(entries)
}

/// Enumerate `("", path)` staged (index) blobs. Fails closed on git error.
/// `-z` keeps paths raw so hostile filenames cannot evade the staged scan.
fn staged_entries(root: &Path) -> Result<Vec<(String, Vec<u8>)>, String> {
    let files = paths_z(&git_bytes(root, &["diff", "--cached", "--name-only", "-z"])?);
    Ok(files.into_iter().map(|f| (String::new(), f)).collect())
}

fn report(
    violations: &[TreeViolation],
    json: bool,
    vis: Visibility,
    posture_reason: &str,
    suppressed: usize,
) -> i32 {
    if json {
        // Additive keys only — `clean`/`count`/`violations` keep their existing
        // shape so any consumer of the previous report still parses.
        let mut v = violations_to_json(violations);
        if let Some(obj) = v.as_object_mut() {
            obj.insert("posture".into(), vis.as_str().into());
            obj.insert("posture_reason".into(), posture_reason.into());
            obj.insert("suppressed_by_posture".into(), suppressed.into());
        }
        println!("{v}");
        return i32::from(!violations.is_empty());
    }
    if violations.is_empty() {
        if suppressed > 0 {
            println!(
                "PII gate: clean (0 violations; {suppressed} internal-identifier \
                 finding(s) not reported at {} posture).",
                vis.as_str()
            );
        } else {
            println!("PII gate: clean (0 violations).");
        }
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

    // Resolve posture BEFORE scanning so `--posture` can answer without work,
    // and so a bad `--visibility` value is a hard error rather than a scan that
    // silently used the wrong strictness.
    let (mut vis, mut posture_reason, mut posture_fallback) = repo_visibility(&root);
    if let Some(over) = visibility_override(&args) {
        match over {
            Ok(v) => {
                vis = v;
                posture_reason = format!("overridden on the command line to {:?}", v.as_str());
                // An explicit operator choice is never a silent fallback.
                posture_fallback = false;
            }
            Err(e) => {
                eprintln!("PII gate ERROR (failing closed): {e}");
                std::process::exit(1);
            }
        }
    }

    if args.iter().any(|a| a == "--posture") {
        println!("{} ({posture_reason})", vis.as_str());
        std::process::exit(0);
    }

    // Announce a fallback resolution ONCE, on stderr, in every mode — including
    // a plain `--json` run, where stdout must stay a clean parseable report.
    // A correctly-declared posture stays quiet so normal pushes are not noisy.
    if posture_fallback {
        eprintln!("PII gate: posture {} — {posture_reason}", vis.as_str());
    }

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
        Ok(violations) => {
            let (kept, suppressed) = filter_by_posture(violations, vis);
            std::process::exit(report(&kept, json, vis, &posture_reason, suppressed))
        }
        Err(e) => {
            eprintln!("PII gate ERROR (failing closed): {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod posture_tests {
    use super::*;
    use terminus_rs::github::pii::PiiRuleSet;

    fn repo_with(toml_body: Option<&str>) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        if let Some(body) = toml_body {
            std::fs::write(dir.path().join(".moosenet-repo.toml"), body).expect("write");
        }
        dir
    }

    fn scan_text(dir: &Path, name: &str, body: &str) -> Vec<TreeViolation> {
        std::fs::write(dir.join(name), body).expect("write sample");
        PiiRuleSet::new().scan_tree(dir)
    }

    // ── posture resolution ────────────────────────────────────────────────

    #[test]
    fn internal_declaration_resolves_internal() {
        let d = repo_with(Some("[repository]\nvisibility = \"internal\"\n"));
        let (vis, _, fallback) = repo_visibility(d.path());
        assert_eq!(vis, Visibility::Internal);
        assert!(!fallback, "a valid declaration is not a fallback");
    }

    #[test]
    fn public_declaration_resolves_public() {
        let d = repo_with(Some("[repository]\nvisibility = \"public\"\n"));
        let (vis, _, fallback) = repo_visibility(d.path());
        assert_eq!(vis, Visibility::Public);
        assert!(!fallback, "a valid declaration is not a fallback");
    }

    #[test]
    fn visibility_parse_is_case_insensitive() {
        let d = repo_with(Some("[repository]\nvisibility = \"Internal\"\n"));
        assert_eq!(repo_visibility(d.path()).0, Visibility::Internal);
    }

    /// The three fail-closed paths. A malformed posture declaration must never
    /// be read as "internal" — that would silently weaken the gate.
    #[test]
    fn missing_malformed_and_unknown_all_fail_closed_to_public() {
        let absent = repo_with(None);
        let (v, why, _) = repo_visibility(absent.path());
        assert_eq!(v, Visibility::Public, "absent file must fail closed");
        assert!(why.contains("unreadable"), "reason should explain: {why}");

        let bad = repo_with(Some("this is not valid toml ["));
        assert_eq!(
            repo_visibility(bad.path()).0,
            Visibility::Public,
            "unparseable file must fail closed"
        );

        let unknown = repo_with(Some("[repository]\nvisibility = \"semi-public\"\n"));
        assert_eq!(
            repo_visibility(unknown.path()).0,
            Visibility::Public,
            "unrecognized value must fail closed"
        );

        let no_key = repo_with(Some("[repository]\ndescription = \"x\"\n"));
        assert_eq!(
            repo_visibility(no_key.path()).0,
            Visibility::Public,
            "absent visibility key must fail closed"
        );
    }

    // ── override flag ─────────────────────────────────────────────────────

    #[test]
    fn visibility_override_wins_in_both_directions() {
        let to_pub = ["--visibility".to_string(), "public".to_string()];
        assert_eq!(visibility_override(&to_pub).unwrap().unwrap(), Visibility::Public);
        let to_int = ["--visibility".to_string(), "internal".to_string()];
        assert_eq!(
            visibility_override(&to_int).unwrap().unwrap(),
            Visibility::Internal
        );
        assert!(visibility_override(&["--json".to_string()]).is_none());
    }

    #[test]
    fn bad_or_missing_visibility_value_is_an_error_not_a_fallback() {
        let bad = ["--visibility".to_string(), "sorta".to_string()];
        assert!(visibility_override(&bad).unwrap().is_err());
        let missing = ["--visibility".to_string()];
        assert!(visibility_override(&missing).unwrap().is_err());
    }

    // ── the filter ────────────────────────────────────────────────────────

    fn v(kind: &str) -> TreeViolation {
        TreeViolation {
            file: "f.rs".into(),
            line: 1,
            pattern_kind: kind.into(),
            context: "ctx".into(),
        }
    }

    #[test]
    fn public_posture_is_an_exact_no_op() {
        let input: Vec<TreeViolation> =
            INTERNAL_SUPPRESSED.iter().map(|k| v(k)).chain([v("api_key")]).collect();
        let n = input.len();
        let (kept, suppressed) = filter_by_posture(input, Visibility::Public);
        assert_eq!(kept.len(), n, "public must not drop anything");
        assert_eq!(suppressed, 0);
    }

    /// Pins every entry of `INTERNAL_SUPPRESSED` against the categories the
    /// engine ACTUALLY emits. Two names differ from their struct fields
    /// (`internal_hostname`, `uuid_secret`); a typo would make the filter a
    /// silent no-op that still looks like it works.
    #[test]
    fn internal_posture_suppresses_every_listed_category() {
        for kind in INTERNAL_SUPPRESSED {
            let (kept, suppressed) = filter_by_posture(vec![v(kind)], Visibility::Internal);
            assert!(kept.is_empty(), "{kind} should be suppressed at internal");
            assert_eq!(suppressed, 1);
        }
    }

    /// The other half of the pin: each suppressed name must be a category the
    /// engine can really produce, proven by scanning content that triggers it.
    #[test]
    fn every_suppressed_name_is_a_real_engine_category() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sample = concat!(
            "host <host> and <host> here\n", // pii-test-fixture
            "see example.com for docs\n", // pii-test-fixture
            "path <path>/repos/x\n", // pii-test-fixture
            // `uuid_secret` is CONTEXT-gated: a bare UUID is deliberately
            // allowed, and it only fires within ~40 chars of an infra-secret
            // cue (`project_id` here). A fixture without a cue silently
            // produces no uuid finding at all.
            "project_id <uuid> set\n", // pii-test-fixture
            "call <phone> now\n", // pii-test-fixture
            "ask <operator> about it\n", // pii-test-fixture
            "runs <secret-manager> on the box\n", // pii-test-fixture
        );
        let found: std::collections::HashSet<String> = scan_text(dir.path(), "s.txt", sample)
            .into_iter()
            .map(|x| x.pattern_kind)
            .collect();
        for kind in INTERNAL_SUPPRESSED {
            assert!(
                found.contains(*kind),
                "INTERNAL_SUPPRESSED lists {kind:?}, but no such category was emitted \
                 by a scan that should trigger it — the name is wrong and the filter \
                 would silently suppress nothing. Emitted: {found:?}"
            );
        }
    }

    /// Every fail-closed path must REPORT itself, not just fail closed. A
    /// broken posture declaration that silently resolves to public looks
    /// identical to a healthy repo from the operator's side; `main` logs the
    /// reason to stderr whenever this flag is set.
    #[test]
    fn every_fail_closed_path_is_flagged_as_a_fallback() {
        let cases: Vec<(&str, Option<&str>)> = vec![
            ("absent file", None),
            ("unparseable toml", Some("this is not valid toml [")),
            (
                "unknown value",
                Some("[repository]\nvisibility = \"semi-public\"\n"),
            ),
            ("missing key", Some("[repository]\ndescription = \"x\"\n")),
        ];
        for (label, body) in cases {
            let d = repo_with(body);
            let (vis, reason, fallback) = repo_visibility(d.path());
            assert_eq!(vis, Visibility::Public, "{label} must fail closed");
            assert!(fallback, "{label} must be flagged as a fallback so it gets logged");
            assert!(
                !reason.trim().is_empty(),
                "{label} must carry an explanatory reason"
            );
        }
    }

    /// The security-critical half: credentials are never posture-gated.
    #[test]
    fn internal_posture_never_suppresses_a_real_credential() {
        let always = [
            "private_ip",
            "api_key",
            "email",
            "jwt",
            "ssh_key",
            "aws_access_key",
            "google_api_key",
            "slack_user_token",
            "generic_secret",
            "config_term",
            "config_pattern",
        ];
        for kind in always {
            let (kept, suppressed) = filter_by_posture(vec![v(kind)], Visibility::Internal);
            assert_eq!(kept.len(), 1, "{kind} must ALWAYS be reported");
            assert_eq!(suppressed, 0);
        }
    }

    /// End-to-end over a real scan: an internal repo hides its own identifiers
    /// but still blocks a leaked private IP and API key.
    #[test]
    fn internal_scan_hides_identifiers_but_still_blocks_secrets() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sample = concat!(
            "deployed on <host> at <host> under <path>/repos\n", // pii-test-fixture
            "gateway <internal-ip> is the host\n", // pii-test-fixture
            "key = \"<REDACTED-SECRET>\"\n", // pii-test-fixture
        );
        let all = scan_text(dir.path(), "s.txt", sample);
        let (kept, suppressed) = filter_by_posture(all, Visibility::Internal);
        assert!(suppressed > 0, "identifiers should have been suppressed");
        let kinds: std::collections::HashSet<&str> =
            kept.iter().map(|x| x.pattern_kind.as_str()).collect();
        assert!(kinds.contains("private_ip"), "private IP must still block: {kinds:?}");
        assert!(kinds.contains("api_key"), "API key must still block: {kinds:?}");
        assert!(!kinds.contains("container_id"), "CT### should be hidden: {kinds:?}");
        assert!(!kinds.contains("internal_path"), "path should be hidden: {kinds:?}");
    }
}
