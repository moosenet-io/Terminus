//! Mandatory PII gate for all GitHub WRITE operations.
//!
//! Every tool in the `github` module that pushes content to GitHub (repo
//! descriptions, file contents, commit/PR bodies, mirror operations) MUST run
//! its outbound content through [`pii_gate`] BEFORE any network request fires.
//! There is intentionally NO flag, env var, or argument that disables this gate.
//!
//! The only exception is author-attribution emails: an email is allowed when it
//! matches an entry in the comma-separated `GITHUB_ALLOWED_AUTHORS` env var.
//!
//! Patterns are compiled once via [`OnceLock`] and reused.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;
use serde::Deserialize;

use crate::error::ToolError;

/// One detected PII match. `context` is a short, redacted snippet — the full
/// matched secret is NEVER stored or echoed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PiiViolation {
    pub line: usize,
    pub category: String,
    pub context: String,
}

struct Patterns {
    private_ip: Regex,
    container_id: Regex,
    internal_host: Regex,
    internal_domain: Regex,
    email: Regex,
    phone: Regex,
    api_key: Regex,
    internal_path: Regex,
    infra_service: Regex,
    uuid: Regex,
    date_like: Regex,
    operator_name: Regex,
}

fn patterns() -> &'static Patterns {
    static P: OnceLock<Patterns> = OnceLock::new();
    P.get_or_init(|| Patterns {
        private_ip: Regex::new(
            r"\b(?:192\.168|10\.\d{1,3}|172\.(?:1[6-9]|2\d|3[01]))\.\d{1,3}\.\d{1,3}\b",
        )
        .expect("private_ip regex"),
        container_id: Regex::new(r"\bCT\d{3}\b").expect("container_id regex"),
        internal_host: Regex::new(r"(?i)\b(?:<host>|<host>|<host>|<host>|<host>)\b") // pii-test-fixture
            .expect("internal_host regex"),
        internal_domain: Regex::new(r"moosenet\.online|moosenet\.local")
            .expect("internal_domain regex"),
        email: Regex::new(r"[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,}")
            .expect("email regex"),
        phone: Regex::new(r"\+?\d[\d\s\-]{8,}\d").expect("phone regex"),
        api_key: Regex::new(r"\b(?:sk-|ghp_|gsk_|glpat-|xox[bpasr]-)\S+") // pii-test-fixture
            .expect("api_key regex"),
        internal_path: Regex::new(r"<path>/|<path>/|<path>/|/opt/lumina[a-z0-9-]*/") // pii-test-fixture
            .expect("internal_path regex"),
        infra_service: Regex::new(r"(?i)\b(?:<matrix-server>|<secret-manager>|<media-service>|<container-mgr>)\b") // pii-test-fixture
            .expect("infra_service regex"),
        uuid: Regex::new(
            r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}",
        )
        .expect("uuid regex"),
        // Bare ISO date (`YYYY-MM-DD`) OR an ISO date immediately followed by
        // a `T` (the start of an ISO-8601 datetime, e.g. a JSON
        // `"timestamp": "2026-07-10T14:32:07Z"` field or a truncated
        // `"2026-07-09T…"` doc example). The plain-date alt keeps its `\b`
        // word-boundary anchors (needed so it doesn't swallow a longer digit
        // run); the datetime alt intentionally has none at the end because
        // `\d{2}\b` fails to match right before `T` (both are "word" chars,
        // so `\b` never fires there) — GHMRFIX-gate-tune found this was
        // letting ISO datetimes fall through to the `phone` matcher.
        date_like: Regex::new(r"\b\d{4}-\d{2}-\d{2}T|\b\d{4}-\d{2}-\d{2}\b")
            .expect("date_like regex"),
        // Whole-word, case-insensitive match on the operator's real first
        // name. `\b...\b` anchors on both sides so this never fires on a
        // substring occurrence (e.g. a surname or an unrelated word
        // containing the same letters) — only the standalone name/token.
        // Found by an independent PII sweep leaking into shipped tool
        // description strings, docs, and env var names (GHMRFIX-3); this
        // closes that gap the same way `internal_host`/`infra_service`
        // close theirs, as an unconditional built-in pattern (not an
        // opt-in config term) so it's caught everywhere `scan_for_pii`
        // runs, including this crate's own self-check.
        operator_name: Regex::new(r"(?i)\bpeter\b").expect("operator_name regex"),
    })
}

/// Parse `GITHUB_ALLOWED_AUTHORS` into trimmed, lowercased, non-empty entries.
fn allowed_authors() -> Vec<String> {
    std::env::var("GITHUB_ALLOWED_AUTHORS")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

/// An email is allowed if any allow-list entry is a case-insensitive substring
/// of the matched email (covers both name-fragment and full-email entries).
fn email_is_allowed(email: &str, allow: &[String]) -> bool {
    let lower = email.to_lowercase();
    allow.iter().any(|entry| lower.contains(entry.as_str()))
}

/// Produce a short, redacted snippet for a matched span so we never echo the
/// full secret. Keeps the category meaningful without leaking the value.
fn redact(matched: &str) -> String {
    let n = matched.chars().count();
    if n <= 4 {
        "[redacted]".to_string()
    } else {
        let head: String = matched.chars().take(2).collect();
        let tail: String = matched.chars().rev().take(2).collect::<Vec<_>>().into_iter().rev().collect();
        format!("{head}…{tail} [{n} chars redacted]")
    }
}

/// Whether a UUID match should be blocked: only when its line contains an
/// infra-secret cue (`<secret-manager>`, `project_id`, `workspace_id`, // pii-test-fixture
/// `machine_identity`) within ~40 chars of the match. Bare UUIDs are allowed.
fn uuid_is_sensitive(line: &str, m_start: usize, m_end: usize) -> bool {
    let cues = ["<secret-manager>", "project_id", "workspace_id", "machine_identity"]; // pii-test-fixture
    let lower = line.to_lowercase();
    // Window: 40 chars before the match start through 40 chars after the end.
    let win_start = m_start.saturating_sub(40);
    let win_end = (m_end + 40).min(lower.len());
    // Snap to char boundaries (line is ascii-cued but content may be utf8).
    let win = &lower[byte_floor(&lower, win_start)..byte_ceil(&lower, win_end)];
    cues.iter().any(|c| win.contains(c))
}

fn byte_floor(s: &str, mut i: usize) -> usize {
    if i > s.len() {
        return s.len();
    }
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn byte_ceil(s: &str, mut i: usize) -> usize {
    if i > s.len() {
        return s.len();
    }
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// Whether a raw `phone` regex match is actually phone-SHAPED rather than a
/// hash-digest fragment, a unix timestamp, or an arbitrary large integer ID.
///
/// The `phone` regex (`\+?\d[\d\s\-]{8,}\d`) matches any run of 10+ digits
/// with optional internal spaces/hyphens and an optional leading `+` — which
/// also matches: digit runs embedded in a hex string (a SHA prefix whose
/// digits happen to have no letters), bare unix timestamps (`1717000000`),
/// and large signed integer IDs. None of those are phone numbers.
///
/// GHMRFIX-gate-tune's fix: require the match to carry an actual
/// phone-shaped cue — either a leading `+` (E.164-style, e.g. a
/// country-code-prefixed number) or an internal separator (space/hyphen, // pii-test-fixture
/// e.g. a 3-3-4 hyphen-grouped number) — AND at least 7 digit characters
/// (rules out short hyphen-separated non-phone shapes like the `8-4-4-4-12`
/// UUID-shape description in docs, which has only 6 digits). A bare,
/// unseparated digit run (no `+`, no internal space/hyphen) is never treated
/// as a phone number under this rule, since that shape is indistinguishable
/// from a hash fragment/timestamp/integer-ID and — per the regression
/// guards below — every real-phone test fixture in this repo carries either
/// a leading `+` or a visible separator.
fn phone_match_is_phone_shaped(matched: &str) -> bool {
    let digit_count = matched.chars().filter(|c| c.is_ascii_digit()).count();
    if digit_count < 7 {
        return false;
    }
    let has_plus = matched.starts_with('+');
    let has_internal_sep = matched.contains(' ') || matched.contains('-');
    has_plus || has_internal_sep
}

/// Scan a single `line` (1-based `line_no`) for every built-in PII pattern plus
/// any `extra` rules, appending one [`PiiViolation`] per match into `out`.
///
/// This is the single source of truth for per-line PII detection — both the
/// legacy [`scan_for_pii`] content gate and the tree-sweep [`PiiRuleSet`] route
/// through here, so their coverage can never silently diverge.
fn scan_line(
    p: &Patterns,
    extra: &[(String, Regex)],
    allow: &[String],
    line_no: usize,
    line: &str,
    out: &mut Vec<PiiViolation>,
    skip_phone: bool,
) {
    let mut push = |category: &str, matched: &str| {
        out.push(PiiViolation {
            line: line_no,
            category: category.to_string(),
            context: redact(matched),
        });
    };

    for m in p.private_ip.find_iter(line) {
        push("private_ip", m.as_str());
    }
    for m in p.container_id.find_iter(line) {
        push("container_id", m.as_str());
    }
    for m in p.internal_host.find_iter(line) {
        push("internal_hostname", m.as_str());
    }
    for m in p.internal_domain.find_iter(line) {
        push("internal_domain", m.as_str());
    }
    for m in p.api_key.find_iter(line) {
        push("api_key", m.as_str());
    }
    for m in p.internal_path.find_iter(line) {
        push("internal_path", m.as_str());
    }
    for m in p.infra_service.find_iter(line) {
        push("infra_service", m.as_str());
    }
    for m in p.operator_name.find_iter(line) {
        push("operator_name", m.as_str());
    }

    // Emails: allow-list exception for author attribution.
    for m in p.email.find_iter(line) {
        if !email_is_allowed(m.as_str(), allow) {
            push("email", m.as_str());
        }
    }

    // UUIDs first: context-dependent — only block near an infra-secret cue.
    // Collect their spans so the phone matcher (digit/hyphen runs) does not
    // misfire on a UUID's hex+hyphen segments and flag a bare UUID as a phone.
    let uuid_spans: Vec<(usize, usize)> =
        p.uuid.find_iter(line).map(|m| (m.start(), m.end())).collect();
    for &(s, e) in &uuid_spans {
        if uuid_is_sensitive(line, s, e) {
            push("uuid_secret", &line[s..e]);
        }
    }

    // Bare ISO dates (YYYY-MM-DD, e.g. an MCP protocolVersion string) share
    // the phone regex's digit/hyphen shape. Collect their spans so the
    // phone matcher can skip them the same way it already skips UUIDs.
    let date_spans: Vec<(usize, usize)> =
        p.date_like.find_iter(line).map(|m| (m.start(), m.end())).collect();

    // Phone: skip any match that overlaps a UUID span (those digits belong
    // to the UUID, not a phone number), a bare ISO-date span, or — when
    // `skip_phone` is set (SVG files only, see `PiiRuleSet::scan_tree`) —
    // every match on the line. SVG path/viewBox coordinate data (e.g.
    // `viewBox="..."`, `d="M... L..."`) is digit-and-space/ // pii-test-fixture
    // hyphen shaped exactly like the phone pattern and is never a phone
    // number, so scanning `.svg` files for this one pattern is pure noise —
    // GHMR-fix (see `github/mod.rs::github_pii_scan` docs) narrows this to
    // the file extension rather than weakening the phone regex itself, so a
    // real phone number embedded in a non-SVG file is still caught exactly
    // as before.
    if !skip_phone {
        for m in p.phone.find_iter(line) {
            let overlaps_uuid = uuid_spans
                .iter()
                .any(|&(s, e)| m.start() < e && s < m.end());
            let overlaps_date = date_spans
                .iter()
                .any(|&(s, e)| m.start() < e && s < m.end());
            if !overlaps_uuid && !overlaps_date && phone_match_is_phone_shaped(m.as_str()) {
                push("phone", m.as_str());
            }
        }
    }

    // Extension / config-supplied rules (JWTs, SSH keys, cloud keys, quoted
    // secrets, and any repo-configured terms). Kept out of the built-in set so
    // the legacy content gate's behavior is unchanged; the tree-sweep ruleset
    // opts in via its `extra` list.
    for (kind, re) in extra {
        for m in re.find_iter(line) {
            // `generic_secret` matches ANY 8+ char quoted string after
            // password/secret/token — including bare identifiers used as
            // struct-field/JSON-key names next to short synthetic test
            // values ("testtoken", "test-shared-secret") and mirror-sweep
            // placeholder tokens ("<EMPTYPAT>", "<REDACTED_HOST>"). Require
            // the captured value to look secret-SHAPED (see
            // `generic_secret_value_shaped`) before flagging it — this is a
            // value-shape/entropy-proxy filter, not a weaker keyword match:
            // it never touches api_key/jwt/ssh_key/aws/google/slack, which
            // all have their own dedicated, unrelated patterns.
            if kind == "generic_secret" && !generic_secret_value_shaped(m.as_str()) {
                continue;
            }
            push(kind, m.as_str());
        }
    }
}

/// Extract the value between the first and second occurrence of whichever
/// quote character (`"` or `'`) the `generic_secret` match used. The
/// `generic_secret` regex always captures exactly one quoted value, so this
/// should always succeed for a real match.
fn extract_quoted_value(full_match: &str) -> Option<&str> {
    let quote = full_match.bytes().find(|&b| b == b'"' || b == b'\'')? as char;
    let first = full_match.find(quote)?;
    let rest = &full_match[first + 1..];
    let second = rest.find(quote)?;
    Some(&rest[..second])
}

/// Whether a `generic_secret` match's quoted value looks like an actual
/// secret VALUE rather than a synthetic test literal or a placeholder token.
///
/// Every false positive observed in practice (test fixtures like
/// `"testtoken"`, `"test-token"`, `"test-shared-secret"`,
/// `"correct-horse-battery-staple"`, `"jwt-signing-key-for-tests-only"`, and
/// mirror-sweep placeholder tokens like `"<EMPTYPAT>"`/`"<REDACTED_HOST>"`)
/// is a human-readable word/phrase or an all-caps `<...>` placeholder — NONE
/// of them contain an ASCII digit. Every existing/regression-tested TRUE
/// positive (e.g. `password = "<REDACTED-SECRET>"`, the crate's own
/// `scan_tree_covers_python_gate_extension_patterns` fixture) does contain a
/// digit, matching how generated tokens/passwords are actually shaped
/// (base64/hex/alphanumeric token generators virtually always mix in
/// digits). Requiring at least one digit is therefore used as a cheap,
/// robust, well-tested proxy for "looks high-entropy / generated" without
/// the fragility of a full Shannon-entropy threshold (which sits too close
/// between the observed false- and true-positive samples to be reliable).
/// Angle-bracket `<...>` placeholder tokens are rejected outright regardless
/// of digits, since that shape is exclusively used by the mirror-sweep's own
/// placeholder config, never a real secret.
fn generic_secret_value_shaped(full_match: &str) -> bool {
    match extract_quoted_value(full_match) {
        Some(value) => {
            if value.starts_with('<') && value.ends_with('>') {
                return false;
            }
            value.bytes().any(|b| b.is_ascii_digit())
        }
        // Regex guarantees a quoted value; fail open (flag) rather than
        // silently drop a match we couldn't parse.
        None => true,
    }
}

/// Scan `content` for PII, returning one [`PiiViolation`] per match with a
/// 1-based line number and a redacted context snippet.
pub fn scan_for_pii(content: &str) -> Vec<PiiViolation> {
    let p = patterns();
    let allow = allowed_authors();
    let mut out = Vec::new();
    for (idx, line) in content.lines().enumerate() {
        scan_line(p, &[], &allow, idx + 1, line, &mut out, false);
    }
    out
}

/// Redaction-capable sibling of [`scan_for_pii`], used by the docgen
/// pre-inference input gate (DOCGEN-02, `crate::tools::docgen::pii_gate`).
///
/// Detection is IDENTICAL to [`scan_for_pii`] -- same [`patterns`], same
/// [`extension_rules`], same [`allowed_authors`] / [`email_is_allowed`] /
/// [`uuid_is_sensitive`] / [`phone_match_is_phone_shaped`] /
/// [`generic_secret_value_shaped`] helpers. This is NOT a second scanner;
/// it is the same rule set with match *spans* retained (rather than
/// discarded after producing a redacted context snippet) so each match can
/// be replaced in place with a `[REDACTED:{category}]` placeholder.
///
/// A `// pii-test-fixture`-tagged line is skipped entirely (left
/// unredacted, no violations recorded for it), matching the exemption
/// convention used by [`PiiRuleSet::scan_tree`] / [`strip_fixture_lines`].
///
/// Returns the redacted content plus every [`PiiViolation`] found (line +
/// category + a short redacted context snippet -- never the raw matched
/// value, same discipline as [`scan_for_pii`]).
pub fn scan_and_redact(content: &str) -> (String, Vec<PiiViolation>) {
    let p = patterns();
    let allow = allowed_authors();
    let extra = extension_rules();
    let mut violations = Vec::new();
    let mut out_lines: Vec<String> = Vec::with_capacity(content.lines().count());

    for (idx, line) in content.lines().enumerate() {
        let line_no = idx + 1;

        if line.contains("pii-test-fixture") {
            out_lines.push(line.to_string());
            continue;
        }

        let mut spans: Vec<(usize, usize, String)> = Vec::new();

        macro_rules! collect {
            ($re:expr, $cat:expr) => {
                for m in $re.find_iter(line) {
                    spans.push((m.start(), m.end(), $cat.to_string()));
                }
            };
        }

        collect!(p.private_ip, "private_ip");
        collect!(p.container_id, "container_id");
        collect!(p.internal_host, "internal_hostname");
        collect!(p.internal_domain, "internal_domain");
        collect!(p.api_key, "api_key");
        collect!(p.internal_path, "internal_path");
        collect!(p.infra_service, "infra_service");
        collect!(p.operator_name, "operator_name");

        for m in p.email.find_iter(line) {
            if !email_is_allowed(m.as_str(), &allow) {
                spans.push((m.start(), m.end(), "email".to_string()));
            }
        }

        let uuid_spans: Vec<(usize, usize)> =
            p.uuid.find_iter(line).map(|m| (m.start(), m.end())).collect();
        for &(s, e) in &uuid_spans {
            if uuid_is_sensitive(line, s, e) {
                spans.push((s, e, "uuid_secret".to_string()));
            }
        }

        let date_spans: Vec<(usize, usize)> =
            p.date_like.find_iter(line).map(|m| (m.start(), m.end())).collect();

        for m in p.phone.find_iter(line) {
            let overlaps_uuid = uuid_spans.iter().any(|&(s, e)| m.start() < e && s < m.end());
            let overlaps_date = date_spans.iter().any(|&(s, e)| m.start() < e && s < m.end());
            if !overlaps_uuid && !overlaps_date && phone_match_is_phone_shaped(m.as_str()) {
                spans.push((m.start(), m.end(), "phone".to_string()));
            }
        }

        for (kind, re) in &extra {
            for m in re.find_iter(line) {
                if kind == "generic_secret" && !generic_secret_value_shaped(m.as_str()) {
                    continue;
                }
                spans.push((m.start(), m.end(), kind.clone()));
            }
        }

        if spans.is_empty() {
            out_lines.push(line.to_string());
            continue;
        }

        // Merge overlapping/adjacent spans so a byte range is never
        // redacted twice (e.g. an `internal_path` span nested inside a
        // wider `generic_secret` span).
        spans.sort_by_key(|&(s, _, _)| s);
        let mut merged: Vec<(usize, usize, String)> = Vec::new();
        for (s, e, cat) in spans {
            if let Some(last) = merged.last_mut() {
                if s <= last.1 {
                    if e > last.1 {
                        last.1 = e;
                    }
                    continue;
                }
            }
            merged.push((s, e, cat));
        }

        let mut redacted_line = String::with_capacity(line.len());
        let mut cursor = 0usize;
        for (s, e, cat) in &merged {
            redacted_line.push_str(&line[cursor..*s]);
            redacted_line.push_str(&format!("[REDACTED:{cat}]"));
            violations.push(PiiViolation {
                line: line_no,
                category: cat.clone(),
                context: redact(&line[*s..*e]),
            });
            cursor = *e;
        }
        redacted_line.push_str(&line[cursor..]);
        out_lines.push(redacted_line);
    }

    (out_lines.join("\n"), violations)
}

/// Mandatory gate. Returns `Ok(())` when clean, otherwise an
/// [`ToolError::InvalidArgument`] whose message lists every violation by
/// line/category. Logs a one-line audit record (pass/fail + count) without
/// ever logging secret values.
pub fn pii_gate(content: &str) -> Result<(), ToolError> {
    // Full authoritative rule set: built-in patterns + extension rules (JWTs,
    // PEM keys, cloud keys, quoted secrets) + any `TERMINUS_PII_CONFIG` terms.
    // The runtime service has no repo checkout, so config comes from the
    // `TERMINUS_PII_CONFIG` env var (the service's materialized config), not a
    // repo-root `pii-gate.toml` — hence `None`. The pre-push hook, which DOES
    // run in a checkout, additionally reads `<root>/pii-gate.toml`; both surfaces
    // resolve through the same `ruleset_from_config` so the built-in + extension
    // coverage is identical and any env-configured terms apply everywhere.
    let violations = ruleset_from_config(None).scan_content(content);

    if violations.is_empty() {
        tracing::info!(target: "github.pii", outcome = "pass", count = 0, "PII gate scan passed");
        return Ok(());
    }

    tracing::warn!(
        target: "github.pii",
        outcome = "blocked",
        count = violations.len(),
        "PII gate blocked GitHub write"
    );

    let detail: Vec<String> = violations
        .iter()
        .map(|v| format!("line {} [{}]: {}", v.line, v.category, v.context))
        .collect();

    Err(ToolError::InvalidArgument(format!(
        "BLOCKED: {} PII pattern(s) detected — refusing GitHub write. {}",
        violations.len(),
        detail.join("; ")
    )))
}

// ── Tree-sweep engine (GHMR-01) ───────────────────────────────────────────────
//
// The functions above ([`scan_for_pii`] / [`pii_gate`]) are the runtime WRITE
// gate: they scan a single outbound content string before a GitHub API call.
// The engine below is the authoritative *tree* sweep — it walks a directory of
// a candidate mirror derivative and returns structured, per-file violations. It
// is the Rust replacement for the legacy `.githooks/pii_gate.py` pre-push hook
// and the library surface consumed by the mirror engine (GHMR-03/04).

/// One violation located during a tree sweep. `pattern_kind` is the rule that
/// fired; `context` is a short redacted snippet — the full matched secret is
/// NEVER stored or echoed (same discipline as [`PiiViolation`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeViolation {
    pub file: String,
    pub line: usize,
    pub pattern_kind: String,
    pub context: String,
}

/// Repo-root configuration for the sweep. Loaded from a TOML file
/// (`pii-gate.toml` at the repo root, or a path in `TERMINUS_PII_CONFIG`).
///
/// The built-in *patterns* are generic (RFC-1918 ranges, key prefixes, email/
/// phone shapes); any repo-specific *terms* (infra hostnames, service names)
/// live here in config, not hardcoded in this source file.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct PiiConfig {
    /// Literal terms (hostnames, service names, org names) matched
    /// case-insensitively on a word boundary. Escaped before compiling.
    pub extra_terms: Vec<String>,
    /// Raw additional regexes. Invalid patterns are logged and skipped.
    pub extra_patterns: Vec<String>,
    /// Emails permitted in addition to `GITHUB_ALLOWED_AUTHORS` (e.g. bot
    /// no-reply author addresses, or placeholder example-domain addresses).
    pub allowed_emails: Vec<String>,
    /// File base-names to skip entirely (added to the built-in defaults).
    pub excluded_files: Vec<String>,
    /// File extensions (without the dot) to skip (added to defaults).
    pub excluded_extensions: Vec<String>,
    /// Directory base-names to prune from the walk (added to defaults).
    pub excluded_dirs: Vec<String>,
}

/// Built-in extension rules layered on top of [`patterns`] for the tree sweep.
/// These are all GENERIC shapes (no infra-specific literals) that the Python
/// gate covered but the runtime content gate historically did not: JWTs, PEM
/// private keys, cloud provider keys, Slack user tokens, and quoted secrets.
/// Kept as ruleset extras (not in `patterns()`) so the write gate's behavior is
/// byte-for-byte unchanged.
fn extension_rules() -> Vec<(String, Regex)> {
    let raw: &[(&str, &str)] = &[
        ("jwt", r"\beyJ[a-zA-Z0-9_-]+\.[a-zA-Z0-9_-]+\.[a-zA-Z0-9_-]*"),
        ("ssh_key", r"-----BEGIN [A-Z ]*PRIVATE KEY-----"),
        ("aws_access_key", r"\bAKIA[A-Z0-9]{16}\b"),
        ("google_api_key", r"\bAIza[a-zA-Z0-9_-]{35}\b"),
        ("slack_user_token", r"\bxoxp-[a-zA-Z0-9-]{10,}"),
        (
            "generic_secret",
            r#"(?i)(?:password|secret|token)\s*[=:]\s*["'][^"']{8,}["']"#,
        ),
    ];
    raw.iter()
        .map(|(k, p)| ((*k).to_string(), Regex::new(p).expect("extension rule regex")))
        .collect()
}

/// Default file base-names never scanned (the scanner sources themselves, lock
/// files, the config, and the audit log). Mirrors the Python gate's list.
fn default_excluded_files() -> HashSet<String> {
    [
        "Cargo.lock",
        ".gitignore",
        // A worktree's `.git` is a FILE (`gitdir: /abs/path/to/.git/worktrees/<name>`),
        // not the `.git` directory the tree-walk already prunes by dir name. It is
        // local dev-box tooling metadata — never git-tracked content, never shipped
        // to a mirror (git itself excludes it from every tree/commit) — so scanning
        // it for the dev box's own absolute path is pure noise, not a real leak risk.
        ".git",
        "pii.rs",       // the scanner itself — holds pattern strings
        "pii_gate.rs",  // the hook binary — holds pattern strings
        "pii_gate.py",  // the retired Python gate, if still present
        "pii-gate.toml",
        ".moosenet-repo.toml",
        "pii-gate-audit.jsonl",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

fn default_excluded_exts() -> HashSet<String> {
    [
        "png", "jpg", "jpeg", "gif", "bmp", "ico", "pdf", "doc", "docx", "zip",
        "tar", "gz", "exe", "dll", "so", "dylib", "bin", "lock", "crate",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

/// Whether a tree-relative path is exempt from the `infra_service` category
/// specifically (every OTHER category is still fully scanned at this path).
///
/// `infra_service` (see the named service list in `patterns()` above) exists // pii-test-fixture
/// to catch an internal service NAME leaking where it shouldn't. But
/// Terminus's own public documentation site legitimately documents these
/// integrations by name — this repo's `docs/tools/**` pages and their // pii-test-fixture
/// accompanying `assets/**` architecture-diagram SVGs, plus each service's
/// own integration module under `src/<service>/**` — because they ARE // pii-test-fixture
/// public product names that Terminus ships first-class tool support for, // pii-test-fixture
/// not a leaked internal hostname or credential. Flagging every mention of
/// a product name a repo is documented to integrate with produces pure
/// noise on every doc build.
///
/// CONSERVATIVE scope, by design — operator/reviewer should re-confirm this
/// relaxation:
///   - `docs/` — the doc site content itself.
///   - `assets/` — the doc site's diagram SVGs (referenced from `docs/`).
///   - `src/<service>/` for each of the named services above — each // pii-test-fixture
///     service's own integration module (currently 0 hits there, but
///     kept in lockstep with the doc-site exemption per the task ask).
/// GHMRFIX-gate-tune: also exempt `specs/**` (spec/sprint history, e.g. a
/// git-tool-domains spec naming supported secret-manager/git providers // pii-test-fixture
/// by name) and `data/**` (e.g. `prefix_registry.toml` module-registry
/// descriptions naming the same runtime-secret-client integrations). Same
/// reasoning as the doc-site exemption above — every hit found in these two
/// paths is the same public product name mentioned as a supported
/// integration, never a leaked hostname/credential — so this is a
/// conservative, same-category broadening rather than a new relaxation.
fn infra_service_path_exempt(rel: &str) -> bool {
    let rel = rel.replace('\\', "/");
    rel.starts_with("docs/")
        || rel.starts_with("assets/")
        || rel.starts_with("specs/")
        || rel.starts_with("data/")
        || rel.starts_with("src/<secret-manager>/") // pii-test-fixture
        || rel.starts_with("src/<media-service>/") // pii-test-fixture
        || rel.starts_with("src/<container-mgr>/") // pii-test-fixture
        || rel.starts_with("src/<matrix-server>/") // pii-test-fixture
}

fn default_excluded_dirs() -> HashSet<String> {
    [".git", "target", "node_modules", ".cargo"]
        .into_iter()
        .map(String::from)
        .collect()
}

/// A configured PII rule set: the built-in [`patterns`] plus extension rules and
/// any repo-configured terms/patterns, with the file/dir exclusion posture used
/// when walking a tree.
pub struct PiiRuleSet {
    extra: Vec<(String, Regex)>,
    allow_emails: Vec<String>,
    excluded_files: HashSet<String>,
    excluded_exts: HashSet<String>,
    excluded_dirs: HashSet<String>,
    max_file_bytes: u64,
}

impl PiiRuleSet {
    /// The default rule set: built-in patterns + extension rules, default
    /// exclusions, no repo-specific config.
    pub fn new() -> Self {
        Self {
            extra: extension_rules(),
            allow_emails: Vec::new(),
            excluded_files: default_excluded_files(),
            excluded_exts: default_excluded_exts(),
            excluded_dirs: default_excluded_dirs(),
            max_file_bytes: 5 * 1024 * 1024,
        }
    }

    /// Build a rule set from repo config, layering the config's extras and
    /// exclusions on top of the defaults. Invalid `extra_patterns` are skipped
    /// with a warning rather than aborting the whole sweep.
    pub fn from_config(cfg: &PiiConfig) -> Self {
        let mut rs = Self::new();
        for term in &cfg.extra_terms {
            let pat = format!(r"(?i)\b{}\b", regex::escape(term));
            match Regex::new(&pat) {
                Ok(re) => rs.extra.push(("config_term".to_string(), re)),
                Err(e) => tracing::warn!(target: "github.pii", "invalid config term {term:?}: {e}"),
            }
        }
        for pat in &cfg.extra_patterns {
            match Regex::new(pat) {
                Ok(re) => rs.extra.push(("config_pattern".to_string(), re)),
                Err(e) => {
                    tracing::warn!(target: "github.pii", "invalid extra_pattern {pat:?}: {e}")
                }
            }
        }
        rs.allow_emails = cfg.allowed_emails.clone();
        rs.excluded_files
            .extend(cfg.excluded_files.iter().cloned());
        rs.excluded_exts
            .extend(cfg.excluded_extensions.iter().map(|e| e.trim_start_matches('.').to_string()));
        rs.excluded_dirs.extend(cfg.excluded_dirs.iter().cloned());
        rs
    }

    /// Load config from `path` (TOML) and build a rule set. A missing file
    /// yields the default rule set (not an error).
    pub fn from_config_file(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => match toml::from_str::<PiiConfig>(&text) {
                Ok(cfg) => Self::from_config(&cfg),
                Err(e) => {
                    tracing::warn!(target: "github.pii", "malformed {}: {e} — using defaults", path.display());
                    Self::new()
                }
            },
            Err(_) => Self::new(),
        }
    }

    /// Scan a single content string with this rule set (built-ins + extras).
    pub fn scan_content(&self, content: &str) -> Vec<PiiViolation> {
        self.scan_content_impl(content, false)
    }

    /// As [`Self::scan_content`], but with `skip_phone` controlling whether
    /// the `phone` pattern is scanned at all (see the `skip_phone` doc on
    /// [`scan_line`] — used by [`Self::scan_tree`] for `.svg` files, where
    /// coordinate/viewBox data is digit/hyphen shaped exactly like a phone
    /// number and produces pure noise).
    fn scan_content_impl(&self, content: &str, skip_phone: bool) -> Vec<PiiViolation> {
        let p = patterns();
        let mut allow = allowed_authors();
        allow.extend(self.allow_emails.iter().cloned());
        let mut out = Vec::new();
        for (idx, line) in content.lines().enumerate() {
            scan_line(p, &self.extra, &allow, idx + 1, line, &mut out, skip_phone);
        }
        out
    }

    /// Whether `path` is excluded from scanning by base-name or extension. Used
    /// by [`Self::scan_tree`] and by the pre-push hook binary so hook modes and
    /// tree mode honor exactly the same exclusion posture.
    pub fn is_excluded(&self, path: &Path) -> bool {
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if self.excluded_files.contains(name) {
                return true;
            }
        }
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            if self.excluded_exts.contains(ext) {
                return true;
            }
        }
        false
    }

    /// Whether a directory base-name is pruned from the walk by this rule set
    /// (the built-in defaults plus any config `excluded_dirs`). Lets an external
    /// walker — the GHMR-02 mirror sweep — honor exactly the same directory
    /// exclusion posture as [`Self::scan_tree`], so a rewrite pass and the
    /// residual scan can never diverge on which directories they touch.
    pub fn is_excluded_dir(&self, name: &str) -> bool {
        self.excluded_dirs.contains(name)
    }

    /// Walk the directory tree rooted at `root` and return every violation,
    /// honoring the `// pii-test-fixture` line exemption exactly as the crate's
    /// own self-check does. Binary / oversized / unreadable files are skipped
    /// without panicking.
    pub fn scan_tree(&self, root: &Path) -> Vec<TreeViolation> {
        let mut files = Vec::new();
        self.collect_files(root, &mut files);
        let mut out = Vec::new();
        for path in files {
            if self.is_excluded(&path) {
                continue;
            }
            let content = match read_text_lossy(&path, self.max_file_bytes) {
                Some(c) => c,
                None => continue, // binary / oversized / unreadable — skip
            };
            let scrubbed = strip_fixture_lines(&content);
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            // SVG files are diagram markup (path coordinates, viewBox data)
            // — never a phone number. See the `skip_phone` doc on
            // `scan_line` for why this is scoped to the pattern, not a
            // blanket file-extension exclusion (other categories, e.g. a
            // real email or private IP accidentally embedded in a diagram's
            // text labels, are still scanned normally).
            let is_svg = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("svg"))
                .unwrap_or(false);
            for v in self.scan_content_impl(&scrubbed, is_svg) {
                if v.category == "infra_service" && infra_service_path_exempt(&rel) {
                    // See `infra_service_path_exempt` doc: this repo's job
                    // is to document/integrate these public product names.
                    continue;
                }
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

    fn collect_files(&self, dir: &Path, out: &mut Vec<PathBuf>) {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            // Use the entry's own file type (does NOT follow symlinks). Skipping
            // symlinks prevents both traversal outside the requested root and
            // unbounded recursion on a symlink cycle.
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
                    .map(|n| self.excluded_dirs.contains(n))
                    .unwrap_or(false);
                if skip {
                    continue;
                }
                self.collect_files(&path, out);
            } else if ft.is_file() {
                out.push(path);
            }
        }
    }
}

impl Default for PiiRuleSet {
    fn default() -> Self {
        Self::new()
    }
}

/// Read a file as UTF-8 (lossily), skipping it entirely when it is larger than
/// `max_bytes` or looks binary (contains a NUL byte). Returns `None` to skip.
fn read_text_lossy(path: &Path, max_bytes: u64) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > max_bytes {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    if bytes.contains(&0) {
        return None; // binary
    }
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// Replace any line carrying the `pii-test-fixture` exemption marker with an
/// empty line, preserving line numbering. This is the ONLY exemption path — it
/// is line-exact (a tagged line is cleared; untagged lines are always scanned),
/// so it can never become a blanket bypass.
fn strip_fixture_lines(content: &str) -> String {
    content
        .lines()
        .map(|line| if line.contains("pii-test-fixture") { "" } else { line })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Convenience: sweep a tree with the default rule set.
pub fn scan_tree(root: &Path) -> Vec<TreeViolation> {
    PiiRuleSet::new().scan_tree(root)
}

/// Resolve a rule set from configuration: `TERMINUS_PII_CONFIG` (a config file
/// path) takes precedence; otherwise `<root>/pii-gate.toml` when `root` is
/// given; otherwise the built-in default rule set. This is the single place
/// every surface (runtime write gate, the `github_pii_scan` tool, and the
/// pre-push hook binary) loads config, so they stay in lockstep.
pub fn ruleset_from_config(root: Option<&Path>) -> PiiRuleSet {
    if let Ok(p) = std::env::var("TERMINUS_PII_CONFIG") {
        return PiiRuleSet::from_config_file(Path::new(&p));
    }
    if let Some(r) = root {
        let cfg = r.join("pii-gate.toml");
        if cfg.is_file() {
            return PiiRuleSet::from_config_file(&cfg);
        }
    }
    PiiRuleSet::new()
}

/// Base-names the gate ALWAYS excludes from scanning ([`default_excluded_files`])
/// that hold REAL sensitive values BY CONSTRUCTION — the gate config (its
/// `extra_terms`/`extra_patterns` catalog) and the gate's audit log (which records
/// the flagged PII contexts verbatim). Because they are base-name-excluded, a
/// nested instance at ANY depth is skipped by the scanner too, so every in-tree
/// instance must be dropped from the approved mirror rather than shipped unscanned.
///
/// Deliberately NOT the other excluded base-names: `pii.rs`/`pii_gate.rs`/
/// `pii_gate.py` hold only GENERIC pattern shapes (no real infra literals, per the
/// spec), and `Cargo.lock`/`.gitignore`/`.moosenet-repo.toml`/images are ordinary
/// repo content — all legitimate mirror content that must still ship.
const MIRROR_DROP_BASENAMES: &[&str] = &["pii-gate.toml", "pii-gate-audit.jsonl"];

/// The gate-internal file paths (relative to `work_dir`) that GHMR-03 must DROP
/// from an approved mirror commit — every one that lives INSIDE `work_dir`.
///
/// A gate config catalogs the REAL private matcher values the gate maps, and the
/// gate deliberately excludes it (and its audit log) from scanning — so committing
/// them into the approved mirror would ship raw values into public history even
/// though the gate reports 0 residuals. Coverage:
///
///  1. An explicit, non-empty `TERMINUS_PII_CONFIG` pointing inside the work dir —
///     the active gate config (ANY base-name), loaded by [`ruleset_from_config`].
///  2. EVERY in-tree file whose base-name is in [`MIRROR_DROP_BASENAMES`], at any
///     depth — all are base-name-excluded from scanning regardless of the env var
///     (unset / empty / externally-pointed all leave them unscanned), so a nested
///     `subdir/pii-gate.toml` (or an audit log) would otherwise ship unscanned.
///
/// Paths are de-duplicated. An env var pointing at a DIFFERENTLY-named file inside
/// the tree that is NOT base-name-excluded is scanned normally and blocks approval
/// on its own residual — nothing to special-case there.
pub fn active_gate_config_relpaths(work_dir: &Path) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let work_canon = work_dir.canonicalize().ok();
    // Case 1: explicit non-empty env config resolving inside the work dir (any name).
    if let Ok(p) = std::env::var("TERMINUS_PII_CONFIG") {
        if !p.is_empty() {
            if let (Some(w), Some(abs)) = (work_canon.as_ref(), Path::new(&p).canonicalize().ok()) {
                if let Ok(rel) = abs.strip_prefix(w) {
                    out.push(rel.to_string_lossy().into_owned());
                }
            }
        }
    }
    // Case 2: every base-name-excluded gate-internal file at any depth.
    collect_mirror_drop_files(work_dir, work_dir, &mut out);
    out.sort();
    out.dedup();
    out
}

/// Recursively collect (relative to `root`) every file under `dir` whose base-name
/// is in [`MIRROR_DROP_BASENAMES`]. Prunes the same directories the scanner does
/// ([`default_excluded_dirs`], e.g. `.git`/`target`) and never follows symlinks.
fn collect_mirror_drop_files(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let excluded_dirs = default_excluded_dirs();
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
            let prune = path
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| excluded_dirs.contains(n))
                .unwrap_or(false);
            if !prune {
                collect_mirror_drop_files(root, &path, out);
            }
        } else if ft.is_file() {
            let base = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if MIRROR_DROP_BASENAMES.contains(&base) {
                if let Ok(rel) = path.strip_prefix(root) {
                    out.push(rel.to_string_lossy().into_owned());
                }
            }
        }
    }
}

/// Render a list of tree violations as a stable machine-readable JSON report.
pub fn violations_to_json(violations: &[TreeViolation]) -> serde_json::Value {
    serde_json::json!({
        "clean": violations.is_empty(),
        "count": violations.len(),
        "violations": violations.iter().map(|v| serde_json::json!({
            "file": v.file,
            "line": v.line,
            "pattern_kind": v.pattern_kind,
            "context": v.context,
        })).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn clear_allow() {
        std::env::remove_var("GITHUB_ALLOWED_AUTHORS");
    }

    #[test]
    #[serial]
    fn private_ip_is_blocked() {
        clear_allow();
        let v = scan_for_pii("server at <internal-ip> listening"); // pii-test-fixture
        assert!(v.iter().any(|x| x.category == "private_ip"));
        assert!(pii_gate("<internal-ip>").is_err()); // pii-test-fixture
    }

    #[test]
    #[serial]
    fn container_id_is_blocked() {
        clear_allow();
        let v = scan_for_pii("deployed to <host> today"); // pii-test-fixture
        assert!(v.iter().any(|x| x.category == "container_id"));
        assert!(pii_gate("<host>").is_err()); // pii-test-fixture
    }

    #[test]
    #[serial]
    fn internal_hostname_is_blocked() {
        clear_allow();
        let v = scan_for_pii("ran on <host> build host"); // pii-test-fixture
        assert!(v.iter().any(|x| x.category == "internal_hostname"));
        assert!(pii_gate("<host>").is_err()); // pii-test-fixture
        // case-insensitive
        assert!(!scan_for_pii("<host>").is_empty()); // pii-test-fixture
    }

    #[test]
    #[serial]
    fn internal_domain_is_blocked() {
        clear_allow();
        let v = scan_for_pii("visit git.example.com for repos"); // pii-test-fixture
        assert!(v.iter().any(|x| x.category == "internal_domain"));
        assert!(pii_gate("example.com").is_err()); // pii-test-fixture
    }

    #[test]
    #[serial]
    fn operator_name_is_blocked() {
        clear_allow();
        let v = scan_for_pii("please ask <operator> about it"); // pii-test-fixture
        assert!(v.iter().any(|x| x.category == "operator_name"));
        assert!(pii_gate("<operator> must approve this").is_err()); // pii-test-fixture
        // case-insensitive
        assert!(!scan_for_pii("<operator> wants this").is_empty()); // pii-test-fixture
        // whole-word only: a surname/longer word containing the same
        // letters must NOT trigger a false positive.
        assert!(scan_for_pii("the peters family moved in").is_empty()); // pii-test-fixture
    }

    #[test]
    #[serial]
    fn operator_name_fixture_tag_is_exempt_from_tree_sweep() {
        let dir = temp_tree("operator-name-fixture");
        let untagged_line = "an untagged mention: <operator> approved this"; // pii-test-fixture
        let tagged_line = "a tagged mention: <operator> approved this // pii-test-fixture"; // pii-test-fixture
        let contents = format!("{untagged_line}\n{tagged_line}\n");
        write_file(&dir, "note.txt", &contents);
        let rs = PiiRuleSet::new();
        let violations = rs.scan_tree(&dir);
        let operator_hits: Vec<_> = violations
            .iter()
            .filter(|v| v.pattern_kind == "operator_name")
            .collect();
        // Only the untagged line (line 1) should be flagged; the
        // `// pii-test-fixture`-tagged line (line 2) is stripped before
        // scanning and must never appear.
        assert_eq!(operator_hits.len(), 1, "{operator_hits:?}");
        assert_eq!(operator_hits[0].line, 1);
    }

    #[test]
    #[serial]
    fn api_key_is_blocked() {
        clear_allow();
        let v = scan_for_pii("token <REDACTED-SECRET> here"); // pii-test-fixture
        assert!(v.iter().any(|x| x.category == "api_key"));
        assert!(pii_gate("<REDACTED-SECRET>").is_err()); // pii-test-fixture
    }

    #[test]
    #[serial]
    fn phone_is_blocked() {
        clear_allow();
        let v = scan_for_pii("call <phone> now"); // pii-test-fixture
        assert!(v.iter().any(|x| x.category == "phone"));
        assert!(pii_gate("<phone>").is_err()); // pii-test-fixture
    }

    #[test]
    #[serial]
    fn bare_iso_date_is_not_flagged_as_phone() {
        clear_allow();
        let v = scan_for_pii("2024-11-05"); // pii-test-fixture (ISO date, not a phone number)
        assert!(
            !v.iter().any(|x| x.category == "phone"),
            "bare ISO date must not be flagged as phone: {v:?}"
        );
        assert!(pii_gate("2024-11-05").is_ok());
    }

    #[test]
    #[serial]
    fn iso_date_embedded_in_sentence_is_not_flagged_as_phone() {
        clear_allow();
        let v = scan_for_pii("protocolVersion: 2024-11-05 was negotiated"); // pii-test-fixture
        assert!(
            !v.iter().any(|x| x.category == "phone"),
            "embedded ISO date must not be flagged as phone: {v:?}"
        );
    }

    #[test]
    #[serial]
    fn e164_phone_still_flagged_regression_guard() {
        clear_allow();
        let v = scan_for_pii("call <phone> today"); // pii-test-fixture
        assert!(v.iter().any(|x| x.category == "phone"), "e.164-shaped phone must still flag: {v:?}");
    }

    #[test]
    #[serial]
    fn hyphenated_phone_still_flagged_regression_guard() {
        clear_allow();
        let v = scan_for_pii("reach me at <phone>"); // pii-test-fixture
        assert!(v.iter().any(|x| x.category == "phone"), "hyphenated phone must still flag: {v:?}");
    }

    #[test]
    #[serial]
    fn date_suppression_is_span_scoped_not_whole_line() {
        clear_allow();
        // A date and a genuine phone number on the SAME line: suppression must be
        // scoped to the date's own span, not blanket-suppress the whole line.
        let v = scan_for_pii("released 2024-11-05, call <phone>"); // pii-test-fixture
        assert!(
            v.iter().any(|x| x.category == "phone"),
            "a real phone elsewhere on a line containing a date must still flag: {v:?}"
        );
        // And the date itself must still not be flagged as a phone.
        let date_only = scan_for_pii("released 2024-11-05 today"); // pii-test-fixture
        assert!(
            !date_only.iter().any(|x| x.category == "phone"),
            "the date span itself must not be flagged: {date_only:?}"
        );
    }

    /// GHMRFIX-gate-tune: an ISO-8601 *datetime* (`2026-07-10T14:32:07Z`) is
    /// digit/hyphen shaped exactly like the bare ISO date, but the trailing
    /// `T` (a word character) means the bare-date regex's closing `\b` never
    /// fires right after the day, so the date-exclusion span used to miss it
    /// and the date's own digits fell through to the `phone` matcher. This
    /// was the root cause of ~15 of the 40 non-SVG phone false positives
    /// (JSON `"timestamp"`/`"created"`/`"last_updated"` fields across
    /// `docs/**`).
    #[test]
    #[serial]
    fn iso_datetime_is_not_flagged_as_phone() {
        clear_allow();
        for sample in [
            r#""timestamp": "2026-07-10T14:32:07Z""#,
            r#""created": "2026-06-01T10:00:00Z""#,
            r#""last_updated": "2026-07-09T…""#,
        ] {
            let v = scan_for_pii(sample); // pii-test-fixture
            assert!(
                !v.iter().any(|x| x.category == "phone"),
                "ISO datetime must not be flagged as phone: {sample:?} -> {v:?}"
            );
        }
        // A genuine phone number sharing a line with an ISO datetime must
        // still flag — the exclusion is span-scoped, not line-wide.
        let mixed = scan_for_pii("at 2026-07-10T14:32:07Z call <phone>"); // pii-test-fixture
        assert!(
            mixed.iter().any(|x| x.category == "phone"),
            "a real phone alongside an ISO datetime must still flag: {mixed:?}"
        );
    }

    /// GHMRFIX-gate-tune: hash digests (git SHAs), unix timestamps, and large
    /// integer IDs are digit runs 10+ chars long with no `+` prefix and no
    /// internal space/hyphen separator — the exact shape the bare `phone`
    /// regex used to accept. These are the other ~25 of the 40 non-SVG false
    /// positives (JSON `vram_before`/`vram_after` byte counts in the intake
    /// corpus, git SHA fragments in `docs/tools/code-git/github.md`, a
    /// negative `ADVISORY_LOCK_KEY` constant, Prometheus unix timestamps).
    #[test]
    #[serial]
    fn unseparated_digit_runs_are_not_flagged_as_phone() {
        clear_allow();
        for sample in [
            "\"vram_before\": 31479877632", // pii-test-fixture (byte count)
            "\"timestamp\": 1717000000.0",  // pii-test-fixture (unix ts)
            "ADVISORY_LOCK_KEY = -5322992491554488081", // pii-test-fixture (integer ID)
            "\"base_sha\": \"a1b2c3d4e5f60718293a4b5c6d7e8f9012345678\"", // pii-test-fixture (git SHA)
        ] {
            let v = scan_for_pii(sample); // pii-test-fixture
            assert!(
                !v.iter().any(|x| x.category == "phone"),
                "unseparated digit run must not be flagged as phone: {sample:?} -> {v:?}"
            );
        }
    }

    /// A short hyphen-separated digit shape that is NOT a phone number (e.g.
    /// docs prose describing the canonical UUID shape as `8-4-4-4-12`) must
    /// not flag even though it has an internal separator — it has too few
    /// digits (6) to plausibly be a phone number. Real hyphenated phones
    /// (10+ digits, see `hyphenated_phone_still_flagged_regression_guard`)
    /// are unaffected by this minimum.
    #[test]
    #[serial]
    fn short_hyphenated_non_phone_shape_is_not_flagged() {
        clear_allow();
        let v = scan_for_pii("the canonical 8-4-4-4-12 hyphenated shape"); // pii-test-fixture
        assert!(
            !v.iter().any(|x| x.category == "phone"),
            "short hyphenated non-phone shape must not be flagged: {v:?}"
        );
    }

    #[test]
    #[serial]
    fn allowed_author_email_is_permitted() {
        std::env::set_var("GITHUB_ALLOWED_AUTHORS", "<email>, Moose"); // pii-test-fixture
        let v = scan_for_pii("Co-Authored-By: <email>"); // pii-test-fixture
        assert!(
            !v.iter().any(|x| x.category == "email"),
            "allow-listed author email must not be flagged: {v:?}"
        );
        clear_allow();
    }

    #[test]
    #[serial]
    fn non_allowed_email_is_blocked() {
        clear_allow();
        let v = scan_for_pii("contact <email>"); // pii-test-fixture
        assert!(v.iter().any(|x| x.category == "email"));
    }

    #[test]
    #[serial]
    fn bare_uuid_is_allowed() {
        clear_allow();
        let uuid = "550e8400-e29b-41d4-a716-446655440000"; // pii-test-fixture
        let v = scan_for_pii(&format!("request id {uuid} completed"));
        assert!(
            !v.iter().any(|x| x.category == "uuid_secret"),
            "bare generic UUID must be allowed: {v:?}"
        );
        assert!(pii_gate(uuid).is_ok());
    }

    #[test]
    #[serial]
    fn infisical_uuid_is_blocked() {
        clear_allow();
        let line = "<secret-manager> project fc51cfe1-0000-0000-0000-000000000000"; // pii-test-fixture
        let v = scan_for_pii(line);
        assert!(
            v.iter().any(|x| x.category == "uuid_secret"),
            "<secret-manager>-cued UUID must be blocked: {v:?}" // pii-test-fixture
        );
        assert!(pii_gate(line).is_err());
    }

    #[test]
    #[serial]
    fn project_id_uuid_is_blocked() {
        clear_allow();
        let line = "project_id: <uuid>"; // pii-test-fixture
        let v = scan_for_pii(line);
        assert!(v.iter().any(|x| x.category == "uuid_secret"));
    }

    #[test]
    #[serial]
    fn clean_rust_source_is_allowed() {
        clear_allow();
        let src = r#"
fn add(a: usize, b: usize) -> usize {
    a + b
}

#[test]
fn it_works() {
    assert_eq!(add(2, 3), 5);
}
"#;
        let v = scan_for_pii(src);
        assert!(v.is_empty(), "clean source must have no violations: {v:?}");
        assert!(pii_gate(src).is_ok());
    }

    #[test]
    #[serial]
    fn batch_with_one_dirty_content_is_rejected() {
        clear_allow();
        let mut contents: Vec<String> = (0..9).map(|i| format!("clean line {i}\n")).collect();
        contents.push("oops <internal-ip> leaked".to_string()); // pii-test-fixture
        assert_eq!(contents.len(), 10);

        // Mirror the batch semantics used by write tools: any violation rejects all.
        let mut any = false;
        for c in &contents {
            if pii_gate(c).is_err() {
                any = true;
                break;
            }
        }
        assert!(any, "batch containing one PII content must be rejected");
    }

    #[test]
    #[serial]
    fn gate_returns_err_not_ok_on_violation() {
        clear_allow();
        // The API path is only reachable on Ok(()); prove a violation yields Err.
        let r = pii_gate("host <host> at <internal-ip>"); // pii-test-fixture
        assert!(r.is_err());
        match r {
            Err(ToolError::InvalidArgument(msg)) => {
                assert!(msg.starts_with("BLOCKED:"), "msg was: {msg}");
                assert!(msg.contains("PII pattern"));
            }
            other => panic!("expected InvalidArgument Err, got {other:?}"),
        }
    }

    #[test]
    #[serial]
    fn context_is_redacted_not_full_secret() {
        clear_allow();
        let secret = "<REDACTED-SECRET>"; // pii-test-fixture
        let v = scan_for_pii(secret);
        let api = v.iter().find(|x| x.category == "api_key").unwrap();
        assert!(
            !api.context.contains("SUPERSECRETVALUE"),
            "context must not echo the full secret: {}",
            api.context
        );
    }

    #[test]
    #[serial]
    fn line_numbers_are_one_based() {
        clear_allow();
        let content = "clean\nclean\nCT327\n"; // pii-test-fixture
        let v = scan_for_pii(content);
        let ct = v.iter().find(|x| x.category == "container_id").unwrap();
        assert_eq!(ct.line, 3);
    }

    #[test]
    #[serial]
    fn internal_path_is_blocked() {
        clear_allow();
        let v = scan_for_pii("see <path>/repos/x for details"); // pii-test-fixture
        assert!(v.iter().any(|x| x.category == "internal_path"));
    }

    /// GHMRFIX-gate-tune: `localhost`/`127.0.0.1`/`0.0.0.0` are loopback/
    /// bind-all addresses, never internal-fleet-specific — every one of the
    /// 25 residual hits from this shape in `docs/**`/`terminus-client/**`
    /// was a legitimate localhost example, not a leak. The `local_url`
    /// category (which only ever matched these three loopback shapes) is
    /// retired from the write gate entirely; a real `192.168.x`/`10.x`
    /// address with a port is still caught by `private_ip` regardless of
    /// any trailing `:port`.
    #[test]
    #[serial]
    fn local_url_loopback_addresses_are_not_flagged() {
        clear_allow();
        for sample in [
            "proxy on localhost:4000 active",
            "bind at 127.0.0.1:8300 for enroll",
            "listening on 0.0.0.0:8080",
        ] {
            let v = scan_for_pii(sample); // pii-test-fixture
            assert!(
                !v.iter().any(|x| x.category == "local_url"),
                "loopback URL must not be flagged: {sample:?} -> {v:?}"
            );
        }
    }

    #[test]
    #[serial]
    fn real_private_ip_with_port_is_still_blocked() {
        clear_allow();
        let v = scan_for_pii("bind at <internal-ip>:8300 for enroll"); // pii-test-fixture
        assert!(
            v.iter().any(|x| x.category == "private_ip"),
            "a real LAN IP with a port must still flag via private_ip: {v:?}"
        );
    }

    #[test]
    #[serial]
    fn infra_service_is_blocked() {
        clear_allow();
        let v = scan_for_pii("secrets in <secret-manager> vault"); // pii-test-fixture
        assert!(v.iter().any(|x| x.category == "infra_service"));
    }

    /// Root-cause regression guard: walk this crate's own `src/` tree and run
    /// [`scan_for_pii`] against every `.rs` file, exactly as it would be run
    /// against outbound content before a GitHub write. Lines carrying the
    /// `pii-test-fixture` marker (the repo-wide convention for deliberate
    /// PII-shaped test literals — see `src/cortex/mod.rs`, `src/cortex/audit.rs`,
    /// `src/bin/review_daemon/sandbox.rs`, `src/bin/review_daemon/egress_proxy.rs`,
    /// and this file's own test module) are stripped before scanning.
    ///
    /// This is the self-check for the 2026-07 PII comment-scrub remediation:
    /// it must fail loudly, with exact file:line:category detail, if a future
    /// change reintroduces a real infra identifier (container ID, `<host>`-style // pii-test-fixture
    /// hostname, private IP, internal path, etc.) into a doc/code comment
    /// anywhere in the crate.
    #[test]
    #[serial]
    fn no_pii_in_own_source_tree() {
        clear_allow();

        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let src_dir = manifest_dir.join("src");
        assert!(
            src_dir.is_dir(),
            "expected {src_dir:?} to exist — self-check is walking the wrong tree"
        );

        let mut rs_files = Vec::new();
        collect_rs_files(&src_dir, &mut rs_files);
        assert!(
            rs_files.len() > 50,
            "expected to find a substantial number of .rs files under {src_dir:?}, \
             found {} — self-check may be misconfigured",
            rs_files.len()
        );

        let mut findings: Vec<String> = Vec::new();

        for path in &rs_files {
            let content = match std::fs::read_to_string(path) {
                Ok(c) => c,
                Err(e) => {
                    findings.push(format!("{}: <unreadable: {e}>", path.display()));
                    continue;
                }
            };

            // Strip any line carrying the exemption marker before scanning,
            // exactly as production PII-gate callers are expected to do for
            // deliberate test fixtures.
            let scrubbed: String = content
                .lines()
                .map(|line| {
                    if line.contains("pii-test-fixture") {
                        ""
                    } else {
                        line
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");

            for v in scan_for_pii(&scrubbed) {
                findings.push(format!(
                    "{}:{}:{}: {}",
                    path.display(),
                    v.line,
                    v.category,
                    v.context
                ));
            }
        }

        assert!(
            findings.is_empty(),
            "PII self-check found {} violation(s) in this crate's own source tree \
             (file:line:category:context) — tag deliberate test fixtures with \
             `// pii-test-fixture` or rewrite the offending comment generically:\n{}",
            findings.len(),
            findings.join("\n")
        );
    }

    /// Recursively collect `.rs` file paths under `dir`, skipping any
    /// `target/` build-output directory.
    fn collect_rs_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path.file_name().and_then(|n| n.to_str()) == Some("target") {
                    continue;
                }
                collect_rs_files(&path, out);
            } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                out.push(path);
            }
        }
    }

    // ── Tree-sweep engine tests (GHMR-01) ────────────────────────────────────

    use std::io::Write;

    fn write_file(dir: &std::path::Path, rel: &str, content: &str) {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
    }

    /// A fresh temp dir under the OS temp root, unique per test.
    fn temp_tree(tag: &str) -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!(
            "ghmr01-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    #[serial]
    fn scan_tree_flags_each_pattern_kind() {
        clear_allow();
        let dir = temp_tree("kinds");
        write_file(&dir, "a.txt", "server at <internal-ip> listening\n"); // pii-test-fixture
        write_file(&dir, "b.txt", "deployed to <host> build\n"); // pii-test-fixture
        write_file(&dir, "sub/c.md", "ran on <host> host\n"); // pii-test-fixture
        write_file(&dir, "d.txt", "contact <email> now\n"); // pii-test-fixture

        let v = scan_tree(&dir);
        let kinds: HashSet<&str> = v.iter().map(|x| x.pattern_kind.as_str()).collect();
        assert!(kinds.contains("private_ip"), "{v:?}");
        assert!(kinds.contains("container_id"), "{v:?}");
        assert!(kinds.contains("internal_hostname"), "{v:?}");
        assert!(kinds.contains("email"), "{v:?}");
        // file/line are populated
        assert!(v.iter().all(|x| !x.file.is_empty() && x.line >= 1));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    #[serial]
    fn scan_tree_covers_python_gate_extension_patterns() {
        clear_allow();
        let dir = temp_tree("parity");
        // The pattern kinds the legacy pii_gate.py covered — assert parity.
        write_file(&dir, "ip.txt", "<internal-ip>\n"); // pii-test-fixture
        write_file(&dir, "ghp.txt", "<REDACTED-SECRET>\n"); // pii-test-fixture
        write_file(&dir, "sk.txt", "<REDACTED-SECRET>\n"); // pii-test-fixture
        write_file(&dir, "glpat.txt", "<REDACTED-SECRET>\n"); // pii-test-fixture
        write_file(&dir, "aws.txt", "<REDACTED-SECRET>\n"); // pii-test-fixture
        write_file(&dir, "goog.txt", "<REDACTED-SECRET>\n"); // pii-test-fixture
        write_file(&dir, "slack.txt", "<REDACTED-SECRET>\n"); // pii-test-fixture
        write_file(&dir, "jwt.txt", "<REDACTED-SECRET>\n"); // pii-test-fixture
        write_file(&dir, "pem.txt", "<REDACTED-SECRET>\n"); // pii-test-fixture
        write_file(&dir, "sec.txt", "password = \"hunter2hunter2\"\n"); // pii-test-fixture
        write_file(&dir, "host.txt", "example.com\n"); // pii-test-fixture
        write_file(&dir, "path.txt", "see <path>/repos/x\n"); // pii-test-fixture
        write_file(&dir, "phone.txt", "call <phone> now\n"); // pii-test-fixture

        let v = scan_tree(&dir);
        let kinds: HashSet<&str> = v.iter().map(|x| x.pattern_kind.as_str()).collect();
        for expect in [
            "private_ip",
            "api_key",
            "aws_access_key",
            "google_api_key",
            "slack_user_token",
            "jwt",
            "ssh_key",
            "generic_secret",
            "internal_domain",
            "internal_path",
            "phone",
        ] {
            assert!(kinds.contains(expect), "missing parity kind {expect}: {kinds:?}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    #[serial]
    fn fixture_tag_is_line_exact_not_a_blanket_bypass() {
        clear_allow();
        let dir = temp_tree("fixture");
        // First line tagged (exempt), second line an untagged REAL violation.
        write_file(
            &dir,
            "mix.txt",
            "host <host> fixture line // pii-test-fixture\nleaked <internal-ip> here\n",
        );
        let v = scan_tree(&dir);
        // The tagged line's hostname token must be exempt...
        assert!(
            !v.iter().any(|x| x.pattern_kind == "internal_hostname"),
            "tagged line must be exempt: {v:?}"
        );
        // ...but the untagged private IP on the next line must still flag.
        assert!(
            v.iter().any(|x| x.pattern_kind == "private_ip"),
            "untagged violation must still flag: {v:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    #[serial]
    fn clean_tree_yields_zero() {
        clear_allow();
        let dir = temp_tree("clean");
        write_file(&dir, "ok.rs", "fn add(a: usize, b: usize) -> usize { a + b }\n");
        write_file(&dir, "readme.md", "# Title\n\nJust prose, nothing secret.\n");
        let v = scan_tree(&dir);
        assert!(v.is_empty(), "clean tree must be empty: {v:?}");
        assert!(violations_to_json(&v)["clean"].as_bool().unwrap());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    #[serial]
    fn excluded_files_dirs_and_binaries_are_skipped() {
        clear_allow();
        let dir = temp_tree("excl");
        // Excluded by dir (target/), by ext (.png), by name (Cargo.lock).
        write_file(&dir, "target/gen.txt", "leak <internal-ip>\n"); // pii-test-fixture
        write_file(&dir, "img.png", "<internal-ip> inside a png-named file\n"); // pii-test-fixture
        write_file(&dir, "Cargo.lock", "<internal-ip> lockfile\n"); // pii-test-fixture
        // Binary file (NUL byte) must be skipped, not panic.
        write_file(&dir, "blob.dat", "start\0\x01\x02<internal-ip> end\n"); // pii-test-fixture
        let v = scan_tree(&dir);
        assert!(v.is_empty(), "excluded/binary content must be skipped: {v:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    #[serial]
    fn config_driven_terms_and_allowed_emails() {
        clear_allow();
        let dir = temp_tree("config");
        write_file(&dir, "svc.txt", "the frobnicator service is down\n");
        write_file(&dir, "mail.txt", "reach <email> today\n"); // pii-test-fixture

        let cfg = PiiConfig {
            extra_terms: vec!["frobnicator".to_string()],
            allowed_emails: vec!["@placeholder.test".to_string()],
            ..Default::default()
        };
        let rs = PiiRuleSet::from_config(&cfg);
        let v = rs.scan_tree(&dir);
        let kinds: HashSet<&str> = v.iter().map(|x| x.pattern_kind.as_str()).collect();
        assert!(kinds.contains("config_term"), "config term must flag: {v:?}");
        // The allow-listed placeholder email must NOT flag.
        assert!(
            !v.iter().any(|x| x.pattern_kind == "email"),
            "allow-listed email must be permitted: {v:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // ── GHMR-fix regression tests (phone/svg, infra_service paths, generic_secret) ──

    #[test]
    #[serial]
    fn svg_coordinate_data_is_not_flagged_as_phone() {
        clear_allow();
        let dir = temp_tree("svg-phone");
        write_file(&dir, "diagram.svg", "<svg viewBox=\"0 0 1280 760\"><path d=\"M120 340 L960 340\"/></svg>\n"); // pii-test-fixture
        let v = scan_tree(&dir);
        assert!(
            !v.iter().any(|x| x.pattern_kind == "phone"),
            "SVG coordinate/viewBox data must not be flagged as phone: {v:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    #[serial]
    fn real_phone_shaped_string_still_flagged_in_non_svg_file() {
        clear_allow();
        let dir = temp_tree("real-phone");
        write_file(&dir, "contact.txt", "call <phone> for support\n"); // pii-test-fixture
        let v = scan_tree(&dir);
        assert!(
            v.iter().any(|x| x.pattern_kind == "phone"),
            "a real phone-shaped string in a non-svg file must still flag: {v:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    #[serial]
    fn real_phone_shaped_string_still_flagged_even_in_svg_text_content() {
        // Even inside an .svg file, only the `phone` category is suppressed —
        // this guards against accidentally suppressing everything for the
        // whole file. (No pattern kind currently depends on digit shape other
        // than phone/uuid/date_like, so this asserts private_ip still fires.)
        clear_allow();
        let dir = temp_tree("svg-other");
        write_file(
            &dir,
            "diagram.svg",
            "<svg><text>leaked at <internal-ip></text></svg>\n", // pii-test-fixture
        );
        let v = scan_tree(&dir);
        assert!(
            v.iter().any(|x| x.pattern_kind == "private_ip"),
            "non-phone categories must still be scanned inside .svg files: {v:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    #[serial]
    fn infra_service_product_name_exempt_in_docs_and_assets() {
        clear_allow();
        let dir = temp_tree("infra-docs");
        write_file(&dir, "docs/tools/infra-ops/<container-mgr>.md", "<container-mgr> manages containers.\n"); // pii-test-fixture
        write_file(&dir, "assets/architecture.svg", "<svg><text><secret-manager></text></svg>\n"); // pii-test-fixture
        write_file(&dir, "src/<secret-manager>/mod.rs", "// <secret-manager> client module\n"); // pii-test-fixture
        let v = scan_tree(&dir);
        assert!(
            !v.iter().any(|x| x.pattern_kind == "infra_service"),
            "product names in docs/assets/service-module paths must be exempt: {v:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    #[serial]
    fn infra_service_still_flagged_outside_exempt_paths() {
        clear_allow();
        let dir = temp_tree("infra-nonexempt");
        write_file(&dir, "notes.txt", "secrets in <secret-manager> vault\n"); // pii-test-fixture
        let v = scan_tree(&dir);
        assert!(
            v.iter().any(|x| x.pattern_kind == "infra_service"),
            "a product-name mention OUTSIDE docs/assets/service-module paths must still flag: {v:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    #[serial]
    fn generic_secret_ignores_bare_field_names_and_placeholders() {
        clear_allow();
        let dir = temp_tree("secret-shape");
        write_file(&dir, "struct.rs", "pub secret: String,\n");
        write_file(&dir, "call.rs", ".field(\"token\", something)\n");
        write_file(&dir, "test_lit.rs", "let token = \"testtoken\".to_string();\n");
        write_file(&dir, "placeholder.toml", "token = \"<EMPTYPAT>\"\n");
        let v = scan_tree(&dir);
        assert!(
            !v.iter().any(|x| x.pattern_kind == "generic_secret"),
            "bare field names, test literals, and placeholder tokens must not flag: {v:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    #[serial]
    fn generic_secret_still_flags_real_value_shaped_secret() {
        clear_allow();
        let dir = temp_tree("secret-real");
        write_file(&dir, "leak.txt", "secret = \"aGVsbG9tZXNzYWdlMTIzNDU2\"\n"); // pii-test-fixture
        let v = scan_tree(&dir);
        assert!(
            v.iter().any(|x| x.pattern_kind == "generic_secret"),
            "a digit-bearing, non-placeholder quoted value must still flag: {v:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    #[serial]
    fn scan_content_matches_legacy_gate_for_builtins() {
        clear_allow();
        // The ruleset's built-in coverage must be a superset of the legacy gate.
        let sample = "host <host> at <internal-ip> <host>"; // pii-test-fixture
        let legacy: HashSet<String> =
            scan_for_pii(sample).into_iter().map(|v| v.category).collect();
        let rs: HashSet<String> = PiiRuleSet::new()
            .scan_content(sample)
            .into_iter()
            .map(|v| v.category)
            .collect();
        assert!(legacy.is_subset(&rs), "legacy {legacy:?} not subset of ruleset {rs:?}");
    }
}
