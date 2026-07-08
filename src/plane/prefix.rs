//! Plane prefix registry — a queryable/maintainable library of USED/ACTIVE
//! sub-project + issue prefixes (the 2-8 char item-ID prefixes like SCRB, ROUT,
//! RMDR), exposed as a SUB-MODULE of the Plane helper. These are the per-spec
//! item prefixes, NOT the per-repo Plane *project* prefixes (HARM/LUM/CHRD/
//! TERM/RAIL/HW/PSH).
//!
//! ## Why this exists
//! The uniqueness rule for prefixes ("a prefix must be unique — check the
//! registry") previously had no programmatic backing: the only registry was a
//! hand-maintained table in the moosenet-spec skill that stopped being updated
//! years of sessions ago. This module gives that rule a real, queryable store.
//!
//! ## Hybrid store
//! - **Baseline** — a git-versioned TOML file (`data/prefix_registry.toml`),
//!   the reviewed source of truth. It is compiled into the binary via
//!   `include_str!`, so baseline reads always succeed regardless of the current
//!   working directory or whether Redis is up.
//! - **Overlay** — a runtime claim store in the shared Plane Redis (the same
//!   `PLANE_REDIS_URL` backend the GET cache + rate limiter use). A new claim
//!   from `plane_prefix_register` lands here immediately (fast,
//!   cross-instance-visible). Promotion of an overlay claim into the baseline
//!   TOML happens later through a small reviewed PR (add a `[[prefix]]` block,
//!   drop the overlay field).
//!
//! ## Fail-open
//! Every overlay operation is short-timeout-bounded. If Redis is unconfigured
//! or unreachable, reads transparently fall back to the baseline alone, and
//! `plane_prefix_register` / `plane_prefix_retire` return a clear
//! "overlay unavailable — use the file/PR path" result instead of crashing.
//!
//! ## Tools (registered alongside `plane_*` via [`super::register`])
//! - `plane_prefix_list` — list/filter all known prefixes (baseline + overlay)
//! - `plane_prefix_register` — claim a new prefix (collision-checked, written to
//!   the overlay)
//! - `plane_prefix_get` — fetch one prefix's metadata
//! - `plane_prefix_check` — is-free check + next-available suggestions
//! - `plane_prefix_retire` — mark a prefix retired (overlay override)

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

use async_trait::async_trait;
use redis::aio::ConnectionManager;
use redis::IntoConnectionInfo;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::OnceCell;
use tracing::warn;

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

/// The git-versioned baseline, compiled in so a baseline read never depends on
/// the process's working directory or on Redis being reachable.
const BASELINE_TOML: &str = include_str!("../../data/prefix_registry.toml");

/// Valid status values for a prefix entry.
const VALID_STATUSES: &[&str] = &["active", "retired", "ingested", "complete"];

/// Default per-op Redis timeout (ms); overridable via `PLANE_REDIS_TIMEOUT_MS`
/// (shared with the S100 cache/limiter backend).
const REDIS_DEFAULT_TIMEOUT_MS: u64 = 200;

/// Redis hash key holding overlay claims: field = uppercased prefix, value =
/// JSON-encoded [`PrefixEntry`]. Namespaced under the same `plane:` prefix the
/// S100 backend uses so it shares one logical keyspace.
const OVERLAY_HASH_KEY: &str = "plane:prefix:overlay:v1";

// ─── Data model ──────────────────────────────────────────────────────────────

/// One prefix's metadata. Serialized as a TOML `[[prefix]]` block in the
/// baseline and as JSON in the Redis overlay.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PrefixEntry {
    /// The prefix itself, e.g. `SCRB`. Stored/compared uppercased.
    pub prefix: String,
    /// Full human-readable name/title.
    #[serde(default)]
    pub name: String,
    /// Owning Plane project (HARM/LUM/CHRD/TERM/RAIL/HW/PSH).
    #[serde(default)]
    pub project: String,
    /// Originating spec id (`S{session}-{slug}`) when known.
    #[serde(default)]
    pub spec_id: String,
    /// Lifecycle status: active | retired | ingested | complete.
    #[serde(default = "default_status")]
    pub status: String,
    /// One-line summary.
    #[serde(default)]
    pub description: String,
    /// ISO date (YYYY-MM-DD) when known, else empty.
    #[serde(default)]
    pub created: String,
}

fn default_status() -> String {
    "active".to_string()
}

/// TOML wrapper: the baseline file is an array of `[[prefix]]` tables.
#[derive(Debug, Deserialize)]
struct Baseline {
    #[serde(default)]
    prefix: Vec<PrefixEntry>,
}

/// Parse + normalize the compiled-in baseline exactly once.
fn baseline() -> &'static Vec<PrefixEntry> {
    static BASELINE: OnceLock<Vec<PrefixEntry>> = OnceLock::new();
    BASELINE.get_or_init(|| match toml::from_str::<Baseline>(BASELINE_TOML) {
        Ok(b) => b
            .prefix
            .into_iter()
            .map(|mut e| {
                e.prefix = e.prefix.trim().to_uppercase();
                e
            })
            .collect(),
        Err(e) => {
            // Never panic in production over a data-file typo — degrade to an
            // empty baseline (the overlay still works) and log loudly once.
            warn!("prefix baseline TOML failed to parse; baseline empty: {e}");
            Vec::new()
        }
    })
}

/// Validate a prefix string. Rule: 2-8 chars, first char an ASCII uppercase
/// letter, remainder uppercase letters or digits. (Covers historical shapes
/// like `S35`, `COND2`, `DPROMPT`.) Returns the normalized (uppercased) prefix.
fn validate_prefix(raw: &str) -> Result<String, String> {
    let p = raw.trim().to_uppercase();
    if p.len() < 2 || p.len() > 8 {
        return Err(format!(
            "prefix '{p}' must be 2-8 characters (got {})",
            p.len()
        ));
    }
    let mut chars = p.chars();
    let first = chars.next().unwrap();
    if !first.is_ascii_uppercase() {
        return Err(format!("prefix '{p}' must start with a letter A-Z"));
    }
    if !p.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()) {
        return Err(format!(
            "prefix '{p}' may only contain letters A-Z and digits 0-9"
        ));
    }
    Ok(p)
}

fn normalize_status(raw: &str) -> Result<String, String> {
    let s = raw.trim().to_lowercase();
    if VALID_STATUSES.contains(&s.as_str()) {
        Ok(s)
    } else {
        Err(format!(
            "status '{raw}' invalid; expected one of {VALID_STATUSES:?}"
        ))
    }
}

// ─── Redis overlay (fail-open, mirrors the S100 backend pattern) ──────────────

/// Distinguishes "Redis not configured at all" from "configured but this op
/// could not reach it" so tools can phrase their fail-open message precisely.
#[derive(Debug, Clone, Copy, PartialEq)]
enum OverlayError {
    /// A configured Redis overlay could not be reached within the timeout (or
    /// returned an error). Reads fall back to baseline; writes are not durable.
    Unavailable,
}

/// Runtime overlay store backed by the shared Plane Redis. Reuses the S100
/// config surface (`PLANE_REDIS_URL` / `PLANE_REDIS_PASSWORD` /
/// `PLANE_REDIS_TIMEOUT_MS`) so every terminus instance sees the same claims.
struct PrefixOverlay {
    client: redis::Client,
    /// Built lazily so construction stays synchronous and an unreachable Redis
    /// at startup never blocks. A failed init is not cached (retries later).
    conn: OnceCell<ConnectionManager>,
    op_timeout: Duration,
}

/// Hand-written `Debug` that never prints `client` (its `Debug` includes the
/// ConnectionInfo, which can carry the Redis password).
impl std::fmt::Debug for PrefixOverlay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrefixOverlay")
            .field("op_timeout", &self.op_timeout)
            .finish_non_exhaustive()
    }
}

impl PrefixOverlay {
    /// Build from `PLANE_REDIS_URL` (+ optional password/timeout). Returns
    /// `None` when the URL is unset/empty or unparseable — the pure-baseline
    /// path, identical to having no overlay.
    fn from_env() -> Option<Arc<Self>> {
        let url = std::env::var("PLANE_REDIS_URL")
            .ok()
            .filter(|v| !v.trim().is_empty())?;
        let password = std::env::var("PLANE_REDIS_PASSWORD")
            .ok()
            .filter(|v| !v.is_empty());
        let timeout_ms: u64 = std::env::var("PLANE_REDIS_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(REDIS_DEFAULT_TIMEOUT_MS)
            .max(1);
        Self::build(&url, password, Duration::from_millis(timeout_ms))
    }

    /// Shared constructor: parse the URL, layer the password from its own env
    /// var (kept out of the URL so it never lands in a log line), build the
    /// client. Any failure logs once and yields `None` (pure-baseline path).
    fn build(url: &str, password: Option<String>, op_timeout: Duration) -> Option<Arc<Self>> {
        let mut info = match url.into_connection_info() {
            Ok(i) => i,
            Err(e) => {
                warn!(
                    "PLANE_REDIS_URL not a valid Redis URL ({:?}); prefix overlay disabled, baseline-only",
                    e.kind()
                );
                return None;
            }
        };
        if let Some(pw) = password {
            info.redis.password = Some(pw);
        }
        let client = match redis::Client::open(info) {
            Ok(c) => c,
            Err(e) => {
                warn!(
                    "failed to construct prefix overlay Redis client ({:?}); baseline-only",
                    e.kind()
                );
                return None;
            }
        };
        Some(Arc::new(Self {
            client,
            conn: OnceCell::new(),
            op_timeout,
        }))
    }

    async fn conn(&self) -> Option<ConnectionManager> {
        match self
            .conn
            .get_or_try_init(|| ConnectionManager::new(self.client.clone()))
            .await
        {
            Ok(m) => Some(m.clone()),
            Err(_) => None,
        }
    }

    /// All overlay claims (field -> entry). `Err(Unavailable)` on any
    /// Redis error/timeout — the caller treats that as "no overlay".
    async fn list(&self) -> Result<Vec<PrefixEntry>, OverlayError> {
        let fut = async {
            let mut conn = self.conn().await.ok_or(OverlayError::Unavailable)?;
            let map: std::collections::HashMap<String, String> = redis::cmd("HGETALL")
                .arg(OVERLAY_HASH_KEY)
                .query_async(&mut conn)
                .await
                .map_err(|_| OverlayError::Unavailable)?;
            let mut out = Vec::with_capacity(map.len());
            for (_field, raw) in map {
                match serde_json::from_str::<PrefixEntry>(&raw) {
                    Ok(mut e) => {
                        e.prefix = e.prefix.trim().to_uppercase();
                        out.push(e);
                    }
                    // A single corrupt field must not sink the whole read.
                    Err(e) => warn!("skipping unparseable overlay claim: {e}"),
                }
            }
            Ok(out)
        };
        match tokio::time::timeout(self.op_timeout, fut).await {
            Ok(res) => res,
            Err(_) => Err(OverlayError::Unavailable),
        }
    }

    /// Write/replace one claim. `Err(Unavailable)` if the write did not land.
    async fn put(&self, entry: &PrefixEntry) -> Result<(), OverlayError> {
        let field = entry.prefix.to_uppercase();
        let payload = serde_json::to_string(entry).map_err(|_| OverlayError::Unavailable)?;
        let fut = async {
            let mut conn = self.conn().await.ok_or(OverlayError::Unavailable)?;
            redis::cmd("HSET")
                .arg(OVERLAY_HASH_KEY)
                .arg(&field)
                .arg(&payload)
                .query_async::<_, ()>(&mut conn)
                .await
                .map_err(|_| OverlayError::Unavailable)
        };
        match tokio::time::timeout(self.op_timeout, fut).await {
            Ok(res) => res,
            Err(_) => Err(OverlayError::Unavailable),
        }
    }
}

// ─── Merged view ─────────────────────────────────────────────────────────────

/// One prefix's effective row after merging baseline + overlay. The overlay
/// entry (if present) is the effective one — this is how a retire override or a
/// not-yet-promoted claim takes effect — while both source flags are reported.
#[derive(Debug, Clone)]
struct MergedRow {
    entry: PrefixEntry,
    in_baseline: bool,
    in_overlay: bool,
}

impl MergedRow {
    /// Human label for where this row lives and whether it's promoted.
    fn source_label(&self) -> &'static str {
        match (self.in_baseline, self.in_overlay) {
            (true, true) => "baseline+overlay",
            (true, false) => "baseline",
            (false, true) => "overlay-only",
            (false, false) => "unknown",
        }
    }

    fn to_json(&self) -> Value {
        let e = &self.entry;
        json!({
            "prefix": e.prefix,
            "name": e.name,
            "project": e.project,
            "spec_id": e.spec_id,
            "status": e.status,
            "description": e.description,
            "created": e.created,
            "in_baseline": self.in_baseline,
            "in_overlay": self.in_overlay,
            "source": self.source_label(),
            // An overlay claim not yet written into the reviewed baseline file.
            "pending_promotion": self.in_overlay && !self.in_baseline,
        })
    }
}

/// The prefix store: the compiled-in baseline plus an optional Redis overlay.
pub struct PrefixStore {
    overlay: Option<Arc<PrefixOverlay>>,
}

impl PrefixStore {
    pub fn from_env() -> Arc<Self> {
        Arc::new(Self {
            overlay: PrefixOverlay::from_env(),
        })
    }

    /// Build the merged map (uppercased prefix -> row). `overlay_reachable`
    /// reports whether the overlay was consulted successfully: `Some(true)` =
    /// overlay read ok, `Some(false)` = overlay configured but unreachable
    /// (fell back to baseline), `None` = no overlay configured.
    async fn merged(&self) -> (BTreeMap<String, MergedRow>, Option<bool>) {
        let mut map: BTreeMap<String, MergedRow> = BTreeMap::new();
        for e in baseline().iter() {
            map.insert(
                e.prefix.clone(),
                MergedRow {
                    entry: e.clone(),
                    in_baseline: true,
                    in_overlay: false,
                },
            );
        }

        let reachable = match &self.overlay {
            None => None,
            Some(ov) => match ov.list().await {
                Ok(claims) => {
                    for e in claims {
                        let key = e.prefix.to_uppercase();
                        map.entry(key)
                            .and_modify(|row| {
                                // Overlay wins as the effective entry (retire
                                // overrides, field refreshes) but keep the
                                // baseline flag.
                                row.entry = e.clone();
                                row.in_overlay = true;
                            })
                            .or_insert(MergedRow {
                                entry: e.clone(),
                                in_baseline: false,
                                in_overlay: true,
                            });
                    }
                    Some(true)
                }
                Err(OverlayError::Unavailable) => Some(false),
            },
        };

        (map, reachable)
    }
}

/// Human note describing the overlay's state for tool output.
fn overlay_note(reachable: Option<bool>) -> &'static str {
    match reachable {
        None => "no Redis overlay configured (baseline-only)",
        Some(true) => "overlay reachable",
        Some(false) => "overlay configured but unreachable — baseline-only (fail-open)",
    }
}

// ─── Tools ───────────────────────────────────────────────────────────────────

/// `plane_prefix_list` — list/filter all known prefixes.
pub struct PlanePrefixList {
    store: Arc<PrefixStore>,
}

#[async_trait]
impl RustTool for PlanePrefixList {
    fn name(&self) -> &str {
        "plane_prefix_list"
    }
    fn description(&self) -> &str {
        "List the registry of USED/ACTIVE sub-project + issue prefixes (e.g. SCRB, ROUT, RMDR — \
         NOT the per-repo project prefixes HARM/LUM/CHRD/TERM). Merges the reviewed baseline file \
         with the runtime Redis overlay. Optional filters: status, project, source \
         (baseline|overlay|pending), include_retired."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "status": {"type": "string", "description": "Filter by status: active|retired|ingested|complete"},
                "project": {"type": "string", "description": "Filter by owning Plane project, e.g. HARM/LUM/CHRD/TERM"},
                "source": {"type": "string", "description": "Filter by source: baseline | overlay | pending (overlay-only, not yet promoted)"},
                "include_retired": {"type": "boolean", "description": "Include retired prefixes (default true)"}
            }
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let status_f = args.get("status").and_then(|v| v.as_str()).map(|s| s.to_lowercase());
        let project_f = args.get("project").and_then(|v| v.as_str()).map(|s| s.to_uppercase());
        let source_f = args.get("source").and_then(|v| v.as_str()).map(|s| s.to_lowercase());
        let include_retired = args
            .get("include_retired")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        let (map, reachable) = self.store.merged().await;

        let mut rows: Vec<Value> = Vec::new();
        let (mut n_baseline, mut n_overlay_only) = (0usize, 0usize);
        for row in map.values() {
            if let Some(ref s) = status_f {
                if row.entry.status.to_lowercase() != *s {
                    continue;
                }
            }
            if let Some(ref p) = project_f {
                if row.entry.project.to_uppercase() != *p {
                    continue;
                }
            }
            if !include_retired && row.entry.status.eq_ignore_ascii_case("retired") {
                continue;
            }
            let pending = row.in_overlay && !row.in_baseline;
            if let Some(ref src) = source_f {
                let keep = match src.as_str() {
                    "baseline" => row.in_baseline,
                    "overlay" => row.in_overlay,
                    "pending" => pending,
                    _ => true,
                };
                if !keep {
                    continue;
                }
            }
            if row.in_baseline {
                n_baseline += 1;
            }
            if pending {
                n_overlay_only += 1;
            }
            rows.push(row.to_json());
        }

        Ok(json!({
            "count": rows.len(),
            "baseline_count": n_baseline,
            "pending_promotion_count": n_overlay_only,
            "overlay": overlay_note(reachable),
            "prefixes": rows,
        })
        .to_string())
    }
}

/// `plane_prefix_get` — fetch one prefix's merged metadata.
pub struct PlanePrefixGet {
    store: Arc<PrefixStore>,
}

#[async_trait]
impl RustTool for PlanePrefixGet {
    fn name(&self) -> &str {
        "plane_prefix_get"
    }
    fn description(&self) -> &str {
        "Fetch one prefix's metadata (merged baseline + overlay view), including which source it \
         lives in and whether it is pending promotion into the reviewed baseline."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "prefix": {"type": "string", "description": "The prefix to look up, e.g. SCRB"}
            },
            "required": ["prefix"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let raw = args
            .get("prefix")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgument("prefix is required".into()))?;
        let key = raw.trim().to_uppercase();
        let (map, reachable) = self.store.merged().await;
        match map.get(&key) {
            Some(row) => Ok(json!({
                "found": true,
                "overlay": overlay_note(reachable),
                "entry": row.to_json(),
            })
            .to_string()),
            None => Ok(json!({
                "found": false,
                "prefix": key,
                "overlay": overlay_note(reachable),
                "message": format!("prefix '{key}' is not in the registry"),
            })
            .to_string()),
        }
    }
}

/// `plane_prefix_check` — is-free check plus next-available suggestions.
pub struct PlanePrefixCheck {
    store: Arc<PrefixStore>,
}

impl PlanePrefixCheck {
    /// Suggest up to `n` available prefixes derived from `base`: try appending a
    /// digit 2..=9, then a trailing letter A..=Z, skipping anything taken or
    /// over the 8-char cap. Deterministic order so callers get stable output.
    fn suggest(base: &str, taken: &BTreeMap<String, MergedRow>, n: usize) -> Vec<String> {
        let mut out = Vec::new();
        let mut candidates: Vec<String> = Vec::new();
        for d in '2'..='9' {
            candidates.push(format!("{base}{d}"));
        }
        for c in 'A'..='Z' {
            candidates.push(format!("{base}{c}"));
        }
        for cand in candidates {
            if out.len() >= n {
                break;
            }
            if cand.len() > 8 {
                continue;
            }
            if !taken.contains_key(&cand) {
                out.push(cand);
            }
        }
        out
    }
}

#[async_trait]
impl RustTool for PlanePrefixCheck {
    fn name(&self) -> &str {
        "plane_prefix_check"
    }
    fn description(&self) -> &str {
        "Check whether a prefix is free to claim. Returns free/taken (and the existing entry if \
         taken), plus a few next-available suggestions derived from the requested prefix. Use this \
         before writing a new spec to satisfy the 'prefix must be unique' rule."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "prefix": {"type": "string", "description": "Candidate prefix to check, e.g. SCRB"},
                "suggestions": {"type": "integer", "description": "How many next-available suggestions to return (default 3)"}
            },
            "required": ["prefix"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let raw = args
            .get("prefix")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgument("prefix is required".into()))?;
        let n = args
            .get("suggestions")
            .and_then(|v| v.as_u64())
            .unwrap_or(3)
            .min(10) as usize;

        // Validate shape but don't hard-error — report invalid so a caller can
        // still learn the prefix is unusable and see suggestions off the base.
        let normalized = validate_prefix(raw);
        let (map, reachable) = self.store.merged().await;

        match normalized {
            Ok(key) => {
                let taken = map.get(&key);
                let suggestions = if taken.is_some() {
                    Self::suggest(&key, &map, n)
                } else {
                    Vec::new()
                };
                Ok(json!({
                    "prefix": key,
                    "valid": true,
                    "free": taken.is_none(),
                    "overlay": overlay_note(reachable),
                    "existing": taken.map(|r| r.to_json()),
                    "suggestions": suggestions,
                })
                .to_string())
            }
            Err(msg) => {
                // Still offer suggestions off the uppercased raw stem.
                let stem: String = raw
                    .trim()
                    .to_uppercase()
                    .chars()
                    .filter(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
                    .take(6)
                    .collect();
                let suggestions = if stem.is_empty() {
                    Vec::new()
                } else {
                    Self::suggest(&stem, &map, n)
                };
                Ok(json!({
                    "prefix": raw.trim(),
                    "valid": false,
                    "free": false,
                    "reason": msg,
                    "overlay": overlay_note(reachable),
                    "suggestions": suggestions,
                })
                .to_string())
            }
        }
    }
}

/// `plane_prefix_register` — claim a new prefix (collision-checked → overlay).
pub struct PlanePrefixRegister {
    store: Arc<PrefixStore>,
}

#[async_trait]
impl RustTool for PlanePrefixRegister {
    fn name(&self) -> &str {
        "plane_prefix_register"
    }
    fn description(&self) -> &str {
        "Claim a new sub-project/issue prefix. Rejects on collision with the baseline OR the \
         overlay. On success the claim is written to the runtime Redis overlay immediately \
         (cross-instance-visible) and is flagged pending promotion into the reviewed baseline file \
         via a later small PR. If the overlay is unavailable, the collision check still runs but \
         the claim is not persisted — add it to data/prefix_registry.toml via a PR instead."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "prefix": {"type": "string", "description": "New prefix, 2-8 chars, starts with a letter, [A-Z0-9]"},
                "name": {"type": "string", "description": "Full human-readable name/title"},
                "project": {"type": "string", "description": "Owning Plane project (HARM/LUM/CHRD/TERM/RAIL/HW/PSH)"},
                "spec_id": {"type": "string", "description": "Originating spec id, e.g. S101-prefix-library"},
                "description": {"type": "string", "description": "One-line summary"},
                "status": {"type": "string", "description": "Lifecycle status (default active): active|ingested|complete|retired"},
                "created": {"type": "string", "description": "ISO date YYYY-MM-DD (default: today, UTC)"}
            },
            "required": ["prefix"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let raw = args
            .get("prefix")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgument("prefix is required".into()))?;
        let key = validate_prefix(raw).map_err(ToolError::InvalidArgument)?;

        let status = match args.get("status").and_then(|v| v.as_str()) {
            Some(s) => normalize_status(s).map_err(ToolError::InvalidArgument)?,
            None => "active".to_string(),
        };
        let created = args
            .get("created")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| chrono::Utc::now().format("%Y-%m-%d").to_string());

        // Collision check against the merged view (baseline OR overlay).
        let (map, reachable) = self.store.merged().await;
        if let Some(existing) = map.get(&key) {
            return Ok(json!({
                "ok": false,
                "reason": "collision",
                "message": format!(
                    "prefix '{key}' already exists ({}) — pick another; see suggestions or plane_prefix_check",
                    existing.source_label()
                ),
                "existing": existing.to_json(),
                "suggestions": PlanePrefixCheck::suggest(&key, &map, 3),
                "overlay": overlay_note(reachable),
            })
            .to_string());
        }

        let entry = PrefixEntry {
            prefix: key.clone(),
            name: args.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            project: args
                .get("project")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_uppercase(),
            spec_id: args.get("spec_id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            status,
            description: args
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            created,
        };

        // Persist to the overlay. Fail-open: no overlay, or unreachable → the
        // claim is validated + collision-free but not durable; direct the
        // caller to the file/PR path.
        match &self.store.overlay {
            None => Ok(json!({
                "ok": false,
                "persisted": false,
                "reason": "overlay_unconfigured",
                "message": format!(
                    "prefix '{key}' is free and valid, but no Redis overlay is configured — \
                     add it to data/prefix_registry.toml via a PR to record the claim"
                ),
                "entry": entry_json(&entry),
            })
            .to_string()),
            Some(ov) => match ov.put(&entry).await {
                Ok(()) => Ok(json!({
                    "ok": true,
                    "persisted": true,
                    "pending_promotion": true,
                    "message": format!(
                        "prefix '{key}' claimed in the overlay (cross-instance-visible). \
                         Promote it into data/prefix_registry.toml via a later small PR."
                    ),
                    "entry": entry_json(&entry),
                })
                .to_string()),
                Err(OverlayError::Unavailable) => Ok(json!({
                    "ok": false,
                    "persisted": false,
                    "reason": "overlay_unavailable",
                    "message": format!(
                        "prefix '{key}' is free and valid, but the Redis overlay is unavailable \
                         (fail-open) — add it to data/prefix_registry.toml via a PR instead"
                    ),
                    "entry": entry_json(&entry),
                })
                .to_string()),
            },
        }
    }
}

/// `plane_prefix_retire` — mark a prefix retired via an overlay override.
pub struct PlanePrefixRetire {
    store: Arc<PrefixStore>,
}

#[async_trait]
impl RustTool for PlanePrefixRetire {
    fn name(&self) -> &str {
        "plane_prefix_retire"
    }
    fn description(&self) -> &str {
        "Mark an existing prefix retired. Writes a status=retired override into the runtime Redis \
         overlay (so it takes effect across instances); the change should be promoted into the \
         baseline file via a later PR. Fails cleanly (with a file/PR instruction) if the overlay \
         is unavailable."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "prefix": {"type": "string", "description": "Prefix to retire, e.g. OLDX"},
                "reason": {"type": "string", "description": "Optional note appended to the description"}
            },
            "required": ["prefix"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let raw = args
            .get("prefix")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgument("prefix is required".into()))?;
        let key = raw.trim().to_uppercase();
        let reason = args.get("reason").and_then(|v| v.as_str()).map(|s| s.trim().to_string());

        let (map, reachable) = self.store.merged().await;
        let row = match map.get(&key) {
            Some(r) => r,
            None => {
                return Ok(json!({
                    "ok": false,
                    "reason": "not_found",
                    "message": format!("prefix '{key}' is not in the registry; nothing to retire"),
                    "overlay": overlay_note(reachable),
                })
                .to_string());
            }
        };

        if row.entry.status.eq_ignore_ascii_case("retired") {
            return Ok(json!({
                "ok": true,
                "already_retired": true,
                "message": format!("prefix '{key}' is already retired"),
                "entry": row.to_json(),
            })
            .to_string());
        }

        // Build the retired override from the current effective entry.
        let mut entry = row.entry.clone();
        entry.status = "retired".to_string();
        if let Some(r) = reason {
            if !r.is_empty() {
                entry.description = if entry.description.is_empty() {
                    format!("[retired] {r}")
                } else {
                    format!("{} [retired: {r}]", entry.description)
                };
            }
        }

        match &self.store.overlay {
            None => Ok(json!({
                "ok": false,
                "persisted": false,
                "reason": "overlay_unconfigured",
                "message": format!(
                    "cannot persist a retire without a Redis overlay — set status=retired for \
                     '{key}' in data/prefix_registry.toml via a PR instead"
                ),
                "entry": entry_json(&entry),
            })
            .to_string()),
            Some(ov) => match ov.put(&entry).await {
                Ok(()) => Ok(json!({
                    "ok": true,
                    "persisted": true,
                    "pending_promotion": true,
                    "message": format!(
                        "prefix '{key}' retired in the overlay. Promote the status change into \
                         data/prefix_registry.toml via a later small PR."
                    ),
                    "entry": entry_json(&entry),
                })
                .to_string()),
                Err(OverlayError::Unavailable) => Ok(json!({
                    "ok": false,
                    "persisted": false,
                    "reason": "overlay_unavailable",
                    "message": format!(
                        "the Redis overlay is unavailable (fail-open) — set status=retired for \
                         '{key}' in data/prefix_registry.toml via a PR instead"
                    ),
                    "entry": entry_json(&entry),
                })
                .to_string()),
            },
        }
    }
}

/// Serialize a bare entry (no source flags) for register/retire echoes.
fn entry_json(e: &PrefixEntry) -> Value {
    json!({
        "prefix": e.prefix,
        "name": e.name,
        "project": e.project,
        "spec_id": e.spec_id,
        "status": e.status,
        "description": e.description,
        "created": e.created,
    })
}

/// Register the five prefix sub-tools into the registry. Called from
/// [`super::register`] so they appear alongside the `plane_*` tools in BOTH the
/// core Chord registry and the personal registry.
pub fn register(registry: &mut ToolRegistry) {
    let store = PrefixStore::from_env();
    let tools: Vec<Box<dyn RustTool>> = vec![
        Box::new(PlanePrefixList { store: store.clone() }),
        Box::new(PlanePrefixRegister { store: store.clone() }),
        Box::new(PlanePrefixGet { store: store.clone() }),
        Box::new(PlanePrefixCheck { store: store.clone() }),
        Box::new(PlanePrefixRetire { store: store.clone() }),
    ];
    for tool in tools {
        if let Err(e) = registry.register(tool) {
            warn!("Failed to register plane prefix tool: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A store with no overlay (the pure-baseline path).
    fn baseline_only_store() -> Arc<PrefixStore> {
        Arc::new(PrefixStore { overlay: None })
    }

    fn parse(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn baseline_parses_and_is_substantial() {
        let b = baseline();
        assert!(
            b.len() >= 40,
            "expected a substantial seeded baseline, got {}",
            b.len()
        );
        // A few known seeds must be present and uppercased.
        for p in ["SCRB", "ROUT", "RMDR", "PSEC", "VAULT", "S35", "COND2"] {
            assert!(b.iter().any(|e| e.prefix == p), "missing seed {p}");
        }
    }

    #[test]
    fn baseline_prefixes_are_unique() {
        let b = baseline();
        let mut seen = std::collections::HashSet::new();
        for e in b {
            assert!(seen.insert(&e.prefix), "duplicate baseline prefix {}", e.prefix);
        }
    }

    #[test]
    fn baseline_entries_are_valid_shape() {
        for e in baseline() {
            assert!(
                validate_prefix(&e.prefix).is_ok(),
                "baseline prefix {} fails validation",
                e.prefix
            );
            assert!(
                VALID_STATUSES.contains(&e.status.as_str()),
                "baseline prefix {} has bad status {}",
                e.prefix,
                e.status
            );
        }
    }

    #[test]
    fn validate_prefix_rules() {
        assert_eq!(validate_prefix("scrb").unwrap(), "SCRB");
        assert_eq!(validate_prefix("  cond2 ").unwrap(), "COND2");
        assert_eq!(validate_prefix("S35").unwrap(), "S35");
        assert!(validate_prefix("X").is_err(), "too short");
        assert!(validate_prefix("TOOLONGONE").is_err(), "too long");
        assert!(validate_prefix("3ABC").is_err(), "must start with letter");
        assert!(validate_prefix("A-B").is_err(), "no punctuation");
    }

    #[tokio::test]
    async fn list_returns_baseline_and_filters() {
        let store = baseline_only_store();
        let tool = PlanePrefixList { store: store.clone() };

        let all = parse(&tool.execute(json!({})).await.unwrap());
        assert!(all["count"].as_u64().unwrap() >= 40);
        assert_eq!(all["overlay"], "no Redis overlay configured (baseline-only)");
        assert_eq!(all["pending_promotion_count"], 0);

        // Project filter.
        let term = parse(&tool.execute(json!({"project": "TERM"})).await.unwrap());
        let n_term = term["count"].as_u64().unwrap();
        assert!(n_term > 0 && n_term < all["count"].as_u64().unwrap());
        for row in term["prefixes"].as_array().unwrap() {
            assert_eq!(row["project"], "TERM");
        }

        // Status filter.
        let complete = parse(&tool.execute(json!({"status": "complete"})).await.unwrap());
        for row in complete["prefixes"].as_array().unwrap() {
            assert_eq!(row["status"], "complete");
        }
    }

    #[tokio::test]
    async fn get_hits_and_misses() {
        let store = baseline_only_store();
        let tool = PlanePrefixGet { store };
        let hit = parse(&tool.execute(json!({"prefix": "scrb"})).await.unwrap());
        assert_eq!(hit["found"], true);
        assert_eq!(hit["entry"]["prefix"], "SCRB");
        assert_eq!(hit["entry"]["in_baseline"], true);

        let miss = parse(&tool.execute(json!({"prefix": "ZZZZ"})).await.unwrap());
        assert_eq!(miss["found"], false);
    }

    #[tokio::test]
    async fn check_free_and_taken_with_suggestions() {
        let store = baseline_only_store();
        let tool = PlanePrefixCheck { store };

        let taken = parse(&tool.execute(json!({"prefix": "SCRB"})).await.unwrap());
        assert_eq!(taken["valid"], true);
        assert_eq!(taken["free"], false);
        assert!(taken["existing"].is_object());
        let sugg = taken["suggestions"].as_array().unwrap();
        assert!(!sugg.is_empty(), "taken prefix should suggest alternatives");
        // First suggestion should itself be free.
        assert_eq!(sugg[0], "SCRB2");

        let free = parse(&tool.execute(json!({"prefix": "ZZQW"})).await.unwrap());
        assert_eq!(free["free"], true);
        assert!(free["suggestions"].as_array().unwrap().is_empty());

        let invalid = parse(&tool.execute(json!({"prefix": "a-b-c"})).await.unwrap());
        assert_eq!(invalid["valid"], false);
        assert_eq!(invalid["free"], false);
    }

    #[tokio::test]
    async fn register_collision_rejects() {
        let store = baseline_only_store();
        let tool = PlanePrefixRegister { store };
        let res = parse(&tool.execute(json!({"prefix": "scrb"})).await.unwrap());
        assert_eq!(res["ok"], false);
        assert_eq!(res["reason"], "collision");
        assert!(res["existing"].is_object());
    }

    #[tokio::test]
    async fn register_new_without_overlay_reports_file_path() {
        let store = baseline_only_store();
        let tool = PlanePrefixRegister { store };
        let res = parse(
            &tool
                .execute(json!({"prefix": "ZZQW", "name": "Test", "project": "term"}))
                .await
                .unwrap(),
        );
        assert_eq!(res["ok"], false);
        assert_eq!(res["persisted"], false);
        assert_eq!(res["reason"], "overlay_unconfigured");
        assert_eq!(res["entry"]["prefix"], "ZZQW");
        assert_eq!(res["entry"]["project"], "TERM");
        assert!(res["message"].as_str().unwrap().contains("prefix_registry.toml"));
    }

    #[tokio::test]
    async fn register_invalid_prefix_errors() {
        let store = baseline_only_store();
        let tool = PlanePrefixRegister { store };
        let err = tool.execute(json!({"prefix": "a b"})).await;
        assert!(matches!(err, Err(ToolError::InvalidArgument(_))));
    }

    #[tokio::test]
    async fn register_invalid_status_errors() {
        let store = baseline_only_store();
        let tool = PlanePrefixRegister { store };
        let err = tool
            .execute(json!({"prefix": "ZZQW", "status": "banana"}))
            .await;
        assert!(matches!(err, Err(ToolError::InvalidArgument(_))));
    }

    #[tokio::test]
    async fn retire_without_overlay_reports_file_path() {
        let store = baseline_only_store();
        let tool = PlanePrefixRetire { store };
        let res = parse(&tool.execute(json!({"prefix": "scrb"})).await.unwrap());
        assert_eq!(res["ok"], false);
        assert_eq!(res["reason"], "overlay_unconfigured");
        assert_eq!(res["entry"]["status"], "retired");
        assert!(res["message"].as_str().unwrap().contains("prefix_registry.toml"));
    }

    #[tokio::test]
    async fn retire_unknown_prefix_not_found() {
        let store = baseline_only_store();
        let tool = PlanePrefixRetire { store };
        let res = parse(&tool.execute(json!({"prefix": "ZZZZ"})).await.unwrap());
        assert_eq!(res["ok"], false);
        assert_eq!(res["reason"], "not_found");
    }

    // ── Overlay fail-open: a configured-but-unreachable Redis must degrade to
    // baseline within the op timeout, never hang or error the tool. Uses a
    // routable-but-dead port so the connect attempt fails fast.
    #[tokio::test]
    async fn overlay_unreachable_is_fail_open() {
        let ov = PrefixOverlay::build(
            "redis://127.0.0.1:1/0", // pii-test-fixture
            None,
            Duration::from_millis(150),
        )
        .expect("client builds for a well-formed URL");

        // Direct overlay ops report Unavailable, promptly.
        let start = std::time::Instant::now();
        assert_eq!(ov.list().await, Err(OverlayError::Unavailable));
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "fail-open must be prompt, took {:?}",
            start.elapsed()
        );
        let entry = PrefixEntry {
            prefix: "ZZQW".into(),
            name: String::new(),
            project: String::new(),
            spec_id: String::new(),
            status: "active".into(),
            description: String::new(),
            created: "2026-07-08".into(),
        };
        assert_eq!(ov.put(&entry).await, Err(OverlayError::Unavailable));

        // And the store built on it still lists the baseline (reachable=false).
        let store = Arc::new(PrefixStore { overlay: Some(ov) });
        let (map, reachable) = store.merged().await;
        assert_eq!(reachable, Some(false));
        assert!(map.contains_key("SCRB"));

        // register on that store → overlay_unavailable, not a crash.
        let tool = PlanePrefixRegister { store };
        let res = parse(&tool.execute(json!({"prefix": "ZZQW"})).await.unwrap());
        assert_eq!(res["persisted"], false);
        assert_eq!(res["reason"], "overlay_unavailable");
    }

    // ── No-secret-leak: the overlay's Debug must never print the password even
    // though it is layered into the client's connection info.
    #[test]
    fn overlay_debug_hides_password() {
        let ov = PrefixOverlay::build(
            "redis://127.0.0.1:6399/0", // pii-test-fixture
            Some("hunter2SuperSecret".into()),
            Duration::from_millis(100),
        )
        .expect("client builds");
        let dbg = format!("{ov:?}");
        assert!(!dbg.contains("hunter2SuperSecret"), "password leaked in Debug: {dbg}");
    }
}
