//! `dgem_generate` — general-purpose DiffusionGemma generation.

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::tool::RustTool;

use super::{split_thinking, DgemConfig};

pub struct DgemGenerate {
    cfg: DgemConfig,
}

impl DgemGenerate {
    pub(crate) fn new(cfg: DgemConfig) -> Self {
        Self { cfg }
    }
}

#[async_trait]
impl RustTool for DgemGenerate {
    fn name(&self) -> &str {
        "dgem_generate"
    }

    fn description(&self) -> &str {
        "Generate text locally with DiffusionGemma (a local diffusion LLM, $0, no cloud call). \
Best for batch/offline work where no human is waiting (analysis, enrichment, drafts). The first call \
after an idle period pays a one-time model load (~40s); subsequent calls are fast. Provide a \
'user_prompt' (required) and an optional 'system_prompt'. Returns JSON with the model's 'thinking' trace \
separated from its final 'response'."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "system_prompt": {
                    "type": "string",
                    "description": "Optional system prompt / instructions."
                },
                "user_prompt": {
                    "type": "string",
                    "description": "The prompt to generate from (required)."
                },
                "max_tokens": {
                    "type": "integer",
                    "description": "Maximum output tokens (default 1024). DiffusionGemma generates in fixed canvas blocks.",
                    "minimum": 1
                }
            },
            "required": ["user_prompt"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let user_prompt = args["user_prompt"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidArgument("'user_prompt' is required".into()))?;
        let system_prompt = args["system_prompt"].as_str().unwrap_or("");
        let max_tokens = args["max_tokens"]
            .as_u64()
            .map(|n| n as u32)
            .unwrap_or_else(|| self.cfg.default_max_tokens());

        // Guard against oversized input before paying the model load (avoids OOM on the daemon host).
        self.cfg
            .check_input_size(&format!("{system_prompt}{user_prompt}"))?;

        let resp = self.cfg.generate(system_prompt, user_prompt, max_tokens).await?;
        let (thinking, response) = split_thinking(&resp.text);

        Ok(json!({
            "thinking": thinking,
            "response": response,
            "tokens": resp.tokens,
            "time_ms": resp.time_ms,
            "model_load_ms": resp.model_load_ms,
            "input_tokens": resp.input_tokens,
            "blocks": resp.blocks,
        })
        .to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool() -> DgemGenerate {
        DgemGenerate::new(DgemConfig::test_default())
    }

    #[tokio::test]
    async fn requires_user_prompt() {
        let err = tool().execute(json!({})).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn rejects_oversized_input_without_calling_daemon() {
        // max_input_tokens default is 10000 (~40000 chars); exceed it.
        let big = "x".repeat(45_000);
        let err = tool().execute(json!({"user_prompt": big})).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[test]
    fn name_and_schema() {
        let t = tool();
        assert_eq!(t.name(), "dgem_generate");
        let p = t.parameters();
        assert_eq!(p["required"][0], "user_prompt");
    }
}
