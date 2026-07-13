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
//! - `plane_prefix_promote` — make a claim DURABLE: render the baseline
//!   `[[prefix]]` row, commit it to a branch of the Terminus repo, and open a
//!   Gitea PR. This is the only client-side durable path — the client gateway
//!   has no Redis overlay by design (PROMO-01).
//!
//! ## Client vs server durability (PROMO-01)
//! `plane_prefix_register` only writes the runtime Redis overlay, and the client
//! gateway intentionally runs with NO Redis (a security posture — no shared
//! mutable state reachable from the client surface). That left the client with
//! no way to make a claim durable. `plane_prefix_promote` closes that gap: it
//! needs NO overlay — it writes the reviewed baseline TOML (the real source of
//! truth) directly via a git branch + Gitea PR. On a server where an overlay IS
//! reachable it MAY additionally clear the promoted pending claim, but it never
//! depends on Redis to function.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::warn;

use crate::error::ToolError;
use crate::gitea::GiteaClient;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

/// The git-versioned baseline, compiled in so a baseline read never depends on
/// the process's working directory or on Redis being reachable.
const BASELINE_TOML: &str = include_str!("../../data/prefix_registry.toml");

/// Valid status values for a prefix entry.
const VALID_STATUSES: &[&str] = &["active", "retired", "ingested", "complete"];

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

/// Runtime overlay store backed by the SHARED BLD-20 Redis pool
/// (`crate::redis::RedisBackend`), addressed through the typed
/// [`crate::redis::Namespace::Prefix`] — i.e. the DURABLE logical DB
/// (`REDIS_DB_DURABLE`, server-side `noeviction`) and the `prefix:*` keyspace.
/// It does NOT open its own connection or choose its own DB: it borrows the one
/// shared pool every other consumer uses, so overlay claims are cross-instance
/// visible and always land in the durable DB (never the volatile cache DB).
/// Per-op timeout + fail-open are handled uniformly by `RedisBackend::with_conn`
/// (which bounds connection acquisition AND the op in one deadline).
struct PrefixOverlay {
    backend: Arc<crate::redis::RedisBackend>,
    /// Durable overlay hash key in the typed `prefix:*` namespace
    /// (`prefix:overlay:v1`). One hash; field = uppercased prefix,
    /// value = JSON-encoded [`PrefixEntry`].
    hash_key: String,
}

/// Hand-written `Debug` that never touches `backend` (whose internals carry the
/// Redis password) — only the non-secret hash key is shown.
impl std::fmt::Debug for PrefixOverlay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrefixOverlay")
            .field("hash_key", &self.hash_key)
            .finish_non_exhaustive()
    }
}

impl PrefixOverlay {
    /// Build over the shared BLD-20 Redis pool. The endpoint is resolved by
    /// `RedisBackend` from `REDIS_URL` (legacy `PLANE_REDIS_URL` fallback),
    /// materialized from the vault at boot (S1/S7). Returns `None` when Redis is
    /// not configured — the pure-baseline path, identical to having no overlay.
    /// Routing the overlay through the SAME pool + durable namespace is what
    /// makes `plane_prefix_register` durable cross-instance (BLD-20 step 4).
    fn from_env() -> Option<Arc<Self>> {
        Some(Self::with_backend(crate::redis::RedisBackend::from_env()?))
    }

    /// Build over an already-constructed shared backend (the wiring seam + test
    /// entry point). The overlay lives in the durable `prefix:*` namespace, so
    /// its logical DB and eviction protection follow `Namespace::Prefix`.
    fn with_backend(backend: Arc<crate::redis::RedisBackend>) -> Arc<Self> {
        Arc::new(Self {
            hash_key: crate::redis::Namespace::Prefix.key("overlay:v1"),
            backend,
        })
    }

    /// All overlay claims (field -> entry). `Err(Unavailable)` on any
    /// Redis error/timeout — the caller treats that as "no overlay".
    async fn list(&self) -> Result<Vec<PrefixEntry>, OverlayError> {
        let key = self.hash_key.clone();
        let map: std::collections::HashMap<String, String> = self
            .backend
            .with_conn(crate::redis::Namespace::Prefix, |mut conn| async move {
                redis::cmd("HGETALL").arg(&key).query_async(&mut conn).await
            })
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
    }

    /// Atomically create a claim only if the field does not already exist
    /// (single `HSETNX`). `Ok(true)` = created, `Ok(false)` = a claim was
    /// already there (lost a concurrent race — caller reports a collision),
    /// `Err(Unavailable)` = the write did not land. This closes the TOCTOU gap
    /// between the `merged()` collision read and the write for overlay-only
    /// prefixes, so two concurrent registrations of the same free prefix cannot
    /// both succeed.
    async fn put_new(&self, entry: &PrefixEntry) -> Result<bool, OverlayError> {
        let field = entry.prefix.to_uppercase();
        let payload = serde_json::to_string(entry).map_err(|_| OverlayError::Unavailable)?;
        let key = self.hash_key.clone();
        let created: i64 = self
            .backend
            .with_conn(crate::redis::Namespace::Prefix, |mut conn| async move {
                redis::cmd("HSETNX")
                    .arg(&key)
                    .arg(&field)
                    .arg(&payload)
                    .query_async(&mut conn)
                    .await
            })
            .await
            .map_err(|_| OverlayError::Unavailable)?;
        Ok(created == 1)
    }

    /// Delete one claim from the overlay hash (best-effort). Used by
    /// `plane_prefix_promote` to drop a pending claim once it has been promoted
    /// into the reviewed baseline. `Err(Unavailable)` if the delete did not land;
    /// callers treat that as "left the pending claim in place" — never fatal.
    async fn del(&self, prefix: &str) -> Result<bool, OverlayError> {
        let field = prefix.to_uppercase();
        let key = self.hash_key.clone();
        let removed: i64 = self
            .backend
            .with_conn(crate::redis::Namespace::Prefix, |mut conn| async move {
                redis::cmd("HDEL")
                    .arg(&key)
                    .arg(&field)
                    .query_async(&mut conn)
                    .await
            })
            .await
            .map_err(|_| OverlayError::Unavailable)?;
        Ok(removed == 1)
    }

    /// Write/replace one claim (overwrite). Used by retire, which intentionally
    /// overrides an existing entry's status (or writes a retire override for a
    /// baseline-only prefix). `Err(Unavailable)` if the write did not land.
    async fn put(&self, entry: &PrefixEntry) -> Result<(), OverlayError> {
        let field = entry.prefix.to_uppercase();
        let payload = serde_json::to_string(entry).map_err(|_| OverlayError::Unavailable)?;
        let key = self.hash_key.clone();
        self.backend
            .with_conn(crate::redis::Namespace::Prefix, |mut conn| async move {
                redis::cmd("HSET")
                    .arg(&key)
                    .arg(&field)
                    .arg(&payload)
                    .query_async::<_, ()>(&mut conn)
                    .await
            })
            .await
            .map_err(|_| OverlayError::Unavailable)
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

    /// Best-effort: drop a pending overlay claim after it has been promoted into
    /// the baseline. Returns `Some(true)` if a claim was removed, `Some(false)`
    /// if there was nothing to remove, `None` if no overlay is configured or it
    /// was unreachable. NEVER fatal — promotion does not depend on this.
    async fn clear_overlay_claim(&self, prefix: &str) -> Option<bool> {
        match &self.overlay {
            None => None,
            Some(ov) => ov.del(prefix).await.ok(),
        }
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
            // Atomic create-if-absent so a concurrent claim of the same free
            // prefix cannot double-succeed (codex P2).
            Some(ov) => match ov.put_new(&entry).await {
                Ok(true) => Ok(json!({
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
                Ok(false) => Ok(json!({
                    "ok": false,
                    "persisted": false,
                    "reason": "collision",
                    "message": format!(
                        "prefix '{key}' was just claimed concurrently in the overlay — pick another"
                    ),
                    "suggestions": PlanePrefixCheck::suggest(&key, &map, 3),
                    "overlay": overlay_note(reachable),
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

// ─── plane_prefix_promote (PROMO-01) ─────────────────────────────────────────

/// Repo-relative path of the baseline TOML within the Terminus checkout. This is
/// the exact file compiled in via `include_str!` at the top of this module.
const REGISTRY_REL_PATH: &str = "data/prefix_registry.toml";

/// Fields that MUST be supplied when promoting a prefix that has no overlay
/// claim to seed from — a from-scratch baseline row needs at least these to be
/// meaningful. `spec_id`/`status`/`created` are optional (they default).
const PROMOTE_REQUIRED_FIELDS: &[&str] = &["name", "project", "description"];

/// The per-repo Plane *project* prefixes a baseline row may belong to (the
/// v3.7 consolidation set). A durable baseline write is validated against this
/// set — a typo'd project is rejected before anything is committed.
const VALID_PROJECTS: &[&str] = &["HARM", "LUM", "CHRD", "TERM", "RAIL", "HW", "PSH"];

/// Validate the owning-project field (already uppercased) against
/// [`VALID_PROJECTS`]. Returns a clear error naming the allowed set otherwise.
fn validate_project(project: &str) -> Result<(), String> {
    if VALID_PROJECTS.contains(&project) {
        Ok(())
    } else {
        Err(format!(
            "project '{project}' invalid; expected one of {VALID_PROJECTS:?}"
        ))
    }
}

/// Validate an ISO `YYYY-MM-DD` date. A durable baseline row's `created` is
/// written verbatim, so it must be a real calendar date, not free text.
fn validate_created(created: &str) -> Result<(), String> {
    chrono::NaiveDate::parse_from_str(created.trim(), "%Y-%m-%d")
        .map(|_| ())
        .map_err(|_| format!("created '{created}' is not a valid YYYY-MM-DD date"))
}

/// Resolve the working-tree root whose `data/prefix_registry.toml` is edited.
/// Configurable via `PREFIX_REGISTRY_REPO_DIR` (default `"."`) — NOT a secret,
/// so a plain env read is correct here (mirrors gitea/plane config reads). Git
/// transport runs against this directory; it must be a Terminus checkout with an
/// `origin` remote when `open_pr` is true.
fn repo_dir() -> PathBuf {
    std::env::var("PREFIX_REGISTRY_REPO_DIR")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// The Gitea repo name that hosts this baseline file. Defaults to `"Terminus"`
/// (a repo name, not infra PII), overridable via `PREFIX_REGISTRY_GITEA_REPO`.
/// The owner/org is resolved by [`GiteaClient`] (`GITEA_OWNER`), never hardcoded.
fn gitea_repo_name() -> String {
    std::env::var("PREFIX_REGISTRY_GITEA_REPO")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "Terminus".to_string())
}

/// TOML-serialization wrapper so an entry renders as an array-of-tables
/// `[[prefix]]` block, byte-for-byte the shape the baseline file uses (and that
/// [`Baseline`] round-trips). Field order follows `PrefixEntry`'s declaration
/// order, which matches the file.
#[derive(Serialize)]
struct PrefixRowDoc<'a> {
    prefix: Vec<&'a PrefixEntry>,
}

/// Render one entry as a `[[prefix]]` TOML block (trailing newline included).
/// Uses the `toml` serializer (not hand-rolled string formatting) so any
/// special characters in the fields are escaped correctly and the result
/// round-trips through the same `toml`/serde types the baseline file uses.
fn render_prefix_row(entry: &PrefixEntry) -> Result<String, String> {
    toml::to_string(&PrefixRowDoc { prefix: vec![entry] })
        .map_err(|e| format!("failed to render prefix TOML row: {e}"))
}

/// Idempotently append a rendered `[[prefix]]` row to the existing file content.
/// Returns `(new_content, appended)`; `appended == false` means the prefix is
/// already present in the file (case-insensitive) and the content is unchanged —
/// no duplicate is written.
fn append_row_idempotent(existing: &str, entry: &PrefixEntry) -> Result<(String, bool), String> {
    let parsed: Baseline =
        toml::from_str(existing).map_err(|e| format!("existing registry TOML is malformed: {e}"))?;
    if parsed
        .prefix
        .iter()
        .any(|e| e.prefix.trim().eq_ignore_ascii_case(&entry.prefix))
    {
        return Ok((existing.to_string(), false));
    }
    let block = render_prefix_row(entry)?;
    let mut out = existing.trim_end().to_string();
    out.push_str("\n\n");
    out.push_str(&block);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    Ok((out, true))
}

/// Build the entry to promote from an optional overlay/pending base plus the
/// caller's args. When `base` is `Some`, its fields seed the row and any
/// provided arg overrides them. When `base` is `None`, the row is built purely
/// from args and the [`PROMOTE_REQUIRED_FIELDS`] must all be present — otherwise
/// `Err(missing_fields)` is returned so the caller can report a clean error with
/// NO partial write. `key` is the already-validated (uppercased) prefix.
fn build_promote_entry(
    key: &str,
    base: Option<&PrefixEntry>,
    args: &Value,
) -> Result<PrefixEntry, Vec<String>> {
    // String arg override helper: trimmed, empty treated as "not provided".
    let arg = |field: &str| -> Option<String> {
        args.get(field)
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };

    if base.is_none() {
        // From-scratch promote: require the substantive fields up front.
        let missing: Vec<String> = PROMOTE_REQUIRED_FIELDS
            .iter()
            .filter(|f| arg(f).is_none())
            .map(|f| f.to_string())
            .collect();
        if !missing.is_empty() {
            return Err(missing);
        }
    }

    let b = base.cloned().unwrap_or_else(|| PrefixEntry {
        prefix: key.to_string(),
        name: String::new(),
        project: String::new(),
        spec_id: String::new(),
        status: default_status(),
        description: String::new(),
        created: String::new(),
    });

    Ok(PrefixEntry {
        prefix: key.to_string(),
        name: arg("name").unwrap_or(b.name),
        project: arg("project").map(|p| p.to_uppercase()).unwrap_or(b.project),
        spec_id: arg("spec_id").unwrap_or(b.spec_id),
        // status defaults to the base's (validated separately by the caller).
        status: arg("status").unwrap_or(b.status),
        description: arg("description").unwrap_or(b.description),
        created: arg("created").unwrap_or_else(|| {
            if b.created.trim().is_empty() {
                chrono::Utc::now().format("%Y-%m-%d").to_string()
            } else {
                b.created.clone()
            }
        }),
    })
}

/// `plane_prefix_promote` — make a prefix claim durable by writing the baseline
/// TOML row and opening a Terminus PR. Needs NO client-side Redis.
pub struct PlanePrefixPromote {
    store: Arc<PrefixStore>,
    /// Configured Gitea client for opening the PR. `None` when Gitea is not
    /// configured in this process — `open_pr: true` then returns a clean
    /// `ok: false, reason: "gitea_unconfigured"` before touching git.
    gitea: Option<GiteaClient>,
    /// Working-tree root whose `data/prefix_registry.toml` seeds the promotion.
    /// The live checkout is NEVER mutated: all writes happen in a throwaway
    /// `git worktree` created off `main`. Field (not a global env read) so tests
    /// can point it at a temp repo without touching env or the network.
    repo_dir: PathBuf,
    /// Gitea repo name that hosts the baseline file (owner resolved by the client).
    gitea_repo: String,
}

impl PlanePrefixPromote {
    /// Run a git subcommand in `dir`, returning stdout on success or a `ToolError`
    /// carrying stderr on failure. Minimal `std::process::Command` transport —
    /// acceptable per the PROMO-01 plan (git runs on the dev box). No secrets pass
    /// through here (git auth is the checkout's own credential helper).
    fn git(dir: &Path, args: &[&str]) -> Result<String, ToolError> {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .map_err(|e| ToolError::Http(format!("failed to run `git {}`: {e}", args.join(" "))))?;
        if !out.status.success() {
            return Err(ToolError::Http(format!(
                "`git {}` failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }

    /// Pick a base ref for the throwaway worktree. Best-effort `fetch origin main`
    /// (fetch touches only remote-tracking refs, never the working tree/branches,
    /// so it is safe against the live checkout), then prefer `origin/main` if it
    /// resolves, else fall back to local `main`. In a test repo with no remote the
    /// fetch/rev-parse fail silently and this returns `"main"`.
    fn resolve_base_ref(repo_dir: &Path) -> String {
        let _ = Self::git(repo_dir, &["fetch", "origin", "main"]);
        if Self::git(repo_dir, &["rev-parse", "--verify", "origin/main"]).is_ok() {
            "origin/main".to_string()
        } else {
            "main".to_string()
        }
    }

    /// The side-effecting half: create a THROWAWAY git worktree off `main`, do
    /// the TOML append + commit + push + PR inside it, then ALWAYS remove the
    /// worktree and the local branch — success or error — so the process's own
    /// long-running checkout (terminus-rs runs embedded in the Chord server) is
    /// NEVER mutated. Kept as its own method so unit tests exercise entry-building,
    /// TOML rendering, and the `already_promoted`/validation decisions without git.
    async fn transport(
        &self,
        key: &str,
        entry: &PrefixEntry,
        open_pr: bool,
        identity: Option<&str>,
    ) -> Result<Value, ToolError> {
        let repo_dir = self.repo_dir.clone();
        let branch = format!("prefix-promote-{key}");
        let base_ref = Self::resolve_base_ref(&repo_dir);

        // Unique temp worktree path so concurrent promotes never collide.
        let unique = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let wt = std::env::temp_dir().join(format!("terminus-prefix-promote-{key}-{unique}"));
        let wt_str = wt.to_string_lossy().to_string();

        // Clear any stale local branch of the same name (best-effort) so a retry
        // after an earlier failure can recreate the worktree cleanly.
        let _ = Self::git(&repo_dir, &["worktree", "prune"]);
        let _ = Self::git(&repo_dir, &["branch", "-D", &branch]);

        // Create the isolated worktree ON A NEW BRANCH off the base. Nothing in
        // the live checkout changes.
        Self::git(
            &repo_dir,
            &["worktree", "add", &wt_str, "-b", &branch, &base_ref],
        )?;

        // Do the work; capture the outcome so cleanup ALWAYS runs afterwards.
        let outcome = self
            .transport_in_worktree(&repo_dir, &wt, &branch, key, entry, open_pr, identity)
            .await;

        // Finally-style cleanup: remove the worktree and delete the local branch,
        // regardless of success/failure. (On success the REMOTE branch is kept —
        // the PR needs it; on a post-push failure the remote branch was already
        // deleted inside `transport_in_worktree`.)
        let _ = Self::git(&repo_dir, &["worktree", "remove", "--force", &wt_str]);
        let _ = Self::git(&repo_dir, &["branch", "-D", &branch]);

        outcome
    }

    /// The in-worktree steps, isolated so the caller can guarantee cleanup.
    #[allow(clippy::too_many_arguments)]
    async fn transport_in_worktree(
        &self,
        repo_dir: &Path,
        wt: &Path,
        branch: &str,
        key: &str,
        entry: &PrefixEntry,
        open_pr: bool,
        identity: Option<&str>,
    ) -> Result<Value, ToolError> {
        let file = wt.join(REGISTRY_REL_PATH);

        // Read the base version of the file FROM the worktree and compute the
        // idempotent append.
        let existing = std::fs::read_to_string(&file).map_err(|e| {
            ToolError::Http(format!(
                "cannot read {} in the promotion worktree: {e}",
                file.display()
            ))
        })?;
        let (new_content, appended) =
            append_row_idempotent(&existing, entry).map_err(ToolError::Http)?;

        // Already present in the file → nothing to commit; report idempotently.
        if !appended {
            return Ok(json!({
                "ok": true,
                "appended": false,
                "branch": Value::Null,
                "pr_url": Value::Null,
                "entry": entry_json(entry),
                "note": format!(
                    "prefix '{key}' is already present in {REGISTRY_REL_PATH}; no branch or PR created"
                ),
            }));
        }

        // Write + commit ONTO the worktree's branch.
        std::fs::write(&file, &new_content)
            .map_err(|e| ToolError::Http(format!("failed to write {}: {e}", file.display())))?;
        Self::git(wt, &["add", REGISTRY_REL_PATH])?;
        let commit_msg = format!("chore(prefix): promote {key} into baseline prefix registry");
        Self::git(wt, &["commit", "-m", &commit_msg])?;

        if !open_pr {
            // open_pr=false: leave the commit on the branch, return the diff, no
            // PR. Surface a diff error instead of silently emptying it.
            let diff = Self::git(wt, &["diff", "HEAD~1", "HEAD", "--", REGISTRY_REL_PATH])?;
            return Ok(json!({
                "ok": true,
                "appended": true,
                "branch": branch,
                "pr_url": Value::Null,
                "entry": entry_json(entry),
                "diff": diff,
                "note": "open_pr=false: committed to the throwaway branch, no PR opened",
            }));
        }

        // Push from the worktree (shares the live repo's remotes/credentials).
        let gitea = self.gitea.as_ref().ok_or_else(|| {
            ToolError::NotConfigured("Gitea is not configured; cannot open the promotion PR".into())
        })?;
        Self::git(wt, &["push", "-u", "origin", branch])?;

        // PR base is the remote's default branch NAME (not `origin/main`).
        let pr_body = format!(
            "Promote prefix `{key}` into the durable baseline registry \
             (`{REGISTRY_REL_PATH}`).\n\n\
             - name: {}\n- project: {}\n- spec_id: {}\n- status: {}\n- created: {}\n\n\
             Generated by `plane_prefix_promote` (PROMO-01).",
            entry.name, entry.project, entry.spec_id, entry.status, entry.created
        );
        let mut pr_args = json!({
            "repo": self.gitea_repo,
            "title": format!("chore(prefix): promote {key} into baseline registry"),
            "head": branch,
            "base": "main",
            "body": pr_body,
        });
        if let Some(id) = identity {
            pr_args["identity"] = json!(id);
        }

        let pr = match gitea.create_pull(&pr_args).await {
            Ok(pr) => pr,
            Err(e) => {
                // The branch was pushed but the PR did not open — delete the
                // remote branch (best-effort) so a re-run is a clean, idempotent
                // push rather than a non-fast-forward failure.
                let _ = Self::git(repo_dir, &["push", "origin", "--delete", branch]);
                return Err(e);
            }
        };

        // Best-effort: drop the now-promoted pending overlay claim if reachable.
        // Never fatal, and never adds a Redis dependency (no-op with no overlay).
        let overlay_cleared = self.store.clear_overlay_claim(key).await;

        Ok(json!({
            "ok": true,
            "appended": true,
            "branch": branch,
            "pr_url": pr.html_url,
            "pr_number": pr.number,
            "entry": entry_json(entry),
            "overlay_claim_cleared": overlay_cleared,
        }))
    }
}

#[async_trait]
impl RustTool for PlanePrefixPromote {
    fn name(&self) -> &str {
        "plane_prefix_promote"
    }
    fn description(&self) -> &str {
        "Make a prefix claim DURABLE: render its baseline `[[prefix]]` row, commit it to a branch \
         of the Terminus repo (data/prefix_registry.toml — the reviewed source of truth compiled \
         into the binary), and open a Gitea PR. Unlike plane_prefix_register (which only writes the \
         runtime Redis overlay), this needs NO Redis and is the client-side durable path. Prefers \
         an existing overlay/pending claim as the row's source; otherwise builds the row from the \
         provided args (name/project/description then required). Returns already_promoted (no PR) \
         if the prefix is already in the baseline. Set open_pr=false to commit to the branch and \
         return the diff without opening a PR."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "prefix": {"type": "string", "description": "Prefix to promote, 2-8 chars [A-Z0-9] starting with a letter"},
                "name": {"type": "string", "description": "Override/supply the full human-readable name/title"},
                "project": {"type": "string", "description": "Override/supply the owning Plane project (HARM/LUM/CHRD/TERM/RAIL/HW/PSH)"},
                "spec_id": {"type": "string", "description": "Override/supply the originating spec id, e.g. S101-prefix-library"},
                "description": {"type": "string", "description": "Override/supply the one-line summary"},
                "status": {"type": "string", "description": "Override the lifecycle status: active|ingested|complete|retired"},
                "created": {"type": "string", "description": "Override the ISO date YYYY-MM-DD (defaults to the claim's date, else today UTC)"},
                "open_pr": {"type": "boolean", "description": "Open a Gitea PR (default true). When false, commit to the branch and return the diff only."},
                "identity": {"type": "string", "description": "Gitea identity (GITEA_PAT_<NAME>) to open the PR as; omit for the configured default"}
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

        // Validate any status override early (clean error, no writes).
        if let Some(s) = args.get("status").and_then(|v| v.as_str()) {
            normalize_status(s).map_err(ToolError::InvalidArgument)?;
        }

        let open_pr = args.get("open_pr").and_then(|v| v.as_bool()).unwrap_or(true);
        let identity = args
            .get("identity")
            .and_then(|v| v.as_str())
            .map(|s| s.trim())
            .filter(|s| !s.is_empty());

        // Merged view: is it already in the baseline? Is there a pending claim to
        // seed from? (Reads are fail-open — no overlay is fine.)
        let (map, _reachable) = self.store.merged().await;
        if let Some(row) = map.get(&key) {
            if row.in_baseline {
                // Already durable — drop any stale pending overlay claim too
                // (best-effort; no-op without an overlay).
                let _ = self.store.clear_overlay_claim(&key).await;
                return Ok(json!({
                    "ok": false,
                    "reason": "already_promoted",
                    "message": format!("prefix '{key}' is already in the reviewed baseline registry"),
                    "entry": row.to_json(),
                })
                .to_string());
            }
        }
        // Seed from an existing overlay/pending claim if present (not in baseline).
        let base_entry = map.get(&key).map(|row| row.entry.clone());

        // Build the entry to promote (overlay-seeded or from-args).
        let entry = match build_promote_entry(&key, base_entry.as_ref(), &args) {
            Ok(e) => e,
            Err(missing) => {
                return Ok(json!({
                    "ok": false,
                    "reason": "insufficient_args",
                    "required": PROMOTE_REQUIRED_FIELDS,
                    "missing": missing,
                    "message": format!(
                        "prefix '{key}' has no overlay claim to promote and the request is missing \
                         required field(s): {}. Supply them or register the claim first.",
                        missing.join(", ")
                    ),
                })
                .to_string());
            }
        };
        // Normalize the final status (covers a status inherited from the claim).
        let entry = PrefixEntry {
            status: normalize_status(&entry.status).map_err(ToolError::InvalidArgument)?,
            ..entry
        };

        // Validate the durable-write fields BEFORE any git mutation: `project`
        // must be a known Plane project, and `created` a real YYYY-MM-DD date.
        // These land verbatim in the reviewed baseline, so a typo must fail here.
        validate_project(&entry.project).map_err(ToolError::InvalidArgument)?;
        validate_created(&entry.created).map_err(ToolError::InvalidArgument)?;

        // A PR needs Gitea configured — gate here, AFTER the already_promoted /
        // insufficient_args decisions (those never touch Gitea) but BEFORE any
        // git mutation. open_pr=false never needs Gitea.
        if open_pr && self.gitea.is_none() {
            return Ok(json!({
                "ok": false,
                "reason": "gitea_unconfigured",
                "message": "Gitea is not configured (no GITEA_URL / GITEA_PAT_<NAME>); \
                            set open_pr=false to commit to a branch only, or configure Gitea",
                "entry": entry_json(&entry),
            })
            .to_string());
        }

        let result = self.transport(&key, &entry, open_pr, identity).await?;
        Ok(result.to_string())
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

/// Register the six prefix sub-tools into the registry. Called from
/// [`super::register`] so they appear alongside the `plane_*` tools in BOTH the
/// core Chord registry and the personal registry.
pub fn register(registry: &mut ToolRegistry) {
    let store = PrefixStore::from_env();
    // Gitea client for `plane_prefix_promote`'s PR step. `None` when Gitea is not
    // configured — the tool then only supports `open_pr: false` (branch + diff).
    let gitea = GiteaClient::from_env().ok();
    let tools: Vec<Box<dyn RustTool>> = vec![
        Box::new(PlanePrefixList { store: store.clone() }),
        Box::new(PlanePrefixRegister { store: store.clone() }),
        Box::new(PlanePrefixGet { store: store.clone() }),
        Box::new(PlanePrefixCheck { store: store.clone() }),
        Box::new(PlanePrefixRetire { store: store.clone() }),
        Box::new(PlanePrefixPromote {
            store: store.clone(),
            gitea,
            repo_dir: repo_dir(),
            gitea_repo: gitea_repo_name(),
        }),
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
        let backend = crate::redis::RedisBackend::build(
            "redis://127.0.0.1:1", // pii-test-fixture — routable but dead
            None,
            0,
            1,
            Duration::from_millis(150),
        )
        .expect("backend builds for a well-formed URL");
        let ov = PrefixOverlay::with_backend(backend);

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
        assert_eq!(ov.put_new(&entry).await, Err(OverlayError::Unavailable));

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

    // ── PROMO-01: plane_prefix_promote ────────────────────────────────────────

    fn sample_entry() -> PrefixEntry {
        PrefixEntry {
            prefix: "ZZQW".into(),
            name: "Test promote".into(),
            project: "TERM".into(),
            spec_id: "S200-test-promote".into(),
            status: "active".into(),
            description: "A test prefix for promotion.".into(),
            created: "2026-07-11".into(),
        }
    }

    /// A promote tool with no overlay and no Gitea — enough for the pure
    /// decision paths (already_promoted / validation / insufficient_args) that
    /// never reach git or the network. `repo_dir` is a placeholder; these paths
    /// return before any git call.
    fn promote_tool() -> PlanePrefixPromote {
        PlanePrefixPromote {
            store: baseline_only_store(),
            gitea: None,
            repo_dir: PathBuf::from("."),
            gitea_repo: "Terminus".into(),
        }
    }

    /// Initialise a throwaway git repo on branch `main` with a seeded
    /// `data/prefix_registry.toml`, returning its path. Used by the seam tests so
    /// `transport` runs end-to-end (open_pr:false) without a network or env
    /// globals. Caller removes the dir.
    fn init_temp_repo(seed_entries: &str) -> PathBuf {
        let unique = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let dir = std::env::temp_dir().join(format!("terminus-prefix-promote-test-{unique}"));
        std::fs::create_dir_all(dir.join("data")).unwrap();
        std::fs::write(dir.join("data/prefix_registry.toml"), seed_entries).unwrap();

        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
        };
        run(&["init", "-q"]);
        // Force the default branch to `main` regardless of the host git default.
        run(&["symbolic-ref", "HEAD", "refs/heads/main"]);
        run(&["config", "user.email", "<email>"]); // pii-test-fixture
        run(&["config", "user.name", "Test"]);
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "seed"]);
        dir
    }

    /// A promote tool rooted at a temp repo, no Gitea (so only open_pr:false is
    /// exercised end-to-end through the throwaway-worktree transport).
    fn promote_tool_at(dir: &Path) -> PlanePrefixPromote {
        PlanePrefixPromote {
            store: baseline_only_store(),
            gitea: None,
            repo_dir: dir.to_path_buf(),
            gitea_repo: "Terminus".into(),
        }
    }

    #[test]
    fn render_row_round_trips_through_baseline_types() {
        let entry = sample_entry();
        let block = render_prefix_row(&entry).unwrap();
        // Renders as an array-of-tables block in the baseline shape.
        assert!(block.contains("[[prefix]]"), "block: {block}");
        assert!(block.contains("prefix = \"ZZQW\""));
        // Round-trips through the SAME Baseline/PrefixEntry types the file uses.
        let parsed: Baseline = toml::from_str(&block).unwrap();
        assert_eq!(parsed.prefix.len(), 1);
        assert_eq!(parsed.prefix[0], entry);
    }

    #[test]
    fn render_row_field_order_matches_baseline() {
        let block = render_prefix_row(&sample_entry()).unwrap();
        let order: Vec<&str> = ["prefix", "name", "project", "spec_id", "status", "description", "created"]
            .iter()
            .map(|f| *f)
            .collect();
        let mut last = 0usize;
        for field in order {
            let idx = block.find(&format!("{field} =")).unwrap_or_else(|| panic!("missing field {field} in {block}"));
            assert!(idx >= last, "field {field} out of order in {block}");
            last = idx;
        }
    }

    #[test]
    fn append_row_is_idempotent() {
        let existing = "[[prefix]]\nprefix = \"AAA\"\nname = \"\"\nproject = \"\"\nspec_id = \"\"\nstatus = \"active\"\ndescription = \"\"\ncreated = \"\"\n";
        let entry = sample_entry();
        let (content1, appended1) = append_row_idempotent(existing, &entry).unwrap();
        assert!(appended1, "first append should add the row");
        assert!(content1.contains("prefix = \"ZZQW\""));
        // The combined content must still parse as a valid baseline.
        let parsed: Baseline = toml::from_str(&content1).unwrap();
        assert_eq!(parsed.prefix.len(), 2);
        // Appending the same prefix again is a no-op (case-insensitive).
        let mut dup = entry.clone();
        dup.prefix = "zzqw".into();
        let (content2, appended2) = append_row_idempotent(&content1, &dup).unwrap();
        assert!(!appended2, "second append of same prefix must not duplicate");
        assert_eq!(content1, content2);
    }

    #[test]
    fn build_entry_from_args_with_no_overlay() {
        let args = json!({
            "prefix": "ZZQW",
            "name": "Test promote",
            "project": "term",
            "description": "A test prefix.",
        });
        let entry = build_promote_entry("ZZQW", None, &args).unwrap();
        assert_eq!(entry.prefix, "ZZQW");
        assert_eq!(entry.name, "Test promote");
        assert_eq!(entry.project, "TERM"); // uppercased
        assert_eq!(entry.status, "active"); // defaulted
        assert!(!entry.created.is_empty()); // defaulted to today
        // Renders + round-trips.
        let parsed: Baseline = toml::from_str(&render_prefix_row(&entry).unwrap()).unwrap();
        assert_eq!(parsed.prefix[0].prefix, "ZZQW");
    }

    #[test]
    fn build_entry_from_args_missing_fields_errors() {
        let args = json!({ "prefix": "ZZQW", "name": "Only a name" });
        let err = build_promote_entry("ZZQW", None, &args).unwrap_err();
        // project + description are missing; name is present.
        assert!(err.contains(&"project".to_string()));
        assert!(err.contains(&"description".to_string()));
        assert!(!err.contains(&"name".to_string()));
    }

    #[test]
    fn build_entry_seeds_from_overlay_base_with_overrides() {
        let base = sample_entry();
        // No required fields in args — allowed because base is Some.
        let args = json!({ "prefix": "ZZQW", "description": "Overridden." });
        let entry = build_promote_entry("ZZQW", Some(&base), &args).unwrap();
        assert_eq!(entry.name, base.name); // inherited
        assert_eq!(entry.project, base.project); // inherited
        assert_eq!(entry.description, "Overridden."); // overridden
        assert_eq!(entry.created, base.created); // inherited (non-empty)
    }

    #[tokio::test]
    async fn promote_already_in_baseline_returns_already_promoted() {
        let tool = promote_tool();
        // SCRB is a seeded baseline prefix.
        let res = parse(&tool.execute(json!({"prefix": "scrb"})).await.unwrap());
        assert_eq!(res["ok"], false);
        assert_eq!(res["reason"], "already_promoted");
        assert_eq!(res["entry"]["prefix"], "SCRB");
    }

    #[tokio::test]
    async fn promote_invalid_prefix_errors() {
        let tool = promote_tool();
        let err = tool.execute(json!({"prefix": "a b"})).await;
        assert!(matches!(err, Err(ToolError::InvalidArgument(_))));
    }

    #[tokio::test]
    async fn promote_missing_prefix_errors() {
        let tool = promote_tool();
        let err = tool.execute(json!({})).await;
        assert!(matches!(err, Err(ToolError::InvalidArgument(_))));
    }

    #[tokio::test]
    async fn promote_bad_status_override_errors() {
        let tool = promote_tool();
        let err = tool
            .execute(json!({"prefix": "ZZQW", "status": "banana", "name": "n", "project": "TERM", "description": "d"}))
            .await;
        assert!(matches!(err, Err(ToolError::InvalidArgument(_))));
    }

    #[tokio::test]
    async fn promote_open_pr_true_without_gitea_is_clean_error() {
        // gitea: None → open_pr defaults true → structured gitea_unconfigured,
        // returned BEFORE any git mutation.
        let tool = promote_tool();
        let res = parse(&tool.execute(json!({"prefix": "ZZQW", "name": "n", "project": "TERM", "description": "d"})).await.unwrap());
        assert_eq!(res["ok"], false);
        assert_eq!(res["reason"], "gitea_unconfigured");
    }

    #[tokio::test]
    async fn promote_from_args_no_overlay_missing_fields_reports_required() {
        // A fresh prefix (not in baseline), gitea unconfigured but open_pr=false so
        // we get past the gitea gate to the insufficient_args path.
        let tool = promote_tool();
        let res = parse(&tool.execute(json!({"prefix": "ZZQW", "open_pr": false})).await.unwrap());
        assert_eq!(res["ok"], false);
        assert_eq!(res["reason"], "insufficient_args");
        let required = res["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "name"));
        assert!(required.iter().any(|v| v == "project"));
        assert!(required.iter().any(|v| v == "description"));
    }

    #[tokio::test]
    async fn promote_bad_project_rejected_before_write() {
        let tool = promote_tool();
        let err = tool
            .execute(json!({"prefix": "ZZQW", "name": "n", "project": "NOPE", "description": "d", "open_pr": false}))
            .await;
        assert!(matches!(err, Err(ToolError::InvalidArgument(_))), "bad project must error");
    }

    #[tokio::test]
    async fn promote_bad_created_rejected_before_write() {
        let tool = promote_tool();
        let err = tool
            .execute(json!({
                "prefix": "ZZQW", "name": "n", "project": "TERM", "description": "d",
                "created": "not-a-date", "open_pr": false
            }))
            .await;
        assert!(matches!(err, Err(ToolError::InvalidArgument(_))), "bad created must error");
    }

    #[test]
    fn validate_project_and_created_rules() {
        assert!(validate_project("TERM").is_ok());
        assert!(validate_project("HW").is_ok());
        assert!(validate_project("BOGUS").is_err());
        assert!(validate_project("").is_err());
        assert!(validate_created("2026-07-11").is_ok());
        assert!(validate_created("2026-13-40").is_err());
        assert!(validate_created("July 11").is_err());
        assert!(validate_created("").is_err());
    }

    // ── Seam test: open_pr:false runs the throwaway-worktree transport
    // end-to-end against a temp repo, and the live checkout is never mutated.
    #[tokio::test]
    async fn transport_open_pr_false_end_to_end() {
        let seed = "[[prefix]]\nprefix = \"AAA\"\nname = \"Seed\"\nproject = \"TERM\"\nspec_id = \"\"\nstatus = \"active\"\ndescription = \"seed\"\ncreated = \"2026-01-01\"\n";
        let repo = init_temp_repo(seed);
        let tool = promote_tool_at(&repo);

        let res = parse(
            &tool
                .execute(json!({
                    "prefix": "ZZQW", "name": "New", "project": "TERM",
                    "description": "a new one", "open_pr": false
                }))
                .await
                .unwrap(),
        );
        assert_eq!(res["ok"], true);
        assert_eq!(res["appended"], true);
        assert_eq!(res["branch"], "prefix-promote-ZZQW");
        assert!(res["pr_url"].is_null());
        assert_eq!(res["entry"]["prefix"], "ZZQW");
        let diff = res["diff"].as_str().unwrap();
        assert!(diff.contains("ZZQW"), "diff should show the new row: {diff}");

        // The live checkout was NOT mutated: still on `main`, no promote branch,
        // no throwaway worktree left behind, working tree still just the seed.
        let branches = String::from_utf8(
            std::process::Command::new("git").arg("-C").arg(&repo)
                .args(["branch", "--list"]).output().unwrap().stdout,
        ).unwrap();
        assert!(branches.contains("main"), "branches: {branches}");
        assert!(!branches.contains("prefix-promote-ZZQW"), "promote branch leaked: {branches}");
        let worktrees = String::from_utf8(
            std::process::Command::new("git").arg("-C").arg(&repo)
                .args(["worktree", "list"]).output().unwrap().stdout,
        ).unwrap();
        assert_eq!(worktrees.lines().count(), 1, "throwaway worktree leaked: {worktrees}");
        // The seed file on the live checkout is unchanged (ZZQW only on the branch).
        let live = std::fs::read_to_string(repo.join("data/prefix_registry.toml")).unwrap();
        assert!(!live.contains("ZZQW"), "live checkout file was mutated: {live}");

        std::fs::remove_dir_all(&repo).ok();
    }

    // ── Seam test: an entry already present in the file → appended:false, no
    // branch, no worktree residue.
    #[tokio::test]
    async fn transport_already_in_file_appended_false() {
        let seed = "[[prefix]]\nprefix = \"AAA\"\nname = \"Seed\"\nproject = \"TERM\"\nspec_id = \"\"\nstatus = \"active\"\ndescription = \"seed\"\ncreated = \"2026-01-01\"\n";
        let repo = init_temp_repo(seed);
        let tool = promote_tool_at(&repo);

        // AAA is already in the file (and NOT in the compiled baseline, so it
        // reaches transport). Uppercase override to prove case-insensitivity.
        let res = parse(
            &tool
                .execute(json!({
                    "prefix": "aaa", "name": "Seed", "project": "TERM",
                    "description": "seed", "created": "2026-01-01", "open_pr": false
                }))
                .await
                .unwrap(),
        );
        assert_eq!(res["ok"], true);
        assert_eq!(res["appended"], false);
        assert!(res["branch"].is_null());
        assert!(res["note"].as_str().unwrap().contains("already present"));

        let worktrees = String::from_utf8(
            std::process::Command::new("git").arg("-C").arg(&repo)
                .args(["worktree", "list"]).output().unwrap().stdout,
        ).unwrap();
        assert_eq!(worktrees.lines().count(), 1, "throwaway worktree leaked: {worktrees}");

        std::fs::remove_dir_all(&repo).ok();
    }

    // ── No-secret-leak: the overlay's Debug must never surface the password
    // (which now lives in the shared backend, not the overlay).
    #[test]
    fn overlay_debug_hides_password() {
        let backend = crate::redis::RedisBackend::build(
            "redis://127.0.0.1:6399", // pii-test-fixture
            Some("hunter2SuperSecret"),
            0,
            1,
            Duration::from_millis(100),
        )
        .expect("backend builds");
        let ov = PrefixOverlay::with_backend(backend);
        let dbg = format!("{ov:?}");
        assert!(!dbg.contains("hunter2SuperSecret"), "password leaked in Debug: {dbg}");
    }
}
