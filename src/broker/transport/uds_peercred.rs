//! T1 — UDS + `SO_PEERCRED` only, no TLS (TMOD-02).
//!
//! The weakest of the three tiers: a plain Unix Domain Socket, with the
//! kernel-attested peer uid (`SO_PEERCRED`, read via
//! [`tokio::net::UnixStream::peer_cred`] — no raw `libc::getsockopt` call
//! needed, tokio already exposes this on unix) checked against the worker's
//! configured `expected_uid` before any request is sent. No cryptographic
//! identity at all — same-host only, appropriate only for a `read_only`
//! worker (see [`super::MinTierPolicy`]; `crate::config`'s worker registry
//! refuses to register a `write_scoped`/`secret_holding` worker at this
//! tier).
//!
//! A peer-uid mismatch is fail-closed: the connection is dropped and NO
//! request is ever written to the socket.

use std::path::PathBuf;

use serde_json::Value;
use tokio::net::UnixStream;

use crate::error::ToolError;
use crate::tool::ToolOutput;

use super::{call_over, health_over, list_over, TransportError, WorkerTransport};

/// A T1 UDS+peercred transport to one worker.
pub struct UdsPeercredTransport {
    socket_path: PathBuf,
    /// The uid this worker's process is expected to run as. A connecting
    /// peer whose `SO_PEERCRED` uid doesn't match this is rejected before
    /// any request is written — see the module doc.
    expected_uid: u32,
}

impl UdsPeercredTransport {
    pub fn new(socket_path: impl Into<PathBuf>, expected_uid: u32) -> Self {
        Self { socket_path: socket_path.into(), expected_uid }
    }

    /// Dial the socket and verify `SO_PEERCRED` before returning the stream.
    /// [`TransportError::Unavailable`] when the socket can't be reached at
    /// all (absent, refused); [`TransportError::IdentityMismatch`] when it's
    /// reachable but the peer uid disagrees with `expected_uid`.
    async fn dial_verified(&self) -> Result<UnixStream, TransportError> {
        let stream = UnixStream::connect(&self.socket_path).await.map_err(|e| {
            TransportError::Unavailable(format!(
                "connecting to {}: {e}",
                self.socket_path.display()
            ))
        })?;

        let peer_cred = stream
            .peer_cred()
            .map_err(|e| TransportError::Protocol(format!("SO_PEERCRED read failed: {e}")))?;

        if peer_cred.uid() != self.expected_uid {
            return Err(TransportError::IdentityMismatch(format!(
                "peer uid {} does not match configured worker uid {}",
                peer_cred.uid(),
                self.expected_uid
            )));
        }

        Ok(stream)
    }
}

#[async_trait::async_trait]
impl WorkerTransport for UdsPeercredTransport {
    async fn connect(&self) -> Result<(), TransportError> {
        self.dial_verified().await.map(|_| ())
    }

    async fn call(&self, name: &str, args: Value) -> Result<ToolOutput, ToolError> {
        let stream = self
            .dial_verified()
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?;
        call_over(stream, name, args).await
    }

    async fn list(&self) -> Result<Vec<String>, TransportError> {
        let stream = self.dial_verified().await?;
        list_over(stream).await
    }

    async fn health(&self) -> bool {
        match self.dial_verified().await {
            Ok(stream) => health_over(stream).await,
            Err(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    /// Spawn a tiny in-process "worker" listening on `path`, answering every
    /// request with a fixed canned response until the listener is dropped.
    fn spawn_echo_worker(path: PathBuf) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let listener = UnixListener::bind(&path).expect("bind worker socket");
            loop {
                let (mut stream, _) = match listener.accept().await {
                    Ok(v) => v,
                    Err(_) => return,
                };
                tokio::spawn(async move {
                    let (r, mut w) = stream.split();
                    let mut reader = BufReader::new(r);
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let req: Value = serde_json::from_str(line.trim_end()).unwrap();
                    let resp = match req["op"].as_str() {
                        Some("call") => serde_json::json!({"ok": true, "text": "echo: hi"}),
                        Some("list") => serde_json::json!({"ok": true, "tools": ["echo"]}),
                        Some("health") => serde_json::json!({"ok": true, "healthy": true}),
                        _ => serde_json::json!({"ok": false, "error": "unknown op"}),
                    };
                    let _ = w.write_all(format!("{resp}\n").as_bytes()).await;
                });
            }
        })
    }

    fn current_uid() -> u32 {
        // Safe: getuid() has no failure mode.
        unsafe { libc::getuid() }
    }

    #[tokio::test]
    async fn round_trip_call_succeeds_when_peer_uid_matches() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sock_path = dir.path().join("worker.sock");
        let worker = spawn_echo_worker(sock_path.clone());
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let transport = UdsPeercredTransport::new(sock_path, current_uid());
        let out = transport
            .call("echo", serde_json::json!({"text": "hi"}))
            .await
            .expect("call should succeed against a matching-uid worker");
        assert_eq!(out.text, "echo: hi");

        let tools = transport.list().await.expect("list should succeed");
        assert_eq!(tools, vec!["echo".to_string()]);

        assert!(transport.health().await, "health should report true");

        worker.abort();
    }

    #[tokio::test]
    async fn peer_uid_mismatch_is_rejected_before_any_call() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sock_path = dir.path().join("worker.sock");
        let worker = spawn_echo_worker(sock_path.clone());
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        // A uid that cannot possibly be this test process's own uid.
        let wrong_uid = current_uid().wrapping_add(1);
        let transport = UdsPeercredTransport::new(sock_path, wrong_uid);

        let err = transport
            .call("echo", serde_json::json!({}))
            .await
            .expect_err("a uid mismatch must reject the call, not succeed");
        assert!(matches!(err, ToolError::Execution(msg) if msg.contains("identity mismatch")));

        assert!(!transport.health().await, "health must be false on identity mismatch");

        worker.abort();
    }

    #[tokio::test]
    async fn absent_socket_is_unavailable_not_a_panic() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sock_path = dir.path().join("does-not-exist.sock");
        let transport = UdsPeercredTransport::new(sock_path, current_uid());

        let err = transport
            .call("echo", serde_json::json!({}))
            .await
            .expect_err("an absent socket must error cleanly, not panic");
        assert!(matches!(err, ToolError::Execution(_)));
        assert!(!transport.health().await);

        let list_err = transport.list().await.expect_err("list must also error cleanly");
        assert!(matches!(list_err, TransportError::Unavailable(_)));
    }
}
