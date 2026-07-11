//! Media-domain <media-service> client — request-tracking + discovery, thin. // pii-test-fixture
//!
//! Distinct from [`crate::<media-service>`] (the existing standalone `jellyseerr_*` // pii-test-fixture
//! read-only tool module registered by `register_all`): this is the media
//! domain's OWN client, following the same config/request shape
//! (`src/<media-service>/mod.rs:28-88`) per the blueprint, so later media items // pii-test-fixture
//! (MEDIA-02/03) can compose request-tracking calls alongside Radarr/Sonarr/
//! Plex without importing tool-registration code from an unrelated module.
//! It registers no tools of its own — see [`crate::media::register`].
//!
//! ## Configuration
//! - `JELLYSEERR_URL`     — base URL, e.g. `http://<<media-service>-host>:5055` // pii-test-fixture
//! - `JELLYSEERR_API_KEY` — sent as the `X-Api-Key` header

use serde_json::Value;

use crate::error::ToolError;

#[derive(Clone)]
pub struct JellyseerrClient {
    base_url: String,
    api_key: String,
    http: reqwest::Client,
}

impl JellyseerrClient {
    /// Build a client from `JELLYSEERR_URL` + `JELLYSEERR_API_KEY`. Never
    /// panics; missing/empty config maps to a clear `NotConfigured` error.
    pub fn from_env() -> Result<Self, ToolError> {
        let base_url = std::env::var("JELLYSEERR_URL")
            .ok()
            .map(|s| s.trim().trim_end_matches('/').to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::NotConfigured("JELLYSEERR_URL not set".into()))?;
        let api_key = std::env::var("JELLYSEERR_API_KEY")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::NotConfigured("JELLYSEERR_API_KEY not set".into()))?;
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

    /// `GET /api/v1/status` — server health (thin passthrough).
    pub async fn status(&self) -> Result<Value, ToolError> {
        let url = format!("{}/api/v1/status", self.base_url);
        let resp = self
            .http
            .get(&url)
            .header("Accept", "application/json")
            .header("X-Api-Key", &self.api_key)
            .send()
            .await
            .map_err(|e| ToolError::Http(format!("<media-service> unavailable: {e}")))?; // pii-test-fixture

        map_response(resp).await
    }

    /// `POST /api/v1/request` — register a request for tracking (thin
    /// passthrough; the tiering/confirmation logic that decides whether this
    /// is ever called lives in MEDIA-03, not here).
    pub async fn create_request(&self, body: Value) -> Result<Value, ToolError> {
        let url = format!("{}/api/v1/request", self.base_url);
        let resp = self
            .http
            .post(&url)
            .header("Accept", "application/json")
            .header("X-Api-Key", &self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| ToolError::Http(format!("<media-service> unavailable: {e}")))?; // pii-test-fixture

        map_response(resp).await
    }
}

async fn map_response(resp: reqwest::Response) -> Result<Value, ToolError> {
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(ToolError::NotFound("<media-service> resource not found".into())); // pii-test-fixture
    }
    if status.is_client_error() {
        let body = resp.text().await.unwrap_or_default();
        return Err(ToolError::Http(format!(
            "<media-service> API error (HTTP {status}): {}", // pii-test-fixture
            body.chars().take(200).collect::<String>()
        )));
    }
    if status.is_server_error() {
        return Err(ToolError::Http(format!("<media-service> unavailable (HTTP {status})"))); // pii-test-fixture
    }

    let text = resp.text().await.map_err(|e| ToolError::Http(e.to_string()))?;
    if text.trim().is_empty() {
        return Ok(serde_json::json!({}));
    }
    serde_json::from_str(&text).map_err(|e| ToolError::Http(format!("Invalid JSON from <media-service>: {e}"))) // pii-test-fixture
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;
    use serde_json::json;
    use serial_test::serial;

    fn test_client(base_url: &str) -> JellyseerrClient {
        JellyseerrClient::new(base_url, "testkey", reqwest::Client::new())
    }

    #[test]
    #[serial]
    fn from_env_missing_key_is_not_configured() {
        let url = std::env::var("JELLYSEERR_URL").ok();
        let key = std::env::var("JELLYSEERR_API_KEY").ok();
        std::env::set_var("JELLYSEERR_URL", "http://<media-service>.test:5055"); // pii-test-fixture
        std::env::remove_var("JELLYSEERR_API_KEY");

        let result = JellyseerrClient::from_env();
        assert!(matches!(result, Err(ToolError::NotConfigured(_))));

        if let Some(u) = url { std::env::set_var("JELLYSEERR_URL", u); } else { std::env::remove_var("JELLYSEERR_URL"); }
        if let Some(k) = key { std::env::set_var("JELLYSEERR_API_KEY", k); }
    }

    #[tokio::test]
    async fn status_parses_mocked_200() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/api/v1/status");
            then.status(200).json_body(json!({ "version": "1.9.2" }));
        });

        let client = test_client(&server.base_url());
        let result = client.status().await.unwrap();
        mock.assert();
        assert_eq!(result["version"], "1.9.2");
    }

    #[tokio::test]
    async fn create_request_posts_and_parses_mocked_201() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST).path("/api/v1/request");
            then.status(201).json_body(json!({ "id": 42, "status": 1 }));
        });

        let client = test_client(&server.base_url());
        let result = client.create_request(json!({ "mediaType": "movie", "mediaId": 438631 })).await.unwrap();
        mock.assert();
        assert_eq!(result["id"], 42);
    }

    #[tokio::test]
    async fn server_error_maps_to_http_unavailable() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/v1/status");
            then.status(500);
        });

        let client = test_client(&server.base_url());
        let result = client.status().await;
        assert!(matches!(result, Err(ToolError::Http(_))));
    }
}
