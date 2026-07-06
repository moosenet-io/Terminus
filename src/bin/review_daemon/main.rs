//! review-daemon: a standalone HTTP daemon that shells out to CLI-based code
//! review providers (Claude/"opus", Codex, agy) on behalf of the Terminus
//! `review_run` tool (`src/review/mod.rs`).
//!
//! ## Why this exists
//! `src/tool.rs`'s `RustTool` contract forbids `execute()` from ever shelling
//! out or spawning subprocesses -- only typed HTTP (reqwest) or parameterized
//! SQL (sqlx) are allowed in-tool. Precedent: `src/dgem/mod.rs` wraps an
//! LLM backend behind a persistent local daemon reached over loopback HTTP for
//! exactly this reason. This daemon is the Rust-side equivalent for CLI-backed
//! review providers: it is the ONLY place in this codebase permitted to spawn
//! `claude`/`codex`/`agy` processes; `src/review/mod.rs` only ever talks to it
//! over HTTP.
//!
//! ## Security architecture (the actual point of this daemon)
//!   - `POST /dispatch` accepts ONLY `{"provider": "opus"|"codex"|"agy",
//!     "prompt": <string, <=200KB>, "timeout_secs": <u64, optional, <=600>}`.
//!   - `provider` is a closed Rust enum ([`provider::Provider`]) validated via
//!     serde deserialization. An unrecognized string is a 400 and NEVER
//!     reaches process-spawn code (see `provider.rs` tests).
//!   - Each provider's binary path/base command/model string are hardcoded
//!     constants in `provider.rs`, resolved ONCE at startup
//!     ([`resolve::resolve_on_path`]) and cached; a provider whose binary
//!     wasn't found at startup reports `binary_not_found` for every request
//!     without re-checking the filesystem.
//!   - Process spawning uses `tokio::process::Command::new(binary).args(...)`
//!     with an argv array -- never a shell string, never `sh -c`/`bash -c`.
//!   - No caller-supplied env vars are ever merged into a child process
//!     environment. `sanitize::sanitized_env()` computes a fixed, sanitized
//!     environment ONCE at startup (allowlist, then strip anything
//!     TOKEN/KEY/SECRET/PASSWORD/CREDENTIAL/AUTH or HARMONY_/INFISICAL_/
//!     PLANE_/GITEA_-prefixed), ported from Harmony's
//!     `harmony-core/src/providers/subprocess.rs`.
//!   - Binds `127.0.0.1` only. Port from `REVIEW_DAEMON_PORT` (operator env,
//!     never request-controlled).
//!   - Bearer-token auth: `REVIEW_DAEMON_TOKEN` must be set at startup or the
//!     daemon refuses to start (fail-closed, see `config.rs`). Every
//!     `/dispatch` call needs a matching `Authorization: Bearer <token>` or
//!     gets a 401.
//!   - Concurrency cap: a semaphore of [`config::MAX_CONCURRENCY`] (4)
//!     concurrent subprocess spawns.
//!   - A single failed review (timeout/binary_not_found/empty_output/other)
//!     never crashes the daemon process -- every path returns a structured
//!     error response.

mod config;
mod http;
mod provider;
mod resolve;

use provider::Provider;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;

struct AppState {
    token: String,
    sanitized_env: HashMap<String, String>,
    resolved: HashMap<&'static str, bool>,
    semaphore: Arc<Semaphore>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::try_init().ok();

    let cfg = match config::Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("review-daemon: fatal startup error: {e}");
            std::process::exit(1);
        }
    };

    // Resolve each provider's binary ONCE, at startup. Never re-resolved per
    // request -- a binary missing at boot stays "not found" for the process
    // lifetime.
    let mut resolved = HashMap::new();
    for p in [Provider::Opus, Provider::Codex, Provider::Agy] {
        resolved.insert(p.as_str(), resolve::resolve_on_path(p.binary()));
    }
    for (name, found) in &resolved {
        tracing::info!(provider = name, found, "review-daemon: startup binary resolution");
    }

    let state = Arc::new(AppState {
        token: cfg.token,
        sanitized_env: sanitize_env_once(),
        resolved,
        semaphore: Arc::new(Semaphore::new(config::MAX_CONCURRENCY)),
    });

    // Bind loopback ONLY -- never 0.0.0.0. Port is operator-controlled via
    // REVIEW_DAEMON_PORT, never derived from request content.
    let addr = format!("127.0.0.1:{}", cfg.port);
    let listener = match TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("review-daemon: failed to bind {addr}: {e}");
            std::process::exit(1);
        }
    };
    tracing::info!(addr, "review-daemon: listening");

    loop {
        let (stream, _peer) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                tracing::warn!("review-daemon: accept error: {e}");
                continue;
            }
        };
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, state).await {
                tracing::warn!("review-daemon: connection error: {e}");
            }
        });
    }
}

mod sanitize;

fn sanitize_env_once() -> HashMap<String, String> {
    sanitize::sanitized_env()
}

/// Constant-time byte comparison for the bearer token check -- avoids a
/// timing side-channel that a byte-by-byte `==`/short-circuit comparison
/// would leak (low severity here given loopback-only binding + a random
/// token, but a one-line fix for an auth check).
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod constant_time_eq_tests {
    use super::constant_time_eq;

    #[test]
    fn equal_slices_match() {
        assert!(constant_time_eq(b"abc123", b"abc123"));
    }

    #[test]
    fn different_content_same_length_does_not_match() {
        assert!(!constant_time_eq(b"abc123", b"abc124"));
    }

    #[test]
    fn different_length_does_not_match() {
        assert!(!constant_time_eq(b"short", b"longerstring"));
    }

    #[test]
    fn empty_slices_match() {
        assert!(constant_time_eq(b"", b""));
    }
}

async fn handle_connection(mut stream: TcpStream, state: Arc<AppState>) -> Result<(), String> {
    let req = match http::read_request(&mut stream, config::MAX_PROMPT_BYTES + 8192).await {
        Ok(r) => r,
        Err(http::ReadError::BodyTooLarge) => {
            let _ = http::write_json_response(
                &mut stream,
                400,
                "Bad Request",
                &serde_json::json!({"error": "other", "detail": "request body too large"}),
            )
            .await;
            return Ok(());
        }
        Err(e) => return Err(e.to_string()),
    };

    if req.method != "POST" || req.path != "/dispatch" {
        http::write_json_response(
            &mut stream,
            404,
            "Not Found",
            &serde_json::json!({"error": "other", "detail": "unknown route"}),
        )
        .await
        .map_err(|e| e.to_string())?;
        return Ok(());
    }

    // Bearer-token auth FIRST, before any body parsing / provider logic.
    // `constant_time_eq` avoids a timing side-channel on the token comparison
    // (loopback-only + a random 24-byte token makes this low-severity, but
    // it's a one-line fix for a bearer-auth check).
    let auth_ok = req
        .headers
        .get("authorization")
        .and_then(|v| v.trim().strip_prefix("Bearer "))
        .map(|presented| constant_time_eq(presented.as_bytes(), state.token.as_bytes()))
        .unwrap_or(false);
    if !auth_ok {
        http::write_json_response(
            &mut stream,
            401,
            "Unauthorized",
            &serde_json::json!({"error": "auth_required", "detail": "missing or invalid Authorization: Bearer token"}),
        )
        .await
        .map_err(|e| e.to_string())?;
        return Ok(());
    }

    let (status, reason, body) = handle_dispatch(&req.body, &state).await;
    http::write_json_response(&mut stream, status, reason, &body)
        .await
        .map_err(|e| e.to_string())
}

#[derive(serde::Deserialize)]
struct DispatchBody {
    provider: Provider,
    prompt: String,
    #[serde(default)]
    timeout_secs: Option<u64>,
}

/// Parse + validate + dispatch a `/dispatch` request body. Returns
/// (http_status, reason_phrase, json_body). Never panics -- every failure
/// mode is a structured response, so one bad/slow/erroring review never takes
/// the daemon process down.
async fn handle_dispatch(
    raw_body: &[u8],
    state: &Arc<AppState>,
) -> (u16, &'static str, serde_json::Value) {
    let parsed: DispatchBody = match serde_json::from_slice(raw_body) {
        Ok(v) => v,
        Err(e) => {
            // Covers the unrecognized-`provider`-string case: serde fails to
            // deserialize into the closed `Provider` enum, so this branch is
            // reached instead of any spawn logic ever running.
            return (
                400,
                "Bad Request",
                serde_json::json!({"error": "other", "detail": format!("invalid request body: {e}")}),
            );
        }
    };

    if parsed.prompt.as_bytes().len() > config::MAX_PROMPT_BYTES {
        return (
            400,
            "Bad Request",
            serde_json::json!({"error": "other", "detail": "prompt exceeds 200KB cap"}),
        );
    }
    if parsed.prompt.trim().is_empty() {
        return (
            400,
            "Bad Request",
            serde_json::json!({"error": "other", "detail": "prompt must not be empty"}),
        );
    }

    let timeout_secs = config::clamp_timeout(parsed.timeout_secs);

    if !state.resolved.get(parsed.provider.as_str()).copied().unwrap_or(false) {
        return (
            502,
            "Bad Gateway",
            serde_json::json!({
                "error": "binary_not_found",
                "detail": format!("'{}' binary was not found on PATH at daemon startup", parsed.provider.binary()),
            }),
        );
    }

    // Bounded concurrency: at most MAX_CONCURRENCY subprocesses in flight.
    let _permit = state.semaphore.acquire().await;

    match run_provider(parsed.provider, &parsed.prompt, timeout_secs, &state.sanitized_env).await {
        Ok(text) => (200, "OK", serde_json::json!({"text": text})),
        Err((kind, detail)) => (
            502,
            "Bad Gateway",
            serde_json::json!({"error": kind, "detail": detail}),
        ),
    }
}

/// Spawn the provider's CLI and return its clean reply text, or a
/// `(error_kind, detail)` pair. `error_kind` is one of
/// `"timeout"|"empty_output"|"other"` (`"binary_not_found"` is handled by the
/// caller before this is invoked, and `"auth_required"` before that).
async fn run_provider(
    provider: Provider,
    prompt: &str,
    timeout_secs: u64,
    env: &HashMap<String, String>,
) -> Result<String, (&'static str, String)> {
    let built = provider::build_command(provider, prompt);
    let result = run_built_command(&built, timeout_secs, env).await;

    // Clean up the codex --output-last-message temp file on EVERY exit path
    // (timeout, spawn failure, non-zero exit, empty output, success) -- not
    // just the success path -- so a failing codex run never leaks a file
    // under the daemon's temp dir.
    if let Some(path) = &built.output_path {
        let _ = tokio::fs::remove_file(path).await;
    }

    result
}

/// Core spawn-and-collect logic, split out from [`run_provider`] so the
/// caller can run cleanup (temp-file removal) unconditionally on every exit
/// path, including the early-return error cases here.
async fn run_built_command(
    built: &provider::BuiltCommand,
    timeout_secs: u64,
    env: &HashMap<String, String>,
) -> Result<String, (&'static str, String)> {
    let mut command = tokio::process::Command::new(built.binary);
    command
        .args(&built.args)
        .env_clear()
        .envs(env)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let output = tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), command.output())
        .await
        .map_err(|_| ("timeout", format!("{} timed out after {timeout_secs}s", built.binary)))?
        .map_err(|e| ("other", format!("failed to spawn {}: {e}", built.binary)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let combined: String = format!("{stderr}\n{stdout}").trim().chars().take(500).collect();
        return Err((
            "other",
            format!("{} exited rc={}: {combined}", built.binary, output.status.code().unwrap_or(-1)),
        ));
    }

    let text = if let Some(path) = &built.output_path {
        tokio::fs::read_to_string(path).await.unwrap_or_default().trim().to_string()
    } else {
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };

    if text.is_empty() {
        return Err((
            "empty_output",
            format!("{} exited successfully but produced empty output", built.binary),
        ));
    }

    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unrecognized_provider_string_is_a_clean_400_never_reaches_dispatch() {
        let state = Arc::new(AppState {
            token: "t".into(),
            sanitized_env: HashMap::new(),
            resolved: HashMap::new(),
            semaphore: Arc::new(Semaphore::new(4)),
        });
        let body = br#"{"provider": "gpt5", "prompt": "hi"}"#;
        let (status, _, json) = handle_dispatch(body, &state).await;
        assert_eq!(status, 400);
        assert_eq!(json["error"], "other");
    }

    #[tokio::test]
    async fn oversized_prompt_is_rejected_before_dispatch() {
        let state = Arc::new(AppState {
            token: "t".into(),
            sanitized_env: HashMap::new(),
            resolved: HashMap::from([("opus", true)]),
            semaphore: Arc::new(Semaphore::new(4)),
        });
        let huge = "a".repeat(config::MAX_PROMPT_BYTES + 1);
        let body = serde_json::json!({"provider": "opus", "prompt": huge}).to_string();
        let (status, _, json) = handle_dispatch(body.as_bytes(), &state).await;
        assert_eq!(status, 400);
        assert_eq!(json["error"], "other");
    }

    #[tokio::test]
    async fn missing_binary_reports_binary_not_found_without_spawning() {
        let state = Arc::new(AppState {
            token: "t".into(),
            sanitized_env: HashMap::new(),
            resolved: HashMap::from([("opus", false)]),
            semaphore: Arc::new(Semaphore::new(4)),
        });
        let body = serde_json::json!({"provider": "opus", "prompt": "hello"}).to_string();
        let (status, _, json) = handle_dispatch(body.as_bytes(), &state).await;
        assert_eq!(status, 502);
        assert_eq!(json["error"], "binary_not_found");
    }
}
