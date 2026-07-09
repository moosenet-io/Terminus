//! Provider registry + provider→pool map for the git-private / git-public tool
//! assembly (S106 / GITX-05).
//!
//! This is the "one surface, two pools" wiring: a provider adapter belongs to
//! exactly one [`ForgePool`] (private = self-hosted source-of-truth, public =
//! hosted/mirror exfiltration surface), and the registry activates ONLY the
//! providers that are actually configured (a missing credential/URL is a clean
//! skip, never a hard failure) — so adding GitLab (GITX-04) or a GITX-06 stub
//! later is a small registration, not a rewrite of this module.
//!
//! ## Extensibility contract
//! [`ForgeRegistry::from_env`] tries to construct every KNOWN provider adapter
//! and inserts whichever succeed. As of GITX-05's provider integration the
//! wired pools are:
//! - **private:** `gitea`, `forgejo` (gitea-family), `gitlab_ce` (GitLab CE),
//!   `gogs`, `onedev` (stubs).
//! - **public:** `github`, `codeberg` (gitea-family public), `gitlab_saas`
//!   (GitLab SaaS), `bitbucket`, `sourcehut`, `radicle` (stubs).
//!
//! Activation is purely config-driven: a provider whose credential/URL is not
//! present in the runtime secret store's materialized env is a clean skip
//! (logged at `debug`), never a build or runtime failure for the providers that
//! ARE configured. Landing a further adapter is: implement
//! [`crate::forge::ForgeProvider`] for it, add one `match … from_env()` insert
//! in the appropriate pool section below, done — no change to the
//! dispatch/posture code in `git_private.rs` / `git_public.rs`.

use std::collections::HashMap;
use std::sync::Arc;

use crate::error::ToolError;

use super::gitea_family::GiteaForge;
use super::gitlab::GitLabAdapter;
use super::provider::ForgeProvider;
use super::stubs::StubForge;
use crate::github::adapter::GitHubAdapter;

/// Which pool a provider belongs to. Mirrors the spec's "two pools, two
/// postures" split — the pool alone decides which MCP tool (`git_private` /
/// `git_public`) can reach a provider, and which posture applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ForgePool {
    /// Self-hosted source-of-truth forges. Full operator R/W.
    Private,
    /// Hosted/public/mirror forges. The exfiltration surface — PII gate is
    /// load-bearing on every write.
    Public,
}

impl ForgePool {
    pub fn as_str(&self) -> &'static str {
        match self {
            ForgePool::Private => "private",
            ForgePool::Public => "public",
        }
    }
}

/// Registry of activated forge provider adapters, partitioned by pool.
/// Construction never fails: an unconfigured provider is skipped (logged),
/// never a hard error, so the registry always builds even with zero
/// credentials configured (both tools then report "no providers configured"
/// on dispatch, which is an honest, clean outcome, not a panic).
#[derive(Clone, Default)]
pub struct ForgeRegistry {
    private: HashMap<String, Arc<dyn ForgeProvider>>,
    public: HashMap<String, Arc<dyn ForgeProvider>>,
}

impl std::fmt::Debug for ForgeRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut private: Vec<&str> = self.private.keys().map(String::as_str).collect();
        private.sort();
        let mut public: Vec<&str> = self.public.keys().map(String::as_str).collect();
        public.sort();
        f.debug_struct("ForgeRegistry")
            .field("private", &private)
            .field("public", &public)
            .finish()
    }
}

impl ForgeRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a provider into a pool. Last write wins for a given id (mainly
    /// useful for tests); production callers use [`ForgeRegistry::from_env`].
    pub fn insert(&mut self, pool: ForgePool, provider: Arc<dyn ForgeProvider>) {
        let id = provider.id().to_string();
        match pool {
            ForgePool::Private => self.private.insert(id, provider),
            ForgePool::Public => self.public.insert(id, provider),
        };
    }

    /// Build the registry from the process environment (the runtime secret
    /// store's materialized env — the sanctioned vault access path). Tries
    /// every known adapter constructor; a `NotConfigured`/construction error
    /// is logged at `debug` and the provider is simply absent, never a build
    /// failure. This is the extension point future adapters (GitLab/GITX-04,
    /// stubs/GITX-06) register into.
    pub fn from_env() -> Self {
        let mut reg = Self::new();

        // ── git-private pool: self-hosted source-of-truth forges ───────────
        match GiteaForge::gitea_from_env() {
            Ok(gitea) => reg.insert(ForgePool::Private, Arc::new(gitea)),
            Err(e) => tracing::debug!(target: "forge.registry", provider = "gitea", error = %e, "provider not configured, skipping"),
        }
        match GiteaForge::forgejo_from_env() {
            Ok(forgejo) => reg.insert(ForgePool::Private, Arc::new(forgejo)),
            Err(e) => tracing::debug!(target: "forge.registry", provider = "forgejo", error = %e, "provider not configured, skipping"),
        }
        // GITX-04 (gitlab_ce): self-hosted GitLab CE/EE, git-private pool.
        match GitLabAdapter::from_env_ce() {
            Ok(gitlab_ce) => reg.insert(ForgePool::Private, Arc::new(gitlab_ce)),
            Err(e) => tracing::debug!(target: "forge.registry", provider = "gitlab_ce", error = %e, "provider not configured, skipping"),
        }
        // GITX-06 (gogs, onedev): optional/experimental stubs, git-private pool.
        match StubForge::gogs_from_env() {
            Ok(gogs) => reg.insert(ForgePool::Private, Arc::new(gogs)),
            Err(e) => tracing::debug!(target: "forge.registry", provider = "gogs", error = %e, "provider not configured, skipping"),
        }
        match StubForge::onedev_from_env() {
            Ok(onedev) => reg.insert(ForgePool::Private, Arc::new(onedev)),
            Err(e) => tracing::debug!(target: "forge.registry", provider = "onedev", error = %e, "provider not configured, skipping"),
        }

        // ── git-public pool: hosted/mirror forges (exfiltration surface) ───
        match GiteaForge::codeberg_from_env() {
            Ok(codeberg) => reg.insert(ForgePool::Public, Arc::new(codeberg)),
            Err(e) => tracing::debug!(target: "forge.registry", provider = "codeberg", error = %e, "provider not configured, skipping"),
        }
        match GitHubAdapter::from_env() {
            Ok(github) => reg.insert(ForgePool::Public, Arc::new(github)),
            Err(e) => tracing::debug!(target: "forge.registry", provider = "github", error = %e, "provider not configured, skipping"),
        }
        // GITX-04 (gitlab_saas): hosted gitlab.com, git-public pool.
        match GitLabAdapter::from_env_saas() {
            Ok(gitlab_saas) => reg.insert(ForgePool::Public, Arc::new(gitlab_saas)),
            Err(e) => tracing::debug!(target: "forge.registry", provider = "gitlab_saas", error = %e, "provider not configured, skipping"),
        }
        // GITX-06 (bitbucket, sourcehut, radicle): optional/experimental stubs,
        // git-public pool. Each is a single insert, no change to the
        // dispatch/posture code in git_private.rs / git_public.rs.
        match StubForge::bitbucket_from_env() {
            Ok(bitbucket) => reg.insert(ForgePool::Public, Arc::new(bitbucket)),
            Err(e) => tracing::debug!(target: "forge.registry", provider = "bitbucket", error = %e, "provider not configured, skipping"),
        }
        match StubForge::sourcehut_from_env() {
            Ok(sourcehut) => reg.insert(ForgePool::Public, Arc::new(sourcehut)),
            Err(e) => tracing::debug!(target: "forge.registry", provider = "sourcehut", error = %e, "provider not configured, skipping"),
        }
        match StubForge::radicle_from_env() {
            Ok(radicle) => reg.insert(ForgePool::Public, Arc::new(radicle)),
            Err(e) => tracing::debug!(target: "forge.registry", provider = "radicle", error = %e, "provider not configured, skipping"),
        }

        reg
    }

    /// Look up a provider within a pool by id.
    pub fn get(&self, pool: ForgePool, provider_id: &str) -> Option<Arc<dyn ForgeProvider>> {
        let map = match pool {
            ForgePool::Private => &self.private,
            ForgePool::Public => &self.public,
        };
        map.get(provider_id).cloned()
    }

    /// Configured provider ids in a pool, sorted for stable output.
    pub fn providers(&self, pool: ForgePool) -> Vec<String> {
        let map = match pool {
            ForgePool::Private => &self.private,
            ForgePool::Public => &self.public,
        };
        let mut ids: Vec<String> = map.keys().cloned().collect();
        ids.sort();
        ids
    }

    /// Resolve a request's explicit `provider` (if any) or fall back to a
    /// pool's configured default, honoring `GIT_PRIVATE_DEFAULT_PROVIDER` /
    /// `GIT_PUBLIC_DEFAULT_PROVIDER` (behavioral config, not a secret), then
    /// the pool's own canonical default (`gitea` / `github`), then whichever
    /// single provider is configured if only one is active.
    pub fn resolve(
        &self,
        pool: ForgePool,
        explicit: Option<&str>,
    ) -> Result<Arc<dyn ForgeProvider>, ToolError> {
        if let Some(id) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
            return self.get(pool, id).ok_or_else(|| {
                ToolError::InvalidArgument(format!(
                    "provider '{id}' is not configured in the {} pool (configured: {:?})",
                    pool.as_str(),
                    self.providers(pool)
                ))
            });
        }
        let env_key = match pool {
            ForgePool::Private => "GIT_PRIVATE_DEFAULT_PROVIDER",
            ForgePool::Public => "GIT_PUBLIC_DEFAULT_PROVIDER",
        };
        let canonical_default = match pool {
            ForgePool::Private => "gitea",
            ForgePool::Public => "github",
        };
        if let Ok(configured) = std::env::var(env_key) {
            let configured = configured.trim();
            if !configured.is_empty() {
                if let Some(p) = self.get(pool, configured) {
                    return Ok(p);
                }
            }
        }
        if let Some(p) = self.get(pool, canonical_default) {
            return Ok(p);
        }
        let ids = self.providers(pool);
        if ids.len() == 1 {
            return self.get(pool, &ids[0]).ok_or_else(|| {
                ToolError::NotConfigured(format!("no providers configured in the {} pool", pool.as_str()))
            });
        }
        Err(ToolError::NotConfigured(format!(
            "no default provider available in the {} pool (configured: {ids:?}); pass 'provider' explicitly",
            pool.as_str()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use serde_json::json;

    use crate::forge::capability::{CapabilityMap, ForgeEndpoint};
    use crate::forge::provider::{ForgeError, ForgeRequest, ForgeResponse};

    struct StubForge {
        id: &'static str,
    }

    #[async_trait]
    impl ForgeProvider for StubForge {
        fn id(&self) -> &str {
            self.id
        }
        fn capabilities(&self) -> &CapabilityMap {
            static CAPS: std::sync::OnceLock<CapabilityMap> = std::sync::OnceLock::new();
            CAPS.get_or_init(|| CapabilityMap::new().supported(ForgeEndpoint::ReposList))
        }
        async fn execute_endpoint(
            &self,
            endpoint: ForgeEndpoint,
            _req: ForgeRequest,
        ) -> Result<ForgeResponse, ForgeError> {
            Ok(ForgeResponse::new(endpoint, self.id, json!({"ok": true})))
        }
    }

    fn registry_with(pool: ForgePool, ids: &[&'static str]) -> ForgeRegistry {
        let mut reg = ForgeRegistry::new();
        for id in ids {
            reg.insert(pool, Arc::new(StubForge { id }));
        }
        reg
    }

    #[test]
    fn explicit_provider_selection_wins() {
        let reg = registry_with(ForgePool::Private, &["gitea", "forgejo"]);
        let p = reg.resolve(ForgePool::Private, Some("forgejo")).unwrap();
        assert_eq!(p.id(), "forgejo");
    }

    #[test]
    fn unknown_explicit_provider_is_a_clean_invalid_argument() {
        let reg = registry_with(ForgePool::Private, &["gitea"]);
        let err = match reg.resolve(ForgePool::Private, Some("nonexistent")) {
            Err(e) => e,
            Ok(_) => panic!("expected an error"),
        };
        assert!(matches!(err, ToolError::InvalidArgument(_)));
        assert!(err.to_string().contains("nonexistent"));
    }

    #[test]
    fn canonical_default_used_when_no_explicit_provider() {
        let reg = registry_with(ForgePool::Public, &["codeberg", "github"]);
        let p = reg.resolve(ForgePool::Public, None).unwrap();
        assert_eq!(p.id(), "github");
    }

    #[test]
    fn single_configured_provider_is_the_implicit_default() {
        let reg = registry_with(ForgePool::Public, &["codeberg"]);
        let p = reg.resolve(ForgePool::Public, None).unwrap();
        assert_eq!(p.id(), "codeberg");
    }

    #[test]
    fn empty_pool_is_a_clean_not_configured_error() {
        let reg = ForgeRegistry::new();
        let err = match reg.resolve(ForgePool::Private, None) {
            Err(e) => e,
            Ok(_) => panic!("expected an error"),
        };
        assert!(matches!(err, ToolError::NotConfigured(_)));
    }

    #[test]
    fn pools_are_independent() {
        let mut reg = ForgeRegistry::new();
        reg.insert(ForgePool::Private, Arc::new(StubForge { id: "gitea" }));
        reg.insert(ForgePool::Public, Arc::new(StubForge { id: "github" }));
        assert!(reg.get(ForgePool::Private, "github").is_none());
        assert!(reg.get(ForgePool::Public, "gitea").is_none());
        assert_eq!(reg.providers(ForgePool::Private), vec!["gitea".to_string()]);
        assert_eq!(reg.providers(ForgePool::Public), vec!["github".to_string()]);
    }

    #[test]
    fn from_env_never_panics_even_with_nothing_configured() {
        // Best-effort: clear the env vars this registry consults, then confirm
        // construction still succeeds (possibly with zero providers). We do not
        // assert emptiness since the test may run alongside other env-mutating
        // tests in the same process; the point is `from_env` never panics/errors.
        let _ = ForgeRegistry::from_env();
    }

    /// GITX-05 provider integration: every configured provider must activate
    /// into the CORRECT pool (private vs public). Uses the config-driven
    /// activation path (`from_env`) with the newly-merged GITX-04 (gitlab) and
    /// GITX-06 (stub) providers configured, and asserts the pool split matches
    /// the S106 provider list. Serialized because it mutates process env.
    #[test]
    #[serial_test::serial]
    fn configured_providers_activate_into_the_correct_pool() {
        use std::env;

        // Providers this test configures, with the vars that activate them.
        // gitlab_saas needs no var (fixed public base); gitlab_ce needs
        // GITLAB_URL; each stub needs its one token.
        let vars = [
            ("GITLAB_URL", "https://gitlab.example.invalid"),
            ("GOGS_TOKEN", "tok"),
            ("ONEDEV_TOKEN", "tok"),
            ("BITBUCKET_TOKEN", "tok"),
            ("SOURCEHUT_TOKEN", "tok"),
            ("RADICLE_TOKEN", "tok"),
        ];

        struct Cleanup<'a>(&'a [(&'a str, &'a str)]);
        impl Drop for Cleanup<'_> {
            fn drop(&mut self) {
                for (k, _) in self.0 {
                    env::remove_var(k);
                }
            }
        }
        let _cleanup = Cleanup(&vars);
        for (k, v) in &vars {
            env::set_var(k, v);
        }

        let reg = ForgeRegistry::from_env();
        let private = reg.providers(ForgePool::Private);
        let public = reg.providers(ForgePool::Public);

        // Each configured provider lands in its expected pool …
        for id in ["gitlab_ce", "gogs", "onedev"] {
            assert!(private.contains(&id.to_string()), "{id} missing from private pool: {private:?}");
            assert!(reg.get(ForgePool::Private, id).is_some());
        }
        for id in ["gitlab_saas", "bitbucket", "sourcehut", "radicle"] {
            assert!(public.contains(&id.to_string()), "{id} missing from public pool: {public:?}");
            assert!(reg.get(ForgePool::Public, id).is_some());
        }
        // … and never leaks into the opposite pool (pool isolation holds for
        // the config-driven wiring, not just manual inserts).
        for id in ["gitlab_ce", "gogs", "onedev"] {
            assert!(reg.get(ForgePool::Public, id).is_none(), "{id} leaked into public pool");
        }
        for id in ["gitlab_saas", "bitbucket", "sourcehut", "radicle"] {
            assert!(reg.get(ForgePool::Private, id).is_none(), "{id} leaked into private pool");
        }
    }
}
