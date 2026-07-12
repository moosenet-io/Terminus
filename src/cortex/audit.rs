//! URL validation for `cortex_audit` — the highest-risk tool in this module.
//!
//! `cortex_audit` audits an *external, operator-supplied* public git repository
//! URL. As of CXEG-01 the retired SSH-exec-to-fleet-host relay is gone; the
//! tool is currently a stub whose Atlas-backed clone → `scribe_kg_build` →
//! report backend is rebuilt in CXEG-11. This file's job is unchanged and
//! remains valuable independent of that rebuild: strict, fail-closed
//! validation of the `url` argument.
//!
//! `validate_repo_url` is the front-gate every `cortex_audit` call passes
//! before the URL reaches any backend. It rejects URL shapes that have no
//! legitimate reason to be passed to "clone a public git repo" — non-http(s)
//! schemes, embedded credentials, shell metacharacters, and (crucially)
//! loopback / private / link-local / metadata hosts in any of their common
//! obfuscated encodings. That closes off SSRF-style redirection of a future
//! clone step at internal/private network targets under the guise of a
//! "public repo audit", regardless of what transport CXEG-11 lands on. The
//! guard is deliberately backend-agnostic: it protects the input whether the
//! clone eventually runs in-process on the terminus host or elsewhere.

use crate::error::ToolError;

const MAX_URL_LEN: usize = 500;

/// Validate that `url` is a plausible **public** git repository URL:
/// - `http` or `https` scheme only (no `ssh://`, `git://`, `file://`,
///   `ftp://`, `data:`, etc. — those either escape the "public git host over
///   HTTPS" assumption the tool's own description makes, or have no business
///   being clone targets for an "external public Git repository" audit tool).
/// - No embedded userinfo (`user:pass@host`) — credential smuggling into a
///   downstream clone command has no legitimate use here.
/// - No control characters, whitespace, or shell metacharacters — a URL has
///   no legitimate reason to contain any of these, so rejecting them outright
///   is strictly safer than relying on any downstream quoting alone.
/// - Host must not resolve, textually, to a loopback/private/link-local
///   address or `localhost` — a clone/audit backend has no business being
///   pointed at internal addresses under the guise of "public repo audit",
///   which would be an SSRF-shaped hole.
/// - Length-capped like every other free-text field in this crate.
pub fn validate_repo_url(url: &str) -> Result<(), ToolError> {
    if url.is_empty() {
        return Err(ToolError::InvalidArgument("'url' must not be empty".into()));
    }
    if url.chars().count() > MAX_URL_LEN {
        return Err(ToolError::InvalidArgument(format!(
            "'url' exceeds {MAX_URL_LEN} character limit"
        )));
    }
    if url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(ToolError::InvalidArgument(
            "'url' must not contain whitespace or control characters".into(),
        ));
    }
    const FORBIDDEN_SHELL_CHARS: &[char] =
        &[';', '&', '|', '`', '$', '(', ')', '<', '>', '\\', '"', '\'', '\n', '\r'];
    if url.chars().any(|c| FORBIDDEN_SHELL_CHARS.contains(&c)) {
        return Err(ToolError::InvalidArgument(
            "'url' contains characters that are never valid in a git repo URL".into(),
        ));
    }

    let scheme_end = url.find("://").ok_or_else(|| {
        ToolError::InvalidArgument("'url' must be an http:// or https:// URL".into())
    })?;
    let scheme = &url[..scheme_end];
    if scheme != "http" && scheme != "https" {
        return Err(ToolError::InvalidArgument(format!(
            "'url' scheme '{scheme}' is not allowed — only http/https public git URLs are accepted"
        )));
    }

    let rest = &url[scheme_end + 3..];
    if rest.is_empty() {
        return Err(ToolError::InvalidArgument("'url' has no host".into()));
    }
    // authority ends at the first '/', '?', or '#'
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];

    if authority.contains('@') {
        return Err(ToolError::InvalidArgument(
            "'url' must not contain embedded credentials (user@host)".into(),
        ));
    }

    // Strip a port suffix, if any (but keep IPv6 bracket literals intact for
    // the loopback/private checks below).
    let host = if let Some(bracket_end) = authority.find(']') {
        &authority[..=bracket_end]
    } else {
        authority.split(':').next().unwrap_or(authority)
    };

    if host.is_empty() {
        return Err(ToolError::InvalidArgument("'url' has no host".into()));
    }

    let host_lower = host.to_ascii_lowercase();
    if is_disallowed_host(&host_lower) {
        return Err(ToolError::InvalidArgument(
            "'url' must point to a public host, not a local/private/internal address".into(),
        ));
    }

    Ok(())
}

/// Textual check for loopback / private / link-local / metadata hosts. This
/// is a defense-in-depth string check, not a DNS-resolving check (validation
/// runs on the textual URL before any backend resolves or clones it). It
/// catches the obvious, common ways an operator or an attacker-controlled
/// input could try to point the tool at internal infrastructure.
///
/// ## Fail-closed on ambiguous numeric hosts
///
/// A real HTTP/git client resolves far more host shapes to an IPv4 address
/// than a plain 4-octet decimal dotted-quad: bare decimal integers
/// (`2130706433` == `127.0.0.1`), hex (`0x7f000001`), octal-by-leading-zero — pii-test-fixture
/// (`0177.0.0.1` — glibc's `inet_aton` reads the leading zero as octal `127`,
/// i.e. loopback, even though it parses as decimal `177` under a naive
/// string-to-u8 parse), 2/3-part shorthand (`127.1`), and IPv4-mapped IPv6
/// (`::ffff:127.0.0.1`). An earlier version of this function only recognized
/// the plain 4-octet decimal form and let every other encoding through
/// unchecked — a classic SSRF-filter "IP obfuscation" gap. The fix: treat
/// *any* numeric-looking host as a candidate IP address that must parse
/// strictly (via [`parse_strict_ipv4`]) to be judged safe; if it looks
/// numeric/IP-shaped but doesn't parse strictly, it is rejected outright
/// (fail closed) rather than allowed through by omission (fail open).
fn is_disallowed_host(host_lower: &str) -> bool {
    if host_lower == "localhost" || host_lower.ends_with(".localhost") {
        return true;
    }
    if host_lower == "0" {
        return true;
    }

    // Bracketed IPv6 literal, e.g. "[::1]" or "[::ffff:127.0.0.1]".
    if host_lower.starts_with('[') {
        let inner = host_lower.trim_start_matches('[').trim_end_matches(']');
        // IPv4-mapped IPv6 — validate the embedded IPv4 address itself
        // rather than only pattern-matching the "::ffff:" prefix.
        if let Some(mapped) = inner.strip_prefix("::ffff:") {
            return match parse_strict_ipv4(mapped) {
                Some(v4) => is_disallowed_ipv4(v4),
                None => true, // unparseable/ambiguous embedded address -> fail closed
            };
        }
        return match inner.parse::<std::net::Ipv6Addr>() {
            Ok(v6) => is_disallowed_ipv6(v6),
            Err(_) => true, // unparseable bracketed literal -> fail closed
        };
    }

    // A bare "0x..." hex literal, or a purely digit/dot string, is always
    // treated as an IP-address candidate (no legitimate public DNS hostname
    // is shaped like either) and must parse as a strict dotted-quad to pass.
    let looks_like_ip_candidate = host_lower.starts_with("0x")
        || host_lower.chars().all(|c| c.is_ascii_digit() || c == '.');
    if looks_like_ip_candidate {
        return match parse_strict_ipv4(host_lower) {
            Some(v4) => is_disallowed_ipv4(v4),
            None => true, // decimal-integer/hex/octal/shorthand encoding -> fail closed
        };
    }

    false
}

/// Parse `s` as a strict, unambiguous IPv4 dotted-quad: exactly four
/// dot-separated decimal octets, each 1-3 ASCII digits, none with a leading
/// zero (a leading-zero octet like "0177" is ambiguous — some C library
/// resolvers, e.g. glibc's `inet_aton`, read it as octal, which would
/// silently disagree with a plain decimal reading of the same text).
/// Anything else (bare integers, hex, 2/3-part shorthand, leading zeros)
/// returns `None` so the caller fails closed.
fn parse_strict_ipv4(s: &str) -> Option<std::net::Ipv4Addr> {
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() != 4 {
        return None;
    }
    let mut octets = [0u8; 4];
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() || part.len() > 3 || !part.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        if part.len() > 1 && part.starts_with('0') {
            return None;
        }
        octets[i] = part.parse::<u8>().ok()?;
    }
    Some(std::net::Ipv4Addr::from(octets))
}

/// Range checks using the standard library's own classification methods
/// (`is_loopback`/`is_private`/`is_link_local`/`is_unspecified`) rather than
/// hand-rolled octet comparisons, plus the well-known cloud metadata address.
fn is_disallowed_ipv4(v4: std::net::Ipv4Addr) -> bool {
    v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || v4.is_unspecified()
        || v4.octets()[0] == 0
        || v4 == std::net::Ipv4Addr::new(169, 254, 169, 254)
}

fn is_disallowed_ipv6(v6: std::net::Ipv6Addr) -> bool {
    if v6.is_loopback() || v6.is_unspecified() {
        return true;
    }
    let seg0 = v6.segments()[0];
    (0xfe80..=0xfebf).contains(&seg0) // link-local
        || (0xfc00..=0xfdff).contains(&seg0) // unique local
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_accepts_plausible_public_https_url() {
        assert!(validate_repo_url("https://github.com/owner/repo").is_ok());
    }

    #[test]
    fn test_accepts_http_scheme() {
        assert!(validate_repo_url("http://gitlab.example.com/owner/repo").is_ok());
    }

    #[test]
    fn test_rejects_empty() {
        assert!(validate_repo_url("").is_err());
    }

    #[test]
    fn test_rejects_ssh_scheme() {
        let err = validate_repo_url("ssh://<email>/owner/repo.git").unwrap_err(); // pii-test-fixture
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[test]
    fn test_rejects_file_scheme() {
        assert!(validate_repo_url("file:///etc/passwd").is_err());
    }

    #[test]
    fn test_rejects_git_scheme() {
        assert!(validate_repo_url("git://github.com/owner/repo").is_err());
    }

    #[test]
    fn test_rejects_data_scheme() {
        assert!(validate_repo_url("data:text/plain;base64,aGk=").is_err());
    }

    #[test]
    fn test_rejects_embedded_credentials() {
        assert!(validate_repo_url("https://user:<email>/owner/repo").is_err()); // pii-test-fixture
    }

    #[test]
    fn test_rejects_localhost() {
        assert!(validate_repo_url("https://localhost/owner/repo").is_err());
        assert!(validate_repo_url("https://LOCALHOST/owner/repo").is_err());
    }

    // NOTE: the addresses in the tests below are RFC 1918 private-range /
    // loopback / link-local test fixtures for the SSRF guard, not real
    // infrastructure — they exist to prove `validate_repo_url` actually
    // rejects each reserved range it claims to.

    #[test]
    fn test_rejects_loopback_ipv4() {
        // test fixture: loopback address
        assert!(validate_repo_url("https://127.0.0.1/owner/repo").is_err());
    }

    #[test]
    fn test_rejects_private_ipv4_ranges() {
        // test fixtures: RFC 1918 class A, B, and C private ranges
        assert!(validate_repo_url("https://<internal-ip>/owner/repo").is_err()); // pii-test-fixture
        assert!(validate_repo_url("https://<internal-ip>/owner/repo").is_err()); // pii-test-fixture
        assert!(validate_repo_url("https://<internal-ip>/owner/repo").is_err()); // pii-test-fixture
    }

    #[test]
    fn test_rejects_link_local_and_metadata() {
        // test fixture: the well-known cloud metadata endpoint (AWS/GCP/Azure)
        assert!(validate_repo_url("https://169.254.169.254/latest/meta-data").is_err());
    }

    #[test]
    fn test_rejects_ipv6_loopback() {
        assert!(validate_repo_url("https://[::1]/owner/repo").is_err());
    }

    #[test]
    fn test_rejects_shell_metacharacters() {
        assert!(validate_repo_url("https://github.com/owner/repo; rm -rf /").is_err());
        assert!(validate_repo_url("https://github.com/owner/$(whoami)").is_err());
        assert!(validate_repo_url("https://github.com/owner/`whoami`").is_err());
        assert!(validate_repo_url("https://github.com/owner/repo|cat").is_err());
        assert!(validate_repo_url("https://github.com/owner/repo\nrm -rf /").is_err());
    }

    #[test]
    fn test_rejects_whitespace() {
        assert!(validate_repo_url("https://github.com/owner/repo with spaces").is_err());
    }

    #[test]
    fn test_rejects_oversized_url() {
        let huge = format!("https://github.com/{}", "a".repeat(MAX_URL_LEN));
        assert!(validate_repo_url(&huge).is_err());
    }

    #[test]
    fn test_accepts_url_with_port() {
        assert!(validate_repo_url("https://git.example.com:8443/owner/repo").is_ok());
    }

    // --- SSRF bypass regression tests --------------------------------------
    // Each of these encodes a loopback/private address in a form real HTTP/
    // git clients still resolve, but that a naive "4 decimal octets" check
    // would miss. All must be rejected.

    #[test]
    fn test_rejects_decimal_integer_ipv4() {
        // 2130706433 == 127.0.0.1 as a big-endian u32. — pii-test-fixture
        assert!(validate_repo_url("http://2130706433/owner/repo").is_err()); // pii-test-fixture
    }

    #[test]
    fn test_rejects_hex_ipv4() {
        // 0x7f000001 == 127.0.0.1.
        assert!(validate_repo_url("http://0x7f000001/owner/repo").is_err());
    }

    #[test]
    fn test_rejects_octal_leading_zero_ipv4() {
        // "0177" is ambiguous: some resolvers (e.g. glibc inet_aton) read a
        // leading-zero octet as octal, so "0177.0.0.1" can resolve to
        // 127.0.0.1 even though a naive decimal parse reads it as 177.
        assert!(validate_repo_url("http://0177.0.0.1/owner/repo").is_err());
    }

    #[test]
    fn test_rejects_shorthand_dotted_quad() {
        // "127.1" is shorthand for 127.0.0.1 in many resolvers.
        assert!(validate_repo_url("http://127.1/owner/repo").is_err());
        assert!(validate_repo_url("http://192.168.1/owner/repo").is_err()); // pii-test-fixture
    }

    #[test]
    fn test_rejects_ipv4_mapped_ipv6() {
        assert!(validate_repo_url("https://[::ffff:127.0.0.1]/owner/repo").is_err());
        assert!(validate_repo_url("https://[::ffff:<internal-ip>]/owner/repo").is_err()); // pii-test-fixture
    }

    #[test]
    fn test_rejects_bare_zero_host() {
        // "0" resolves to 0.0.0.0 on most stacks.
        assert!(validate_repo_url("http://0/owner/repo").is_err());
    }

    #[test]
    fn test_still_accepts_ordinary_hostnames_after_numeric_hardening() {
        // Regression guard: the stricter numeric-host handling must not
        // start rejecting normal-looking public hostnames.
        assert!(validate_repo_url("https://github.com/owner/repo").is_ok());
        assert!(validate_repo_url("https://git.example.com:8443/owner/repo").is_ok());
        assert!(validate_repo_url("https://1a2b.example.com/owner/repo").is_ok());
    }
}
