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

pub struct AgentsessTranscript;

#[async_trait]
impl RustTool for AgentsessTranscript {
    fn name(&self) -> &str {
        "agentsess_transcript"
    }

    fn description(&self) -> &str {
        "Recent activity for one coder CLI agent session: a summarised, redacted stream of \
         what it has been doing (tool calls with their primary argument, messages), read from \
         the tail of the session's transcript. Read-only."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": {
                    "type": "string",
                    "description": "Session id from agentsess_list. Resolves that session's transcript."
                },
                "transcript_path": {
                    "type": "string",
                    "description": "Explicit transcript path instead of a session id. Must be inside the configured transcript root."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Maximum events to return, newest first (default 50)."
                },
                "host": {
                    "type": "string",
                    "enum": ["local", "dev"],
                    "description": "Which host the session is on. Default 'local'."
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
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(50)
            .clamp(1, 500) as usize;

        // Resolve the transcript path either from an explicit argument (jailed
        // to the configured root) or by looking the session up. An explicit
        // path is NOT trusted just because it came from a caller — it goes
        // through the same jail either way.
        let root = super::discover::transcript_root_for(exec.as_ref())
            .map_err(ToolError::NotConfigured)?;

        let lexical = if let Some(p) = args.get("transcript_path").and_then(Value::as_str) {
            super::transcript::resolve_transcript_path(&root, p)?
        } else if let Some(sid) = args.get("session_id").and_then(Value::as_str) {
            let snapshot = super::discover::discover(exec.as_ref(), None).await?;
            let session = snapshot
                .sessions
                .iter()
                .find(|s| s.id == sid)
                .ok_or_else(|| ToolError::NotFound(format!("no live session with id '{sid}'")))?;
            let p = session.transcript_path.clone().ok_or_else(|| {
                ToolError::NotFound(format!(
                    "session '{sid}' has no transcript (its agent may not write one)"
                ))
            })?;
            super::transcript::resolve_transcript_path(&root, &p)?
        } else {
            return Err(ToolError::InvalidArgument(
                "one of 'session_id' or 'transcript_path' is required".into(),
            ));
        };

        // The lexical jail cannot see symlinks and `tail` follows them, so the
        // path is resolved on the host that will read it and re-checked. Both
        // halves are applied to a discovered path too — a transcript path that
        // came from discovery is not automatically trustworthy.
        let path =
            super::transcript::resolve_transcript_path_on_host(exec.as_ref(), &root, &lexical)
                .await?;

        let tail = super::transcript::read_tail(exec.as_ref(), &path, limit).await?;
        let structured = serde_json::to_value(&tail)
            .map_err(|e| ToolError::Execution(format!("failed to serialize transcript: {e}")))?;
        let text = serde_json::to_string_pretty(&structured).unwrap_or_else(|_| "{}".to_string());
        Ok(ToolOutput::with_structured(text, structured))
    }
}

pub struct AgentsessCapture;

#[async_trait]
impl RustTool for AgentsessCapture {
    fn name(&self) -> &str {
        "agentsess_capture"
    }

    fn description(&self) -> &str {
        "Capture the recent scrollback of the tmux pane a coder CLI agent session is attached \
         to, so it can be rendered as a read-only terminal view. Bounded and redacted. This \
         tool captures only — it cannot send input to a session."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": {
                    "type": "string",
                    "description": "Session id from agentsess_list; its attached pane is captured."
                },
                "target": {
                    "type": "string",
                    "description": "Explicit tmux pane target 'session:window.pane' instead of a session id."
                },
                "lines": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Scrollback lines to capture (default 200, capped by AGENTSESS_CAPTURE_MAX_LINES)."
                },
                "host": {
                    "type": "string",
                    "enum": ["local", "dev"],
                    "description": "Which host the session is on. Default 'local'."
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
        // Clamp rather than `as u32`: a JSON integer is arbitrary, and a bare
        // cast WRAPS — 4294967296 would silently become 0 and then be bumped to
        // 1 line, quietly returning almost nothing instead of a big capture.
        let lines = args
            .get("lines")
            .and_then(Value::as_u64)
            .map(|n| u32::try_from(n).unwrap_or(u32::MAX));

        let target = if let Some(t) = args.get("target").and_then(Value::as_str) {
            t.to_string()
        } else if let Some(sid) = args.get("session_id").and_then(Value::as_str) {
            let snapshot = super::discover::discover(exec.as_ref(), None).await?;
            let session = snapshot
                .sessions
                .iter()
                .find(|s| s.id == sid)
                .ok_or_else(|| ToolError::NotFound(format!("no live session with id '{sid}'")))?;
            // A session with no pane is a clear error, not an empty capture:
            // "nothing to show" and "not attached to a terminal" are different
            // answers and an operator needs to tell them apart.
            session
                .attachment
                .as_ref()
                .map(|a| a.target.clone())
                .ok_or_else(|| {
                    ToolError::NotFound(format!(
                        "session '{sid}' is not attached to a tmux pane, so there is nothing to capture"
                    ))
                })?
        } else {
            return Err(ToolError::InvalidArgument(
                "one of 'session_id' or 'target' is required".into(),
            ));
        };

        let cap = super::capture::capture(exec.as_ref(), &target, lines).await?;
        let structured = serde_json::to_value(&cap)
            .map_err(|e| ToolError::Execution(format!("failed to serialize capture: {e}")))?;
        let text = serde_json::to_string_pretty(&structured).unwrap_or_else(|_| "{}".to_string());
        Ok(ToolOutput::with_structured(text, structured))
    }
}
