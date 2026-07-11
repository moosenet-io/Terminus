//! qtor client — thin typed wrapper for the download-client status/queue.
//!
//! "qtor" is this domain's name for the torrent download client Radarr/
//! Sonarr hand completed indexer grabs to. The exact backend (qBittorrent-
//! style) isn't pinned down further by the spec beyond `QTOR_URL` +
//! `QTOR_CREDS`; MEDIA-01 establishes a thin bearer-token-style client
//! (`QTOR_CREDS` sent as an `Authorization` header) + one representative
//! operation. Later items (MEDIA-03/04) extend this once the exact request/
//! remove operations they need are scoped.
//!
//! ## Configuration
//! - `QTOR_URL`   — base URL of the download client's API
//! - `QTOR_CREDS` — credential sent as `Authorization: {QTOR_CREDS}`

use serde_json::Value;

use crate::error::ToolError;

#[derive(Clone)]
pub struct QtorClient {
    base_url: String,
    creds: String,
    http: reqwest::Client,
}

impl QtorClient {
    /// Build a client from `QTOR_URL` + `QTOR_CREDS`. Never panics;
    /// missing/empty config maps to a clear `NotConfigured` error.
    pub fn from_env() -> Result<Self, ToolError> {
        let base_url = std::env::var("QTOR_URL")
            .ok()
            .map(|s| s.trim().trim_end_matches('/').to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::NotConfigured("QTOR_URL not set".into()))?;
        let creds = std::env::var("QTOR_CREDS")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::NotConfigured("QTOR_CREDS not set".into()))?;
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .map_err(|e| ToolError::Http(format!("Failed to build HTTP client: {e}")))?;
        Ok(Self { base_url, creds, http })
    }

    pub fn new(base_url: impl Into<String>, creds: impl Into<String>, http: reqwest::Client) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            creds: creds.into(),
            http,
        }
    }

    /// `GET /api/status` — download client health/version (thin passthrough;
    /// used later to confirm the acquisition chain is up before a request).
    pub async fn status(&self) -> Result<Value, ToolError> {
        let url = format!("{}/api/status", self.base_url);
        let resp = self
            .http
            .get(&url)
            .header("Authorization", &self.creds)
            .send()
            .await
            .map_err(|e| ToolError::Http(format!("Download client unavailable: {e}")))?;

        map_response(resp).await
    }

    /// `GET /api/queue` — the current download queue (thin passthrough).
    pub async fn queue(&self) -> Result<Value, ToolError> {
        let url = format!("{}/api/queue", self.base_url);
        let resp = self
            .http
            .get(&url)
            .header("Authorization", &self.creds)
            .send()
            .await
            .map_err(|e| ToolError::Http(format!("Download client unavailable: {e}")))?;

        map_response(resp).await
    }
}

async fn map_response(resp: reqwest::Response) -> Result<Value, ToolError> {
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(ToolError::NotFound("Download-client resource not found".into()));
    }
    if status.is_client_error() {
        let body = resp.text().await.unwrap_or_default();
        return Err(ToolError::Http(format!(
            "Download client API error (HTTP {status}): {}",
            body.chars().take(200).collect::<String>()
        )));
    }
    if status.is_server_error() {
        return Err(ToolError::Http(format!("Download client unavailable (HTTP {status})")));
    }

    let text = resp.text().await.map_err(|e| ToolError::Http(e.to_string()))?;
    if text.trim().is_empty() {
        return Ok(serde_json::json!({}));
    }
    serde_json::from_str(&text).map_err(|e| ToolError::Http(format!("Invalid JSON from download client: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;
    use serde_json::json;
    use serial_test::serial;

    fn test_client(base_url: &str) -> QtorClient {
        QtorClient::new(base_url, "Bearer testtoken", reqwest::Client::new())
    }

    #[test]
    #[serial]
    fn from_env_missing_creds_is_not_configured() {
        let url = std::env::var("QTOR_URL").ok();
        let creds = std::env::var("QTOR_CREDS").ok();
        std::env::set_var("QTOR_URL", "http://qtor.test:8080");
        std::env::remove_var("QTOR_CREDS");

        let result = QtorClient::from_env();
        assert!(matches!(result, Err(ToolError::NotConfigured(_))));

        if let Some(u) = url { std::env::set_var("QTOR_URL", u); } else { std::env::remove_var("QTOR_URL"); }
        if let Some(c) = creds { std::env::set_var("QTOR_CREDS", c); }
    }

    #[test]
    #[serial]
    fn from_env_builds_when_both_set() {
        let url = std::env::var("QTOR_URL").ok();
        let creds = std::env::var("QTOR_CREDS").ok();
        std::env::set_var("QTOR_URL", "http://qtor.test:8080/");
        std::env::set_var("QTOR_CREDS", "tok");

        let client = QtorClient::from_env().expect("should construct");
        assert_eq!(client.base_url, "http://qtor.test:8080");

        if let Some(u) = url { std::env::set_var("QTOR_URL", u); } else { std::env::remove_var("QTOR_URL"); }
        if let Some(c) = creds { std::env::set_var("QTOR_CREDS", c); } else { std::env::remove_var("QTOR_CREDS"); }
    }

    #[tokio::test]
    async fn status_parses_mocked_200() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/api/status");
            then.status(200).json_body(json!({ "version": "4.6.0" }));
        });

        let client = test_client(&server.base_url());
        let result = client.status().await.unwrap();
        mock.assert();
        assert_eq!(result["version"], "4.6.0");
    }

    #[tokio::test]
    async fn queue_maps_not_found() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/queue");
            then.status(404);
        });

        let client = test_client(&server.base_url());
        let result = client.queue().await;
        assert!(matches!(result, Err(ToolError::NotFound(_))));
    }

    #[tokio::test]
    async fn server_error_maps_to_http_unavailable() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/api/status");
            then.status(500);
        });

        let client = test_client(&server.base_url());
        let result = client.status().await;
        assert!(matches!(result, Err(ToolError::Http(_))));
    }
}
