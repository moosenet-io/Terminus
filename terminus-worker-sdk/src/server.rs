//! Minimal, self-contained worker-side JSON-RPC server.
//!
//! Speaks the same three-method subset and the same JSON-RPC 2.0 request/
//! result *shapes* the main `terminus-rs` daemon's `mcp_server::handle_mcp`
//! speaks over HTTP (`initialize` / `tools/list` / `tools/call`, with
//! `tools/call` results framed as `{"content": [{"type": "text", ...}],
//! "isError": bool}`), but over a Unix domain socket rather than HTTP, and
//! newline-delimited rather than SSE-framed — this is a worker's own
//! listener, not a drop-in replacement for that HTTP server.
//!
//! Deliberately does NOT depend on `terminus-rs::broker::transport` (a
//! sibling item owns the daemon-side `WorkerTransport` client, unmerged as
//! of this item). Reconciling this listener's framing with that client's
//! expectations is a follow-up once both sides exist.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use terminus_rs::error::ToolError;
use terminus_rs::registry::ToolInfo;
use terminus_rs::tool::RustTool;

use crate::manifest::WorkerManifest;

/// Errors serving a worker socket.
#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    #[error("io error binding/serving worker socket: {0}")]
    Io(#[from] std::io::Error),
}

/// Shared, immutable state one bound worker socket serves from: the tool
/// map (dispatch by name) plus the manifest returned on `initialize`.
pub struct WorkerState {
    pub(crate) manifest: WorkerManifest,
    pub(crate) tools: HashMap<String, Box<dyn RustTool>>,
}

/// Bind `socket_path` (removing any stale socket file left behind by a
/// prior, uncleanly-terminated run) and serve JSON-RPC requests forever, one
/// task per accepted connection.
///
/// Each line of a connection is expected to be exactly one JSON-RPC request
/// object; each response is written back as exactly one JSON-RPC response
/// object followed by `\n`. A connection that sends a notification (a
/// request object with no `"id"`) gets no reply for that line, matching
/// JSON-RPC 2.0 notification semantics and the daemon's own
/// `notifications/initialized` handling.
pub async fn serve(socket_path: &str, state: Arc<WorkerState>) -> Result<(), ServeError> {
    // Best-effort cleanup of a stale socket file from a prior run that
    // didn't shut down cleanly -- UnixListener::bind fails with AddrInUse
    // otherwise, even though nothing is actually listening anymore.
    let _ = std::fs::remove_file(socket_path);
    let listener = UnixListener::bind(socket_path)?;
    tracing::info!(
        socket_path,
        worker = %state.manifest.name,
        tools = state.tools.len(),
        "terminus-worker-sdk: listening"
    );

    loop {
        let (stream, _addr) = listener.accept().await?;
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, state).await {
                tracing::warn!("terminus-worker-sdk: connection error: {e}");
            }
        });
    }
}

async fn handle_connection(stream: UnixStream, state: Arc<WorkerState>) -> std::io::Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            // EOF -- peer closed the connection.
            return Ok(());
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let response = dispatch_line(trimmed, &state).await;
        if let Some(resp) = response {
            let mut out = serde_json::to_vec(&resp).unwrap_or_default();
            out.push(b'\n');
            write_half.write_all(&out).await?;
        }
    }
}

/// Parse one JSON-RPC request line and dispatch it, returning `None` for a
/// notification (no `"id"`) per JSON-RPC 2.0.
async fn dispatch_line(line: &str, state: &WorkerState) -> Option<Value> {
    let req: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            return Some(json!({
                "jsonrpc": "2.0",
                "id": Value::Null,
                "error": {"code": -32700, "message": format!("Parse error: {e}")}
            }))
        }
    };

    let id = req.get("id").cloned();
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);

    let id = id?; // notification -- no response

    let result = match method {
        "initialize" => Ok(initialize_result(state)),
        "tools/list" => Ok(tools_list_result(state)),
        "tools/call" => tools_call_result(state, &params).await,
        other => Err((-32601, format!("Method not found: {other}"))),
    };

    Some(match result {
        Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
        Err((code, message)) => {
            json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
        }
    })
}

fn initialize_result(state: &WorkerState) -> Value {
    json!({
        "protocolVersion": "2024-11-05",
        "capabilities": {"tools": {"listChanged": false}},
        "serverInfo": {"name": state.manifest.name, "version": state.manifest.semver},
        "manifest": {
            "name": state.manifest.name,
            "semver": state.manifest.semver,
            "capabilityClass": state.manifest.capability_class,
            "tools": tool_infos(state).iter().map(tool_info_json).collect::<Vec<_>>(),
        }
    })
}

fn tools_list_result(state: &WorkerState) -> Value {
    let tools: Vec<Value> = tool_infos(state).iter().map(tool_info_json).collect();
    json!({"tools": tools})
}

fn tool_infos(state: &WorkerState) -> Vec<ToolInfo> {
    state
        .tools
        .values()
        .map(|t| ToolInfo {
            name: t.name().to_string(),
            description: t.description().to_string(),
            parameters: t.parameters(),
        })
        .collect()
}

fn tool_info_json(t: &ToolInfo) -> Value {
    json!({
        "name": t.name,
        "description": t.description,
        "inputSchema": t.parameters,
    })
}

async fn tools_call_result(state: &WorkerState, params: &Value) -> Result<Value, (i64, String)> {
    let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let arguments = params.get("arguments").cloned().unwrap_or(json!({}));

    match state.tools.get(name) {
        Some(tool) => match tool.execute_structured(arguments).await {
            Ok(output) => {
                let mut result = json!({
                    "content": [{"type": "text", "text": output.text}],
                    "isError": false
                });
                if let Some(structured) = output.structured {
                    result["structuredContent"] = structured;
                }
                Ok(result)
            }
            Err(e) => Ok(tool_error_result(e)),
        },
        None => Ok(json!({
            "content": [{"type": "text", "text": format!("Unknown tool: {name}")}],
            "isError": true
        })),
    }
}

fn tool_error_result(e: ToolError) -> Value {
    json!({
        "content": [{"type": "text", "text": e.to_string()}],
        "isError": true
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::WorkerManifest;
    use terminus_rs::tool::ToolOutput;
    use tokio::io::BufReader;
    use tokio::net::UnixStream;

    struct Echo;

    #[async_trait::async_trait]
    impl RustTool for Echo {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "Echoes its input back"
        }
        fn parameters(&self) -> Value {
            json!({"type": "object", "properties": {"text": {"type": "string"}}})
        }
        async fn execute(&self, args: Value) -> Result<String, ToolError> {
            Ok(args.get("text").and_then(Value::as_str).unwrap_or("").to_string())
        }
        async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
            let text = args.get("text").and_then(Value::as_str).unwrap_or("").to_string();
            Ok(ToolOutput::with_structured(text.clone(), json!({"echoed": text})))
        }
    }

    struct Failing;

    #[async_trait::async_trait]
    impl RustTool for Failing {
        fn name(&self) -> &str {
            "failing"
        }
        fn description(&self) -> &str {
            "Always fails"
        }
        fn parameters(&self) -> Value {
            json!({"type": "object", "properties": {}})
        }
        async fn execute(&self, _args: Value) -> Result<String, ToolError> {
            Err(ToolError::Execution("boom".to_string()))
        }
    }

    fn test_state(tools: Vec<Box<dyn RustTool>>) -> Arc<WorkerState> {
        let mut map: HashMap<String, Box<dyn RustTool>> = HashMap::new();
        let mut infos = Vec::new();
        for t in tools {
            infos.push(ToolInfo {
                name: t.name().to_string(),
                description: t.description().to_string(),
                parameters: t.parameters(),
            });
            map.insert(t.name().to_string(), t);
        }
        Arc::new(WorkerState {
            manifest: WorkerManifest {
                name: "test-worker".to_string(),
                semver: "0.1.0".to_string(),
                capability_class: "core".to_string(),
                tools: infos,
            },
            tools: map,
        })
    }

    fn temp_socket_path(tag: &str) -> String {
        let dir = std::env::temp_dir();
        format!(
            "{}/terminus-worker-sdk-test-{}-{}.sock",
            dir.display(),
            tag,
            std::process::id()
        )
    }

    async fn roundtrip(stream: &mut UnixStream, request: Value) -> Value {
        let payload = format!("{}\n", request);
        stream.write_all(payload.as_bytes()).await.unwrap();
        let (read_half, write_half) = stream.split();
        let mut reader = BufReader::new(read_half);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let _ = write_half; // keep split alive for the duration of the read
        serde_json::from_str(line.trim()).unwrap()
    }

    #[tokio::test]
    async fn initialize_returns_manifest() {
        let socket_path = temp_socket_path("init");
        let state = test_state(vec![Box::new(Echo)]);
        let listener_socket = socket_path.clone();
        tokio::spawn(async move {
            let _ = serve(&listener_socket, state).await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut stream = UnixStream::connect(&socket_path).await.unwrap();
        let resp = roundtrip(
            &mut stream,
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}),
        )
        .await;

        assert_eq!(resp["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(resp["result"]["serverInfo"]["name"], "test-worker");
        assert_eq!(resp["result"]["manifest"]["name"], "test-worker");
        assert_eq!(resp["result"]["manifest"]["semver"], "0.1.0");
        assert_eq!(resp["result"]["manifest"]["capabilityClass"], "core");
        assert_eq!(resp["result"]["manifest"]["tools"][0]["name"], "echo");

        let _ = std::fs::remove_file(&socket_path);
    }

    #[tokio::test]
    async fn tools_list_and_call_roundtrip() {
        let socket_path = temp_socket_path("call");
        let state = test_state(vec![Box::new(Echo)]);
        let listener_socket = socket_path.clone();
        tokio::spawn(async move {
            let _ = serve(&listener_socket, state).await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut stream = UnixStream::connect(&socket_path).await.unwrap();

        let list_resp = roundtrip(
            &mut stream,
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
        )
        .await;
        assert_eq!(list_resp["result"]["tools"][0]["name"], "echo");

        let call_resp = roundtrip(
            &mut stream,
            json!({
                "jsonrpc": "2.0", "id": 3, "method": "tools/call",
                "params": {"name": "echo", "arguments": {"text": "hi"}}
            }),
        )
        .await;
        assert_eq!(call_resp["result"]["content"][0]["text"], "hi");
        assert_eq!(call_resp["result"]["isError"], false);
        assert_eq!(call_resp["result"]["structuredContent"]["echoed"], "hi");

        let _ = std::fs::remove_file(&socket_path);
    }

    #[tokio::test]
    async fn unknown_tool_is_a_tool_error_not_a_protocol_error() {
        let socket_path = temp_socket_path("unknown");
        let state = test_state(vec![]);
        let listener_socket = socket_path.clone();
        tokio::spawn(async move {
            let _ = serve(&listener_socket, state).await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut stream = UnixStream::connect(&socket_path).await.unwrap();
        let resp = roundtrip(
            &mut stream,
            json!({
                "jsonrpc": "2.0", "id": 4, "method": "tools/call",
                "params": {"name": "nope", "arguments": {}}
            }),
        )
        .await;
        assert_eq!(resp["result"]["isError"], true);
        assert!(resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Unknown tool"));

        let _ = std::fs::remove_file(&socket_path);
    }

    #[tokio::test]
    async fn tool_execution_error_surfaces_as_tool_error() {
        let socket_path = temp_socket_path("failing");
        let state = test_state(vec![Box::new(Failing)]);
        let listener_socket = socket_path.clone();
        tokio::spawn(async move {
            let _ = serve(&listener_socket, state).await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut stream = UnixStream::connect(&socket_path).await.unwrap();
        let resp = roundtrip(
            &mut stream,
            json!({
                "jsonrpc": "2.0", "id": 5, "method": "tools/call",
                "params": {"name": "failing", "arguments": {}}
            }),
        )
        .await;
        assert_eq!(resp["result"]["isError"], true);
        assert!(resp["result"]["content"][0]["text"].as_str().unwrap().contains("boom"));

        let _ = std::fs::remove_file(&socket_path);
    }

    #[tokio::test]
    async fn unknown_method_is_a_protocol_error() {
        let socket_path = temp_socket_path("method");
        let state = test_state(vec![]);
        let listener_socket = socket_path.clone();
        tokio::spawn(async move {
            let _ = serve(&listener_socket, state).await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut stream = UnixStream::connect(&socket_path).await.unwrap();
        let resp = roundtrip(
            &mut stream,
            json!({"jsonrpc": "2.0", "id": 6, "method": "bogus/method"}),
        )
        .await;
        assert_eq!(resp["error"]["code"], -32601);

        let _ = std::fs::remove_file(&socket_path);
    }
}
