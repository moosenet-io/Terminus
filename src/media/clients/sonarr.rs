//! Sonarr client — thin typed wrapper for TV series search/status lookups.
//!
//! Sonarr is Radarr's TV counterpart: it owns the series library and drives
//! season/episode acquisition through the same indexer + download-client
//! chain. Mirrors [`crate::media::clients::radarr::RadarrClient`]'s shape —
//! see that module's doc comment for the rationale (thin, one representative
//! lookup operation + library listing, MEDIA-01 scaffold only).
//!
//! ## Configuration
//! - `SONARR_URL`     — base URL, e.g. `http://<sonarr-host>:8989`
//! - `SONARR_API_KEY` — sent as the `X-Api-Key` header

use serde_json::Value;

use crate::error::ToolError;

#[derive(Clone)]
pub struct SonarrClient {
    base_url: String,
    api_key: String,
    http: reqwest::Client,
}

impl SonarrClient {
    /// Build a client from `SONARR_URL` + `SONARR_API_KEY`. Never panics;
    /// missing/empty config maps to a clear `NotConfigured` error.
    pub fn from_env() -> Result<Self, ToolError> {
        let base_url = std::env::var("SONARR_URL")
            .ok()
            .map(|s| s.trim().trim_end_matches('/').to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::NotConfigured("SONARR_URL not set".into()))?;
        let api_key = std::env::var("SONARR_API_KEY")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::NotConfigured("SONARR_API_KEY not set".into()))?;
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .map_err(|e| ToolError::Http(format!("Failed to build HTTP client: {e}")))?;
        Ok(Self { base_url, api_key, http })
    }

    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>, http: reqwest::Client) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            http,
        }
    }

    /// `GET /api/v3/series/lookup?term={term}` — resolve a free-text title to
    /// candidate series. Thin passthrough; response shaping is MEDIA-02's job.
    pub async fn lookup_series(&self, term: &str) -> Result<Value, ToolError> {
        let url = format!("{}/api/v3/series/lookup", self.base_url);
        let resp = self
            .http
            .get(&url)
            .header("X-Api-Key", &self.api_key)
            .query(&[("term", term)])
            .send()
            .await
            .map_err(|e| ToolError::Http(format!("Sonarr unavailable: {e}")))?;

        map_response(resp).await
    }

    /// `GET /api/v3/series` — the current library.
    pub async fn library(&self) -> Result<Value, ToolError> {
        let url = format!("{}/api/v3/series", self.base_url);
        let resp = self
            .http
            .get(&url)
            .header("X-Api-Key", &self.api_key)
            .send()
            .await
            .map_err(|e| ToolError::Http(format!("Sonarr unavailable: {e}")))?;

        map_response(resp).await
    }
}

async fn map_response(resp: reqwest::Response) -> Result<Value, ToolError> {
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(ToolError::NotFound("Sonarr resource not found".into()));
    }
    if status.is_client_error() {
        let body = resp.text().await.unwrap_or_default();
        return Err(ToolError::Http(format!(
            "Sonarr API error (HTTP {status}): {}",
            body.chars().take(200).collect::<String>()
        )));
    }
    if status.is_server_error() {
        return Err(ToolError::Http(format!("Sonarr unavailable (HTTP {status})")));
    }

    let text = resp.text().await.map_err(|e| ToolError::Http(e.to_string()))?;
    if text.trim().is_empty() {
        return Ok(serde_json::json!({}));
    }
    serde_json::from_str(&text).map_err(|e| ToolError::Http(format!("Invalid JSON from Sonarr: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;
    use serde_json::json;
    use serial_test::serial;

    fn test_client(base_url: &str) -> SonarrClient {
        SonarrClient::new(base_url, "testkey", reqwest::Client::new())
    }

    #[test]
    #[serial]
    fn from_env_missing_url_is_not_configured() {
        let url = std::env::var("SONARR_URL").ok();
        let key = std::env::var("SONARR_API_KEY").ok();
        std::env::remove_var("SONARR_URL");
        std::env::remove_var("SONARR_API_KEY");

        let result = SonarrClient::from_env();
        assert!(matches!(result, Err(ToolError::NotConfigured(_))));

        if let Some(u) = url { std::env::set_var("SONARR_URL", u); }
        if let Some(k) = key { std::env::set_var("SONARR_API_KEY", k); }
    }

    #[test]
    #[serial]
    fn from_env_missing_key_is_not_configured() {
        let url = std::env::var("SONARR_URL").ok();
        let key = std::env::var("SONARR_API_KEY").ok();
        std::env::set_var("SONARR_URL", "http://sonarr.test:8989/");
        std::env::remove_var("SONARR_API_KEY");

        let result = SonarrClient::from_env();
        assert!(matches!(result, Err(ToolError::NotConfigured(_))));

        if let Some(u) = url { std::env::set_var("SONARR_URL", u); } else { std::env::remove_var("SONARR_URL"); }
        if let Some(k) = key { std::env::set_var("SONARR_API_KEY", k); }
    }

    #[test]
    #[serial]
    fn from_env_builds_when_both_set() {
        let url = std::env::var("SONARR_URL").ok();
        let key = std::env::var("SONARR_API_KEY").ok();
        std::env::set_var("SONARR_URL", "http://sonarr.test:8989/");
        std::env::set_var("SONARR_API_KEY", "abc123");

        let client = SonarrClient::from_env().expect("should construct");
        assert_eq!(client.base_url, "http://sonarr.test:8989");

        if let Some(u) = url { std::env::set_var("SONARR_URL", u); } else { std::env::remove_var("SONARR_URL"); }
        if let Some(k) = key { std::env::set_var("SONARR_API_KEY", k); } else { std::env::remove_var("SONARR_API_KEY"); }
    }

    #[tokio::test]
    async fn lookup_series_parses_mocked_200() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/api/v3/series/lookup").query_param("term", "foundation");
            then.status(200).json_body(json!([{ "title": "Foundation", "tvdbId": 358903 }]));
        });

        let client = test_client(&server.base_url());
        let result = client.lookup_series("foundation").await.unwrap();
        mock.assert();
        assert_eq!(result[0]["title"], "Foundation");
    }

    #[tokio::test]
    async fn library_parses_mocked_200() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/api/v3/series");
            then.status(200).json_body(json!([{ "title": "Severance", "monitored": true }]));
        });

        let client = test_client(&server.base_url());
        let result = client.library().await.unwrap();
        mock.assert();
        assert_eq!(result[0]["title"], "Severance");
    }

    #[tokio::test]
    async fn not_found_maps_to_not_found_error() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/v3/series/lookup");
            then.status(404);
        });

        let client = test_client(&server.base_url());
        let result = client.lookup_series("nothing").await;
        assert!(matches!(result, Err(ToolError::NotFound(_))));
    }

    #[tokio::test]
    async fn server_error_maps_to_http_unavailable() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/v3/series");
            then.status(503);
        });

        let client = test_client(&server.base_url());
        let result = client.library().await;
        assert!(matches!(result, Err(ToolError::Http(_))));
    }
}
