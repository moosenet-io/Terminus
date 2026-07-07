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

use std::sync::OnceLock;

use regex::Regex;

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
    local_url: Regex,
    infra_service: Regex,
    uuid: Regex,
    date_like: Regex,
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
        local_url: Regex::new(r"(?:localhost|127\.0\.0\.1|0\.0\.0\.0):\d{4,5}")
            .expect("local_url regex"),
        infra_service: Regex::new(r"(?i)\b(?:<matrix-server>|<secret-manager>|<media-service>|<container-mgr>)\b") // pii-test-fixture
            .expect("infra_service regex"),
        uuid: Regex::new(
            r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}",
        )
        .expect("uuid regex"),
        date_like: Regex::new(r"\b\d{4}-\d{2}-\d{2}\b").expect("date_like regex"),
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

/// Scan `content` for PII, returning one [`PiiViolation`] per match with a
/// 1-based line number and a redacted context snippet.
pub fn scan_for_pii(content: &str) -> Vec<PiiViolation> {
    let p = patterns();
    let allow = allowed_authors();
    let mut out = Vec::new();

    for (idx, line) in content.lines().enumerate() {
        let line_no = idx + 1;

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
        for m in p.local_url.find_iter(line) {
            push("local_url", m.as_str());
        }
        for m in p.infra_service.find_iter(line) {
            push("infra_service", m.as_str());
        }

        // Emails: allow-list exception for author attribution.
        for m in p.email.find_iter(line) {
            if !email_is_allowed(m.as_str(), &allow) {
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
        // to the UUID, not a phone number) or a bare ISO-date span.
        for m in p.phone.find_iter(line) {
            let overlaps_uuid = uuid_spans
                .iter()
                .any(|&(s, e)| m.start() < e && s < m.end());
            let overlaps_date = date_spans
                .iter()
                .any(|&(s, e)| m.start() < e && s < m.end());
            if !overlaps_uuid && !overlaps_date {
                push("phone", m.as_str());
            }
        }
    }

    out
}

/// Mandatory gate. Returns `Ok(())` when clean, otherwise an
/// [`ToolError::InvalidArgument`] whose message lists every violation by
/// line/category. Logs a one-line audit record (pass/fail + count) without
/// ever logging secret values.
pub fn pii_gate(content: &str) -> Result<(), ToolError> {
    let violations = scan_for_pii(content);

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
    fn real_phone_numbers_still_flagged_regression_guard() {
        clear_allow();
        let v1 = scan_for_pii("call <phone> today"); // pii-test-fixture
        assert!(v1.iter().any(|x| x.category == "phone"), "e.164-shaped phone must still flag: {v1:?}");

        let v2 = scan_for_pii("reach me at <phone>"); // pii-test-fixture
        assert!(v2.iter().any(|x| x.category == "phone"), "hyphenated phone must still flag: {v2:?}");
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

    #[test]
    #[serial]
    fn local_url_is_blocked() {
        clear_allow();
        let v = scan_for_pii("proxy on localhost:4000 active"); // pii-test-fixture
        assert!(v.iter().any(|x| x.category == "local_url"));
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
}
