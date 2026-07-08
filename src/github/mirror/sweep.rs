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

/// A regex that matches the already-minted indexed tokens for `base` and captures
/// the numeric suffix — e.g. base `<REDACTED_LAN_IP>` matches
/// `<REDACTED_LAN_IP_7>` capturing `7`. Used to seed the counter so incremental
/// sweeps continue numbering past existing placeholders (never reusing `_1`).
fn index_regex(base: &str) -> Regex {
    let pat = if let Some(stem) = base.strip_suffix('>') {
        format!(r"{}_(\d+)>", regex::escape(stem))
    } else {
        format!(r"{}_(\d+)", regex::escape(base))
    };
    Regex::new(&pat).expect("index regex")
}

/// Seed the token counters from placeholders ALREADY present in the tree so a
/// subsequent (incremental) sweep assigns fresh indices instead of restarting at
/// `_1` and colliding a new distinct value with an existing token. Only distinct
/// rules carry indices; the counter is set to the max existing suffix per base.
fn seed_token_state(contents: &[(PathBuf, String, String)], rules: &[CompiledRule], state: &mut TokenState) {
    for rule in rules {
        if !rule.distinct {
            continue;
        }
        let idx_re = index_regex(&rule.token);
        let mut max = 0usize;
        for (_, _, content) in contents {
            for caps in idx_re.captures_iter(content) {
                if let Some(n) = caps.get(1).and_then(|m| m.as_str().parse::<usize>().ok()) {
                    max = max.max(n);
                }
            }
        }
        if max > 0 {
            let entry = state.counters.entry(rule.token.clone()).or_insert(0);
            *entry = (*entry).max(max);
        }
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
            // Honor the `// pii-test-fixture` exemption exactly as GHMR-01's
            // scan_tree does: a match on a fixture-marked line is left intact,
            // so the rewrite pass and the authoritative residual scan agree on
            // which literals are deliberately preserved (test expectations,
            // rule-definition fixtures) rather than the sweep mangling them.
            let line_start = current[..m.start()].rfind('\n').map(|i| i + 1).unwrap_or(0);
            let line_end = current[m.end()..]
                .find('\n')
                .map(|i| m.end() + i)
                .unwrap_or(current.len());
            if current[line_start..line_end].contains("pii-test-fixture") {
                // Pass the matched span through unchanged.
                result.push_str(&current[last..m.end()]);
                last = m.end();
                continue;
            }
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

/// Read a file as text for rewriting, returning `None` (skip) when it is
/// oversized, binary (contains a NUL byte), OR not valid UTF-8. The strict
/// UTF-8 requirement matters here (unlike GHMR-01's read-only lossy scan): the
/// sweep WRITES the result back, and lossily decoding invalid bytes to U+FFFD
/// then rewriting would silently corrupt the file's unrelated bytes. Such a file
/// is left byte-for-byte intact and still surfaces via the (read-only) residual
/// scan — "flag rather than silently break".
fn read_text(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > MAX_FILE_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    if bytes.contains(&0) {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// Base-name of the active placeholder configuration. Never rewritten: the
/// config legitimately holds the real values a matcher would otherwise rewrite
/// inside its own `term`/`pattern`, which would corrupt the config and break
/// later incremental sweeps.
const CONFIG_BASENAME: &str = "mirror-placeholders.toml";

/// Collect the files to rewrite under `root`, honoring the ruleset's file-level
/// exclusions (name/extension) AND its configured directory exclusions — the
/// same posture `scan_tree` uses — plus any `skip` paths (the resolved config
/// file) and the active-config base-name. Symlinks are skipped (no traversal
/// outside `root`, no symlink-cycle recursion). Returned sorted for deterministic
/// token assignment.
fn collect_files(root: &Path, rs: &PiiRuleSet, skip: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    fn walk(dir: &Path, rs: &PiiRuleSet, skip: &[PathBuf], out: &mut Vec<PathBuf>) {
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
                // Honor the ruleset's configured dir exclusions (defaults +
                // pii-gate.toml `excluded_dirs`), matching scan_tree exactly.
                let excluded = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| rs.is_excluded_dir(n))
                    .unwrap_or(false);
                if !excluded {
                    walk(&path, rs, skip, out);
                }
            } else if ft.is_file() {
                if rs.is_excluded(&path) {
                    continue;
                }
                // Skip the single active placeholder config (compared canonically
                // so relative/symlinked paths still match). ONLY the exact resolved
                // config path is exempt — a same-named file nested elsewhere in the
                // tree is ordinary content and is swept normally.
                if !skip.is_empty() {
                    if let Ok(canon) = path.canonicalize() {
                        if skip.contains(&canon) {
                            continue;
                        }
                    }
                }
                out.push(path);
            }
        }
    }
    walk(root, rs, skip, &mut out);
    out.sort();
    out
}

/// The canonicalized path of the single active placeholder config that must
/// never be rewritten or counted as residual PII — mirroring
/// `placeholder_config_from`'s resolution exactly: the
/// `TERMINUS_MIRROR_PLACEHOLDERS`-pointed file when set, otherwise the repo-root
/// `<work_dir>/mirror-placeholders.toml`. Returns the exact path (not a
/// base-name), so a same-named file nested elsewhere in the tree is NOT exempt
/// and is swept/scanned as ordinary content.
fn active_config_skip(work_dir: &Path) -> Vec<PathBuf> {
    if let Ok(p) = std::env::var("TERMINUS_MIRROR_PLACEHOLDERS") {
        if !p.is_empty() {
            return Path::new(&p).canonicalize().ok().into_iter().collect();
        }
    }
    work_dir
        .join(CONFIG_BASENAME)
        .canonicalize()
        .ok()
        .into_iter()
        .collect()
}

/// The `skip` config paths (canonical) expressed relative to `work_dir`, so they
/// can be matched against `TreeViolation.file` (a work-dir-relative path) to drop
/// the active config from residual results.
fn skip_relative_names(work_dir: &Path, skip: &[PathBuf]) -> Vec<String> {
    let work_canon = work_dir.canonicalize().ok();
    skip.iter()
        .filter_map(|p| {
            work_canon
                .as_ref()
                .and_then(|w| p.strip_prefix(w).ok())
                .map(|r| r.to_string_lossy().into_owned())
        })
        .collect()
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
    // Reuse GHMR-01's ruleset (incl. any repo pii-gate.toml) for file/dir
    // exclusions during the walk AND, below, for the authoritative residual
    // detection — the two surfaces can never diverge on what they touch.
    let ruleset = ruleset_from_config(Some(work_dir));
    // Never rewrite the active config file itself (its `term`/`pattern` values
    // are exactly what the matchers would corrupt). This is the one exact
    // resolved config path — env-pointed, else repo-root mirror-placeholders.toml.
    let skip = active_config_skip(work_dir);
    let files = collect_files(work_dir, &ruleset, &skip);

    // Read every candidate once (strict UTF-8; binaries/oversized skipped).
    let contents: Vec<(PathBuf, String, String)> = files
        .into_iter()
        .filter_map(|path| {
            let content = read_text(&path)?;
            let rel = path
                .strip_prefix(work_dir)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            Some((path, rel, content))
        })
        .collect();

    let mut state = TokenState::default();
    // Seed BEFORE any assignment so a new distinct value in one file cannot
    // collide with an existing indexed token elsewhere in the tree.
    seed_token_state(&contents, &rules, &mut state);

    let mut records: Vec<Replacement> = Vec::new();
    let mut files_rewritten = 0usize;

    for (path, rel, content) in &contents {
        let rewritten = rewrite_content(content, &rules, &mut state, rel, &mut records);
        if &rewritten != content {
            std::fs::write(path, &rewritten).map_err(|e| {
                ToolError::Execution(format!("failed writing swept file {}: {e}", path.display()))
            })?;
            files_rewritten += 1;
        }
    }

    // Residual = whatever the authoritative gate STILL flags after the mechanical
    // pass. This is the single source of "what is still PII", so mechanical vs
    // residual can never drift from GHMR-01's coverage. The active placeholder
    // config is excluded (as GHMR-01 already excludes its own pii-gate.toml):
    // it legitimately holds the real values the matchers map, is never rewritten,
    // and is not content shipped to the public mirror — so it must not make an
    // otherwise-clean tree look dirty and block the GHMR-03 approval tag. Only the
    // exact active-config path is dropped (via skip_rels), never a same-named
    // nested file, which stays visible in the residual report.
    let skip_rels = skip_relative_names(work_dir, &skip);
    let residual_violations: Vec<TreeViolation> = ruleset
        .scan_tree(work_dir)
        .into_iter()
        .filter(|v| !skip_rels.contains(&v.file))
        .collect();

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

    // ── Incremental sweep: no cross-run token collision (codex P1) ───────────

    #[test]
    #[serial]
    fn incremental_sweep_does_not_reuse_existing_index() {
        clear_env();
        let dir = temp_tree("incr");
        // Tree already carries a swept token for some prior value, and a NEW raw
        // IP arrives. The new value must NOT reuse `_1` (which already means the
        // prior value) — it must continue past the existing max index.
        write_file(&dir, "old.txt", "old node at <REDACTED_LAN_IP_1>\n");
        write_file(&dir, "new.txt", "new node at <internal-ip>\n"); // pii-test-fixture
        let report = sweep_tree(&dir, &PlaceholderConfig::default()).unwrap();
        let new = read(&dir, "new.txt");
        assert!(
            new.contains("<REDACTED_LAN_IP_2>"),
            "new distinct value must get a fresh index, not collide with _1: {new}"
        );
        assert!(!new.contains("<REDACTED_LAN_IP_1>"), "must not reuse _1: {new}");
        // The pre-existing token is left untouched.
        assert_eq!(read(&dir, "old.txt"), "old node at <REDACTED_LAN_IP_1>\n");
        assert!(report.is_clean());
    }

    // ── Active config file is never rewritten (codex P1) ─────────────────────

    #[test]
    #[serial]
    fn active_config_file_is_not_rewritten() {
        clear_env();
        let dir = temp_tree("cfgfile");
        let toml = "[[placeholder]]\nterm = \"WidgetInc\"\ntoken = \"<ORG>\"\n";
        write_file(&dir, "mirror-placeholders.toml", toml);
        write_file(&dir, "readme.md", "By WidgetInc.\n");
        let cfg = placeholder_config_from(Some(dir.as_path()));
        let _ = sweep_tree(&dir, &cfg).unwrap();
        // The matcher rewrote the doc but left its own config intact.
        assert!(!read(&dir, "readme.md").contains("WidgetInc"));
        assert_eq!(read(&dir, "mirror-placeholders.toml"), toml, "config must be untouched");
    }

    // ── Configured excluded_dirs honored by the rewrite walk (codex P2) ──────

    #[test]
    #[serial]
    fn configured_excluded_dir_is_not_rewritten() {
        clear_env();
        let dir = temp_tree("excldir");
        // pii-gate.toml (GHMR-01 config) prunes `vendor/`; the sweep walk must
        // honor the same posture and leave files there untouched.
        write_file(&dir, "pii-gate.toml", "excluded_dirs = [\"vendor\"]\n");
        write_file(&dir, "vendor/lib.txt", "vendored <internal-ip>\n"); // pii-test-fixture
        write_file(&dir, "app.txt", "app at <internal-ip>\n"); // pii-test-fixture
        let report = sweep_tree(&dir, &PlaceholderConfig::default()).unwrap();
        assert_eq!(
            read(&dir, "vendor/lib.txt"),
            "vendored <internal-ip>\n", // pii-test-fixture
            "excluded dir must be left untouched"
        );
        assert!(!read(&dir, "app.txt").contains("<internal-ip>"), "non-excluded file swept"); // pii-test-fixture
        // Residual scan also skips the excluded dir → tree is clean.
        assert!(report.is_clean(), "excluded-dir content must not surface as residual: {report:?}");
    }

    // ── Non-UTF-8 files are not corrupted (codex P2) ─────────────────────────

    #[test]
    #[serial]
    fn invalid_utf8_file_is_left_intact() {
        clear_env();
        let dir = temp_tree("badutf8");
        // NUL-free but invalid UTF-8 (0xFF 0xFE), plus a mechanical-looking IP.
        let raw = b"start \xff\xfe <internal-ip> end\n"; // pii-test-fixture
        std::fs::write(dir.join("bin.txt"), raw).unwrap();
        let _ = sweep_tree(&dir, &PlaceholderConfig::default()).unwrap();
        let after = std::fs::read(dir.join("bin.txt")).unwrap();
        assert_eq!(after, raw, "invalid-UTF-8 file must be byte-for-byte intact");
    }

    // ── Fixture-marked lines are preserved, not rewritten (codex P1) ─────────

    #[test]
    #[serial]
    fn fixture_marked_line_is_not_rewritten() {
        clear_env();
        let dir = temp_tree("fixture");
        // A line carrying the exemption marker (a deliberate test literal) plus a
        // separate untagged line with a real value. The gate exempts the first;
        // the sweep must do the same, but still rewrite the untagged one.
        write_file(
            &dir,
            "scanner_test.txt",
            "expect <internal-ip> flagged // pii-test-fixture\nreal host <internal-ip> here\n",
        );
        let report = sweep_tree(&dir, &PlaceholderConfig::default()).unwrap();
        let content = read(&dir, "scanner_test.txt");
        // Fixture line preserved verbatim...
        assert!(
            content.contains("expect <internal-ip> flagged // pii-test-fixture"),
            "fixture-marked literal must be preserved: {content}"
        );
        // ...untagged value rewritten...
        assert!(!content.contains("<internal-ip>"), "untagged value must be swept: {content}"); // pii-test-fixture
        // ...and the tree is clean (the fixture line is exempt from residual too).
        assert!(report.is_clean(), "fixture exemption must match residual scan: {report:?}");
    }

    // ── Config holding a real value doesn't become residual (codex P1) ───────

    #[test]
    #[serial]
    fn config_with_real_value_is_not_residual() {
        clear_env();
        let dir = temp_tree("cfgresidual");
        // The active config legitimately holds a real private IP inside a matcher.
        write_file(
            &dir,
            "mirror-placeholders.toml",
            "[[placeholder]]\npattern = '10\\.10\\.0\\.9'\ntoken = \"<REDACTED_LAN_IP>\"\n",
        );
        write_file(&dir, "doc.txt", "reaches <internal-ip> internally\n"); // pii-test-fixture
        let cfg = placeholder_config_from(Some(dir.as_path()));
        let report = sweep_tree(&dir, &cfg).unwrap();
        // Doc swept, config preserved, and the config's own IP is NOT residual.
        assert!(!read(&dir, "doc.txt").contains("<internal-ip>"), "doc swept"); // pii-test-fixture
        assert!(read(&dir, "mirror-placeholders.toml").contains("10\\.10\\.0\\.9"), "config kept");
        assert!(
            report.is_clean(),
            "config-only real value must not surface as residual: {:?}",
            report.residual_violations
        );
    }

    // ── Only the EXACT active config is exempt, not a same-named nested file ──

    #[test]
    #[serial]
    fn nested_same_named_config_is_swept_not_exempt() {
        clear_env();
        let dir = temp_tree("nestedcfg");
        // The real active config lives at the repo root and is exempt.
        write_file(
            &dir,
            "mirror-placeholders.toml",
            "[[placeholder]]\npattern = '10\\.10\\.0\\.9'\ntoken = \"<REDACTED_LAN_IP>\"\n",
        );
        // A DIFFERENT file that merely shares the base-name, nested under docs/,
        // holding a real mechanical value. It is NOT the active config, so it must
        // be swept like any other content — not silently exempted.
        write_file(&dir, "docs/mirror-placeholders.toml", "sample host <internal-ip>\n"); // pii-test-fixture
        let cfg = placeholder_config_from(Some(dir.as_path()));
        let report = sweep_tree(&dir, &cfg).unwrap();
        // Root config untouched; nested same-named file swept; tree clean.
        assert!(read(&dir, "mirror-placeholders.toml").contains("10\\.10\\.0\\.9"), "root config kept");
        assert!(
            !read(&dir, "docs/mirror-placeholders.toml").contains("<internal-ip>"), // pii-test-fixture
            "nested same-named file must be swept, not exempt"
        );
        assert!(report.is_clean(), "swept nested file must not remain residual: {report:?}");
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
