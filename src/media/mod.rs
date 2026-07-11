//! Media domain — sovereign orchestration of the self-hosted media stack
//! (Radarr/Sonarr/Prowlarr/qtor/Plex/<media-service>/TMDb), S94. // pii-test-fixture
//!
//! **MEDIA-01 scope (this item):** establish the domain + one thin, typed,
//! mock-friendly client per service (`crate::media::clients::*`) and their
//! env-backed configuration. No user-facing search/request/recommend tools
//! yet — those land in MEDIA-02 through MEDIA-07. This module registers
//! exactly one internal tool, `media_domain_status`, purely to give the
//! domain something concrete to register/test end-to-end (registry wiring,
//! graceful per-service degradation) before any real tool exists.
//!
//! ## Secrets
//! There is no `vault::manager().get()` accessor in this crate — every
//! client reads its URL/credential via `std::env::var` after the value has
//! been materialized into the process environment at startup (see
//! `crate::secrets_bootstrap::GITEA_PLANE_GITHUB_SECRET_KEYS`, which this
//! item extends with the media service keys). No literal secret values live
//! in this domain's code.
//!
//! ## Graceful degradation
//! Each client's `from_env()` returns `Err(ToolError::NotConfigured(..))`
//! when its env vars are missing/empty — it never panics. A service being
//! unreachable or misconfigured disables only that service's own future
//! tools; the domain always loads, and `media_domain_status` reports
//! per-service configuration state without ever failing itself.

pub mod clients;
pub mod organize;
pub mod request;
pub mod search;

use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::instrument;

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

/// Internal status tool: reports which of the seven media services have
/// their required env vars configured. Deliberately does NOT make any live
/// network call (that would violate the "never touch the running ARR stack"
/// operator constraint during this build) — configuration presence only.
struct MediaDomainStatus;

#[async_trait]
impl RustTool for MediaDomainStatus {
    fn name(&self) -> &str {
        "media_domain_status"
    }

    fn description(&self) -> &str {
        "Report which media-stack services (Radarr, Sonarr, Prowlarr, qtor, Plex, <media-service>, TMDb) are configured in this environment. Configuration presence only -- does not contact any service." // pii-test-fixture
    }

    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    #[instrument(skip(self, _args), fields(tool = "media_domain_status"))]
    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        let radarr = clients::radarr::RadarrClient::from_env().is_ok();
        let sonarr = clients::sonarr::SonarrClient::from_env().is_ok();
        let prowlarr = clients::prowlarr::ProwlarrClient::from_env().is_ok();
        let qtor = clients::qtor::QtorClient::from_env().is_ok();
        let plex = clients::plex::PlexClient::from_env().is_ok();
        let <media-service> = clients::<media-service>::JellyseerrClient::from_env().is_ok(); // pii-test-fixture
        let tmdb = clients::tmdb::TmdbClient::from_env().is_ok();

        let configured_count = [radarr, sonarr, prowlarr, qtor, plex, <media-service>, tmdb] // pii-test-fixture
            .iter()
            .filter(|c| **c)
            .count();

        let out = json!({
            "radarr": radarr,
            "sonarr": sonarr,
            "prowlarr": prowlarr,
            "qtor": qtor,
            "plex": plex,
            "<media-service>": <media-service>, // pii-test-fixture
            "tmdb": tmdb,
            "configured_count": configured_count,
            "total_services": 7,
        });
        Ok(out.to_string())
    }
}

/// Register the media domain's tools. MEDIA-01 registers one internal
/// status tool; MEDIA-02 adds the read/search surface (`media_search`,
/// `media_status`); MEDIA-03 adds the tiered request/download tool; MEDIA-04
/// adds organize + hard-gated destructive ops; MEDIA-05..07 add the
/// remaining recommend/surface tools on top of the clients this item
/// establishes.
pub fn register(registry: &mut ToolRegistry) {
    registry.register_or_replace(Box::new(MediaDomainStatus));
    search::register(registry);
    request::register(registry);
    organize::register(registry);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn clear_all_media_env() {
        for key in [
            "RADARR_URL", "RADARR_API_KEY",
            "SONARR_URL", "SONARR_API_KEY",
            "PROWLARR_URL", "PROWLARR_API_KEY",
            "QTOR_URL", "QTOR_CREDS",
            "PLEX_URL", "PLEX_TOKEN",
            "JELLYSEERR_URL", "JELLYSEERR_API_KEY",
            "TMDB_API_KEY", "TMDB_API_URL",
        ] {
            std::env::remove_var(key);
        }
    }

    #[test]
    fn register_adds_media_domain_status_tool() {
        let mut reg = ToolRegistry::new();
        register(&mut reg);
        assert!(reg.contains("media_domain_status"));
    }

    #[tokio::test]
    #[serial]
    async fn status_reports_all_unconfigured_without_panicking() {
        clear_all_media_env();
        let tool = MediaDomainStatus;
        let result = tool.execute(json!({})).await.unwrap();
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["radarr"], false);
        assert_eq!(parsed["configured_count"], 0);
        assert_eq!(parsed["total_services"], 7);
    }

    #[tokio::test]
    #[serial]
    async fn status_reports_configured_services_independently() {
        clear_all_media_env();
        std::env::set_var("RADARR_URL", "http://radarr.test:7878");
        std::env::set_var("RADARR_API_KEY", "k");
        std::env::set_var("TMDB_API_KEY", "k2");

        let tool = MediaDomainStatus;
        let result = tool.execute(json!({})).await.unwrap();
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["radarr"], true);
        assert_eq!(parsed["tmdb"], true);
        // A service missing its config (e.g. Plex) must independently
        // report unconfigured -- one service being set never masks another.
        assert_eq!(parsed["plex"], false);
        assert_eq!(parsed["configured_count"], 2);

        clear_all_media_env();
    }

    #[test]
    fn tool_metadata_is_valid() {
        let tool = MediaDomainStatus;
        assert_eq!(tool.name(), "media_domain_status");
        assert!(!tool.description().is_empty());
        assert_eq!(tool.parameters()["type"], "object");
    }
}
