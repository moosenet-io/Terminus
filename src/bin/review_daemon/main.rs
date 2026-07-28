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
mod egress_proxy;
mod http;
mod provider;
mod resolve;
mod sandbox;

use provider::Provider;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;

struct AppState {
    token: String,
    sanitized_env: HashMap<String, String>,
    /// Each provider's ABSOLUTE resolved binary path, cached once at startup.
    /// `None` means the binary wasn't found on PATH at boot -- reported as
    /// `binary_not_found` for every request without re-checking the
    /// filesystem. Spawning always uses this cached absolute path (never
    /// `Command::new("claude")` by bare name), so PATH mutations or
    /// TOCTOU-style swaps after startup can't change which binary actually
    /// runs. Also holds the resolved path for [`sandbox::BWRAP_BIN`], keyed
    /// the same way -- `agy` dispatch is unavailable (fail-closed) if that
    /// key is `None`.
    resolved: HashMap<&'static str, Option<std::path::PathBuf>>,
    semaphore: Arc<Semaphore>,
    /// Loopback port of the `egress_proxy` accept loop spawned at startup.
    /// `None` means the proxy failed to bind -- `agy` dispatch must be
    /// treated as unavailable in that case (fail-closed: agy is never
    /// dispatched unproxied). Not used by `opus`/`codex`.
    agy_proxy_port: Option<u16>,
    /// `$HOME` at daemon startup, used only to scope the two filesystem
    /// binds the `agy` sandbox needs (see `sandbox.rs`). Never caller input.
    home_dir: String,
    /// `$HOME/.gemini/antigravity-cli` (agy's own app-data/cache dir).
    gemini_cache_dir: String,
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
    for p in [Provider::Opus, Provider::Codex, Provider::Agy, Provider::Fable] {
        resolved.insert(p.as_str(), resolve::resolve_on_path(p.binary()));
    }
    // bwrap is agy's sandbox wrapper -- resolved once at startup exactly like
    // the provider binaries, never re-resolved per request.
    resolved.insert(sandbox::BWRAP_BIN, resolve::resolve_on_path(sandbox::BWRAP_BIN));
    for (name, path) in &resolved {
        tracing::info!(
            provider = name,
            found = path.is_some(),
            resolved_path = ?path,
            "review-daemon: startup binary resolution"
        );
    }

    // The agy egress proxy (see egress_proxy.rs) is started once, for the
    // life of the process. If it fails to bind, agy dispatch fails closed
    // (reports unavailable) rather than ever running agy unproxied.
    let agy_proxy_port = match egress_proxy::spawn().await {
        Ok(port) => {
            tracing::info!(port, "review-daemon: agy egress proxy listening");
            Some(port)
        }
        Err(e) => {
            tracing::error!("review-daemon: agy egress proxy failed to bind: {e} -- agy dispatch will report unavailable");
            None
        }
    };

    let home_dir = std::env::var("HOME").unwrap_or_default();
    let gemini_cache_dir = format!("{home_dir}/.gemini/antigravity-cli");

    let state = Arc::new(AppState {
        token: cfg.token,
        sanitized_env: sanitize_env_once(),
        resolved,
        semaphore: Arc::new(Semaphore::new(config::MAX_CONCURRENCY)),
        agy_proxy_port,
        home_dir,
        gemini_cache_dir,
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
    /// Epic-capstone explore mode: the claude slots get read-only tools + a repo
    /// cwd. Defaults off (routine reviews are unchanged).
    #[serde(default)]
    explore: bool,
    /// Progress/stall window (secs): kill a provider only after this many secs of
    /// NO output. `None` → pure wall-clock (`timeout_secs`), the pre-capstone behavior.
    #[serde(default)]
    stall_secs: Option<u64>,
    /// Repo checkout the explore-mode auditors may read from (the subprocess cwd).
    #[serde(default)]
    repo_path: Option<String>,
    /// REVCAP-01 PART B: requested reasoning/thinking effort (e.g. `"high"`) for
    /// an intensive-substitute review. `None` (the default, and every pre-PART-B
    /// caller) reproduces the exact pre-PART-B argv -- see
    /// `provider::build_command`'s doc.
    #[serde(default)]
    reasoning_effort: Option<String>,
    /// REVX-07/08: an explicit provider-native model override (currently only
    /// meaningful for `codex` -- its dynamic GPT-5.6 sol/terra/luna tier
    /// selection). Validated against `config::clamp_codex_model`'s closed
    /// allowlist before it ever reaches `build_command_with_model`; an
    /// unrecognized value drops to `None` (the fixed `CODEX_MODEL` default).
    #[serde(default)]
    model: Option<String>,
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

    let Some(Some(resolved_path)) = state.resolved.get(parsed.provider.as_str()) else {
        return (
            502,
            "Bad Gateway",
            serde_json::json!({
                "error": "binary_not_found",
                "detail": format!("'{}' binary was not found on PATH at daemon startup", parsed.provider.binary()),
            }),
        );
    };

    // Explore-mode cwd: only honored when `explore` is set. Must be an EXISTING
    // directory (canonicalized); an absent/invalid path silently falls back to no
    // cwd (the auditor still runs, just without repo read access) rather than
    // failing the dispatch. Read-only tools mean the cwd only grants READ access.
    let cwd: Option<std::path::PathBuf> = if parsed.explore {
        parsed
            .repo_path
            .as_deref()
            .and_then(|p| std::fs::canonicalize(p).ok())
            .filter(|p| p.is_dir())
    } else {
        None
    };
    let stall = StallConfig {
        timeout_secs,
        stall_secs: parsed.stall_secs.map(config::clamp_stall),
    };

    // Bounded concurrency: at most MAX_CONCURRENCY subprocesses in flight.
    let _permit = state.semaphore.acquire().await;

    // REVCAP-01 PART B / REVX-07: validate against the PER-PROVIDER closed
    // allowlist -- an unrecognized/blank/absent value, or a level the
    // requested provider doesn't support (e.g. `xhigh` for claude), drops to
    // `None` (the pre-PART-B argv shape), never forwarded verbatim into a
    // provider's own config syntax.
    let reasoning_effort =
        config::clamp_reasoning_effort_for(parsed.provider, parsed.reasoning_effort.as_deref());
    // REVX-08: same closed-allowlist treatment for a codex model override.
    let model = config::clamp_codex_model(parsed.model.as_deref());

    match run_provider(
        parsed.provider,
        resolved_path,
        &parsed.prompt,
        parsed.explore,
        reasoning_effort.as_deref(),
        model.as_deref(),
        &stall,
        cwd.as_deref(),
        &state.sanitized_env,
        &state,
    )
    .await
    {
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
///
/// `resolved_path` is the ABSOLUTE path cached in `AppState` at startup --
/// spawning always uses this, never `Command::new(provider.binary())` by bare
/// name, so PATH changes after startup can't change which binary runs.
/// How a subprocess run is time-bounded. `timeout_secs` is the wall-clock BACKSTOP
/// (always enforced). When `stall_secs` is `Some(n)`, the primary bound is PROGRESS:
/// AFTER the first output, the run is killed only after `n` secs with NO new output.
/// Before ANY output it is bounded solely by the wall-clock backstop — so a live
/// provider that thinks silently for a while (LLM CLIs commonly buffer output; a
/// whole-repo Fable audit can be silent for minutes before its first token) is never
/// mistaken for a stall. `None` → pure wall-clock (pre-capstone behavior).
#[derive(Clone, Copy)]
struct StallConfig {
    timeout_secs: u64,
    stall_secs: Option<u64>,
}

async fn run_provider(
    provider: Provider,
    resolved_path: &std::path::Path,
    prompt: &str,
    explore: bool,
    reasoning_effort: Option<&str>,
    model: Option<&str>,
    stall: &StallConfig,
    cwd: Option<&std::path::Path>,
    env: &HashMap<String, String>,
    state: &AppState,
) -> Result<String, (&'static str, String)> {
    let built = provider::build_command_with_model(provider, prompt, explore, reasoning_effort, model);

    // agy is the only provider that runs inside the bwrap sandbox (see
    // sandbox.rs / egress_proxy.rs for why: agy's own tool-approval gate is
    // bypassed by --dangerously-skip-permissions, so adversarial prompt
    // content could otherwise trick it into a real file/network action).
    // opus (--tools "") and codex (--sandbox read-only) already close this
    // off for their own providers and are spawned directly, unchanged.
    let (spawn_binary_path, spawn_built): (std::borrow::Cow<'_, std::path::Path>, provider::BuiltCommand) =
        if matches!(provider, Provider::Agy) {
            let Some(Some(bwrap_path)) = state.resolved.get(sandbox::BWRAP_BIN) else {
                return Err((
                    "binary_not_found",
                    "'bwrap' sandbox helper was not found on PATH at daemon startup -- agy dispatch is unavailable without it".to_string(),
                ));
            };
            let Some(proxy_port) = state.agy_proxy_port else {
                return Err((
                    "other",
                    "agy egress proxy is unavailable (failed to bind at startup) -- refusing to dispatch agy unproxied".to_string(),
                ));
            };
            let wrapped_args = sandbox::wrap_agy(
                resolved_path,
                &built.args,
                &state.home_dir,
                &state.gemini_cache_dir,
                proxy_port,
            );
            (
                std::borrow::Cow::Borrowed(bwrap_path.as_path()),
                provider::BuiltCommand {
                    binary: sandbox::BWRAP_BIN,
                    args: wrapped_args,
                    // Preserve whatever build_command(Agy) actually produced
                    // (currently always None -- agy has no --output-* temp
                    // file the way codex does) rather than hardcoding None,
                    // so this doesn't silently stop cleaning up such a file
                    // if a future agy provider variant ever gains one.
                    output_path: built.output_path.clone(),
                    // TERM #495: an over-large agy prompt is delivered on stdin
                    // rather than argv. bwrap forwards its own stdin to the
                    // sandboxed child, so carry the payload through the wrapper
                    // unchanged (the spawn site pipes it to bwrap's stdin).
                    stdin_prompt: built.stdin_prompt.clone(),
                },
            )
        } else {
            (std::borrow::Cow::Borrowed(resolved_path), built)
        };

    // agy's OAuth access-token refresh races when agy runs concurrently or in
    // rapid succession -- route it through the serialize+retry wrapper. Every
    // other provider spawns directly, unchanged.
    let result = if matches!(provider, Provider::Agy) {
        // agy is sandboxed (bwrap) and never explore-mode → no cwd.
        run_agy_with_retry(&spawn_built, &spawn_binary_path, stall, env).await
    } else {
        run_built_command(&spawn_built, &spawn_binary_path, stall, cwd, env).await
    };

    // Clean up the codex --output-last-message temp file on EVERY exit path
    // (timeout, spawn failure, non-zero exit, empty output, success) -- not
    // just the success path -- so a failing codex run never leaks a file
    // under the daemon's temp dir.
    if let Some(path) = &spawn_built.output_path {
        let _ = tokio::fs::remove_file(path).await;
    }

    result
}

/// Process-global mutex serializing `agy` spawns. agy's OAuth access-token
/// refresh races when two agy processes run at once (each refresh consults the
/// same on-disk refresh token + Google's token endpoint concurrently), so agy
/// -- and ONLY agy -- is serialized daemon-wide. opus/codex are unaffected and
/// keep running concurrently under the normal `AppState::semaphore` cap.
fn agy_serialize_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// Number of EXTRA agy attempts (beyond the first) on an auth-transient.
const AGY_AUTH_RETRIES: usize = 2;
/// Base backoff between agy retries; attempt N sleeps N * this (4s, then 8s)
/// so agy's OAuth refresh state settles before the next try.
const AGY_RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_secs(4);

/// Whether an agy failure `detail` matches the TRANSIENT OAuth-refresh signature
/// a retry can plausibly clear -- agy's own "Authentication required / please
/// visit the URL to log in / .../o/oauth2/auth" message. A genuine hard error
/// (missing binary, timeout, real credential expiry that keeps repeating) does
/// NOT loop forever: retries are bounded by [`AGY_AUTH_RETRIES`].
fn is_agy_auth_transient(detail: &str) -> bool {
    let d = detail.to_ascii_lowercase();
    d.contains("authentication required")
        || d.contains("please visit the url to log in")
        || d.contains("o/oauth2/auth")
}

/// The error RETURNED when agy's auth-transient persists through every retry.
/// Deliberately a FIXED, URL-free message: the raw agy `detail` embeds a Google
/// OAuth login URL (client_id / redirect_uri / code_challenge query params), and
/// returning it verbatim would propagate that auth material into the caller's
/// `/dispatch` response body (and any log that records provider errors). We keep
/// the classification, drop the URL.
fn agy_auth_exhausted_error() -> (&'static str, String) {
    (
        "other",
        format!("agy auth-transient persisted after {AGY_AUTH_RETRIES} retries (login URL redacted)"),
    )
}

/// Run agy under the serialize lock, retrying a bounded number of times on the
/// auth-transient (see [`is_agy_auth_transient`]) with escalating backoff. An
/// auth-transient is a FAST failure (agy exits rc=1 in seconds, not the full
/// timeout), so even the worst case stays well under the caller's dispatch
/// timeout. Any non-transient error (or success) returns immediately.
///
/// NOTE on the time budget: `timeout_secs` is applied PER ATTEMPT (each
/// `run_built_command` call gets the full budget), so a pathological run that
/// somehow hit the timeout on every attempt could take up to
/// `(AGY_AUTH_RETRIES + 1) * timeout_secs` plus the backoffs. That is a hard
/// upper bound (never unbounded), and in practice the auth-transient fails in a
/// few seconds -- a genuine per-attempt timeout is a different, non-transient
/// error kind (`"timeout"`), which `is_agy_auth_transient` excludes, so it is
/// returned immediately without consuming a retry.
async fn run_agy_with_retry(
    built: &provider::BuiltCommand,
    resolved_path: &std::path::Path,
    stall: &StallConfig,
    env: &HashMap<String, String>,
) -> Result<String, (&'static str, String)> {
    let _guard = agy_serialize_lock().lock().await;
    let mut attempt = 0usize;
    loop {
        // agy never runs explore-mode → no cwd.
        let result = run_built_command(built, resolved_path, stall, None, env).await;
        let is_transient = matches!(&result, Err((_, d)) if is_agy_auth_transient(d));
        if is_transient && attempt < AGY_AUTH_RETRIES {
            attempt += 1;
            // Log the CLASSIFICATION + attempt only -- never the raw `detail`.
            // agy's auth-transient text embeds a Google OAuth login URL whose
            // query string (client_id, redirect_uri, code_challenge, ...) is
            // auth material that must not be expanded into the daemon's logs.
            tracing::warn!(
                attempt,
                "review-daemon: agy OAuth auth-transient (detail redacted), retrying after backoff"
            );
            tokio::time::sleep(AGY_RETRY_BACKOFF * attempt as u32).await;
            continue;
        }
        if is_transient {
            // Retries exhausted: return the REDACTED error, never the raw detail
            // (which still carries the OAuth login URL) up to the caller/response.
            return Err(agy_auth_exhausted_error());
        }
        // Success, or a non-transient error (timeout / binary_not_found /
        // empty_output / other) -- return as-is, immediately.
        return result;
    }
}

/// Core spawn-and-collect logic, split out from [`run_provider`] so the
/// caller can run cleanup (temp-file removal) unconditionally on every exit
/// path, including the early-return error cases here.
/// Drain a child pipe into `buf`, stamping `last_ms` (elapsed-ms since `start`) on
/// every chunk so the watchdog can tell "making progress" from "stalled". Ends at
/// EOF or on a read error (both mean the pipe is done).
async fn pump_pipe<R>(
    pipe: Option<R>,
    buf: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    last_ms: std::sync::Arc<std::sync::atomic::AtomicU64>,
    start: std::time::Instant,
) where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt as _;
    let Some(mut r) = pipe else { return };
    let mut chunk = [0u8; 8192];
    loop {
        match r.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if let Ok(mut b) = buf.lock() {
                    b.extend_from_slice(&chunk[..n]);
                }
                // Store elapsed-ms + 1 so 0 is an UNAMBIGUOUS "no output yet"
                // sentinel: a first chunk read within the first millisecond
                // (elapsed_ms == 0) must not be mistaken for "never spoke", or a
                // print-then-wedge provider would escape stall detection.
                let stamp = (start.elapsed().as_millis() as u64).saturating_add(1);
                last_ms.store(stamp, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }
}

async fn run_built_command(
    built: &provider::BuiltCommand,
    resolved_path: &std::path::Path,
    stall: &StallConfig,
    cwd: Option<&std::path::Path>,
    env: &HashMap<String, String>,
) -> Result<String, (&'static str, String)> {
    let mut command = tokio::process::Command::new(resolved_path);
    command
        .args(&built.args)
        .env_clear()
        .envs(env)
        // TERM #495: an over-large prompt (a big diff) is delivered on the child's
        // stdin instead of as an argv element (which would overflow ARG_MAX and
        // fail spawn()). Pipe stdin only in that case; the normal path keeps the
        // exact `Stdio::null()` behavior as before.
        .stdin(if built.stdin_prompt.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // kill_on_drop so an early return / dropped future never leaves an orphan
        // subprocess running after the daemon released its concurrency permit.
        .kill_on_drop(true);
    // Explore mode only: run in the repo checkout so read-only tools can read it.
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }

    let mut child = command
        .spawn()
        .map_err(|e| ("other", format!("failed to spawn {}: {e}", built.binary)))?;

    // TERM #495: feed the over-large prompt to the child on stdin, then close it
    // (EOF) so the CLI stops reading and begins its review. A big-diff payload can
    // exceed the OS pipe buffer, so write from a SPAWNED task (concurrent with the
    // stdout/stderr pumps below) -- a blocking write here, before the readers
    // start, could deadlock if the child writes to stdout while its stdin pipe is
    // still full. Dropping the handle after the write closes the pipe.
    if let Some(payload) = built.stdin_prompt.clone() {
        if let Some(mut child_stdin) = child.stdin.take() {
            tokio::spawn(async move {
                use tokio::io::AsyncWriteExt as _;
                let _ = child_stdin.write_all(payload.as_bytes()).await;
                let _ = child_stdin.shutdown().await;
                drop(child_stdin);
            });
        }
    }

    // Drain both pipes concurrently, stamping last-output time — so a chatty child
    // never deadlocks on a full pipe AND the watchdog can distinguish progress from
    // a stall. Buffers + activity clock are shared with the reader tasks.
    let start = std::time::Instant::now();
    let last_ms = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let out_buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let err_buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let out_task = tokio::spawn(pump_pipe(child.stdout.take(), out_buf.clone(), last_ms.clone(), start));
    let err_task = tokio::spawn(pump_pipe(child.stderr.take(), err_buf.clone(), last_ms.clone(), start));

    // Wait for exit, watching for a stall (no output for `stall_secs`) and a
    // wall-clock backstop (`timeout_secs`). `child.wait()` is cancel-safe, so
    // re-entering it after each watchdog tick is fine.
    let tick = std::time::Duration::from_secs(2);
    let exit_status = loop {
        tokio::select! {
            waited = child.wait() => {
                break waited.map_err(|e| ("other", format!("{}: {e}", built.binary)))?;
            }
            _ = tokio::time::sleep(tick) => {
                let elapsed = start.elapsed().as_secs();
                if elapsed >= stall.timeout_secs {
                    let _ = child.kill().await;
                    return Err((
                        "timeout",
                        format!("{} timed out after {}s (wall-clock backstop)", built.binary, stall.timeout_secs),
                    ));
                }
                if let Some(stall_window) = stall.stall_secs {
                    // Only enforce stall detection AFTER the first output. A live
                    // provider that is legitimately silent while it works (many LLM
                    // CLIs buffer everything until the final answer, and a whole-repo
                    // audit can think for minutes before its first token) must NOT be
                    // killed as "stalled" — pre-first-output it is bounded only by the
                    // generous wall-clock backstop above. Once output has FLOWED, a
                    // subsequent `stall_window` of silence is a genuine stall.
                    let stamp = last_ms.load(std::sync::atomic::Ordering::Relaxed);
                    if stamp > 0 {
                        // Undo the +1 sentinel to recover the real elapsed-ms of the
                        // last output (see `pump_pipe`).
                        let last_out_ms = stamp - 1;
                        let idle = elapsed.saturating_sub(last_out_ms / 1000);
                        if idle >= stall_window {
                            let _ = child.kill().await;
                            return Err((
                                "timeout",
                                format!("{} stalled: no output for {stall_window}s after starting", built.binary),
                            ));
                        }
                    }
                }
            }
        }
    };

    // Child exited on its own — let the readers finish draining, then collect.
    let _ = out_task.await;
    let _ = err_task.await;
    let stdout_bytes = out_buf.lock().map(|b| b.clone()).unwrap_or_default();
    let stderr_bytes = err_buf.lock().map(|b| b.clone()).unwrap_or_default();

    if !exit_status.success() {
        let stderr = String::from_utf8_lossy(&stderr_bytes);
        let stdout = String::from_utf8_lossy(&stdout_bytes);
        let combined: String = format!("{stderr}\n{stdout}").trim().chars().take(500).collect();
        return Err((
            "other",
            format!("{} exited rc={}: {combined}", built.binary, exit_status.code().unwrap_or(-1)),
        ));
    }

    let text = if let Some(path) = &built.output_path {
        tokio::fs::read_to_string(path).await.unwrap_or_default().trim().to_string()
    } else {
        String::from_utf8_lossy(&stdout_bytes).trim().to_string()
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

    /// The codex-caught case: a process SILENT for longer than the stall window and
    /// THEN producing output must NOT be stall-killed (stall detection only applies
    /// after the first output). Uses a real subprocess.
    #[tokio::test]
    async fn stall_does_not_kill_a_silent_then_speaking_process() {
        let built = provider::BuiltCommand {
            binary: "sh",
            args: vec!["-c".into(), "sleep 2 && echo READY".into()],
            output_path: None,
            stdin_prompt: None,
        };
        let stall = StallConfig { timeout_secs: 30, stall_secs: Some(1) };
        let out = run_built_command(
            &built,
            std::path::Path::new("/bin/sh"),
            &stall,
            None,
            &HashMap::new(),
        )
        .await;
        assert_eq!(out.expect("silent-then-speaking must complete"), "READY");
    }

    /// The `last_ms==0` sentinel edge case: a provider that prints IMMEDIATELY
    /// (first output within the first ms → elapsed 0) and THEN wedges must still be
    /// stall-killed — not mistaken for "never spoke". The +1 sentinel guarantees this.
    #[tokio::test]
    async fn stall_kills_a_print_then_wedge_process() {
        let built = provider::BuiltCommand {
            binary: "sh",
            args: vec!["-c".into(), "printf READY; sleep 60".into()],
            output_path: None,
            stdin_prompt: None,
        };
        // stall window 1s, wall-clock backstop 30s: the stall detector (not the
        // backstop) must fire — fast — once output stops after the immediate print.
        let stall = StallConfig { timeout_secs: 30, stall_secs: Some(1) };
        let start = std::time::Instant::now();
        let out = run_built_command(
            &built,
            std::path::Path::new("/bin/sh"),
            &stall,
            None,
            &HashMap::new(),
        )
        .await;
        assert!(matches!(out, Err(("timeout", _))), "print-then-wedge must be stall-killed");
        assert!(start.elapsed().as_secs() < 15, "killed by the STALL window, not the wall-clock backstop");
    }

    /// The wall-clock backstop still kills a genuinely wedged (silent, endless) process.
    #[tokio::test]
    async fn wall_clock_backstop_kills_a_wedged_process() {
        let built = provider::BuiltCommand {
            binary: "sh",
            args: vec!["-c".into(), "sleep 30".into()],
            output_path: None,
            stdin_prompt: None,
        };
        let stall = StallConfig { timeout_secs: 1, stall_secs: Some(10) };
        let out = run_built_command(
            &built,
            std::path::Path::new("/bin/sh"),
            &stall,
            None,
            &HashMap::new(),
        )
        .await;
        assert!(matches!(out, Err(("timeout", _))), "wedged process must hit the backstop");
    }

    /// Shared test-state builder so each test only specifies what it cares
    /// about; defaults leave agy's sandbox prerequisites absent (bwrap
    /// unresolved, no proxy port) since most tests aren't exercising agy.
    fn test_state(resolved: HashMap<&'static str, Option<std::path::PathBuf>>) -> AppState {
        AppState {
            token: "t".into(),
            sanitized_env: HashMap::new(),
            resolved,
            semaphore: Arc::new(Semaphore::new(4)),
            agy_proxy_port: None,
            home_dir: "/tmp/test-home".into(),
            gemini_cache_dir: "/tmp/test-home/.gemini/antigravity-cli".into(),
        }
    }

    #[test]
    fn agy_auth_transient_matches_the_observed_oauth_signature() {
        // The exact failure surfaced live: agy exits rc=1 carrying this text.
        let real = "bwrap exited rc=1: 2 tcmalloc parameters.cc:586 ... \
                    Authentication required. Please visit the URL to log in:\n  \
                    https://accounts.google.com/o/oauth2/auth?access_type=offline&client_id=x";
        assert!(is_agy_auth_transient(real));
        // Case-insensitive on each key phrase.
        assert!(is_agy_auth_transient("AUTHENTICATION REQUIRED"));
        assert!(is_agy_auth_transient("redirected to /o/oauth2/auth?foo"));
        assert!(is_agy_auth_transient("Please visit the URL to log in: https://..."));
    }

    #[test]
    fn agy_auth_transient_does_not_match_hard_errors() {
        // These must NOT be treated as retryable -- retrying only prolongs them.
        assert!(!is_agy_auth_transient("agy timed out after 120s"));
        assert!(!is_agy_auth_transient("'bwrap' binary was not found on PATH at daemon startup"));
        assert!(!is_agy_auth_transient("bwrap exited rc=1: some unrelated segfault"));
        assert!(!is_agy_auth_transient("exited successfully but produced empty output"));
    }

    #[test]
    fn agy_exhausted_error_carries_no_oauth_url_or_query_material() {
        // The final returned error (after retries are exhausted) must NOT leak
        // the OAuth login URL / its query params into the caller's response.
        let (kind, msg) = agy_auth_exhausted_error();
        let low = msg.to_ascii_lowercase();
        assert!(!low.contains("http"), "must not contain a URL: {msg}");
        assert!(!low.contains("oauth"), "must not contain oauth material: {msg}");
        assert!(!low.contains("accounts.google"), "must not contain the login host: {msg}");
        assert!(!low.contains("client_id") && !low.contains("code_challenge"));
        assert!(low.contains("redacted"), "should state it was redacted: {msg}");
        assert_eq!(kind, "other");
    }

    #[tokio::test]
    async fn unrecognized_provider_string_is_a_clean_400_never_reaches_dispatch() {
        let state = Arc::new(test_state(HashMap::new()));
        let body = br#"{"provider": "gpt5", "prompt": "hi"}"#;
        let (status, _, json) = handle_dispatch(body, &state).await;
        assert_eq!(status, 400);
        assert_eq!(json["error"], "other");
    }

    #[tokio::test]
    async fn oversized_prompt_is_rejected_before_dispatch() {
        let state = Arc::new(test_state(HashMap::from([(
            "opus",
            Some(std::path::PathBuf::from("/bin/true")),
        )])));
        let huge = "a".repeat(config::MAX_PROMPT_BYTES + 1);
        let body = serde_json::json!({"provider": "opus", "prompt": huge}).to_string();
        let (status, _, json) = handle_dispatch(body.as_bytes(), &state).await;
        assert_eq!(status, 400);
        assert_eq!(json["error"], "other");
    }

    #[tokio::test]
    async fn missing_binary_reports_binary_not_found_without_spawning() {
        let state = Arc::new(test_state(HashMap::from([("opus", None)])));
        let body = serde_json::json!({"provider": "opus", "prompt": "hello"}).to_string();
        let (status, _, json) = handle_dispatch(body.as_bytes(), &state).await;
        assert_eq!(status, 502);
        assert_eq!(json["error"], "binary_not_found");
    }

    // ── agy sandbox fail-closed paths ────────────────────────────────────

    #[tokio::test]
    async fn agy_dispatch_fails_closed_when_bwrap_is_not_resolved() {
        // agy binary itself resolves fine, but bwrap does not -- agy
        // dispatch must refuse (fail-closed), never fall back to spawning
        // agy unsandboxed.
        let mut state = test_state(HashMap::from([(
            "agy",
            Some(std::path::PathBuf::from("/bin/true")),
        )]));
        state.agy_proxy_port = Some(12345);
        let state = Arc::new(state);
        let body = serde_json::json!({"provider": "agy", "prompt": "hello"}).to_string();
        let (status, _, json) = handle_dispatch(body.as_bytes(), &state).await;
        assert_eq!(status, 502);
        assert_eq!(json["error"], "binary_not_found");
        assert!(json["detail"].as_str().unwrap().contains("bwrap"));
    }

    #[tokio::test]
    async fn agy_dispatch_fails_closed_when_egress_proxy_is_unavailable() {
        // Both agy and bwrap resolve, but the egress proxy never bound --
        // agy must never be dispatched unproxied.
        let mut state = test_state(HashMap::from([
            ("agy", Some(std::path::PathBuf::from("/bin/true"))),
            (sandbox::BWRAP_BIN, Some(std::path::PathBuf::from("/usr/bin/bwrap"))),
        ]));
        state.agy_proxy_port = None;
        let state = Arc::new(state);
        let body = serde_json::json!({"provider": "agy", "prompt": "hello"}).to_string();
        let (status, _, json) = handle_dispatch(body.as_bytes(), &state).await;
        assert_eq!(status, 502);
        assert_eq!(json["error"], "other");
        assert!(json["detail"].as_str().unwrap().contains("proxy"));
    }
}
