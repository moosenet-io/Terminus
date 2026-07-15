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

/// The configured public-mirror GitHub org (`TERMINUS_MIRROR_GITHUB_ORG`,
/// default `moosenet-io`).
pub(crate) fn github_org() -> String {
    std::env::var(GITHUB_ORG_ENV)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_GITHUB_ORG.to_string())
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
    let public_name = name_map().get(gitea_repo).cloned().unwrap_or_else(|| gitea_repo.to_string());
    match ops.exists(&org, &public_name).await {
        Ok(true) => Some(format!("https://github.com/{org}/{public_name}.git")),
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

/// Parse `(owner, repo)` from a GitHub remote URL. Handles the HTTPS form
/// `https://github.com/<owner>/<repo>[.git][/]` and the scp-like
/// `<email>:<owner>/<repo>[.git]` form by taking the last two path
/// segments (and stripping a `host:` prefix off the owner in the scp form).
/// Returns `None` when two segments can't be found — a parse failure is a
/// hard "cannot verify", which callers treat as fail-closed (do NOT push).
pub(crate) fn parse_github_owner_repo(remote: &str) -> Option<(String, String)> {
    let s = remote.trim().trim_end_matches('/');
    let s = s.strip_suffix(".git").unwrap_or(s);
    let parts: Vec<&str> = s.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() < 2 {
        return None;
    }
    let repo = parts[parts.len() - 1];
    let owner_raw = parts[parts.len() - 2];
    // scp-like `<email>:owner` → keep only the segment after the last ':'.
    let owner = owner_raw.rsplit(':').next().unwrap_or(owner_raw);
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some((owner.to_string(), repo.to_string()))
}

/// Verify that an override remote URL points to an existing public repo we
/// can see AND is under the configured mirror org (`TERMINUS_MIRROR_GITHUB_ORG`,
/// default `moosenet-io`). `Ok(())` = verified, safe to push. `Err(reason)` =
/// do NOT push — one of: the URL can't be parsed, its owner isn't the
/// configured org (a foreign/wrong target — e.g. `github.com/attacker/x`),
/// `repo_exists` is `false`, or the existence check itself errored (→
/// fail-closed, never assume it exists). This is the sole gate an override
/// must pass, mirroring the guarantee `discover_public_remote_with` already
/// bakes into discovered remotes.
pub(crate) async fn verify_public_remote(ops: &dyn PublicRepoExists, remote: &str) -> Result<(), String> {
    let (owner, repo) =
        parse_github_owner_repo(remote).ok_or_else(|| format!("could not parse owner/repo from remote URL '{remote}'"))?;
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

    // ── parse_github_owner_repo / verify_public_remote ───────────────────────

    #[test]
    fn parse_owner_repo_https_with_and_without_dot_git() {
        assert_eq!(
            parse_github_owner_repo("https://github.com/moosenet-io/Terminus.git"),
            Some(("moosenet-io".to_string(), "Terminus".to_string()))
        );
        assert_eq!(
            parse_github_owner_repo("https://github.com/moosenet-io/Terminus"),
            Some(("moosenet-io".to_string(), "Terminus".to_string()))
        );
        assert_eq!(
            parse_github_owner_repo("https://github.com/moosenet-io/Terminus/"),
            Some(("moosenet-io".to_string(), "Terminus".to_string()))
        );
    }

    #[test]
    fn parse_owner_repo_scp_like_form() {
        assert_eq!(
            parse_github_owner_repo("<email>:moosenet-io/Terminus.git"),
            Some(("moosenet-io".to_string(), "Terminus".to_string()))
        );
    }

    #[test]
    fn parse_owner_repo_rejects_garbage() {
        assert_eq!(parse_github_owner_repo("not-a-url"), None);
        assert_eq!(parse_github_owner_repo(""), None);
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
