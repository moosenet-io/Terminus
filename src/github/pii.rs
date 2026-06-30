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
}

fn patterns() -> &'static Patterns {
    static P: OnceLock<Patterns> = OnceLock::new();
    P.get_or_init(|| Patterns {
        private_ip: Regex::new(
            r"\b(?:192\.168|10\.\d{1,3}|172\.(?:1[6-9]|2\d|3[01]))\.\d{1,3}\.\d{1,3}\b",
        )
        .expect("private_ip regex"),
        container_id: Regex::new(r"\bCT\d{3}\b").expect("container_id regex"),
        internal_host: Regex::new(r"(?i)\b(?:pvf1|pvm|pvs|pve|ironclaw)\b")
            .expect("internal_host regex"),
        internal_domain: Regex::new(r"moosenet\.online|moosenet\.local")
            .expect("internal_domain regex"),
        email: Regex::new(r"[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,}")
            .expect("email regex"),
        phone: Regex::new(r"\+?\d[\d\s\-]{8,}\d").expect("phone regex"),
        api_key: Regex::new(r"\b(?:sk-|ghp_|gsk_|glpat-|xox[bpasr]-)\S+")
            .expect("api_key regex"),
        internal_path: Regex::new(r"/home/coder/|/srv/tn-working/|/opt/chord/|/opt/lumina/")
            .expect("internal_path regex"),
        local_url: Regex::new(r"(?:localhost|127\.0\.0\.1|0\.0\.0\.0):\d{4,5}")
            .expect("local_url regex"),
        infra_service: Regex::new(r"(?i)\b(?:tuwunel|infisical|jellyseerr|portainer)\b")
            .expect("infra_service regex"),
        uuid: Regex::new(
            r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}",
        )
        .expect("uuid regex"),
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
/// infra-secret cue (`infisical`, `project_id`, `workspace_id`,
/// `machine_identity`) within ~40 chars of the match. Bare UUIDs are allowed.
fn uuid_is_sensitive(line: &str, m_start: usize, m_end: usize) -> bool {
    let cues = ["infisical", "project_id", "workspace_id", "machine_identity"];
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

        // Phone: skip any match that overlaps a UUID span (those digits belong
        // to the UUID, not a phone number).
        for m in p.phone.find_iter(line) {
            let overlaps_uuid = uuid_spans
                .iter()
                .any(|&(s, e)| m.start() < e && s < m.end());
            if !overlaps_uuid {
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
        let v = scan_for_pii("server at 10.0.0.1 listening");
        assert!(v.iter().any(|x| x.category == "private_ip"));
        assert!(pii_gate("10.0.0.1").is_err());
    }

    #[test]
    #[serial]
    fn container_id_is_blocked() {
        clear_allow();
        let v = scan_for_pii("deployed to CT327 today");
        assert!(v.iter().any(|x| x.category == "container_id"));
        assert!(pii_gate("CT327").is_err());
    }

    #[test]
    #[serial]
    fn internal_hostname_is_blocked() {
        clear_allow();
        let v = scan_for_pii("ran on pvf1 build host");
        assert!(v.iter().any(|x| x.category == "internal_hostname"));
        assert!(pii_gate("pvf1").is_err());
        // case-insensitive
        assert!(!scan_for_pii("PVF1").is_empty());
    }

    #[test]
    #[serial]
    fn internal_domain_is_blocked() {
        clear_allow();
        let v = scan_for_pii("visit git.moosenet.online for repos");
        assert!(v.iter().any(|x| x.category == "internal_domain"));
        assert!(pii_gate("moosenet.online").is_err());
    }

    #[test]
    #[serial]
    fn api_key_is_blocked() {
        clear_allow();
        let v = scan_for_pii("token sk-ant-api03-XXXXXXXXXXXX here");
        assert!(v.iter().any(|x| x.category == "api_key"));
        assert!(pii_gate("sk-ant-api03-XXXX").is_err());
    }

    #[test]
    #[serial]
    fn phone_is_blocked() {
        clear_allow();
        let v = scan_for_pii("call +442071234567 now");
        assert!(v.iter().any(|x| x.category == "phone"));
        assert!(pii_gate("+442071234567").is_err());
    }

    #[test]
    #[serial]
    fn allowed_author_email_is_permitted() {
        std::env::set_var("GITHUB_ALLOWED_AUTHORS", "noreply@anthropic.com, Moose");
        let v = scan_for_pii("Co-Authored-By: noreply@anthropic.com");
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
        let v = scan_for_pii("contact someone@example.com");
        assert!(v.iter().any(|x| x.category == "email"));
    }

    #[test]
    #[serial]
    fn bare_uuid_is_allowed() {
        clear_allow();
        let uuid = "550e8400-e29b-41d4-a716-446655440000";
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
        let line = "infisical project fc51cfe1-0000-0000-0000-000000000000";
        let v = scan_for_pii(line);
        assert!(
            v.iter().any(|x| x.category == "uuid_secret"),
            "infisical-cued UUID must be blocked: {v:?}"
        );
        assert!(pii_gate(line).is_err());
    }

    #[test]
    #[serial]
    fn project_id_uuid_is_blocked() {
        clear_allow();
        let line = "project_id: fc51cfe1-1111-2222-3333-444455556666";
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
        contents.push("oops 10.0.0.42 leaked".to_string());
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
        let r = pii_gate("host pvf1 at 10.0.0.1");
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
        let secret = "sk-ant-api03-SUPERSECRETVALUE123456";
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
        let content = "clean\nclean\nCT327\n";
        let v = scan_for_pii(content);
        let ct = v.iter().find(|x| x.category == "container_id").unwrap();
        assert_eq!(ct.line, 3);
    }

    #[test]
    #[serial]
    fn internal_path_is_blocked() {
        clear_allow();
        let v = scan_for_pii("see /home/coder/repos/x for details");
        assert!(v.iter().any(|x| x.category == "internal_path"));
    }

    #[test]
    #[serial]
    fn local_url_is_blocked() {
        clear_allow();
        let v = scan_for_pii("proxy on localhost:4000 active");
        assert!(v.iter().any(|x| x.category == "local_url"));
    }

    #[test]
    #[serial]
    fn infra_service_is_blocked() {
        clear_allow();
        let v = scan_for_pii("secrets in Infisical vault");
        assert!(v.iter().any(|x| x.category == "infra_service"));
    }
}
