//! CONST-26: the constellation aggregation layer's activity feed.
//!
//! `GET /api/terminus/activity?limit=N` — a viewer-readable, PROTECTED
//! (`crate::constellation::mod::protected_router`) endpoint that tail-reads
//! [`crate::constellation::audit`]'s mutating-request JSONL sink and returns
//! the most recent entries as `{entries: [{ts, method, path, principal,
//! system}]}`, matching `constellation-web/src/lib/aggregationClient.ts`'s
//! `ActivityFeedResponse` contract.
//!
//! ## Never bodies
//! [`audit::ConstellationAuditEntry`] carries a `body_summary` field (already
//! S6-sanitized/truncated by [`audit::sanitize_body`] before it ever hits
//! disk) — this endpoint deliberately does NOT surface it. An activity feed
//! is "what happened" (who, what verb, what path, which system), never "what
//! was in the request" — even a sanitized summary is more than a browser
//! notification feed should ever need, and dropping the field here is a
//! second, independent line of defense on top of the sanitization already
//! applied at write time. The response is additionally passed through
//! [`mask::mask_response`] like every other `/api/*` body, in case a
//! `path`/`principal` value ever turns out to look secret-shaped.
//!
//! ## Efficient tail-read
//! The audit log is an append-only, potentially long-lived JSONL file — this
//! module must never read the whole thing into memory just to return the
//! last `limit` lines. [`tail_lines`] seeks from the END of the file and
//! reads fixed-size blocks backward, accumulating lines until it has
//! collected more than `limit` (a small over-read cushion so a few corrupt
//! trailing lines don't starve the response below what the caller asked
//! for) or reached the start of the file.
//!
//! ## Edge cases
//! - Missing file (never written yet, or rotated out from under a fresh
//!   read) → empty `entries: []`, `200 OK` — never an error status.
//! - Zero-length file → same: empty `200`.
//! - A corrupt/non-JSON line → skipped, counted (via `tracing::warn!`), and
//!   otherwise ignored — one bad line must never fail the whole read.
//! - Log rotated mid-read (the file this handler opened is truncated or
//!   replaced between `open` and `read` — e.g. an external log-rotation
//!   tool): every fs operation here is a fresh `std::fs::File::open` +
//!   `read`/`seek` per request (this module holds no long-lived file
//!   handle/watcher across requests), so the NEXT request simply reopens
//!   whatever is at the configured path now; within a single request, any
//!   I/O error encountered mid-tail is treated the same as "nothing more to
//!   read" (return what was collected so far) rather than surfacing a `500`.

use axum::extract::{RawQuery, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};
use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;

use crate::config;
use crate::constellation::{audit, mask};
use crate::mcp_server::McpServerState;

/// Block size (bytes) read backward from the end of the audit log per
/// iteration of [`tail_lines`]. Small enough that a typical request (a
/// couple hundred short JSONL lines) only touches a handful of blocks near
/// the end of the file, never the whole thing.
const TAIL_BLOCK_SIZE: u64 = 8 * 1024;

/// `GET /api/terminus/activity?limit=N` handler. `limit` is parsed from the
/// raw query string (matching this crate's existing `RawQuery` convention —
/// see `crate::constellation::proxy`); a missing/unparsable/zero value falls
/// back to [`config::constellation_activity_tail_limit`], which ALSO acts as
/// the hard ceiling a caller-supplied value can never exceed (a caller may
/// ask for fewer entries than the configured cap, never more).
pub async fn handle_activity(State(_state): State<Arc<McpServerState>>, RawQuery(query): RawQuery) -> Response {
    let cap = config::constellation_activity_tail_limit();
    let limit = parse_limit(query.as_deref()).map(|n| n.min(cap)).unwrap_or(cap).max(1);

    let path = config::constellation_audit_log_path();
    let entries = match tail_lines(&path, limit) {
        Ok(lines) => parse_entries(lines, limit),
        Err(e) => {
            // Missing file, permission error, or any other I/O failure
            // reading the audit log — degrade to an empty feed rather than
            // failing the request (see this module's doc: "never an error
            // status").
            tracing::warn!("constellation: activity tail-read failed for {path}: {e}");
            Vec::new()
        }
    };

    let body = json!({ "entries": entries });
    let masked = mask::mask_response(body);
    (StatusCode::OK, [("content-type", "application/json")], masked.to_string()).into_response()
}

/// Parse a raw query string's `limit` parameter, e.g. `"limit=50"` or
/// `"limit=50&other=x"`. Returns `None` on a missing key, unparsable value,
/// or non-positive value (all of which fall back to the configured default
/// in [`handle_activity`]).
fn parse_limit(query: Option<&str>) -> Option<usize> {
    let query = query?;
    for pair in query.split('&') {
        let mut parts = pair.splitn(2, '=');
        let key = parts.next()?;
        if key != "limit" {
            continue;
        }
        let value = parts.next()?;
        return value.parse::<usize>().ok().filter(|n| *n > 0);
    }
    None
}

/// Tail-read up to `limit + 1` non-empty lines from the end of the file at
/// `path`, without ever reading the whole file into memory (see this
/// module's doc). Returns them in FILE ORDER (oldest of the collected batch
/// first, most recent last) — the extra `+1` cushion means a caller that
/// asks for `limit` entries still gets `limit` valid ones even if the very
/// last physical line happens to be corrupt/truncated (e.g. a write in
/// progress when this read raced it).
///
/// A missing file is reported as a plain `Ok(vec![])` (not an `Err`) — see
/// [`handle_activity`]'s "missing file → empty 200" contract; this only
/// returns `Err` for a genuine I/O failure on a file that DOES exist (e.g.
/// a permission error), which the caller also degrades to an empty feed.
fn tail_lines(path: &str, limit: usize) -> std::io::Result<Vec<String>> {
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };

    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(Vec::new());
    }

    // The "+1" cushion described in this function's doc.
    let want = limit.saturating_add(1);
    let mut collected_newlines = 0usize;
    let mut pos = len;
    let mut buf: Vec<u8> = Vec::new();

    while pos > 0 && collected_newlines <= want {
        let read_size = TAIL_BLOCK_SIZE.min(pos);
        pos -= read_size;
        file.seek(SeekFrom::Start(pos))?;
        let mut block = vec![0u8; read_size as usize];
        file.read_exact(&mut block)?;
        collected_newlines += block.iter().filter(|b| **b == b'\n').count();
        block.extend_from_slice(&buf);
        buf = block;
    }

    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<String> = text.lines().map(str::to_string).filter(|l| !l.trim().is_empty()).collect();
    // We may have collected MORE than `want` complete lines once `pos` hit
    // 0 (the last block read can straddle well past the first newline we
    // actually needed) — keep only the tail end.
    if lines.len() > want {
        let drop = lines.len() - want;
        lines.drain(0..drop);
    }
    Ok(lines)
}

/// Parse each raw JSONL line as an [`audit::ConstellationAuditEntry`],
/// silently skipping (and warning on) any line that doesn't parse — a
/// corrupt/partial line (e.g. a write torn by a concurrent append, or a
/// stray non-JSON line) must never fail the whole feed. Returns at most
/// `limit` entries, most-recent-last (matching `tail_lines`' file-order
/// contract), dropping the OLDEST of the parsed batch first if the corrupt-
/// line cushion left more than `limit` valid ones.
fn parse_entries(lines: Vec<String>, limit: usize) -> Vec<Value> {
    let mut corrupt = 0usize;
    let mut entries: Vec<Value> = lines
        .into_iter()
        .filter_map(|line| match serde_json::from_str::<audit::ConstellationAuditEntry>(&line) {
            Ok(entry) => Some(json!({
                "ts": entry.timestamp,
                "method": entry.method,
                "path": entry.path,
                "principal": entry.principal,
                "system": entry.system,
            })),
            Err(_) => {
                corrupt += 1;
                None
            }
        })
        .collect();

    if corrupt > 0 {
        tracing::warn!("constellation: activity feed skipped {corrupt} corrupt audit-log line(s)");
    }

    if entries.len() > limit {
        let drop = entries.len() - limit;
        entries.drain(0..drop);
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_swap::ArcSwap;
    use axum::body::Body;
    use axum::http::Request;
    use serial_test::serial;
    use tower::ServiceExt;

    fn test_state() -> Arc<McpServerState> {
        Arc::new(McpServerState {
            registry: ArcSwap::from_pointee(crate::registry::ToolRegistry::new()),
            server_name: "constellation-test".to_string(),
            server_version: "0.0.0-test".to_string(),
            auth_token: None,
            personal_federation: None,
            inference_proxy: None,
            gateway: None,
            mesh_pool: None,
            principal_resolver: crate::mesh::PrincipalResolver::default(),
            broker_routes: crate::broker::routes::RouteTable::new(),
        })
    }

    /// A fresh temp path, guaranteed not to exist yet, for the "missing
    /// file" edge case, and a helper to point `CONSTELLATION_AUDIT_LOG_PATH`
    /// at a scratch file for the others.
    fn scratch_path() -> String {
        let dir = std::env::temp_dir();
        let name = format!("const26-activity-test-{}-{}.jsonl", std::process::id(), rand_suffix());
        dir.join(name).to_str().unwrap().to_string()
    }

    fn rand_suffix() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64
    }

    fn write_lines(path: &str, lines: &[&str]) {
        std::fs::write(path, lines.join("\n") + "\n").unwrap();
    }

    async fn get_activity(router: axum::Router, path: &str) -> (StatusCode, Value) {
        let (token, _exp) = crate::pki::enroll::mint_jwt_with_ttl("test-operator", 300).unwrap();
        let req = Request::builder()
            .method("GET")
            .uri(path)
            .header("cookie", format!("constellation_session={token}"))
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
    }

    fn router_with_signing_key() -> axum::Router {
        std::env::set_var("TERMINUS_JWT_SIGNING_KEY", "test-signing-key-activity-tests");
        crate::constellation::constellation_router(test_state())
    }

    #[tokio::test]
    #[serial]
    async fn missing_file_yields_empty_200() {
        let path = scratch_path();
        std::env::set_var("CONSTELLATION_AUDIT_LOG_PATH", &path);
        // Deliberately do NOT create the file.
        let router = router_with_signing_key();
        let (status, body) = get_activity(router, "/api/terminus/activity").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["entries"].as_array().unwrap().len(), 0);
        std::env::remove_var("CONSTELLATION_AUDIT_LOG_PATH");
        std::env::remove_var("TERMINUS_JWT_SIGNING_KEY");
    }

    #[tokio::test]
    #[serial]
    async fn zero_length_file_yields_empty_200() {
        let path = scratch_path();
        std::fs::write(&path, "").unwrap();
        std::env::set_var("CONSTELLATION_AUDIT_LOG_PATH", &path);
        let router = router_with_signing_key();
        let (status, body) = get_activity(router, "/api/terminus/activity").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["entries"].as_array().unwrap().len(), 0);
        std::fs::remove_file(&path).ok();
        std::env::remove_var("CONSTELLATION_AUDIT_LOG_PATH");
        std::env::remove_var("TERMINUS_JWT_SIGNING_KEY");
    }

    #[tokio::test]
    #[serial]
    async fn corrupt_line_is_skipped_valid_ones_still_returned() {
        let path = scratch_path();
        write_lines(
            &path,
            &[
                r#"{"timestamp":"2026-07-19T00:00:00Z","system":"harmony","method":"POST","path":"/api/harmony/engine/stop","principal":"operator","body_summary":null}"#,
                "this is not json at all {{{",
                r#"{"timestamp":"2026-07-19T00:01:00Z","system":"chord","method":"PUT","path":"/api/chord/mode","principal":"operator","body_summary":null}"#,
            ],
        );
        std::env::set_var("CONSTELLATION_AUDIT_LOG_PATH", &path);
        let router = router_with_signing_key();
        let (status, body) = get_activity(router, "/api/terminus/activity").await;
        assert_eq!(status, StatusCode::OK);
        let entries = body["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 2, "expected the corrupt line to be skipped, got {entries:?}");
        assert_eq!(entries[0]["system"], "harmony");
        assert_eq!(entries[1]["system"], "chord");
        std::fs::remove_file(&path).ok();
        std::env::remove_var("CONSTELLATION_AUDIT_LOG_PATH");
        std::env::remove_var("TERMINUS_JWT_SIGNING_KEY");
    }

    #[tokio::test]
    #[serial]
    async fn response_shape_never_includes_body_content() {
        let path = scratch_path();
        write_lines(
            &path,
            &[r#"{"timestamp":"2026-07-19T00:00:00Z","system":"harmony","method":"POST","path":"/api/harmony/engine/stop","principal":"operator","body_summary":"some sanitized body text"}"#],
        );
        std::env::set_var("CONSTELLATION_AUDIT_LOG_PATH", &path);
        let router = router_with_signing_key();
        let (status, body) = get_activity(router, "/api/terminus/activity").await;
        assert_eq!(status, StatusCode::OK);
        let entries = body["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        let mut keys: Vec<&String> = entry.as_object().unwrap().keys().collect();
        keys.sort();
        assert_eq!(keys, vec!["method", "path", "principal", "system", "ts"]);
        assert!(!body.to_string().contains("some sanitized body text"));
        std::fs::remove_file(&path).ok();
        std::env::remove_var("CONSTELLATION_AUDIT_LOG_PATH");
        std::env::remove_var("TERMINUS_JWT_SIGNING_KEY");
    }

    #[tokio::test]
    #[serial]
    async fn secret_shaped_principal_value_is_masked() {
        let path = scratch_path();
        // "ghp_" is one of mask::value_looks_secret_shaped's recognized
        // provider-token prefixes -- this must be masked even though the
        // FIELD NAME "principal" isn't itself secret-shaped.
        write_lines(
            &path,
            &[r#"{"timestamp":"2026-07-19T00:00:00Z","system":"harmony","method":"POST","path":"/api/harmony/engine/stop","principal":"<REDACTED-SECRET>","body_summary":null}"#],
        );
        std::env::set_var("CONSTELLATION_AUDIT_LOG_PATH", &path);
        let router = router_with_signing_key();
        let (status, body) = get_activity(router, "/api/terminus/activity").await;
        assert_eq!(status, StatusCode::OK);
        let entries = body["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_ne!(entries[0]["principal"], "<REDACTED-SECRET>");
        assert!(!body.to_string().contains("<REDACTED-SECRET>"));
        std::fs::remove_file(&path).ok();
        std::env::remove_var("CONSTELLATION_AUDIT_LOG_PATH");
        std::env::remove_var("TERMINUS_JWT_SIGNING_KEY");
    }

    #[tokio::test]
    #[serial]
    async fn limit_query_param_caps_returned_entries() {
        let path = scratch_path();
        write_lines(
            &path,
            &[
                r#"{"timestamp":"2026-07-19T00:00:00Z","system":"harmony","method":"POST","path":"/a","principal":null,"body_summary":null}"#,
                r#"{"timestamp":"2026-07-19T00:01:00Z","system":"harmony","method":"POST","path":"/b","principal":null,"body_summary":null}"#,
                r#"{"timestamp":"2026-07-19T00:02:00Z","system":"harmony","method":"POST","path":"/c","principal":null,"body_summary":null}"#,
            ],
        );
        std::env::set_var("CONSTELLATION_AUDIT_LOG_PATH", &path);
        let router = router_with_signing_key();
        let (status, body) = get_activity(router, "/api/terminus/activity?limit=1").await;
        assert_eq!(status, StatusCode::OK);
        let entries = body["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        // Most-recent-last ordering: with limit=1 the ONE entry returned must be the newest.
        assert_eq!(entries[0]["path"], "/c");
        std::fs::remove_file(&path).ok();
        std::env::remove_var("CONSTELLATION_AUDIT_LOG_PATH");
        std::env::remove_var("TERMINUS_JWT_SIGNING_KEY");
    }

    #[tokio::test]
    #[serial]
    async fn limit_query_param_cannot_exceed_the_configured_cap() {
        let path = scratch_path();
        write_lines(
            &path,
            &[
                r#"{"timestamp":"2026-07-19T00:00:00Z","system":"harmony","method":"POST","path":"/a","principal":null,"body_summary":null}"#,
                r#"{"timestamp":"2026-07-19T00:01:00Z","system":"harmony","method":"POST","path":"/b","principal":null,"body_summary":null}"#,
            ],
        );
        std::env::set_var("CONSTELLATION_AUDIT_LOG_PATH", &path);
        std::env::set_var("CONSTELLATION_ACTIVITY_TAIL_LIMIT", "1");
        let router = router_with_signing_key();
        let (status, body) = get_activity(router, "/api/terminus/activity?limit=1000").await;
        assert_eq!(status, StatusCode::OK);
        let entries = body["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 1, "a caller-supplied limit must never exceed the configured cap");
        std::fs::remove_file(&path).ok();
        std::env::remove_var("CONSTELLATION_AUDIT_LOG_PATH");
        std::env::remove_var("CONSTELLATION_ACTIVITY_TAIL_LIMIT");
        std::env::remove_var("TERMINUS_JWT_SIGNING_KEY");
    }

    #[tokio::test]
    #[serial]
    async fn unauthenticated_request_is_rejected_401() {
        let path = scratch_path();
        std::env::set_var("CONSTELLATION_AUDIT_LOG_PATH", &path);
        std::env::remove_var("TERMINUS_JWT_SIGNING_KEY");
        let router = crate::constellation::constellation_router(test_state());
        let req = Request::builder()
            .method("GET")
            .uri("/api/terminus/activity")
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        std::env::remove_var("CONSTELLATION_AUDIT_LOG_PATH");
    }

    #[test]
    fn parse_limit_reads_the_limit_key_out_of_an_arbitrary_query_string() {
        assert_eq!(parse_limit(Some("limit=50")), Some(50));
        assert_eq!(parse_limit(Some("other=x&limit=7&more=y")), Some(7));
        assert_eq!(parse_limit(Some("other=x")), None);
        assert_eq!(parse_limit(Some("limit=0")), None);
        assert_eq!(parse_limit(Some("limit=notanumber")), None);
        assert_eq!(parse_limit(None), None);
    }

    #[test]
    fn tail_lines_reads_the_most_recent_lines_without_the_whole_file() {
        let path = scratch_path();
        // Padded lines so the file exceeds TAIL_BLOCK_SIZE (8KB) many times
        // over -- forcing tail_lines to actually loop backward across
        // several blocks rather than degenerating to a single whole-file
        // read (which a small file would trivially satisfy either way).
        let mut lines = Vec::new();
        for i in 0..5_000 {
            lines.push(format!(r#"{{"n":{i},"pad":"{}"}}"#, "x".repeat(40)));
        }
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        let total_len = std::fs::metadata(&path).unwrap().len();
        assert!(total_len > TAIL_BLOCK_SIZE * 4, "test fixture must span multiple tail blocks");

        let tail = tail_lines(&path, 3).unwrap();
        // At least the requested count came back, and the LAST physical
        // line in the file is present at the end (file-order, most-recent-
        // last), proving this actually tailed rather than reading from the
        // front -- exercised here across several 8KB blocks (see the
        // `total_len` assertion above), not just a degenerate single read.
        assert!(tail.len() >= 3);
        assert!(tail.last().unwrap().contains(r#""n":4999"#));
        // A small limit's result must stay small too -- if this function
        // had actually read (and returned) the whole file, this would be
        // ~5000 lines instead.
        assert!(tail.len() < 20, "tail_lines returned far more lines than a limit=3 request needs");

        std::fs::remove_file(&path).ok();
    }
}
