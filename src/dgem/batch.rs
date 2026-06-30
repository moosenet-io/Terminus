//! `dgem_batch` — multi-prompt batch through the persistent DiffusionGemma session (DGEM-04).
//!
//! Processes a list of jobs sequentially through the daemon's persistent session. The diffusion canvas
//! is single-threaded, so jobs run one at a time; the model is loaded once (on the first job) and stays
//! resident for the whole batch (each job resets the daemon's idle timer). A job that fails — oversized
//! input, daemon error, timeout — records its error and the batch continues with the next job.

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::tool::RustTool;

use super::{split_thinking, DgemConfig};

pub struct DgemBatch {
    cfg: DgemConfig,
}

impl DgemBatch {
    pub(crate) fn new(cfg: DgemConfig) -> Self {
        Self { cfg }
    }
}

struct Job {
    id: String,
    system_prompt: String,
    user_prompt: String,
    max_tokens: u32,
}

fn parse_jobs(args: &Value, default_max_tokens: u32) -> Result<Vec<Job>, ToolError> {
    let arr = args["jobs"]
        .as_array()
        .ok_or_else(|| ToolError::InvalidArgument("'jobs' must be an array".into()))?;
    let mut jobs = Vec::with_capacity(arr.len());
    for (i, j) in arr.iter().enumerate() {
        let user_prompt = j["user_prompt"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                ToolError::InvalidArgument(format!("job[{i}] missing 'user_prompt'"))
            })?
            .to_string();
        let id = j["id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(String::from)
            .unwrap_or_else(|| format!("job-{i}"));
        let system_prompt = j["system_prompt"].as_str().unwrap_or("").to_string();
        let max_tokens = j["max_tokens"]
            .as_u64()
            .map(|n| n as u32)
            .unwrap_or(default_max_tokens);
        jobs.push(Job {
            id,
            system_prompt,
            user_prompt,
            max_tokens,
        });
    }
    Ok(jobs)
}

#[async_trait]
impl RustTool for DgemBatch {
    fn name(&self) -> &str {
        "dgem_batch"
    }

    fn description(&self) -> &str {
        "Run a batch of prompts through DiffusionGemma's persistent local session ($0, no cloud). \
Pass 'jobs' as a list of {id, user_prompt, system_prompt?, max_tokens?}. Jobs run sequentially; the \
model loads once and stays resident for the whole batch. A failing job does not abort the batch. \
Returns per-job results with timing and success/failure, plus total time and whether the session was \
already warm. Ideal for spec enrichment, nightly digests, and bulk analysis."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "jobs": {
                    "type": "array",
                    "description": "List of generation jobs to run sequentially.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": { "type": "string", "description": "Caller id for the job (defaults to job-N)." },
                            "user_prompt": { "type": "string", "description": "The prompt (required)." },
                            "system_prompt": { "type": "string", "description": "Optional system prompt." },
                            "max_tokens": { "type": "integer", "description": "Optional max output tokens.", "minimum": 1 }
                        },
                        "required": ["user_prompt"]
                    }
                }
            },
            "required": ["jobs"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let jobs = parse_jobs(&args, self.cfg.default_max_tokens())?;

        // Empty batch is valid → empty results.
        if jobs.is_empty() {
            return Ok(json!({
                "results": [],
                "total_time_ms": 0,
                "session_was_warm": false,
            })
            .to_string());
        }

        // Warm = the daemon already has the model loaded before we start.
        let session_was_warm = self
            .cfg
            .status()
            .await
            .map(|s| s.model_loaded)
            .unwrap_or(false);

        let mut results = Vec::with_capacity(jobs.len());
        let mut total_time_ms: i64 = 0;
        let total = jobs.len();

        for (i, job) in jobs.into_iter().enumerate() {
            // Per-job oversized-input guard: record the failure, keep going.
            if let Err(e) = self
                .cfg
                .check_input_size(&format!("{}{}", job.system_prompt, job.user_prompt))
            {
                results.push(json!({
                    "id": job.id,
                    "success": false,
                    "error": e.to_string(),
                }));
                continue;
            }

            match self
                .cfg
                .generate(&job.system_prompt, &job.user_prompt, job.max_tokens)
                .await
            {
                Ok(resp) => {
                    total_time_ms += resp.time_ms;
                    let (thinking, response) = split_thinking(&resp.text);
                    tracing::info!("dgem_batch progress={}/{} id={}", i + 1, total, job.id);
                    results.push(json!({
                        "id": job.id,
                        "success": true,
                        "thinking": thinking,
                        "response": response,
                        "tokens": resp.tokens,
                        "time_ms": resp.time_ms,
                        "error": Value::Null,
                    }));
                }
                Err(e) => {
                    results.push(json!({
                        "id": job.id,
                        "success": false,
                        "error": e.to_string(),
                    }));
                }
            }
        }

        Ok(json!({
            "results": results,
            "total_time_ms": total_time_ms,
            "session_was_warm": session_was_warm,
        })
        .to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool() -> DgemBatch {
        DgemBatch::new(DgemConfig::test_with_url("http://127.0.0.1:1"))
    }

    #[test]
    fn parse_jobs_assigns_default_ids_and_max_tokens() {
        let args = json!({"jobs": [
            {"user_prompt": "a"},
            {"id": "x", "user_prompt": "b", "max_tokens": 256}
        ]});
        let jobs = parse_jobs(&args, 1024).unwrap();
        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs[0].id, "job-0");
        assert_eq!(jobs[0].max_tokens, 1024);
        assert_eq!(jobs[1].id, "x");
        assert_eq!(jobs[1].max_tokens, 256);
    }

    #[test]
    fn parse_jobs_rejects_missing_user_prompt() {
        let args = json!({"jobs": [{"id": "a"}]});
        assert!(parse_jobs(&args, 1024).is_err());
    }

    #[test]
    fn parse_jobs_requires_array() {
        assert!(parse_jobs(&json!({"jobs": "not-an-array"}), 1024).is_err());
    }

    #[tokio::test]
    async fn empty_batch_returns_empty_results() {
        let out = tool().execute(json!({"jobs": []})).await.unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["results"].as_array().unwrap().len(), 0);
        assert_eq!(v["total_time_ms"], 0);
    }

    #[tokio::test]
    async fn failing_jobs_do_not_abort_batch() {
        // Daemon unreachable (port 1) → every job fails, but the batch completes with per-job errors.
        let out = tool()
            .execute(json!({"jobs": [
                {"id": "one", "user_prompt": "hello"},
                {"id": "two", "user_prompt": "world"}
            ]}))
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        let results = v["results"].as_array().unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["success"], false);
        assert_eq!(results[1]["success"], false);
        assert_eq!(results[0]["id"], "one");
        assert!(results[0]["error"].is_string());
    }

    #[tokio::test]
    async fn oversized_job_records_error_and_continues() {
        // First job oversized (guard trips before any network), second also fails on unreachable daemon.
        let cfg = DgemConfig::test_with_url("http://127.0.0.1:1");
        let big = "x".repeat(45_000);
        let out = DgemBatch::new(cfg)
            .execute(json!({"jobs": [
                {"id": "big", "user_prompt": big},
                {"id": "small", "user_prompt": "ok"}
            ]}))
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        let results = v["results"].as_array().unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["success"], false);
        assert!(results[0]["error"].as_str().unwrap().contains("too large"));
    }
}
