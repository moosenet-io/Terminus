//! The `agentsess_*` tool surface (AGSS-01).

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::tool::{RustTool, ToolOutput};

use super::discover::discover;
use super::exec::executor_for;

fn host_arg(args: &Value) -> Option<&str> {
    args.get("host").and_then(Value::as_str)
}

pub struct AgentsessList;

#[async_trait]
impl RustTool for AgentsessList {
    fn name(&self) -> &str {
        "agentsess_list"
    }

    fn description(&self) -> &str {
        "List live coder CLI agent sessions (Claude Code, codex, aider) on a host, each \
         correlated to the git repo, branch and work-item it is working on, plus its tmux \
         pane and most recent activity time. Read-only."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "host": {
                    "type": "string",
                    "enum": ["local", "dev"],
                    "description": "Which host to observe. 'local' (default) is the host this \
                                    terminus instance runs on; 'dev' uses the configured dev SSH door."
                },
                "repo": {
                    "type": "string",
                    "description": "Optional repository NAME to filter by (e.g. 'Terminus')."
                }
            },
            "required": []
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.execute_structured(args).await?.text)
    }

    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let exec = executor_for(host_arg(&args))?;
        let repo = args.get("repo").and_then(Value::as_str);
        let snapshot = discover(exec.as_ref(), repo).await?;

        let structured = serde_json::to_value(&snapshot)
            .map_err(|e| ToolError::Execution(format!("failed to serialize snapshot: {e}")))?;
        let text = serde_json::to_string_pretty(&structured)
            .unwrap_or_else(|_| "{}".to_string());
        Ok(ToolOutput::with_structured(text, structured))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unknown_host_is_rejected_before_any_probe_runs() {
        let err = AgentsessList
            .execute(json!({"host": "somewhere-else"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)), "got {err:?}");
    }

    #[test]
    fn parameters_are_a_valid_schema_object() {
        let p = AgentsessList.parameters();
        assert_eq!(p["type"], "object");
        assert!(p["properties"]["host"].is_object());
    }
}
