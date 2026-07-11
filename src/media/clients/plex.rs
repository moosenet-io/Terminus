//! Plex client — thin typed wrapper for library/history reads.
//!
//! Plex is the consumption/history layer: library sections, watch history,
//! and on-deck/continue-watching all live here (later items — MEDIA-05
//! recommend/engagement — build on `history`). MEDIA-01 scaffold only:
//! config + `library_sections` (proves the client shape) + `history`.
//!
//! ## Configuration
//! - `PLEX_URL`   — base URL, e.g. `http://<plex-host>:32400`
//! - `PLEX_TOKEN` — sent as the `X-Plex-Token` header (also accepted as a
//!   query param by the real Plex API, but the header form avoids leaking
//!   the token into access logs / URLs).

use serde_json::Value;

use crate::error::ToolError;

#[derive(Clone)]
pub struct PlexClient {
    base_url: String,
    token: String,
    http: reqwest::Client,
}

impl PlexClient {
    /// Build a client from `PLEX_URL` + `PLEX_TOKEN`. Never panics;
    /// missing/empty config maps to a clear `NotConfigured` error.
    pub fn from_env() -> Result<Self, ToolError> {
        let base_url = std::env::var("PLEX_URL")
            .ok()
            .map(|s| s.trim().trim_end_matches('/').to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::NotConfigured("PLEX_URL not set".into()))?;
        let token = std::env::var("PLEX_TOKEN")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::NotConfigured("PLEX_TOKEN not set".into()))?;
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .map_err(|e| ToolError::Http(format!("Failed to build HTTP client: {e}")))?;
        Ok(Self { base_url, token, http })
    }

    pub fn new(base_url: impl Into<String>, token: impl Into<String>, http: reqwest::Client) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            token: token.into(),
            http,
        }
    }

    /// `GET /library/sections` — the configured library sections.
    pub async fn library_sections(&self) -> Result<Value, ToolError> {
        let url = format!("{}/library/sections", self.base_url);
        let resp = self
            .http
            .get(&url)
            .header("X-Plex-Token", &self.token)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| ToolError::Http(format!("Plex unavailable: {e}")))?;

        map_response(resp).await
    }

    /// `GET /status/sessions/history/all` — recent watch history (thin
    /// passthrough; used by MEDIA-05's recommendation rationale).
    pub async fn history(&self) -> Result<Value, ToolError> {
        let url = format!("{}/status/sessions/history/all", self.base_url);
        let resp = self
            .http
            .get(&url)
            .header("X-Plex-Token", &self.token)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| ToolError::Http(format!("Plex unavailable: {e}")))?;

        map_response(resp).await
    }
}

async fn map_response(resp: reqwest::Response) -> Result<Value, ToolError> {
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(ToolError::NotFound("Plex resource not found".into()));
    }
    if status.is_client_error() {
        let body = resp.text().await.unwrap_or_default();
        return Err(ToolError::Http(format!(
            "Plex API error (HTTP {status}): {}",
            body.chars().take(200).collect::<String>()
        )));
    }
    if status.is_server_error() {
        return Err(ToolError::Http(format!("Plex unavailable (HTTP {status})")));
    }

    let text = resp.text().await.map_err(|e| ToolError::Http(e.to_string()))?;
    if text.trim().is_empty() {
        return Ok(serde_json::json!({}));
    }
    serde_json::from_str(&text).map_err(|e| ToolError::Http(format!("Invalid JSON from Plex: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;
    use serde_json::json;
    use serial_test::serial;

    fn test_client(base_url: &str) -> PlexClient {
        PlexClient::new(base_url, "testtoken", reqwest::Client::new())
    }

    #[test]
    #[serial]
    fn from_env_missing_token_is_not_configured() {
        let url = std::env::var("PLEX_URL").ok();
        let token = std::env::var("PLEX_TOKEN").ok();
        std::env::set_var("PLEX_URL", "http://plex.test:32400");
        std::env::remove_var("PLEX_TOKEN");

        let result = PlexClient::from_env();
        assert!(matches!(result, Err(ToolError::NotConfigured(_))));

        if let Some(u) = url { std::env::set_var("PLEX_URL", u); } else { std::env::remove_var("PLEX_URL"); }
        if let Some(t) = token { std::env::set_var("PLEX_TOKEN", t); }
    }

    #[test]
    #[serial]
    fn from_env_builds_when_both_set() {
        let url = std::env::var("PLEX_URL").ok();
        let token = std::env::var("PLEX_TOKEN").ok();
        std::env::set_var("PLEX_URL", "http://plex.test:32400/");
        std::env::set_var("PLEX_TOKEN", "tok");

        let client = PlexClient::from_env().expect("should construct");
        assert_eq!(client.base_url, "http://plex.test:32400");

        if let Some(u) = url { std::env::set_var("PLEX_URL", u); } else { std::env::remove_var("PLEX_URL"); }
        if let Some(t) = token { std::env::set_var("PLEX_TOKEN", t); } else { std::env::remove_var("PLEX_TOKEN"); }
    }

    #[tokio::test]
    async fn library_sections_parses_mocked_200() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/library/sections");
            then.status(200).json_body(json!({ "MediaContainer": { "size": 2 } }));
        });

        let client = test_client(&server.base_url());
        let result = client.library_sections().await.unwrap();
        mock.assert();
        assert_eq!(result["MediaContainer"]["size"], 2);
    }

    #[tokio::test]
    async fn history_maps_server_error_to_unavailable() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/status/sessions/history/all");
            then.status(503);
        });

        let client = test_client(&server.base_url());
        let result = client.history().await;
        assert!(matches!(result, Err(ToolError::Http(_))));
    }

    #[tokio::test]
    async fn unauthorized_maps_to_http_api_error_not_panic() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/library/sections");
            then.status(401);
        });

        let client = test_client(&server.base_url());
        let result = client.library_sections().await;
        assert!(matches!(result, Err(ToolError::Http(_))));
    }
}
