//! `dgem_status` — DiffusionGemma daemon/session status.

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::tool::RustTool;

use super::DgemConfig;

pub struct DgemStatus {
    cfg: DgemConfig,
}

impl DgemStatus {
    pub(crate) fn new(cfg: DgemConfig) -> Self {
        Self { cfg }
    }
}

#[async_trait]
impl RustTool for DgemStatus {
    fn name(&self) -> &str {
        "dgem_status"
    }

    fn description(&self) -> &str {
        "Check the DiffusionGemma local-inference daemon status: whether it is running, whether the model \
is currently loaded in VRAM, uptime, requests served, seconds since the last request, and the idle \
timeout. Use this before a batch of local reviews to know if the first call will pay the model load."
    }

    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        match self.cfg.status().await {
            Some(s) => Ok(json!({
                "running": s.running,
                "model_loaded": s.model_loaded,
                "uptime_secs": s.uptime_secs,
                "requests_served": s.requests_served,
                "last_request_secs_ago": s.last_request_secs_ago,
                "idle_timeout_secs": s.idle_timeout_secs,
                "model_load_ms": s.model_load_ms,
            })
            .to_string()),
            // Daemon unreachable: report not-running rather than erroring, so callers can branch on it
            // (e.g. the pipeline falls back to a cloud reviewer).
            None => Ok(json!({
                "running": false,
                "model_loaded": false,
                "uptime_secs": 0,
                "requests_served": 0,
                "last_request_secs_ago": 0,
                "idle_timeout_secs": 0,
                "model_load_ms": 0,
                "note": "DiffusionGemma daemon unreachable",
            })
            .to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn status_unreachable_reports_not_running() {
        // Point at a port nothing is listening on → graceful running=false.
        let cfg = DgemConfig::test_with_url("http://127.0.0.1:1");
        let out = DgemStatus::new(cfg).execute(json!({})).await.unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["running"], false);
        assert_eq!(v["model_loaded"], false);
    }

    #[test]
    fn name_and_empty_schema() {
        let t = DgemStatus::new(DgemConfig::test_default());
        assert_eq!(t.name(), "dgem_status");
        assert_eq!(t.parameters()["type"], "object");
    }
}
