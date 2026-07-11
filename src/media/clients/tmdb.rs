//! TMDb client — thin typed wrapper for title resolution.
//!
//! TMDb (The Movie Database) is the one genuinely external call in this
//! domain (per the spec's pre-flight: "All internal/LAN except TMDb lookup —
//! the one external call — title→ID resolution; no PII sent"). It resolves
//! fuzzy natural-language titles to real TMDb IDs that Radarr/Sonarr/
//! <media-service> all key off of. MEDIA-01 scaffold only: config + one // pii-test-fixture
//! representative operation (`search_multi`, matching TMDb's own combined
//! movie+TV+person search endpoint).
//!
//! ## Configuration
//! - `TMDB_API_KEY` — TMDb v3 API key, sent as the `api_key` query param
//! - `TMDB_API_URL` — optional base URL override (default
//!   `https://api.themoviedb.org/3`); not a secret, just an escape hatch
//!   for tests/self-hosted proxies, same pattern as `OPENWEATHER_API_URL`
//!   in [`crate::weather`].

use serde_json::Value;

use crate::error::ToolError;

/// TMDb's public API host. Not a secret or internal infra value — this is
/// the one deliberately external call in the media domain (title resolution
/// only, no PII sent).
const DEFAULT_BASE_URL: &str = "https://api.themoviedb.org/3";

#[derive(Clone)]
pub struct TmdbClient {
    base_url: String,
    api_key: String,
    http: reqwest::Client,
}

impl TmdbClient {
    /// Build a client from `TMDB_API_KEY` (required) + optional
    /// `TMDB_API_URL` override. Never panics; missing/empty key maps to a
    /// clear `NotConfigured` error.
    pub fn from_env() -> Result<Self, ToolError> {
        let api_key = std::env::var("TMDB_API_KEY")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::NotConfigured("TMDB_API_KEY not set".into()))?;
        let base_url = std::env::var("TMDB_API_URL")
            .ok()
            .map(|s| s.trim().trim_end_matches('/').to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
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

    /// `GET /search/multi?query={query}&api_key={key}` — combined movie/TV/
    /// person search, TMDb's title-resolution entry point. Thin passthrough
    /// of the parsed JSON; fuzzy-match ranking/shaping is MEDIA-02's job.
    pub async fn search_multi(&self, query: &str) -> Result<Value, ToolError> {
        let url = format!("{}/search/multi", self.base_url);
        let resp = self
            .http
            .get(&url)
            .query(&[("query", query), ("api_key", self.api_key.as_str())])
            .send()
            .await
            .map_err(|e| ToolError::Http(format!("TMDb unavailable: {e}")))?;

        map_response(resp).await
    }
}

async fn map_response(resp: reqwest::Response) -> Result<Value, ToolError> {
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(ToolError::NotFound("TMDb resource not found".into()));
    }
    if status.is_client_error() {
        let body = resp.text().await.unwrap_or_default();
        return Err(ToolError::Http(format!(
            "TMDb API error (HTTP {status}): {}",
            body.chars().take(200).collect::<String>()
        )));
    }
    if status.is_server_error() {
        return Err(ToolError::Http(format!("TMDb unavailable (HTTP {status})")));
    }

    let text = resp.text().await.map_err(|e| ToolError::Http(e.to_string()))?;
    if text.trim().is_empty() {
        return Ok(serde_json::json!({}));
    }
    serde_json::from_str(&text).map_err(|e| ToolError::Http(format!("Invalid JSON from TMDb: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;
    use serde_json::json;
    use serial_test::serial;

    fn test_client(base_url: &str) -> TmdbClient {
        TmdbClient::new(base_url, "testkey", reqwest::Client::new())
    }

    #[test]
    #[serial]
    fn from_env_missing_key_is_not_configured() {
        let key = std::env::var("TMDB_API_KEY").ok();
        std::env::remove_var("TMDB_API_KEY");

        let result = TmdbClient::from_env();
        assert!(matches!(result, Err(ToolError::NotConfigured(_))));

        if let Some(k) = key { std::env::set_var("TMDB_API_KEY", k); }
    }

    #[test]
    #[serial]
    fn from_env_defaults_base_url_when_unset() {
        let key = std::env::var("TMDB_API_KEY").ok();
        let url = std::env::var("TMDB_API_URL").ok();
        std::env::set_var("TMDB_API_KEY", "abc123");
        std::env::remove_var("TMDB_API_URL");

        let client = TmdbClient::from_env().expect("should construct");
        assert_eq!(client.base_url, DEFAULT_BASE_URL);

        if let Some(k) = key { std::env::set_var("TMDB_API_KEY", k); } else { std::env::remove_var("TMDB_API_KEY"); }
        if let Some(u) = url { std::env::set_var("TMDB_API_URL", u); }
    }

    #[tokio::test]
    async fn search_multi_parses_mocked_200() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/search/multi").query_param("query", "dune");
            then.status(200).json_body(json!({ "results": [{ "title": "Dune", "id": 438631 }] }));
        });

        let client = test_client(&server.base_url());
        let result = client.search_multi("dune").await.unwrap();
        mock.assert();
        assert_eq!(result["results"][0]["title"], "Dune");
    }

    #[tokio::test]
    async fn unauthorized_key_maps_to_http_api_error() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/search/multi");
            then.status(401).json_body(json!({ "status_message": "Invalid API key" }));
        });

        let client = test_client(&server.base_url());
        let result = client.search_multi("x").await;
        match result {
            Err(ToolError::Http(msg)) => assert!(msg.contains("401")),
            other => panic!("expected Http error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn server_error_maps_to_http_unavailable() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/search/multi");
            then.status(500);
        });

        let client = test_client(&server.base_url());
        let result = client.search_multi("x").await;
        assert!(matches!(result, Err(ToolError::Http(_))));
    }
}
