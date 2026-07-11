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

    /// `POST /api/v3/series` — add a series to the library, driving Sonarr's
    /// own indexer search + grab (which hands completed downloads to the
    /// configured download client, qtor) when `addOptions.searchForMissingEpisodes`
    /// is `true` in `body`. MEDIA-03: thin passthrough of a caller-built
    /// body -- whole-series vs. single-season monitoring and the
    /// tiering/confirmation decision live in `crate::media::request`.
    pub async fn add_series(&self, body: Value) -> Result<Value, ToolError> {
        let url = format!("{}/api/v3/series", self.base_url);
        let resp = self
            .http
            .post(&url)
            .header("X-Api-Key", &self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| ToolError::Http(format!("Sonarr unavailable: {e}")))?;

        map_response(resp).await
    }

    /// `PUT /api/v3/series/{id}` — update an existing series resource (tags,
    /// `monitored`, `qualityProfileId`, ...). See
    /// [`crate::media::clients::radarr::RadarrClient::update_movie`] for the
    /// full-resource-body caveat; same shape here.
    pub async fn update_series(&self, id: i64, body: Value) -> Result<Value, ToolError> {
        let url = format!("{}/api/v3/series/{id}", self.base_url);
        let resp = self
            .http
            .put(&url)
            .header("X-Api-Key", &self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| ToolError::Http(format!("Sonarr unavailable: {e}")))?;

        map_response(resp).await
    }

    /// `DELETE /api/v3/series/{id}?deleteFiles=true&addImportListExclusion=false`
    /// — remove a series from the Sonarr library and delete its files. See
    /// [`crate::media::clients::radarr::RadarrClient::delete_movie`] for the
    /// rationale (thin/unconditional; the hard-confirm gate lives in
    /// `crate::media::organize`) and the `Ok(false)`-on-404 no-op contract.
    pub async fn delete_series(&self, id: i64) -> Result<bool, ToolError> {
        let url = format!("{}/api/v3/series/{id}", self.base_url);
        let resp = self
            .http
            .delete(&url)
            .header("X-Api-Key", &self.api_key)
            .query(&[("deleteFiles", "true"), ("addImportListExclusion", "false")])
            .send()
            .await
            .map_err(|e| ToolError::Http(format!("Sonarr unavailable: {e}")))?;

        match map_response(resp).await {
            Ok(_) => Ok(true),
            Err(ToolError::NotFound(_)) => Ok(false),
            Err(e) => Err(e),
        }
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
    async fn add_series_posts_body_and_parses_mocked_201() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST).path("/api/v3/series");
            then.status(201).json_body(json!({ "id": 9, "title": "Foundation" }));
        });

        let client = test_client(&server.base_url());
        let body = json!({ "tvdbId": 358903, "title": "Foundation", "qualityProfileId": 1, "rootFolderPath": "/tv", "monitored": true, "addOptions": { "searchForMissingEpisodes": true } });
        let result = client.add_series(body).await.unwrap();
        mock.assert();
        assert_eq!(result["id"], 9);
    }

    #[tokio::test]
    async fn add_series_server_error_maps_to_http() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/api/v3/series");
            then.status(500);
        });

        let client = test_client(&server.base_url());
        let result = client.add_series(json!({})).await;
        assert!(matches!(result, Err(ToolError::Http(_))));
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

    #[tokio::test]
    async fn update_series_puts_body_and_parses_mocked_200() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(PUT).path("/api/v3/series/9");
            then.status(200).json_body(json!({ "id": 9, "monitored": false }));
        });

        let client = test_client(&server.base_url());
        let result = client.update_series(9, json!({ "id": 9, "monitored": false })).await.unwrap();
        mock.assert();
        assert_eq!(result["id"], 9);
    }

    #[tokio::test]
    async fn delete_series_present_returns_true() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(DELETE)
                .path("/api/v3/series/9")
                .query_param("deleteFiles", "true");
            then.status(200);
        });

        let client = test_client(&server.base_url());
        let deleted = client.delete_series(9).await.unwrap();
        mock.assert();
        assert!(deleted);
    }

    #[tokio::test]
    async fn delete_series_not_present_returns_false_not_error() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(DELETE).path("/api/v3/series/999");
            then.status(404);
        });

        let client = test_client(&server.base_url());
        let deleted = client.delete_series(999).await.unwrap();
        assert!(!deleted, "a 404 from Sonarr must map to Ok(false), not an error");
    }
}
