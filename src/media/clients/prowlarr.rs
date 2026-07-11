//! Prowlarr client — thin typed wrapper for indexer status/search.
//!
//! Prowlarr aggregates indexers and syncs them to Radarr/Sonarr; it isn't
//! searched directly by end users but its own `/search` endpoint is useful
//! for diagnosing "why can't Radarr/Sonarr find this" without touching the
//! arr apps. MEDIA-01 scaffold only: config + one representative operation.
//!
//! ## Configuration
//! - `PROWLARR_URL`     — base URL, e.g. `http://<prowlarr-host>:9696`
//! - `PROWLARR_API_KEY` — sent as the `X-Api-Key` header

use serde_json::Value;

use crate::error::ToolError;

#[derive(Clone)]
pub struct ProwlarrClient {
    base_url: String,
    api_key: String,
    http: reqwest::Client,
}

impl ProwlarrClient {
    /// Build a client from `PROWLARR_URL` + `PROWLARR_API_KEY`. Never
    /// panics; missing/empty config maps to a clear `NotConfigured` error.
    pub fn from_env() -> Result<Self, ToolError> {
        let base_url = std::env::var("PROWLARR_URL")
            .ok()
            .map(|s| s.trim().trim_end_matches('/').to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::NotConfigured("PROWLARR_URL not set".into()))?;
        let api_key = std::env::var("PROWLARR_API_KEY")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::NotConfigured("PROWLARR_API_KEY not set".into()))?;
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

    /// `GET /api/v1/search?query={query}` — search across configured
    /// indexers. Thin passthrough of the parsed JSON array.
    pub async fn search(&self, query: &str) -> Result<Value, ToolError> {
        let url = format!("{}/api/v1/search", self.base_url);
        let resp = self
            .http
            .get(&url)
            .header("X-Api-Key", &self.api_key)
            .query(&[("query", query)])
            .send()
            .await
            .map_err(|e| ToolError::Http(format!("Prowlarr unavailable: {e}")))?;

        map_response(resp).await
    }

    /// `GET /api/v1/indexer` — configured indexers + their status.
    pub async fn indexers(&self) -> Result<Value, ToolError> {
        let url = format!("{}/api/v1/indexer", self.base_url);
        let resp = self
            .http
            .get(&url)
            .header("X-Api-Key", &self.api_key)
            .send()
            .await
            .map_err(|e| ToolError::Http(format!("Prowlarr unavailable: {e}")))?;

        map_response(resp).await
    }
}

async fn map_response(resp: reqwest::Response) -> Result<Value, ToolError> {
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(ToolError::NotFound("Prowlarr resource not found".into()));
    }
    if status.is_client_error() {
        let body = resp.text().await.unwrap_or_default();
        return Err(ToolError::Http(format!(
            "Prowlarr API error (HTTP {status}): {}",
            body.chars().take(200).collect::<String>()
        )));
    }
    if status.is_server_error() {
        return Err(ToolError::Http(format!("Prowlarr unavailable (HTTP {status})")));
    }

    let text = resp.text().await.map_err(|e| ToolError::Http(e.to_string()))?;
    if text.trim().is_empty() {
        return Ok(serde_json::json!({}));
    }
    serde_json::from_str(&text).map_err(|e| ToolError::Http(format!("Invalid JSON from Prowlarr: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;
    use serde_json::json;
    use serial_test::serial;

    fn test_client(base_url: &str) -> ProwlarrClient {
        ProwlarrClient::new(base_url, "testkey", reqwest::Client::new())
    }

    #[test]
    #[serial]
    fn from_env_missing_url_is_not_configured() {
        let url = std::env::var("PROWLARR_URL").ok();
        let key = std::env::var("PROWLARR_API_KEY").ok();
        std::env::remove_var("PROWLARR_URL");
        std::env::remove_var("PROWLARR_API_KEY");

        let result = ProwlarrClient::from_env();
        assert!(matches!(result, Err(ToolError::NotConfigured(_))));

        if let Some(u) = url { std::env::set_var("PROWLARR_URL", u); }
        if let Some(k) = key { std::env::set_var("PROWLARR_API_KEY", k); }
    }

    #[test]
    #[serial]
    fn from_env_builds_when_both_set() {
        let url = std::env::var("PROWLARR_URL").ok();
        let key = std::env::var("PROWLARR_API_KEY").ok();
        std::env::set_var("PROWLARR_URL", "http://prowlarr.test:9696");
        std::env::set_var("PROWLARR_API_KEY", "abc123");

        let client = ProwlarrClient::from_env().expect("should construct");
        assert_eq!(client.base_url, "http://prowlarr.test:9696");

        if let Some(u) = url { std::env::set_var("PROWLARR_URL", u); } else { std::env::remove_var("PROWLARR_URL"); }
        if let Some(k) = key { std::env::set_var("PROWLARR_API_KEY", k); } else { std::env::remove_var("PROWLARR_API_KEY"); }
    }

    #[tokio::test]
    async fn search_parses_mocked_200() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/api/v1/search").query_param("query", "dune");
            then.status(200).json_body(json!([{ "title": "Dune.2021.2160p" }]));
        });

        let client = test_client(&server.base_url());
        let result = client.search("dune").await.unwrap();
        mock.assert();
        assert_eq!(result[0]["title"], "Dune.2021.2160p");
    }

    #[tokio::test]
    async fn indexers_maps_server_error() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/v1/indexer");
            then.status(502);
        });

        let client = test_client(&server.base_url());
        let result = client.indexers().await;
        assert!(matches!(result, Err(ToolError::Http(_))));
    }

    #[tokio::test]
    async fn unauthorized_maps_to_http_api_error() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/v1/search");
            then.status(401);
        });

        let client = test_client(&server.base_url());
        let result = client.search("x").await;
        assert!(matches!(result, Err(ToolError::Http(_))));
    }
}
