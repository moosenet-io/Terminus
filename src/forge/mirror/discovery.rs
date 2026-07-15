//! MIRROR-AUTO — name-based public-remote DISCOVERY.
//!
//! Maps a gitea (internal) repo name to a public GitHub mirror target
//! *iff* that public repo already exists — this existence check IS the
//! opt-out mechanism (S1 in the MIRROR-AUTO spec): an operator who has not
//! created/publicized `https://github.com/<org>/<repo>` simply never gets a
//! repo mirrored, no per-repo YAML edit required. See [`runner`](super::runner)
//! for how this is combined with the blacklist and the explicit
//! `mirror_ready: false` opt-out into the full repo-selection pass.
//!
//! ## Fail-closed on discovery errors (load-bearing)
//! A transient failure of the existence check itself (network error, GitHub
//! outage, bad credential) is treated as "unknown" and mapped to `None` —
//! the SAME outcome as "confirmed absent" — never to "assume it exists and
//! mirror anyway". MIRROR-AUTO's hard safety net is the PII gate (see
//! `super::tools`'s `bootstrap_first_push` / the established-lineage sync
//! path), but discovery failing open would let an unverified guess decide
//! whether an internal repo gets published at all, which is a distinct and
//! avoidable risk this module refuses to take. Every discovery error is
//! logged (`target: "mirror_audit", event = "discovery_error"`) so a
//! persistently-failing check is visible to an operator, not silently
//! swallowed forever.

use std::collections::{HashMap, HashSet};

use crate::error::ToolError;

/// GitHub org the public mirror lives under. Default `moosenet-io`.
pub(crate) const GITHUB_ORG_ENV: &str = "TERMINUS_MIRROR_GITHUB_ORG";
const DEFAULT_GITHUB_ORG: &str = "moosenet-io";

/// Optional override map for the rare repo whose public name doesn't match
/// its internal (gitea) name: `gitea_name1=public_name1,gitea_name2=public_name2`.
pub(crate) const NAME_MAP_ENV: &str = "TERMINUS_MIRROR_NAME_MAP";

/// Repos to NEVER mirror regardless of a public target existing — comma
/// and/or whitespace separated repo names, matched exactly (case-sensitive)
/// against the gitea repo (directory) name.
pub(crate) const BLACKLIST_ENV: &str = "TERMINUS_MIRROR_BLACKLIST";

/// The git remote HOST public mirrors live on. `TERMINUS_MIRROR_GITHUB_HOST`,
/// default `github.com`. This is the host of the git remote we PUSH to (and
/// build discovered remote URLs from) — distinct from the API base
/// (`api.github.com` / `GITHUB_API_BASE`) the credential/existence-check path
/// uses. It is REQUIRED to match on any override remote (see
/// [`verify_public_remote`]): the existence check confirms a repo on
/// `api.github.com`, so the push remote's host must be pinned to the same
/// GitHub host — otherwise an override like
/// `https://evil.example/<org>/<name>.git` would pass a github.com
/// `repo_exists` check yet push internal code to `evil.example`.
pub(crate) const GITHUB_HOST_ENV: &str = "TERMINUS_MIRROR_GITHUB_HOST";
const DEFAULT_GITHUB_HOST: &str = "github.com";

/// The configured public-mirror GitHub org (`TERMINUS_MIRROR_GITHUB_ORG`,
/// default `moosenet-io`).
pub(crate) fn github_org() -> String {
    std::env::var(GITHUB_ORG_ENV)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_GITHUB_ORG.to_string())
}

/// The configured GitHub git-remote host (`TERMINUS_MIRROR_GITHUB_HOST`,
/// default `github.com`). Not an arbitrary host — the default is GitHub and
/// an operator must explicitly opt into a different one.
pub(crate) fn github_host() -> String {
    std::env::var(GITHUB_HOST_ENV)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_GITHUB_HOST.to_string())
}

/// Parse `TERMINUS_MIRROR_NAME_MAP` into a gitea-name → public-name map.
/// Malformed entries (no `=`, empty key/value) are skipped, not errors —
/// this is a best-effort convenience override, not a hard-fail config.
pub(crate) fn name_map() -> HashMap<String, String> {
    let mut map = HashMap::new();
    if let Ok(raw) = std::env::var(NAME_MAP_ENV) {
        for pair in raw.split(',') {
            let pair = pair.trim();
            if pair.is_empty() {
                continue;
            }
            if let Some((k, v)) = pair.split_once('=') {
                let (k, v) = (k.trim(), v.trim());
                if !k.is_empty() && !v.is_empty() {
                    map.insert(k.to_string(), v.to_string());
                }
            }
        }
    }
    map
}

/// Parse `TERMINUS_MIRROR_BLACKLIST` into a set of repo names to always skip.
pub(crate) fn blacklist() -> HashSet<String> {
    let mut set = HashSet::new();
    if let Ok(raw) = std::env::var(BLACKLIST_ENV) {
        for tok in raw.split(|c: char| c == ',' || c.is_whitespace()) {
            let tok = tok.trim();
            if !tok.is_empty() {
                set.insert(tok.to_string());
            }
        }
    }
    set
}

/// The public-repo existence check, as a trait so tests can mock it without
/// ever hitting real GitHub (mirrors `runner::HistoryOps`'s seam pattern).
#[async_trait::async_trait]
pub(crate) trait PublicRepoExists: Send + Sync {
    async fn exists(&self, owner: &str, repo: &str) -> Result<bool, ToolError>;
}

/// Production implementation: routes through `crate::github::repo_exists`,
/// which resolves the GitHub credential via the same `github_token()` path
/// every other mirror-push call uses — never a literal/duplicated token read.
pub(crate) struct RealPublicRepoExists;

#[async_trait::async_trait]
impl PublicRepoExists for RealPublicRepoExists {
    async fn exists(&self, owner: &str, repo: &str) -> Result<bool, ToolError> {
        crate::github::repo_exists(owner, repo).await
    }
}

/// Map `gitea_repo` to its public GitHub remote URL, IF a public repo by
/// that (possibly overridden) name actually exists under the configured org.
/// `None` covers BOTH "confirmed absent" (the natural opt-out — nothing to
/// do) AND "the check itself failed" (fail-closed — see module doc); callers
/// that need to distinguish the two should watch the `mirror_audit` log, not
/// branch on this return value, which is deliberately a plain `Option` to
/// keep the opt-out/error-fail-closed cases indistinguishable to selection
/// logic (both mean "don't mirror this tick").
pub(crate) async fn discover_public_remote_with(ops: &dyn PublicRepoExists, gitea_repo: &str) -> Option<String> {
    let org = github_org();
    let host = github_host();
    let public_name = name_map().get(gitea_repo).cloned().unwrap_or_else(|| gitea_repo.to_string());
    match ops.exists(&org, &public_name).await {
        Ok(true) => Some(format!("https://{host}/{org}/{public_name}.git")),
        Ok(false) => None,
        Err(e) => {
            tracing::warn!(
                target: "mirror_audit",
                event = "discovery_error",
                repo = %gitea_repo,
                org = %org,
                public_name = %public_name,
                error = %e,
                "MIRROR-AUTO: public-remote existence check failed — treating as unknown, not mirroring this tick"
            );
            None
        }
    }
}

/// [`discover_public_remote_with`] wired to the real GitHub existence check —
/// the entry point `runner`'s production discovery pass uses.
pub(crate) async fn discover_public_remote(gitea_repo: &str) -> Option<String> {
    discover_public_remote_with(&RealPublicRepoExists, gitea_repo).await
}

// ── Override-remote verification (closes the two verification-bypass gaps) ─────
//
// The safety invariant of MIRROR-AUTO's auto-push is: we ONLY ever push to a
// verified, org-matched public repo that `github::repo_exists` confirms.
// `discover_public_remote_with` already enforces that for DISCOVERED remotes
// (it only returns `Some` when the repo exists, and always builds the URL
// under the configured org). But an operator-supplied OVERRIDE remote
// (call-level `github_remote`, or `TERMINUS_MIRROR_REMOTE[_<REPO>]`), and the
// explicit-`repo` path, must be run through the SAME check before any push —
// otherwise an override could redirect an auto-baseline at an
// unverified/wrong/foreign remote. [`verify_public_remote`] is that check.

/// Parse `(host, owner, repo)` from a git remote URL. Handles the HTTPS form
/// `https://<host>[:port]/<owner>/<repo>[.git][/]` and the scp-like
/// `[user@]<host>:<owner>/<repo>[.git]` form. Crucially it extracts the TRUE
/// HOST (not just the trailing path segments) so callers can pin it — an
/// override like `https://evil.example/moosenet-io/Terminus.git` parses with
/// `host = "evil.example"`, letting [`verify_public_remote`] reject it even
/// though its owner/repo would pass a github.com existence check.
///
/// ## Fail-closed RFC-3986 authority parsing (userinfo hijack defense)
/// The HTTPS authority is isolated as the substring between `://` and the
/// first `/`, `?`, or `#`, and the whole remote is REJECTED if that authority
/// contains an `@` — i.e. any userinfo. This defeats the
/// `https://github.com:<email>/<org>/<repo>.git` hijack, where per
/// URL semantics the real host is `evil.example` and `github.com:443` is
/// userinfo (a naive `split(':')` would wrongly read the host as
/// `github.com`). Userinfo is never legitimate for our mirror remotes anyway —
/// the GitHub credential is injected via `GIT_ASKPASS`, never embedded in the
/// URL — so its mere presence is a hard reject. This is the same
/// isolate-the-authority-before-splitting lesson as the DSN guard.
///
/// For the scp form the host is the token between an optional trailing-most
/// `user@` and the `:` (scp legitimately has `<email>:` — the `@` is
/// separating userinfo there, so we take the piece after the LAST `@`; a
/// spoof like `<email>:o/r` correctly yields `evil.example`).
///
/// Returns `None` when the host or the two path segments can't be found, or
/// when https userinfo is present — every parse failure is a hard "cannot
/// verify", which callers treat as fail-closed (do NOT push).
pub(crate) fn parse_github_remote(remote: &str) -> Option<(String, String, String)> {
    let s = remote.trim().trim_end_matches('/');
    let s = s.strip_suffix(".git").unwrap_or(s);

    let (host, path): (&str, &str) = if let Some(rest) = s.strip_prefix("https://").or_else(|| s.strip_prefix("http://")) {
        // rest = authority[/path…]. Isolate the RFC-3986 authority: everything
        // up to the first '/', '?', or '#'. A remote with no path can't name an
        // owner/repo, so a missing delimiter is a reject.
        let authority_end = rest.find(|c| c == '/' || c == '?' || c == '#')?;
        let authority = &rest[..authority_end];
        let path = &rest[authority_end..];
        // FAIL-CLOSED: userinfo (anything before an '@' in the authority) is
        // never legitimate here — reject the whole remote rather than trust a
        // host parsed around it. This blocks the `host:<email>`
        // userinfo hijack.
        if authority.contains('@') {
            return None;
        }
        // host = authority minus an optional :port.
        let h = authority.split(':').next().unwrap_or(authority);
        (h, path)
    } else if let Some((userhost, p)) = s.split_once(':') {
        // scp-like: [user@]host:owner/repo (the ':' here is the path
        // separator, NOT a port). host = the part after the LAST `@`.
        let h = userhost.rsplit('@').next().unwrap_or(userhost);
        (h, p)
    } else {
        // No scheme and no ':' — not a remote URL we can safely attribute a
        // host to. Reject rather than guess (fail-closed).
        return None;
    };

    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() < 2 {
        return None;
    }
    let owner = parts[parts.len() - 2];
    let repo = parts[parts.len() - 1];
    if host.is_empty() || owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some((host.to_string(), owner.to_string(), repo.to_string()))
}

/// Verify that an override remote URL is a safe push target: its HOST is the
/// configured GitHub host (`TERMINUS_MIRROR_GITHUB_HOST`, default
/// `github.com`), its owner is the configured mirror org
/// (`TERMINUS_MIRROR_GITHUB_ORG`, default `moosenet-io`), AND `repo_exists`
/// confirms it. `Ok(())` = verified, safe to push. `Err(reason)` = do NOT
/// push — one of: the URL can't be parsed, the host is NOT the GitHub host
/// (e.g. `evil.example` / a gitea host — the verification-target-≠-push-target
/// bypass), the owner isn't the configured org (e.g. `github.com/attacker/x`),
/// `repo_exists` is `false`, or the existence check itself errored (→
/// fail-closed). Host + org + existence are ALL checked on the EXACT remote
/// that would be pushed, so the invariant "we only ever push to
/// `<github-host>/<org>/<name>` proven to exist" holds. This mirrors the
/// guarantee `discover_public_remote_with` bakes into discovered remotes
/// (which are built from the same host + org).
pub(crate) async fn verify_public_remote(ops: &dyn PublicRepoExists, remote: &str) -> Result<(), String> {
    let (host, owner, repo) = parse_github_remote(remote)
        .ok_or_else(|| format!("could not parse host/owner/repo from remote URL '{remote}'"))?;
    let expected_host = github_host();
    if !host.eq_ignore_ascii_case(&expected_host) {
        return Err(format!(
            "remote host '{host}' is not the configured GitHub host '{expected_host}' — refusing to push \
             to a non-GitHub host (the existence check verifies a repo on GitHub, so the push target's \
             host must match; set TERMINUS_MIRROR_GITHUB_HOST if this host is intended)"
        ));
    }
    let org = github_org();
    if owner != org {
        return Err(format!(
            "remote owner '{owner}' is not the configured mirror org '{org}' — refusing to push to a \
             non-org target (set TERMINUS_MIRROR_GITHUB_ORG if this org is intended)"
        ));
    }
    match ops.exists(&owner, &repo).await {
        Ok(true) => Ok(()),
        Ok(false) => Err(format!("remote '{remote}' does not point to an existing public repo (repo_exists=false)")),
        Err(e) => Err(format!("could not verify remote '{remote}' (repo_exists check errored → fail-closed): {e}")),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct StubExists {
        result: Option<Result<bool, ToolError>>,
        calls: AtomicUsize,
        seen_owner: std::sync::Mutex<Option<String>>,
        seen_repo: std::sync::Mutex<Option<String>>,
    }

    impl StubExists {
        fn ok(v: bool) -> Self {
            Self { result: Some(Ok(v)), calls: AtomicUsize::new(0), seen_owner: Default::default(), seen_repo: Default::default() }
        }
        fn err(msg: &str) -> Self {
            Self {
                result: Some(Err(ToolError::Http(msg.to_string()))),
                calls: AtomicUsize::new(0),
                seen_owner: Default::default(),
                seen_repo: Default::default(),
            }
        }
    }

    #[async_trait::async_trait]
    impl PublicRepoExists for StubExists {
        async fn exists(&self, owner: &str, repo: &str) -> Result<bool, ToolError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            *self.seen_owner.lock().unwrap() = Some(owner.to_string());
            *self.seen_repo.lock().unwrap() = Some(repo.to_string());
            match &self.result {
                Some(Ok(v)) => Ok(*v),
                Some(Err(ToolError::Http(m))) => Err(ToolError::Http(m.clone())),
                Some(Err(_)) => Err(ToolError::Execution("stub".into())),
                None => panic!("exists stub not set"),
            }
        }
    }

    #[tokio::test]
    async fn some_only_when_public_repo_exists() {
        let ops = StubExists::ok(true);
        let remote = discover_public_remote_with(&ops, "Terminus").await;
        assert_eq!(remote.as_deref(), Some("https://github.com/moosenet-io/Terminus.git"));
        assert_eq!(ops.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn none_when_public_repo_absent() {
        let ops = StubExists::ok(false);
        let remote = discover_public_remote_with(&ops, "SecretInternalThing").await;
        assert_eq!(remote, None);
    }

    #[tokio::test]
    async fn none_and_no_crash_when_existence_check_errors() {
        let ops = StubExists::err("network timeout");
        let remote = discover_public_remote_with(&ops, "Terminus").await;
        assert_eq!(remote, None, "a broken existence check must fail closed (skip), never fail open");
    }

    #[tokio::test]
    #[serial]
    async fn org_default_is_moosenet_io() {
        let had = std::env::var(GITHUB_ORG_ENV).ok();
        // SAFETY (test-only): serialized via #[serial] against every other
        // test in this file that touches GITHUB_ORG_ENV/NAME_MAP_ENV/BLACKLIST_ENV.
        unsafe {
            std::env::remove_var(GITHUB_ORG_ENV);
        }
        let ops = StubExists::ok(true);
        let remote = discover_public_remote_with(&ops, "Chord").await;
        unsafe {
            if let Some(v) = had {
                std::env::set_var(GITHUB_ORG_ENV, v);
            }
        }
        assert_eq!(remote.as_deref(), Some("https://github.com/moosenet-io/Chord.git"));
    }

    #[tokio::test]
    #[serial]
    async fn org_is_configurable() {
        let had = std::env::var(GITHUB_ORG_ENV).ok();
        // SAFETY (test-only): serialized via #[serial].
        unsafe {
            std::env::set_var(GITHUB_ORG_ENV, "some-other-org");
        }
        let ops = StubExists::ok(true);
        let remote = discover_public_remote_with(&ops, "Chord").await;
        unsafe {
            match had {
                Some(v) => std::env::set_var(GITHUB_ORG_ENV, v),
                None => std::env::remove_var(GITHUB_ORG_ENV),
            }
        }
        assert_eq!(remote.as_deref(), Some("https://github.com/some-other-org/Chord.git"));
    }

    #[tokio::test]
    #[serial]
    async fn name_map_overrides_public_name() {
        let had = std::env::var(NAME_MAP_ENV).ok();
        // SAFETY (test-only): serialized via #[serial].
        unsafe {
            std::env::set_var(NAME_MAP_ENV, "internal-name=public-name, other=stuff");
        }
        let ops = StubExists::ok(true);
        let remote = discover_public_remote_with(&ops, "internal-name").await;
        let seen_repo = ops.seen_repo.lock().unwrap().clone();
        unsafe {
            match had {
                Some(v) => std::env::set_var(NAME_MAP_ENV, v),
                None => std::env::remove_var(NAME_MAP_ENV),
            }
        }
        assert_eq!(remote.as_deref(), Some("https://github.com/moosenet-io/public-name.git"));
        assert_eq!(seen_repo.as_deref(), Some("public-name"));
    }

    // ── parse_github_remote / verify_public_remote ───────────────────────────

    #[test]
    fn parse_remote_https_extracts_host_owner_repo() {
        assert_eq!(
            parse_github_remote("https://github.com/moosenet-io/Terminus.git"),
            Some(("github.com".to_string(), "moosenet-io".to_string(), "Terminus".to_string()))
        );
        assert_eq!(
            parse_github_remote("https://github.com/moosenet-io/Terminus"),
            Some(("github.com".to_string(), "moosenet-io".to_string(), "Terminus".to_string()))
        );
        assert_eq!(
            parse_github_remote("https://github.com/moosenet-io/Terminus/"),
            Some(("github.com".to_string(), "moosenet-io".to_string(), "Terminus".to_string()))
        );
        // Host with a port is still attributed correctly.
        assert_eq!(
            parse_github_remote("https://github.com:443/moosenet-io/Terminus.git"),
            Some(("github.com".to_string(), "moosenet-io".to_string(), "Terminus".to_string()))
        );
    }

    #[test]
    fn parse_remote_scp_like_extracts_host_owner_repo() {
        assert_eq!(
            parse_github_remote("<email>:moosenet-io/Terminus.git"),
            Some(("github.com".to_string(), "moosenet-io".to_string(), "Terminus".to_string()))
        );
    }

    #[test]
    fn parse_remote_captures_non_github_host() {
        // The load-bearing case: a foreign host is captured as such (NOT
        // silently reduced to owner/repo), so verify can reject it.
        assert_eq!(
            parse_github_remote("https://evil.example/moosenet-io/Terminus.git"),
            Some(("evil.example".to_string(), "moosenet-io".to_string(), "Terminus".to_string()))
        );
        assert_eq!(
            parse_github_remote("<email>:moosenet-io/Terminus.git"),
            Some(("evil.example".to_string(), "moosenet-io".to_string(), "Terminus".to_string()))
        );
    }

    #[test]
    fn parse_remote_rejects_https_userinfo_hijack() {
        // RFC-3986: the real host is `evil.example`; `github.com:443` is
        // USERINFO (before the '@'). A naive split(':') would read the host as
        // `github.com`. Userinfo is never legitimate for our mirror remotes →
        // reject the whole remote (fail-closed), so verify can't be fooled.
        assert_eq!(parse_github_remote("https://github.com:<email>/moosenet-io/Terminus.git"), None);
        // …also without a port, and with a plain userinfo token.
        assert_eq!(parse_github_remote("https://<email>/moosenet-io/Terminus.git"), None);
        assert_eq!(parse_github_remote("https://user:<email>/moosenet-io/Terminus.git"), None);
    }

    #[test]
    fn parse_remote_scp_spoof_takes_host_after_last_at() {
        // scp `<email>:o/r` → the true host is `evil.example`
        // (after the LAST '@'), NOT github.com — captured as such so verify
        // rejects it on the host mismatch.
        assert_eq!(
            parse_github_remote("<email>:moosenet-io/Terminus.git"),
            Some(("evil.example".to_string(), "moosenet-io".to_string(), "Terminus".to_string()))
        );
    }

    #[test]
    fn parse_remote_rejects_garbage() {
        assert_eq!(parse_github_remote("not-a-url"), None);
        assert_eq!(parse_github_remote(""), None);
        // No scheme and no ':' → can't attribute a host → reject.
        assert_eq!(parse_github_remote("github.com/moosenet-io/Terminus"), None);
    }

    #[tokio::test]
    #[serial]
    async fn verify_public_remote_ok_when_org_matches_and_exists() {
        let had = std::env::var(GITHUB_ORG_ENV).ok();
        // SAFETY (test-only): #[serial].
        unsafe {
            std::env::remove_var(GITHUB_ORG_ENV);
        }
        let ops = StubExists::ok(true);
        let res = verify_public_remote(&ops, "https://github.com/moosenet-io/Terminus.git").await;
        unsafe {
            if let Some(v) = had {
                std::env::set_var(GITHUB_ORG_ENV, v);
            }
        }
        assert!(res.is_ok(), "org-matched + existing remote must verify: {res:?}");
    }

    #[tokio::test]
    #[serial]
    async fn verify_public_remote_rejects_nonexistent_repo() {
        let had = std::env::var(GITHUB_ORG_ENV).ok();
        // SAFETY (test-only): #[serial].
        unsafe {
            std::env::remove_var(GITHUB_ORG_ENV);
        }
        let ops = StubExists::ok(false);
        let res = verify_public_remote(&ops, "https://github.com/moosenet-io/Ghost.git").await;
        unsafe {
            if let Some(v) = had {
                std::env::set_var(GITHUB_ORG_ENV, v);
            }
        }
        assert!(res.is_err(), "a remote whose repo_exists=false must be rejected");
        assert!(res.unwrap_err().contains("repo_exists=false"));
    }

    #[tokio::test]
    #[serial]
    async fn verify_public_remote_rejects_foreign_org_without_even_checking_existence() {
        let had = std::env::var(GITHUB_ORG_ENV).ok();
        // SAFETY (test-only): #[serial].
        unsafe {
            std::env::remove_var(GITHUB_ORG_ENV);
        }
        // exists() would return true, but the owner is NOT the configured org
        // — must be rejected before the (irrelevant) existence result matters.
        let ops = StubExists::ok(true);
        let res = verify_public_remote(&ops, "https://github.com/attacker/moosenet-secret.git").await;
        unsafe {
            if let Some(v) = had {
                std::env::set_var(GITHUB_ORG_ENV, v);
            }
        }
        assert!(res.is_err(), "a foreign-org remote must be rejected even if it exists");
        assert!(res.unwrap_err().contains("not the configured mirror org"));
    }

    /// THE codex/opus hole: an override on a NON-GitHub host whose owner/repo
    /// WOULD pass a github.com existence check must be REJECTED on the host
    /// mismatch — the push target's host must equal the GitHub host the
    /// existence check verifies against, or internal code would be pushed to
    /// the attacker host. Covers both https and scp forms; stub says exists=true
    /// to prove the host check fires first.
    #[tokio::test]
    #[serial]
    async fn verify_public_remote_rejects_non_github_host_even_though_owner_repo_exist() {
        let had_org = std::env::var(GITHUB_ORG_ENV).ok();
        let had_host = std::env::var(GITHUB_HOST_ENV).ok();
        // SAFETY (test-only): #[serial]. Default org=moosenet-io, host=github.com.
        unsafe {
            std::env::remove_var(GITHUB_ORG_ENV);
            std::env::remove_var(GITHUB_HOST_ENV);
        }
        let ops = StubExists::ok(true);
        let https = verify_public_remote(&ops, "https://evil.example/moosenet-io/Terminus.git").await;
        let scp = verify_public_remote(&ops, "<email>:moosenet-io/Terminus.git").await;
        unsafe {
            match had_org {
                Some(v) => std::env::set_var(GITHUB_ORG_ENV, v),
                None => std::env::remove_var(GITHUB_ORG_ENV),
            }
            match had_host {
                Some(v) => std::env::set_var(GITHUB_HOST_ENV, v),
                None => std::env::remove_var(GITHUB_HOST_ENV),
            }
        }
        assert!(https.is_err(), "https on a non-GitHub host must be rejected");
        assert!(https.unwrap_err().contains("not the configured GitHub host"));
        assert!(scp.is_err(), "scp on a non-GitHub host must be rejected");
        assert!(scp.unwrap_err().contains("not the configured GitHub host"));
    }

    /// THE codex RFC-3986 userinfo-hijack hole:
    /// `https://github.com:<email>/moosenet-io/Terminus.git` — the
    /// REAL host is `evil.example` (github.com:443 is userinfo). The stub says
    /// moosenet-io/Terminus exists, so ONLY correct authority parsing (reject
    /// on userinfo) prevents a Verified→push to evil.example. Must be rejected.
    #[tokio::test]
    #[serial]
    async fn verify_public_remote_rejects_userinfo_hijack_even_though_owner_repo_exist() {
        let had_org = std::env::var(GITHUB_ORG_ENV).ok();
        let had_host = std::env::var(GITHUB_HOST_ENV).ok();
        // SAFETY (test-only): #[serial]. Default org=moosenet-io, host=github.com.
        unsafe {
            std::env::remove_var(GITHUB_ORG_ENV);
            std::env::remove_var(GITHUB_HOST_ENV);
        }
        let ops = StubExists::ok(true);
        let res =
            verify_public_remote(&ops, "https://github.com:<email>/moosenet-io/Terminus.git").await;
        // scp analogue: true host after the last '@' is evil.example.
        let scp = verify_public_remote(&ops, "<email>:moosenet-io/Terminus.git").await;
        unsafe {
            match had_org {
                Some(v) => std::env::set_var(GITHUB_ORG_ENV, v),
                None => std::env::remove_var(GITHUB_ORG_ENV),
            }
            match had_host {
                Some(v) => std::env::set_var(GITHUB_HOST_ENV, v),
                None => std::env::remove_var(GITHUB_HOST_ENV),
            }
        }
        assert!(res.is_err(), "an https userinfo-hijack remote must be rejected, never pushed");
        // Parse rejects userinfo outright → "could not parse host/owner/repo".
        assert!(res.unwrap_err().contains("could not parse"), "userinfo hijack must fail parsing (fail-closed)");
        assert!(scp.is_err(), "an scp host-spoof remote must be rejected (host is after the last '@')");
        assert!(scp.unwrap_err().contains("not the configured GitHub host"));
    }

    /// A normal, credential-free remote (no userinfo) still verifies on both
    /// forms — the hijack defense must not break the happy path.
    #[tokio::test]
    #[serial]
    async fn verify_public_remote_ok_for_normal_https_and_scp() {
        let had_org = std::env::var(GITHUB_ORG_ENV).ok();
        let had_host = std::env::var(GITHUB_HOST_ENV).ok();
        // SAFETY (test-only): #[serial].
        unsafe {
            std::env::remove_var(GITHUB_ORG_ENV);
            std::env::remove_var(GITHUB_HOST_ENV);
        }
        let ops = StubExists::ok(true);
        let https = verify_public_remote(&ops, "https://github.com/moosenet-io/Muse.git").await;
        let scp = verify_public_remote(&ops, "<email>:moosenet-io/Muse.git").await;
        unsafe {
            match had_org {
                Some(v) => std::env::set_var(GITHUB_ORG_ENV, v),
                None => std::env::remove_var(GITHUB_ORG_ENV),
            }
            match had_host {
                Some(v) => std::env::set_var(GITHUB_HOST_ENV, v),
                None => std::env::remove_var(GITHUB_HOST_ENV),
            }
        }
        assert!(https.is_ok(), "normal https remote must verify: {https:?}");
        assert!(scp.is_ok(), "normal scp remote must verify: {scp:?}");
    }

    #[tokio::test]
    #[serial]
    async fn verify_public_remote_fails_closed_on_check_error() {
        let had = std::env::var(GITHUB_ORG_ENV).ok();
        // SAFETY (test-only): #[serial].
        unsafe {
            std::env::remove_var(GITHUB_ORG_ENV);
        }
        let ops = StubExists::err("rate limited");
        let res = verify_public_remote(&ops, "https://github.com/moosenet-io/Terminus.git").await;
        unsafe {
            if let Some(v) = had {
                std::env::set_var(GITHUB_ORG_ENV, v);
            }
        }
        assert!(res.is_err(), "a check error must fail closed (reject), never assume existence");
        assert!(res.unwrap_err().contains("fail-closed"));
    }

    #[tokio::test]
    async fn verify_public_remote_rejects_unparseable_url() {
        let ops = StubExists::ok(true);
        let res = verify_public_remote(&ops, "not-a-real-url").await;
        assert!(res.is_err(), "an unparseable remote can't be verified → reject");
        assert!(res.unwrap_err().contains("could not parse"));
    }

    #[test]
    #[serial]
    fn blacklist_parses_comma_and_whitespace_separated() {
        let had = std::env::var(BLACKLIST_ENV).ok();
        // SAFETY (test-only): serialized via #[serial].
        unsafe {
            std::env::set_var(BLACKLIST_ENV, "Secret1, Secret2  Secret3,,  ");
        }
        let bl = blacklist();
        unsafe {
            match had {
                Some(v) => std::env::set_var(BLACKLIST_ENV, v),
                None => std::env::remove_var(BLACKLIST_ENV),
            }
        }
        assert!(bl.contains("Secret1"));
        assert!(bl.contains("Secret2"));
        assert!(bl.contains("Secret3"));
        assert_eq!(bl.len(), 3);
    }

    #[test]
    #[serial]
    fn blacklist_empty_when_unset() {
        let had = std::env::var(BLACKLIST_ENV).ok();
        // SAFETY (test-only): serialized via #[serial].
        unsafe {
            std::env::remove_var(BLACKLIST_ENV);
        }
        let bl = blacklist();
        unsafe {
            if let Some(v) = had {
                std::env::set_var(BLACKLIST_ENV, v);
            }
        }
        assert!(bl.is_empty());
    }
}
