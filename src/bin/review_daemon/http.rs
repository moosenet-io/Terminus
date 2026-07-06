//! Minimal hand-rolled HTTP/1.1 server primitives.
//!
//! terminus-rs has no HTTP-server framework dependency (only `reqwest` as an
//! HTTP *client*, plus `tokio` with the `full` feature set) and the task this
//! daemon serves does not warrant pulling one in. This module implements just
//! enough of HTTP/1.1 to serve a single JSON endpoint: request-line + header
//! parsing, `Content-Length` body framing, and a JSON response writer. It
//! deliberately does not support keep-alive, chunked transfer-encoding, or
//! anything else `review-daemon` doesn't need.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

pub struct ParsedRequest {
    pub method: String,
    pub path: String,
    pub headers: std::collections::HashMap<String, String>,
    pub body: Vec<u8>,
}

#[derive(Debug)]
pub enum ReadError {
    Io(std::io::Error),
    Malformed(String),
    BodyTooLarge,
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadError::Io(e) => write!(f, "io error: {e}"),
            ReadError::Malformed(s) => write!(f, "malformed request: {s}"),
            ReadError::BodyTooLarge => write!(f, "request body too large"),
        }
    }
}

/// Read and parse one HTTP/1.1 request from `stream`. `max_body_bytes` bounds
/// how much body we are willing to buffer (defense against a caller sending an
/// oversized `Content-Length`); requests declaring a larger body are rejected
/// before we allocate/read that much.
pub async fn read_request(
    stream: &mut TcpStream,
    max_body_bytes: usize,
) -> Result<ParsedRequest, ReadError> {
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];

    // Read until we have the full header block (\r\n\r\n), bounded so a client
    // that never sends it can't grow this buffer unboundedly.
    let header_end = loop {
        if let Some(pos) = find_header_end(&buf) {
            break pos;
        }
        if buf.len() > 64 * 1024 {
            return Err(ReadError::Malformed("header block too large".into()));
        }
        let n = stream.read(&mut chunk).await.map_err(ReadError::Io)?;
        if n == 0 {
            return Err(ReadError::Malformed("connection closed before headers completed".into()));
        }
        buf.extend_from_slice(&chunk[..n]);
    };

    let header_text = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = header_text.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();
    if method.is_empty() || path.is_empty() {
        return Err(ReadError::Malformed("bad request line".into()));
    }

    let mut headers = std::collections::HashMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_lowercase(), v.trim().to_string());
        }
    }

    let content_length: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if content_length > max_body_bytes {
        return Err(ReadError::BodyTooLarge);
    }

    // `buf` already contains the header block; anything after `header_end + 4`
    // (past "\r\n\r\n") is the start of the body already read.
    let body_already = buf.len() - (header_end + 4);
    let mut body = buf[(header_end + 4)..].to_vec();
    if body_already < content_length {
        let remaining = content_length - body_already;
        let mut rest = vec![0u8; remaining];
        stream.read_exact(&mut rest).await.map_err(ReadError::Io)?;
        body.extend_from_slice(&rest);
    } else {
        body.truncate(content_length);
    }

    Ok(ParsedRequest { method, path, headers, body })
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Write a JSON response with the given status code/reason.
pub async fn write_json_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    body: &serde_json::Value,
) -> std::io::Result<()> {
    let payload = body.to_string();
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        payload.len(),
        payload
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_end_found_at_correct_offset() {
        let buf = b"POST /dispatch HTTP/1.1\r\nHost: x\r\n\r\n{\"a\":1}";
        let pos = find_header_end(buf).unwrap();
        assert_eq!(&buf[..pos], b"POST /dispatch HTTP/1.1\r\nHost: x");
    }

    #[test]
    fn header_end_absent_returns_none() {
        let buf = b"POST /dispatch HTTP/1.1\r\nHost: x\r\n";
        assert!(find_header_end(buf).is_none());
    }
}
