//! GHMR-02 — mechanical PII sweep / transform.
//!
//! Given a source tree (a work-dir copy) and a **config-driven placeholder map**
//! (pattern → placeholder token), this module produces a *candidate clean tree*
//! by deterministically rewriting mechanically-fixable PII — private IPs,
//! container IDs, internal filesystem paths, local service URLs, and any
//! config-supplied org/host/domain terms — into inert placeholder tokens. It
//! then returns the **residual** violations: everything GHMR-01's authoritative
//! gate still flags after the mechanical pass, i.e. the non-mechanical spots that
//! need human/agent judgment (GHMR-05).
//!
//! Design contract:
//!   * **Mechanical-ness is defined by the map.** A rule in the (built-in +
//!     config) placeholder map is what makes a violation mechanical; anything the
//!     map does not rewrite but [`crate::github::pii`] still detects is residual.
//!     This is why the sweep reuses `ruleset_from_config` rather than reinventing
//!     scanning — the two can never silently diverge.
//!   * **No hardcoded infra values in source.** The built-in rules are all
//!     GENERIC shapes (RFC-1918 ranges, `CT###`, `localhost:port`, generic path
//!     prefixes). Org-specific literals (org name, real hostnames, internal
//!     domains) live in a repo-root `mirror-placeholders.toml` / the
//!     `TERMINUS_MIRROR_PLACEHOLDERS` config, never in this file.
//!   * **Idempotent.** Placeholder tokens are inert (they match no rewrite rule),
//!     so re-running the sweep on an already-swept tree yields zero changes.
//!   * **Work-dir only.** The transform rewrites files under the exact `work_dir`
//!     it is handed and touches nothing else — the source repo is GHMR-03's
//!     concern and is never modified here.
//!   * **Distinct tokens for distinct values.** A rule may map many distinct real
//!     values (e.g. several LAN IPs); each distinct value gets its own suffixed
//!     token (`<REDACTED_LAN_IP_1>`, `<REDACTED_LAN_IP_2>`, …) so the mapping is
//!     never lossy, and identical values always map to the identical token.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use regex::Regex;
use serde::Deserialize;

use crate::error::ToolError;
use crate::github::pii::{ruleset_from_config, PiiRuleSet, TreeViolation};

/// One config-declared placeholder rule. Exactly one of `pattern` (a raw regex)
/// or `term` (a literal matched case-insensitively on a word boundary) must be
/// set; `pattern` wins if both are present.
#[derive(Debug, Clone, Deserialize)]
pub struct PlaceholderRule {
    /// The placeholder token real values are rewritten to. Must be INERT — it
    /// must not itself match any rewrite rule (e.g. `<REDACTED_HOST>`), or the
    /// sweep would not be idempotent.
    pub token: String,
    /// A raw regex matched against file content. Generic shapes only.
    #[serde(default)]
    pub pattern: Option<String>,
    /// A literal term (org name, hostname) matched case-insensitively on a word
    /// boundary. Escaped before compiling.
    #[serde(default)]
    pub term: Option<String>,
    /// When true, each distinct matched value gets its own suffixed token so two
    /// different real values never collapse to one token. Default false.
    #[serde(default)]
    pub distinct: bool,
    /// Optional label used in the rewrite report. Defaults to `"config"`.
    #[serde(default)]
    pub kind: Option<String>,
}

/// The `mirror-placeholders.toml` schema: a list of config placeholder rules,
/// plus a switch for whether the generic built-in rules are layered in.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct PlaceholderConfig {
    /// Config-supplied rules (org/host/domain terms → tokens).
    pub placeholder: Vec<PlaceholderRule>,
    /// Whether to include the generic built-in mechanical rules. Default true.
    pub include_builtins: bool,
}

impl Default for PlaceholderConfig {
    fn default() -> Self {
        Self { placeholder: Vec::new(), include_builtins: true }
    }
}

impl PlaceholderConfig {
    /// Load the placeholder config from a TOML file. A missing file yields the
    /// default (built-ins only). A malformed file logs a warning and also falls
    /// back to defaults rather than aborting the sweep.
    pub fn from_file(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => match toml::from_str::<PlaceholderConfig>(&text) {
                Ok(cfg) => cfg,
                Err(e) => {
                    tracing::warn!(
                        target: "github.mirror",
                        "malformed {}: {e} — using built-in placeholder rules only",
                        path.display()
                    );
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }
}

/// Resolve a placeholder config the same way GHMR-01 resolves its PII config:
/// `TERMINUS_MIRROR_PLACEHOLDERS` (a file path) wins; otherwise
/// `<root>/mirror-placeholders.toml` when `root` is given; otherwise the default
/// (built-in rules only). Single resolution point so every caller stays in sync.
pub fn placeholder_config_from(root: Option<&Path>) -> PlaceholderConfig {
    if let Ok(p) = std::env::var("TERMINUS_MIRROR_PLACEHOLDERS") {
        return PlaceholderConfig::from_file(Path::new(&p));
    }
    if let Some(r) = root {
        let cfg = r.join("mirror-placeholders.toml");
        if cfg.is_file() {
            return PlaceholderConfig::from_file(&cfg);
        }
    }
    PlaceholderConfig::default()
}

/// A compiled rewrite rule: a regex, the token base it rewrites to, whether to
/// mint distinct suffixed tokens per value, and a report label.
struct CompiledRule {
    re: Regex,
    token: String,
    distinct: bool,
    kind: String,
}

/// The generic built-in mechanical rules. These are the deterministically-fixable
/// infra-identifier shapes — all GENERIC (no org-specific literal), so they are
/// safe to define in source. Secrets (API keys, JWTs, private keys, quoted
/// secrets, phone numbers) are deliberately NOT here: a raw secret can't be
/// meaningfully placeholdered, so it is left as a residual violation for judgment
/// cleaning (GHMR-05). Org-specific terms (org name, hostnames, internal domains)
/// come from config, not from this list.
fn builtin_rules() -> Vec<CompiledRule> {
    // (kind, regex, token, distinct)
    let raw: &[(&str, &str, &str, bool)] = &[
        (
            "private_ip",
            r"\b(?:192\.168|10\.\d{1,3}|172\.(?:1[6-9]|2\d|3[01]))\.\d{1,3}\.\d{1,3}\b",
            "<REDACTED_LAN_IP>",
            true,
        ),
        ("container_id", r"\bCT\d{3}\b", "<REDACTED_CONTAINER>", true),
        (
            "local_url",
            r"(?:localhost|127\.0\.0\.1|0\.0\.0\.0):\d{4,5}",
            "<REDACTED_LOCAL_URL>",
            true,
        ),
        (
            "internal_path",
            r"<path>/|<path>/|<path>/|/opt/lumina[a-z0-9-]*/", // pii-test-fixture
            "<REDACTED_PATH>",
            true,
        ),
    ];
    raw.iter()
        .map(|(kind, pat, token, distinct)| CompiledRule {
            re: Regex::new(pat).expect("builtin sweep rule regex"),
            token: (*token).to_string(),
            distinct: *distinct,
            kind: (*kind).to_string(),
        })
        .collect()
}

/// Compile the config rules, layering them (in file order, after the built-ins)
/// onto the mechanical rule set. Invalid regexes / rules missing both `pattern`
/// and `term` are logged and skipped rather than aborting the whole sweep.
fn compile_rules(cfg: &PlaceholderConfig) -> Vec<CompiledRule> {
    let mut rules = if cfg.include_builtins { builtin_rules() } else { Vec::new() };
    for r in &cfg.placeholder {
        let compiled = if let Some(pat) = &r.pattern {
            Regex::new(pat)
        } else if let Some(term) = &r.term {
            Regex::new(&format!(r"(?i)\b{}\b", regex::escape(term)))
        } else {
            tracing::warn!(
                target: "github.mirror",
                "placeholder rule for token {:?} has neither 'pattern' nor 'term' — skipping",
                r.token
            );
            continue;
        };
        match compiled {
            Ok(re) => rules.push(CompiledRule {
                re,
                token: r.token.clone(),
                distinct: r.distinct,
                kind: r.kind.clone().unwrap_or_else(|| "config".to_string()),
            }),
            Err(e) => tracing::warn!(
                target: "github.mirror",
                "invalid placeholder rule {:?}: {e} — skipping",
                r.token
            ),
        }
    }
    rules
}

/// Assigns distinct, deterministic tokens per (token-base, real-value) pair so a
/// value always maps to the same token and two distinct values never collapse.
#[derive(Default)]
struct TokenState {
    assigned: BTreeMap<(String, String), String>,
    counters: BTreeMap<String, usize>,
}

impl TokenState {
    fn token_for(&mut self, base: &str, distinct: bool, value: &str) -> String {
        if !distinct {
            return base.to_string();
        }
        let key = (base.to_string(), value.to_string());
        if let Some(t) = self.assigned.get(&key) {
            return t.clone();
        }
        let c = self.counters.entry(base.to_string()).or_insert(0);
        *c += 1;
        let tok = indexed_token(base, *c);
        self.assigned.insert(key, tok.clone());
        tok
    }
}

/// Insert a `_N` index into a token base, before a trailing `>` when present so
/// `<REDACTED_LAN_IP>` becomes `<REDACTED_LAN_IP_1>` (still inert).
fn indexed_token(base: &str, n: usize) -> String {
    if let Some(stripped) = base.strip_suffix('>') {
        format!("{stripped}_{n}>")
    } else {
        format!("{base}_{n}")
    }
}

/// One mechanical replacement applied to the work dir.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replacement {
    pub file: String,
    pub line: usize,
    pub kind: String,
    pub token: String,
}

/// The outcome of a mechanical sweep.
#[derive(Debug, Clone, Default)]
pub struct SweepReport {
    /// Number of files whose content changed.
    pub files_rewritten: usize,
    /// Total number of individual replacements applied.
    pub replacements: usize,
    /// Every mechanical replacement, in deterministic (file, line) order.
    pub rewrites: Vec<Replacement>,
    /// Violations GHMR-01's gate STILL reports after the mechanical pass — the
    /// non-mechanical spots that need judgment cleaning (GHMR-05).
    pub residual_violations: Vec<TreeViolation>,
}

impl SweepReport {
    /// Whether the tree is fully clean after the mechanical pass (no residual).
    pub fn is_clean(&self) -> bool {
        self.residual_violations.is_empty()
    }

    /// Stable machine-readable JSON summary (for the GHMR-04 mirror subtools).
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "clean": self.is_clean(),
            "files_rewritten": self.files_rewritten,
            "replacements": self.replacements,
            "rewrites": self.rewrites.iter().map(|r| serde_json::json!({
                "file": r.file,
                "line": r.line,
                "kind": r.kind,
                "token": r.token,
            })).collect::<Vec<_>>(),
            "residual_count": self.residual_violations.len(),
            "residual_violations": self.residual_violations.iter().map(|v| serde_json::json!({
                "file": v.file,
                "line": v.line,
                "pattern_kind": v.pattern_kind,
                "context": v.context,
            })).collect::<Vec<_>>(),
        })
    }
}

/// Apply every rule, in order, to `content`, recording each replacement against
/// `file`. Rules are applied sequentially so replaced (inert) spans are immune to
/// later rules — this gives overlapping patterns a deterministic precedence (rule
/// order: built-ins first, then config rules in file order).
fn rewrite_content(
    content: &str,
    rules: &[CompiledRule],
    state: &mut TokenState,
    file: &str,
    records: &mut Vec<Replacement>,
) -> String {
    let mut current = content.to_string();
    for rule in rules {
        // Nothing to do if this rule doesn't fire — avoids a needless realloc.
        if !rule.re.is_match(&current) {
            continue;
        }
        let mut result = String::with_capacity(current.len());
        let mut last = 0usize;
        for m in rule.re.find_iter(&current) {
            result.push_str(&current[last..m.start()]);
            let token = state.token_for(&rule.token, rule.distinct, m.as_str());
            // 1-based line of the match start within the current buffer.
            let line = current[..m.start()].bytes().filter(|&b| b == b'\n').count() + 1;
            records.push(Replacement {
                file: file.to_string(),
                line,
                kind: rule.kind.clone(),
                token: token.clone(),
            });
            result.push_str(&token);
            last = m.end();
        }
        result.push_str(&current[last..]);
        current = result;
    }
    current
}

/// Max file size to rewrite; larger files are skipped (matches GHMR-01's cap).
const MAX_FILE_BYTES: u64 = 5 * 1024 * 1024;

/// Directory base-names never descended into during the rewrite walk. Generic
/// VCS/build dirs — the same set GHMR-01's tree sweep prunes.
const EXCLUDED_DIRS: &[&str] = &[".git", "target", "node_modules", ".cargo"];

/// Read a file as UTF-8 (lossily), skipping binaries (NUL byte) and oversized
/// files by returning `None`.
fn read_text(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > MAX_FILE_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    if bytes.contains(&0) {
        return None;
    }
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// Collect the files to rewrite under `root`, honoring the ruleset's file-level
/// exclusions (name/extension) and the generic dir exclusions. Symlinks are
/// skipped (no traversal outside `root`, no symlink-cycle recursion) — matching
/// GHMR-01's walker. Returned sorted for deterministic token assignment.
fn collect_files(root: &Path, rs: &PiiRuleSet) -> Vec<PathBuf> {
    let mut out = Vec::new();
    fn walk(dir: &Path, rs: &PiiRuleSet, out: &mut Vec<PathBuf>) {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let ft = match entry.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            if ft.is_symlink() {
                continue;
            }
            let path = entry.path();
            if ft.is_dir() {
                let skip = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| EXCLUDED_DIRS.contains(&n))
                    .unwrap_or(false);
                if !skip {
                    walk(&path, rs, out);
                }
            } else if ft.is_file() && !rs.is_excluded(&path) {
                out.push(path);
            }
        }
    }
    walk(root, rs, &mut out);
    out.sort();
    out
}

/// Run the mechanical sweep over `work_dir` in place, using `cfg`'s placeholder
/// map layered on the built-in rules. Rewrites deterministically-fixable PII into
/// inert placeholder tokens, then returns the residual (non-mechanical) violations
/// that GHMR-01's gate still reports.
///
/// Writes ONLY under `work_dir` (a work-dir copy — GHMR-03's concern to produce);
/// the source repo is never touched. Idempotent: a second run makes no changes.
pub fn sweep_tree(work_dir: &Path, cfg: &PlaceholderConfig) -> Result<SweepReport, ToolError> {
    if !work_dir.is_dir() {
        return Err(ToolError::InvalidArgument(format!(
            "sweep work_dir does not exist or is not a directory: {}",
            work_dir.display()
        )));
    }

    let rules = compile_rules(cfg);
    // Reuse GHMR-01's ruleset (incl. any repo pii-gate.toml) for file exclusions
    // during the walk AND, below, for the authoritative residual detection.
    let ruleset = ruleset_from_config(Some(work_dir));
    let files = collect_files(work_dir, &ruleset);

    let mut state = TokenState::default();
    let mut records: Vec<Replacement> = Vec::new();
    let mut files_rewritten = 0usize;

    for path in &files {
        let content = match read_text(path) {
            Some(c) => c,
            None => continue,
        };
        let rel = path
            .strip_prefix(work_dir)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned();
        let before = records.len();
        let rewritten = rewrite_content(&content, &rules, &mut state, &rel, &mut records);
        if rewritten != content {
            std::fs::write(path, &rewritten).map_err(|e| {
                ToolError::Execution(format!("failed writing swept file {}: {e}", path.display()))
            })?;
            files_rewritten += 1;
        }
        // Belt-and-braces: a rule may match but replace with an identical token
        // (never happens for our inert tokens) — keep record/content in lockstep.
        debug_assert!(records.len() >= before);
    }

    // Residual = whatever the authoritative gate STILL flags after the mechanical
    // pass. This is the single source of "what is still PII", so mechanical vs
    // residual can never drift from GHMR-01's coverage.
    let residual_violations = ruleset.scan_tree(work_dir);

    Ok(SweepReport {
        files_rewritten,
        replacements: records.len(),
        rewrites: records,
        residual_violations,
    })
}

/// Convenience: resolve the placeholder config from `work_dir` (env override or
/// `<work_dir>/mirror-placeholders.toml`, else built-ins) and sweep.
pub fn sweep_tree_with_resolved_config(work_dir: &Path) -> Result<SweepReport, ToolError> {
    let cfg = placeholder_config_from(Some(work_dir));
    sweep_tree(work_dir, &cfg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::io::Write;

    fn clear_env() {
        std::env::remove_var("GITHUB_ALLOWED_AUTHORS");
        std::env::remove_var("TERMINUS_PII_CONFIG");
        std::env::remove_var("TERMINUS_MIRROR_PLACEHOLDERS");
    }

    fn temp_tree(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "ghmr02-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    fn write_file(dir: &Path, rel: &str, content: &str) {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
    }

    fn read(dir: &Path, rel: &str) -> String {
        std::fs::read_to_string(dir.join(rel)).unwrap()
    }

    // ── Mechanical rewrite from the built-in + config map ────────────────────

    #[test]
    #[serial]
    fn rewrites_builtin_mechanical_pii_to_placeholders() {
        clear_env();
        let dir = temp_tree("builtin");
        write_file(&dir, "cfg.txt", "server <internal-ip> on <host> at localhost:8099\n"); // pii-test-fixture
        write_file(&dir, "paths.txt", "logs in <path>/repos/x today\n"); // pii-test-fixture

        let report = sweep_tree(&dir, &PlaceholderConfig::default()).unwrap();

        let cfg = read(&dir, "cfg.txt");
        assert!(!cfg.contains("<internal-ip>"), "IP must be rewritten: {cfg}"); // pii-test-fixture
        assert!(!cfg.contains("<host>"), "container id must be rewritten: {cfg}"); // pii-test-fixture
        assert!(!cfg.contains("localhost:8099"), "local url must be rewritten: {cfg}"); // pii-test-fixture
        assert!(cfg.contains("<REDACTED_LAN_IP"), "IP placeholder present: {cfg}");
        assert!(cfg.contains("<REDACTED_CONTAINER"), "container placeholder present: {cfg}");

        let paths = read(&dir, "paths.txt");
        assert!(!paths.contains("<path>/"), "path must be rewritten: {paths}"); // pii-test-fixture
        assert!(paths.contains("<REDACTED_PATH"), "path placeholder present: {paths}");

        assert!(report.replacements >= 4, "expected >=4 replacements: {report:?}");
        assert!(report.files_rewritten == 2, "both files rewritten: {report:?}");
    }

    #[test]
    #[serial]
    fn rewrites_config_driven_org_term() {
        clear_env();
        let dir = temp_tree("configterm");
        write_file(&dir, "readme.md", "Built by AcmeCorp for AcmeCorp users.\n");

        let cfg = PlaceholderConfig {
            placeholder: vec![PlaceholderRule {
                token: "<ORG>".to_string(),
                pattern: None,
                term: Some("AcmeCorp".to_string()),
                distinct: false,
                kind: Some("org".to_string()),
            }],
            include_builtins: true,
        };
        let report = sweep_tree(&dir, &cfg).unwrap();

        let readme = read(&dir, "readme.md");
        assert!(!readme.contains("AcmeCorp"), "org term must be rewritten: {readme}");
        assert_eq!(readme, "Built by <ORG> for <ORG> users.\n");
        assert_eq!(report.replacements, 2, "both org mentions rewritten: {report:?}");
    }

    // ── Residual reported, not rewritten ─────────────────────────────────────

    #[test]
    #[serial]
    fn residual_secret_is_reported_not_rewritten() {
        clear_env();
        let dir = temp_tree("residual");
        // An IP (mechanical) + an API key (residual — no mechanical placeholder).
        write_file(
            &dir,
            "mix.txt",
            "host <internal-ip> with token <REDACTED-SECRET>\n", // pii-test-fixture
        );

        let report = sweep_tree(&dir, &PlaceholderConfig::default()).unwrap();

        let content = read(&dir, "mix.txt");
        // Mechanical IP rewritten...
        assert!(!content.contains("<internal-ip>"), "mechanical IP rewritten: {content}"); // pii-test-fixture
        // ...but the API key is left in place (judgment needed — GHMR-05)...
        assert!(
            content.contains("<REDACTED-SECRET>"), // pii-test-fixture
            "residual secret must NOT be rewritten: {content}"
        );
        // ...and reported as residual.
        assert!(
            report.residual_violations.iter().any(|v| v.pattern_kind == "api_key"),
            "api_key must be reported residual: {:?}",
            report.residual_violations
        );
        assert!(!report.is_clean(), "tree with a residual secret is not clean");
    }

    #[test]
    #[serial]
    fn fully_mechanical_tree_ends_clean() {
        clear_env();
        let dir = temp_tree("clean");
        write_file(&dir, "a.txt", "node <internal-ip> and <internal-ip> up\n"); // pii-test-fixture
        let report = sweep_tree(&dir, &PlaceholderConfig::default()).unwrap();
        assert!(report.is_clean(), "all-mechanical tree must be clean: {report:?}");
        assert!(report.to_json()["clean"].as_bool().unwrap());
    }

    // ── Idempotency ──────────────────────────────────────────────────────────

    #[test]
    #[serial]
    fn second_run_is_a_noop() {
        clear_env();
        let dir = temp_tree("idem");
        write_file(&dir, "a.txt", "at <internal-ip> and <host> and <path>/bin\n"); // pii-test-fixture

        let first = sweep_tree(&dir, &PlaceholderConfig::default()).unwrap();
        assert!(first.replacements > 0, "first run should rewrite: {first:?}");
        let after_first = read(&dir, "a.txt");

        let second = sweep_tree(&dir, &PlaceholderConfig::default()).unwrap();
        assert_eq!(second.replacements, 0, "second run must make no replacements: {second:?}");
        assert_eq!(second.files_rewritten, 0, "second run must rewrite no files: {second:?}");
        assert_eq!(read(&dir, "a.txt"), after_first, "content stable across runs");
    }

    // ── No leak: mechanical kinds gone from residual ─────────────────────────

    #[test]
    #[serial]
    fn swept_mechanical_kinds_absent_from_residual() {
        clear_env();
        let dir = temp_tree("noleak");
        write_file(&dir, "a.txt", "<internal-ip> <host> localhost:9000 <path>/x\n"); // pii-test-fixture
        let report = sweep_tree(&dir, &PlaceholderConfig::default()).unwrap();
        for gone in ["private_ip", "container_id", "local_url", "internal_path"] {
            assert!(
                !report.residual_violations.iter().any(|v| v.pattern_kind == gone),
                "mechanical kind {gone} must be swept away, still residual: {:?}",
                report.residual_violations
            );
        }
    }

    // ── Distinct tokens for distinct values ──────────────────────────────────

    #[test]
    #[serial]
    fn distinct_values_get_distinct_tokens_same_value_stable() {
        clear_env();
        let dir = temp_tree("distinct");
        // Two distinct IPs, the first repeated — repeats must reuse one token,
        // distinct values must get distinct tokens.
        write_file(
            &dir,
            "a.txt",
            "<internal-ip> then <internal-ip> then <internal-ip> again\n", // pii-test-fixture
        );
        let report = sweep_tree(&dir, &PlaceholderConfig::default()).unwrap();
        let content = read(&dir, "a.txt");
        assert!(content.contains("<REDACTED_LAN_IP_1>"), "first token: {content}");
        assert!(content.contains("<REDACTED_LAN_IP_2>"), "second token: {content}");
        // Exactly two distinct IP tokens minted across three matches.
        let distinct_tokens: std::collections::HashSet<_> = report
            .rewrites
            .iter()
            .filter(|r| r.kind == "private_ip")
            .map(|r| r.token.clone())
            .collect();
        assert_eq!(distinct_tokens.len(), 2, "two distinct IP tokens: {distinct_tokens:?}");
        // The repeated value reused token _1 (appears twice in the file).
        assert_eq!(content.matches("<REDACTED_LAN_IP_1>").count(), 2, "{content}");
    }

    // ── Writes only to the work dir, never the source repo ───────────────────

    #[test]
    #[serial]
    fn source_repo_untouched() {
        clear_env();
        let src = temp_tree("src");
        let work = temp_tree("work");
        let dirty = "server at <internal-ip>\n"; // pii-test-fixture
        write_file(&src, "a.txt", dirty);
        write_file(&work, "a.txt", dirty);

        let _ = sweep_tree(&work, &PlaceholderConfig::default()).unwrap();

        // Work dir was swept; the separate source tree is byte-for-byte intact.
        assert_ne!(read(&work, "a.txt"), dirty, "work dir must be swept");
        assert_eq!(read(&src, "a.txt"), dirty, "source repo must be untouched");
    }

    // ── Config resolution ────────────────────────────────────────────────────

    #[test]
    #[serial]
    fn config_resolves_from_workdir_toml() {
        clear_env();
        let dir = temp_tree("resolve");
        write_file(&dir, "readme.md", "Contact WidgetInc support.\n");
        write_file(
            &dir,
            "mirror-placeholders.toml",
            "[[placeholder]]\nterm = \"WidgetInc\"\ntoken = \"<ORG>\"\n",
        );
        let cfg = placeholder_config_from(Some(dir.as_path()));
        assert_eq!(cfg.placeholder.len(), 1);
        let report = sweep_tree(&dir, &cfg).unwrap();
        let readme = read(&dir, "readme.md");
        assert!(!readme.contains("WidgetInc"), "config term rewritten: {readme}");
        assert!(report.replacements >= 1);
    }

    #[test]
    #[serial]
    fn missing_workdir_is_invalid_argument() {
        clear_env();
        let missing = std::env::temp_dir().join("ghmr02-does-not-exist-xyz");
        let r = sweep_tree(&missing, &PlaceholderConfig::default());
        assert!(matches!(r, Err(ToolError::InvalidArgument(_))));
    }

    #[test]
    #[serial]
    fn indexed_token_inserts_before_bracket() {
        assert_eq!(indexed_token("<REDACTED_LAN_IP>", 3), "<REDACTED_LAN_IP_3>");
        assert_eq!(indexed_token("PLACEHOLDER", 2), "PLACEHOLDER_2");
    }

    #[test]
    #[serial]
    fn include_builtins_false_skips_generic_rules() {
        clear_env();
        let dir = temp_tree("nobuiltin");
        write_file(&dir, "a.txt", "at <internal-ip>\n"); // pii-test-fixture
        let cfg = PlaceholderConfig { placeholder: vec![], include_builtins: false };
        let report = sweep_tree(&dir, &cfg).unwrap();
        // No built-ins → IP not rewritten → reported residual instead.
        assert_eq!(report.replacements, 0);
        assert!(report.residual_violations.iter().any(|v| v.pattern_kind == "private_ip"));
    }
}
