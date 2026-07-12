//! Cortex — code-graph / blast-radius / risk-scoring tools.
//!
//! ## CXEG-01: the SSH-relay era is retired
//!
//! Every previous revision of this module was a thin SSH-exec relay to a
//! script (`ops.py`) on a since-RETIRED external fleet host — the same
//! synchronous SSH-client-library-over-TCP transport pattern
//! `crucible`/`sentinel`/`vigil` still use. That host is gone. Cortex's successor is the in-process Atlas code
//! graph (`crate::scribe::graph`, tools `kg_search`/`kg_neighbors`/
//! `kg_subgraph`/`kg_path`/`kg_stats`/`kg_communities`/`kg_query`/
//! `kg_findings`, plus `scribe_kg_build`/`scribe_kg_status`), which builds,
//! persists, and queries a real graph locally — no SSH, no remote script, no
//! "relay whatever the other end says" response shape.
//!
//! This item (CXEG-01) is the foundation re-scaffold, not the full rebuild:
//!
//! - The 7 pure graph-relay tools (`cortex_stats`, `cortex_build`,
//!   `cortex_deps`, `cortex_recent`, `cortex_community`,
//!   `cortex_architecture`, `cortex_flows`) are REMOVED as live tools. Each
//!   has a structured deprecation-alias replacement in [`deprecated`]
//!   pointing at its `kg_*` (or `scribe_kg_build`) successor — no network, no
//!   SSH, just a pointer.
//! - `cortex_scope` and `cortex_review` keep their tool names/parameter
//!   surface (now keyed by `project_id` instead of the old `repo` enum).
//!   **CXEG-02** replaces `cortex_scope`'s stub with a real Atlas-backed
//!   blast-radius implementation (`src/cortex/scope.rs`) — it now walks the
//!   project's stored Atlas graph via the same `GraphStore`/`KnowledgeGraph`
//!   API `kg_neighbors`/`review::kg_context::build_kg_block` use, rather than
//!   returning a pointer. `cortex_review`'s `execute` body remains a
//!   principled stub — the real Atlas-backed risk-scoring logic lands in
//!   **CXEG-04**. Until then it returns a structured
//!   `{"status":"pending","item":"CXEG-04"}` pointer rather than silently
//!   doing nothing or erroring opaquely.
//! - `cortex_audit` keeps its `url` parameter and its existing
//!   `validate_repo_url` front-gate (`audit.rs` — untouched, SSRF-hardened
//!   URL validation with no dependency on the deleted SSH helpers), but its
//!   `execute` body is likewise a stub: **CXEG-11** rebuilds its backend
//!   (presumably against a sandboxed local clone + Atlas build, not a remote
//!   relay). See the stub `execute` body below for the exact pending-item
//!   reference.
//!
//! Net result: this module registers 11 tool NAMES total (10 unchanged from
//! before CXEG-01, so no MCP-surface churn for callers listing tools, plus
//! `cortex_house_style` added live in **CXEG-06**). Of those, `cortex_scope`
//! and `cortex_house_style` are real, live Atlas-backed tools;
//! `cortex_review`/`cortex_audit` remain Atlas-rebuild-pending stubs; and the
//! other 7 are pure deprecation aliases with no backend at all.
//! `test_cortex_tools_registered` below asserts this shape (11 names
//! present), not the old 10-live-relay-tools implementation.
//!
//! ## CXEG-06: `cortex_house_style` — Atlas-derived house-style exemplars
//!
//! `src/cortex/house_style.rs` derives, per project and per Leiden community
//! (KGRAPH-05), the community's modal patterns (dominant kind/error-type
//! idiom/config-read idiom/`RustTool`-shape presence — all graph-metadata-
//! only, no source-text inspection) plus a handful of representative
//! exemplar node refs, so a future Tier-C reviewer can cite "how THIS
//! codebase does X" instead of generic opinion. Exemplars are selected by
//! nearest-to-centroid embedding similarity (reusing `vec_embed::node_card`/
//! `EmbedClient`, the same card+embed path `metrics`'s semantic-duplication
//! detector and `scribe_kg_build`'s pipeline use), falling back to
//! centrality-only ranking (`degraded:true`) when embeddings are
//! unavailable. Profiles are cached per `(project_id, community)`, keyed by
//! the graph's `build_seq` generation (`house_style::HouseStyleCache`), so a
//! `scribe_kg_build` rebuild transparently invalidates stale entries. See
//! `house_style`'s module doc for the full degrade contract.
//!
//! ## `project_id`, not `repo`
//!
//! The old fixed two-repo-name allowlist named two repos on the retired
//! fleet-host layout. This module is now keyed
//! by the current Plane-project-prefix convention instead: `TERM`, `LUM`,
//! `HARM`, `CHRD`, `RAIL` (see [`PROJECT_IDS`] / [`validate_project_id`]) —
//! the same `project_id` vocabulary the Atlas KG tools use
//! (`crate::scribe::graph`'s `kg_*` tools all take a `project_id`).
//!
//! ## Secrets / config
//!
//! This crate has no separate `SecretManager::get()` / `vault::manager()` API
//! of its own — the runtime secret store is materialized into the process
//! environment at deploy time, so a plain env read via `crate::config` (or,
//! for the Atlas Postgres DSN specifically, `crate::config::atlas_database_url`)
//! already IS the sanctioned secret read, exactly as documented in
//! `crate::pki`'s module doc and `scribe::graph::vec_embed`'s module doc. Every
//! non-secret tuning flag below is read directly via `std::env::var` (matching
//! `crate::config`'s own `env_nonempty`-style local convention), and the one
//! secret-shaped value this module could reference — the Atlas KG's Postgres
//! DSN — is read exclusively through `crate::config::atlas_database_url()`,
//! never a raw `std::env::var("ATLAS_DATABASE_URL")` inline here.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

pub mod audit;
pub mod deprecated;
pub mod house_style;
pub mod metrics;
pub mod scope;

use audit::validate_repo_url;
use crate::scribe::graph::store::GraphStore;
use crate::scribe::ScribeConfig;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Valid `project_id`s, replacing the old fleet-host fixed repo-name allowlist.
/// Mirrors the current Plane-project-prefix convention (`CLAUDE.md`'s
/// "Current Plane project prefixes" table) and the `project_id` vocabulary
/// the Atlas KG (`kg_*`) tools already use.
pub const PROJECT_IDS: &[&str] = &["TERM", "LUM", "HARM", "CHRD", "RAIL"];

const MAX_TEXT_LEN: usize = 2000;

/// Absolute DoS ceiling (chars) on a raw `diff` blob or a `changed_files` CSV
/// string. Set FAR above what a `MAX_CHANGED_FILES`-file diff/list would ever
/// produce, so ordinary oversized-*by-file-count* input TRUNCATES (and flags
/// `truncated:true`) rather than being rejected. Rejection at this ceiling is
/// reserved for a pathologically huge SINGLE blob (few files, enormous
/// content). For a `diff` this is checked only when the parse did NOT already
/// truncate by file count — so a big many-file diff degrades gracefully
/// instead of erroring.
const MAX_DIFF_LEN: usize = 5_000_000;

/// Absolute DoS ceiling on a `changed_files` JSON array's element count. Set
/// FAR above the file-count cap (`MAX_CHANGED_FILES`, 200): arrays between the
/// cap and this ceiling TRUNCATE to the cap (with `truncated:true`), and only
/// a truly abusive array is rejected outright. Each element is additionally
/// length-bounded by [`MAX_TEXT_LEN`] (a single over-long path is malformed →
/// rejected).
const MAX_CHANGED_FILES_ARG: usize = 5000;

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// Atlas-backed Cortex config: thresholds and feature flags for the
/// CXEG-02/04/11 rebuilds, plus the Atlas KG's Postgres DSN. No SSH/remote-
/// script fields remain (see module doc).
#[derive(Debug, Clone)]
pub struct CortexConfig {
    /// Risk score (0-10 scale, matching `cortex_review`'s original
    /// description) at or above which a review should be flagged for
    /// escalation. From `CORTEX_RISK_SCORE_THRESHOLD`, default `7.0`.
    pub risk_score_threshold: f64,
    /// Feature flag gating the (not-yet-built) Tier B analysis pass. From
    /// `CORTEX_ENABLE_TIER_B`, default `false`.
    pub enable_tier_b: bool,
    /// Feature flag gating the (not-yet-built) Tier C analysis pass. From
    /// `CORTEX_ENABLE_TIER_C`, default `false`.
    pub enable_tier_c: bool,
    /// When `true` (the default), elegance/style findings are advisory-only
    /// and never block a review. From `CORTEX_ELEGANCE_ADVISORY_ONLY`,
    /// default `true`.
    pub elegance_advisory_only: bool,
    /// Cosine-similarity threshold (0.0-1.0) above which two code spans are
    /// considered near-duplicates for the (not-yet-built) dup-detection
    /// pass. From `CORTEX_DUP_COSINE_THRESHOLD`, default `0.85`.
    pub dup_cosine: f64,
    /// The Atlas KG's Postgres DSN, read exclusively through
    /// `crate::config::atlas_database_url()` (see module doc's "Secrets /
    /// config" section) — never a raw `std::env::var` in this module.
    /// `None` means the Atlas KG store is not configured; the CXEG-04/11
    /// rebuilds will raise `NotConfigured` rather than guess a DSN.
    /// `cortex_scope` (CXEG-02) does NOT raise on a missing/unloadable graph
    /// -- see `scope::compute_scope`'s degrade contract.
    pub atlas_database_url: Option<String>,
    /// `cortex_scope`'s (CXEG-02) cap on the number of nodes enumerated into
    /// `blast_radius` before it sets `"truncated": true` and stops walking.
    /// From `CORTEX_MAX_BLAST_NODES`, default [`scope::DEFAULT_MAX_BLAST_NODES`].
    pub max_blast_nodes: usize,
    /// CXEG-03's Tier-B metrics engine (`metrics::compute_signals`): the
    /// percentile cut-point (0-100) a touched node's PageRank/degree/
    /// complexity-proxy/out-degree must exceed, relative to the PROJECT'S
    /// OWN current-node distribution, to fire a `centrality_spike`/
    /// `complexity_spike`/`fan_out_explosion` signal. Self-calibrating by
    /// design (see `metrics` module doc) — never a hardcoded absolute. From
    /// `CORTEX_TIER_B_PERCENTILE`, default `90.0`.
    pub tier_b_percentile: f64,
    /// CXEG-06's `cortex_house_style`: how many exemplar nodes to select per
    /// `(community, kind)` bucket. From `CORTEX_HOUSE_STYLE_K`, default
    /// [`house_style::DEFAULT_EXEMPLARS_K`]. A zero/unparseable value falls
    /// back to the default (a zero K would silently return no exemplars at
    /// all, never the intent of an unset/misconfigured value — same
    /// reasoning as `max_blast_nodes`).
    pub house_style_exemplars_k: usize,
}

impl CortexConfig {
    pub fn from_env() -> Self {
        CortexConfig {
            risk_score_threshold: env_f64("CORTEX_RISK_SCORE_THRESHOLD", 7.0),
            enable_tier_b: env_flag("CORTEX_ENABLE_TIER_B", false),
            enable_tier_c: env_flag("CORTEX_ENABLE_TIER_C", false),
            elegance_advisory_only: env_flag("CORTEX_ELEGANCE_ADVISORY_ONLY", true),
            dup_cosine: env_f64("CORTEX_DUP_COSINE_THRESHOLD", 0.85),
            atlas_database_url: crate::config::atlas_database_url(),
            max_blast_nodes: env_usize("CORTEX_MAX_BLAST_NODES", scope::DEFAULT_MAX_BLAST_NODES),
            tier_b_percentile: env_f64("CORTEX_TIER_B_PERCENTILE", 90.0),
            house_style_exemplars_k: env_usize("CORTEX_HOUSE_STYLE_K", house_style::DEFAULT_EXEMPLARS_K),
        }
    }
}

/// Read a non-secret float tuning flag; falls back to `default` when unset
/// or unparseable. Mirrors `crate::config`'s own local env-parsing
/// convention (e.g. `serving_keep_warm_threshold_secs`).
fn env_f64(key: &str, default: f64) -> f64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

/// Read a non-secret boolean tuning flag (`"1"`/`"true"`/`"yes"` are
/// truthy, case-insensitively; anything else, or unset, falls back to
/// `default`).
fn env_flag(key: &str, default: bool) -> bool {
    match std::env::var(key) {
        Ok(v) => matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"),
        Err(_) => default,
    }
}

/// Read a non-secret unsigned-integer tuning flag; falls back to `default`
/// when unset, unparseable, or `0` (a zero bound would silently drop every
/// blast-radius node, which is never the intent of an unset/misconfigured
/// value).
fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(default)
}

// ---------------------------------------------------------------------------
// Validation helpers
// ---------------------------------------------------------------------------

/// Validate a `project_id` against [`PROJECT_IDS`], replacing the old
/// fleet-host repo-name allowlist and its validation helper.
fn validate_project_id(project_id: &str) -> Result<(), ToolError> {
    if PROJECT_IDS.contains(&project_id) {
        Ok(())
    } else {
        Err(ToolError::InvalidArgument(format!(
            "'project_id' must be one of: {}",
            PROJECT_IDS.join(", ")
        )))
    }
}

fn validate_text_len(s: &str, field: &str) -> Result<(), ToolError> {
    if s.chars().count() > MAX_TEXT_LEN {
        Err(ToolError::InvalidArgument(format!(
            "'{field}' exceeds {MAX_TEXT_LEN} character limit"
        )))
    } else {
        Ok(())
    }
}

fn require_str<'a>(args: &'a Value, field: &str) -> Result<&'a str, ToolError> {
    args[field]
        .as_str()
        .ok_or_else(|| ToolError::InvalidArgument(format!("'{field}' must be a string")))
}

// ---------------------------------------------------------------------------
// Tool: cortex_scope (CXEG-02: real Atlas-backed blast-radius)
// ---------------------------------------------------------------------------

pub struct CortexScope {
    config: Arc<CortexConfig>,
}

#[async_trait]
impl RustTool for CortexScope {
    fn name(&self) -> &str {
        "cortex_scope"
    }

    fn description(&self) -> &str {
        "Pre-change blast radius for a planned code change, from the \
         project's Atlas knowledge graph: the touched symbols plus their \
         1-hop callers/callees, the affected communities, a blast_count, \
         and a token_reduction_pct estimate (how much smaller the blast \
         radius is than the whole project, as a proxy for context budget \
         saved). project_id: one of TERM/LUM/HARM/CHRD/RAIL. Provide EITHER \
         changed_files (a comma-separated list, or a JSON array) OR diff (a \
         unified diff -- changed files are parsed from its '+++ b/<path>' \
         headers, same parser review_run uses). Degrades cleanly: if the \
         project has no stored Atlas graph yet (run scribe_kg_build first), \
         returns configured:false with the literal changed_files echoed \
         back as unresolved blast_radius entries, never an error."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "One of TERM/LUM/HARM/CHRD/RAIL", "enum": PROJECT_IDS },
                "changed_files": {
                    "description": "Changed file paths: a comma-separated string or a JSON array e.g. 'src/cortex/mod.rs,src/cortex/audit.rs'",
                    "oneOf": [
                        { "type": "string" },
                        { "type": "array", "items": { "type": "string" } }
                    ]
                },
                "diff": { "type": "string", "description": "Unified diff; used to derive changed_files when changed_files is omitted" }
            },
            "required": ["project_id"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let project_id = require_str(&args, "project_id")?;
        validate_project_id(project_id)?;

        // Reconcile validation vs truncation (CXEG-02 cycle-3): oversized
        // -by-file-COUNT input must TRUNCATE + flag `truncated:true` (handled
        // by the parse below via `input_truncated`), NEVER be rejected. Only
        // genuinely abusive/malformed input is rejected here with
        // InvalidArgument:
        //   - a SINGLE element/path longer than MAX_TEXT_LEN (one absurd blob);
        //   - a DoS-scale raw string/array/diff, at a ceiling set FAR above
        //     what a MAX_CHANGED_FILES-file input would produce.
        match args.get("changed_files") {
            Some(Value::String(s)) => {
                // A merely-long CSV of many short paths is NOT rejected (it
                // truncates by count below); only a DoS-scale blob is.
                if s.chars().count() > MAX_DIFF_LEN {
                    return Err(ToolError::InvalidArgument(format!(
                        "'changed_files' exceeds {MAX_DIFF_LEN}-char DoS ceiling"
                    )));
                }
                // A single over-long path element is malformed → reject.
                for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
                    validate_text_len(part, "changed_files element")?;
                }
            }
            Some(Value::Array(arr)) => {
                // Absolute array-size DoS ceiling, far above the file-count cap
                // (arrays between the cap and this ceiling truncate, not reject).
                if arr.len() > MAX_CHANGED_FILES_ARG {
                    return Err(ToolError::InvalidArgument(format!(
                        "'changed_files' array exceeds {MAX_CHANGED_FILES_ARG}-element DoS ceiling"
                    )));
                }
                for (i, el) in arr.iter().enumerate() {
                    if let Some(s) = el.as_str() {
                        validate_text_len(s, &format!("changed_files[{i}]"))?;
                    }
                }
            }
            _ => {}
        }

        let (changed_files, input_truncated) = scope::changed_files_from_args(&args);
        if changed_files.is_empty() {
            return Err(ToolError::InvalidArgument(
                "must provide a non-empty 'changed_files' (string or array) or 'diff'".to_string(),
            ));
        }

        // A `diff`'s total-length DoS ceiling is applied ONLY when the parse
        // did not already truncate by file count: an ordinary large multi-file
        // diff (> MAX_CHANGED_FILES files) truncates + flags `truncated:true`
        // rather than being rejected; rejection is reserved for a
        // pathologically huge single blob (few files, enormous content).
        if !input_truncated {
            if let Some(diff) = args.get("diff").and_then(|v| v.as_str()) {
                if diff.chars().count() > MAX_DIFF_LEN {
                    return Err(ToolError::InvalidArgument(format!(
                        "'diff' exceeds {MAX_DIFF_LEN}-char DoS ceiling"
                    )));
                }
            }
        }

        let response = scope::compute_scope(project_id, &changed_files, self.config.max_blast_nodes, input_truncated);
        serde_json::to_string_pretty(&response)
            .map_err(|e| ToolError::Execution(format!("JSON render error: {e}")))
    }
}

// ---------------------------------------------------------------------------
// Tool: cortex_review (stub — real Atlas-backed rebuild is CXEG-04)
// ---------------------------------------------------------------------------

pub struct CortexReview {
    config: Arc<CortexConfig>,
}

#[async_trait]
impl RustTool for CortexReview {
    fn name(&self) -> &str {
        "cortex_review"
    }

    fn description(&self) -> &str {
        "PENDING REBUILD (CXEG-04): post-change risk assessment for modified \
         files. The SSH-relay-era implementation has been retired; this tool \
         currently returns a structured pending pointer instead of a live \
         risk score. project_id: one of TERM/LUM/HARM/CHRD/RAIL. \
         changed_files: comma-separated list of modified file paths. In the \
         meantime, use kg_findings / kg_query directly against the Atlas KG."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "One of TERM/LUM/HARM/CHRD/RAIL", "enum": PROJECT_IDS },
                "changed_files": { "type": "string", "description": "Comma-separated file paths that were modified" }
            },
            "required": ["project_id", "changed_files"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let project_id = require_str(&args, "project_id")?;
        let changed_files = require_str(&args, "changed_files")?;
        validate_project_id(project_id)?;
        validate_text_len(changed_files, "changed_files")?;

        let response = json!({
            "status": "pending",
            "item": "CXEG-04",
            "tool": "cortex_review",
            "project_id": project_id,
            "message": "cortex_review's SSH-relay-era backend has been \
                retired; an Atlas-backed risk-scoring implementation lands \
                in CXEG-04. In the meantime, query kg_findings / kg_query \
                directly against the Atlas KG.",
            "risk_score_threshold": self.config.risk_score_threshold,
            "elegance_advisory_only": self.config.elegance_advisory_only,
        });
        serde_json::to_string_pretty(&response)
            .map_err(|e| ToolError::Execution(format!("JSON render error: {e}")))
    }
}

// ---------------------------------------------------------------------------
// Tool: cortex_audit (stub — real backend rebuild is CXEG-11)
// ---------------------------------------------------------------------------

pub struct CortexAudit {
    config: Arc<CortexConfig>,
}

#[async_trait]
impl RustTool for CortexAudit {
    fn name(&self) -> &str {
        "cortex_audit"
    }

    fn description(&self) -> &str {
        "PENDING REBUILD (CXEG-11): audit an external public Git repository. \
         The SSH-relay-era implementation (which delegated clone + graph \
         build + report generation to a script on the now-retired fleet \
         host) has been retired. The url argument still passes through the \
         existing SSRF-hardened validator (only public http/https URLs are \
         accepted), but execute() currently returns a structured pending \
         pointer rather than performing a live audit — the real backend \
         (presumably a sandboxed local clone + Atlas build) lands in \
         CXEG-11."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "Public git repo URL e.g. 'https://github.com/owner/repo'" }
            },
            "required": ["url"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let url = require_str(&args, "url")?;
        // Front-gate unchanged from the SSH-relay era: SSRF-hardened URL
        // validation (`audit.rs`, no dependency on the deleted SSH helpers)
        // runs BEFORE anything else, same as it always has.
        validate_repo_url(url)?;

        // CXEG-11 rebuilds this tool's actual backend (sandboxed local
        // clone + Atlas KG build, replacing the retired remote-script
        // relay). Until then, a valid URL gets a structured pending
        // pointer instead of a live audit -- no network I/O happens here.
        let response = json!({
            "status": "pending",
            "item": "CXEG-11",
            "tool": "cortex_audit",
            "url": url,
            "message": "cortex_audit's SSH-relay-era backend has been \
                retired; a locally-sandboxed clone + Atlas-build \
                implementation lands in CXEG-11. The url has passed \
                SSRF-hardened validation but no audit has been performed.",
            "dup_cosine_threshold": self.config.dup_cosine,
        });
        serde_json::to_string_pretty(&response)
            .map_err(|e| ToolError::Execution(format!("JSON render error: {e}")))
    }
}

// ---------------------------------------------------------------------------
// Tool: cortex_house_style (CXEG-06: Atlas-derived house-style exemplars)
// ---------------------------------------------------------------------------

/// A single `cortex_house_style` call with no explicit `community` computes
/// at most this many communities (ascending cluster-id order) — a bound
/// against enumerating every community of an enormous graph in one response,
/// mirroring `CORTEX_MAX_BLAST_NODES`'s "cap, don't silently do unbounded
/// work" posture. A caller that wants a SPECIFIC community beyond this bound
/// still gets it by passing `community` explicitly.
const MAX_HOUSE_STYLE_COMMUNITIES: usize = 25;

pub struct CortexHouseStyle {
    config: Arc<CortexConfig>,
    cache: Arc<house_style::HouseStyleCache>,
}

#[async_trait]
impl RustTool for CortexHouseStyle {
    fn name(&self) -> &str {
        "cortex_house_style"
    }

    fn description(&self) -> &str {
        "Derive a project's house-style exemplars from its Atlas knowledge graph, \
         scoped per Leiden community (subsystems legitimately differ, so this is \
         never a single global style). project_id: one of TERM/LUM/HARM/CHRD/RAIL. \
         community: optional cluster id; when omitted, returns up to 25 communities \
         (ascending id). Each community's profile carries deterministic modal facts \
         (dominant node kind, an error-type idiom, a from_env() config-read idiom, \
         whether the RustTool 4-method shape is present -- all derived from graph \
         metadata only, no source-text read, no LLM) plus per-kind exemplar node refs \
         (id, file, span, rank, selection score) chosen by nearest-to-centroid \
         embedding similarity, falling back to centrality-only ranking \
         (degraded:true) when embeddings are unavailable. A community below the \
         minimum sample size is flagged profile:'unstable' with no exemplars \
         rather than silently misrepresenting it; a thin (community,kind) bucket \
         is flagged sparse:true. Profiles are cached per (project_id, community), \
         keyed by the graph's build_seq, so a scribe_kg_build rebuild transparently \
         invalidates stale entries. Degrades to configured:false (never an error) \
         when the project has no stored Atlas graph yet -- run scribe_kg_build first."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "One of TERM/LUM/HARM/CHRD/RAIL", "enum": PROJECT_IDS },
                "community": { "type": "integer", "description": "Leiden community/cluster id; omit for up to 25 communities (ascending id)" }
            },
            "required": ["project_id"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let project_id = require_str(&args, "project_id")?;
        validate_project_id(project_id)?;
        let requested_community = match args.get("community") {
            None | Some(Value::Null) => None,
            Some(v) => Some(
                v.as_u64()
                    .ok_or_else(|| ToolError::InvalidArgument("'community' must be a non-negative integer".to_string()))?
                    as u32,
            ),
        };

        let store = GraphStore::from_config(&ScribeConfig::from_env());
        let graph = match store.load(project_id) {
            Ok(Some(g)) => g,
            Ok(None) | Err(_) => {
                let response = json!({
                    "configured": false,
                    "project_id": project_id,
                    "message": "no stored Atlas graph for this project yet -- run scribe_kg_build first",
                });
                return serde_json::to_string_pretty(&response)
                    .map_err(|e| ToolError::Execution(format!("JSON render error: {e}")));
            }
        };

        let communities: Vec<u32> = match requested_community {
            Some(c) => vec![c],
            None => house_style::current_communities(&graph)
                .into_iter()
                .take(MAX_HOUSE_STYLE_COMMUNITIES)
                .collect(),
        };

        let mut profiles = Vec::with_capacity(communities.len());
        for community in communities {
            profiles.push(
                self.cache
                    .get_or_compute(&graph, project_id, community, self.config.house_style_exemplars_k)
                    .await,
            );
        }

        let response = json!({
            "configured": true,
            "project_id": project_id,
            "generation": graph.build_seq,
            "profiles": profiles,
        });
        serde_json::to_string_pretty(&response)
            .map_err(|e| ToolError::Execution(format!("JSON render error: {e}")))
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Register all Cortex tools into the ToolRegistry: the 4 "real" tools
/// (`cortex_scope` — live Atlas-backed blast radius as of CXEG-02;
/// `cortex_house_style` — live Atlas-derived house-style exemplars as of
/// CXEG-06; `cortex_review`/`cortex_audit` — still Atlas-rebuild-pending
/// stubs) plus the 7 deprecation aliases for the retired pure-relay tools
/// (see [`deprecated`]).
pub fn register(registry: &mut ToolRegistry) {
    let config = Arc::new(CortexConfig::from_env());
    let house_style_cache = Arc::new(house_style::HouseStyleCache::new());

    let _ = registry.register(Box::new(CortexScope {
        config: Arc::clone(&config),
    }));
    let _ = registry.register(Box::new(CortexHouseStyle {
        config: Arc::clone(&config),
        cache: house_style_cache,
    }));
    let _ = registry.register(Box::new(CortexReview {
        config: Arc::clone(&config),
    }));
    let _ = registry.register(Box::new(CortexAudit { config }));

    deprecated::register(registry);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> Arc<CortexConfig> {
        Arc::new(CortexConfig {
            risk_score_threshold: 7.0,
            enable_tier_b: false,
            enable_tier_c: false,
            elegance_advisory_only: true,
            dup_cosine: 0.85,
            atlas_database_url: None,
            max_blast_nodes: scope::DEFAULT_MAX_BLAST_NODES,
            tier_b_percentile: 90.0,
            house_style_exemplars_k: house_style::DEFAULT_EXEMPLARS_K,
        })
    }

    // --- validation ----------------------------------------------------------

    #[test]
    fn test_validate_project_id_accepts_known_values() {
        for id in PROJECT_IDS {
            assert!(validate_project_id(id).is_ok(), "{id} should be valid");
        }
    }

    #[test]
    fn test_validate_project_id_rejects_unknown() {
        let err = validate_project_id("NOPE").unwrap_err();
        match err {
            ToolError::InvalidArgument(msg) => {
                for id in PROJECT_IDS {
                    assert!(msg.contains(id), "expected {id} listed in: {msg}");
                }
            }
            other => panic!("expected InvalidArgument, got {other:?}"),
        }
    }

    #[test]
    fn test_validate_project_id_rejects_legacy_repo_names() {
        // The old fleet-host repo names must no longer validate.
        assert!(validate_project_id("lumina-fleet").is_err());
        assert!(validate_project_id("lumina-terminus").is_err());
    }

    #[test]
    fn test_validate_text_len_rejects_oversized() {
        let huge = "x".repeat(MAX_TEXT_LEN + 1);
        assert!(validate_text_len(&huge, "changed_files").is_err());
        assert!(validate_text_len("short", "changed_files").is_ok());
    }

    // --- cortex_scope (CXEG-02: real, Atlas-backed) -----------------------
    //
    // Full blast-radius derivation behavior (touched-node resolution, 1-hop
    // neighbors, communities, truncation, token_reduction_pct) is covered by
    // `scope::tests` against a fixture graph; these tests cover argument
    // validation and the tool-trait wiring (`CortexScope::execute` ->
    // `scope::changed_files_from_args` / `scope::compute_scope`).

    #[tokio::test]
    async fn test_scope_rejects_unknown_project_id() {
        let tool = CortexScope { config: test_config() };
        let err = tool
            .execute(json!({"project_id": "NOPE", "changed_files": "a.rs"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn test_scope_rejects_oversized_single_changed_file_element() {
        // A SINGLE over-MAX_TEXT_LEN path element is malformed -> InvalidArgument
        // (the CSV string has no commas, so it is one element). This is the
        // per-element reject that must SURVIVE the cycle-3 truncation
        // reconciliation (count overflow truncates, single-blob rejects).
        let tool = CortexScope { config: test_config() };
        let huge = "x".repeat(MAX_TEXT_LEN + 1);
        let err = tool
            .execute(json!({"project_id": "TERM", "changed_files": huge}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn test_scope_truncates_long_csv_of_short_paths_not_rejects() {
        // A CSV whose TOTAL length far exceeds MAX_TEXT_LEN but is just many
        // short paths must TRUNCATE (truncated:true), not be rejected — the
        // cycle-3 fix for the count-vs-length reconciliation.
        let store_dir = std::env::temp_dir().join(format!("atlas-cortexmod-csv-{}", std::process::id()));
        std::env::set_var("SCRIBE_KG_STORE_DIR", &store_dir);

        let csv = (0..500).map(|i| format!("src/f{i}.rs")).collect::<Vec<_>>().join(",");
        assert!(csv.len() > MAX_TEXT_LEN, "fixture must exceed the per-element cap in total");
        let tool = CortexScope { config: test_config() };
        let out = tool
            .execute(json!({"project_id": "TERM", "changed_files": csv}))
            .await
            .expect("a long CSV of short paths must truncate, not error");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["truncated"], true, "count-overflow CSV must set truncated:true");

        std::env::remove_var("SCRIBE_KG_STORE_DIR");
    }

    #[tokio::test]
    async fn test_scope_rejects_when_no_changed_files_or_diff() {
        let tool = CortexScope { config: test_config() };
        let err = tool.execute(json!({"project_id": "TERM"})).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn test_scope_degrades_to_configured_false_without_a_stored_graph() {
        let store_dir = std::env::temp_dir().join(format!("atlas-cortexmod-test-{}", std::process::id()));
        std::env::set_var("SCRIBE_KG_STORE_DIR", &store_dir);

        let tool = CortexScope { config: test_config() };
        let out = tool
            .execute(json!({"project_id": "TERM", "changed_files": "src/cortex/mod.rs"}))
            .await
            .expect("no stored graph must degrade, not error");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["configured"], false);
        assert_eq!(v["project_id"], "TERM");
        assert_eq!(v["blast_radius"][0]["resolved"], false);

        std::env::remove_var("SCRIBE_KG_STORE_DIR");
    }

    #[tokio::test]
    async fn test_scope_accepts_changed_files_array_form() {
        let tool = CortexScope { config: test_config() };
        let out = tool
            .execute(json!({"project_id": "TERM", "changed_files": ["src/a.rs", "src/b.rs"]}))
            .await
            .expect("array changed_files must be accepted");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["changed_files"], json!(["src/a.rs", "src/b.rs"]));
    }

    #[tokio::test]
    async fn test_scope_accepts_diff_only_input() {
        let tool = CortexScope { config: test_config() };
        let diff = "diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1 +1 @@\n-x\n+y\n";
        let out = tool
            .execute(json!({"project_id": "TERM", "diff": diff}))
            .await
            .expect("diff-only input must be accepted");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["changed_files"], json!(["src/a.rs"]));
    }

    #[tokio::test]
    async fn test_scope_rejects_oversized_changed_files_array_size() {
        let tool = CortexScope { config: test_config() };
        let big: Vec<String> = (0..MAX_CHANGED_FILES_ARG + 1).map(|i| format!("f{i}.rs")).collect();
        let err = tool
            .execute(json!({"project_id": "TERM", "changed_files": big}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn test_scope_rejects_oversized_changed_files_array_element() {
        let tool = CortexScope { config: test_config() };
        let huge = "x".repeat(MAX_TEXT_LEN + 1);
        let err = tool
            .execute(json!({"project_id": "TERM", "changed_files": ["ok.rs", huge]}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn test_scope_rejects_pathologically_huge_single_blob_diff() {
        // A diff with FEW files (one) but DoS-scale content (> MAX_DIFF_LEN)
        // is rejected: it did not truncate by file count, so the total-length
        // ceiling applies. One valid `+++` header ensures the parse yields a
        // (non-empty, non-truncated) file so we exercise the length reject,
        // not the empty-input reject.
        let tool = CortexScope { config: test_config() };
        let diff = format!("+++ b/src/a.rs\n{}", "x".repeat(MAX_DIFF_LEN + 1));
        let err = tool
            .execute(json!({"project_id": "TERM", "diff": diff}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn test_scope_truncates_diff_with_many_files_not_rejects() {
        // An ordinary big multi-file diff (far MORE files than the file-count
        // cap, but well under the DoS byte ceiling) must TRUNCATE + flag
        // `truncated:true`, NEVER be rejected — the core cycle-3 fix.
        let store_dir = std::env::temp_dir().join(format!("atlas-cortexmod-diffcount-{}", std::process::id()));
        std::env::set_var("SCRIBE_KG_STORE_DIR", &store_dir);

        let mut diff = String::new();
        for i in 0..1000 {
            diff.push_str(&format!("+++ b/src/f{i}.rs\n"));
        }
        assert!(diff.len() < MAX_DIFF_LEN, "fixture must stay under the DoS byte ceiling");
        let tool = CortexScope { config: test_config() };
        let out = tool
            .execute(json!({"project_id": "TERM", "diff": diff}))
            .await
            .expect("a many-file diff must truncate, not error");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["truncated"], true, "count-overflow diff must set truncated:true");

        std::env::remove_var("SCRIBE_KG_STORE_DIR");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn test_scope_flags_truncated_on_oversized_input_file_list() {
        // An input file list far larger than MAX_CHANGED_FILES must surface
        // `truncated:true` (input-file cap) rather than being silently capped
        // by derive_changed_files. Runs against an empty store dir so the
        // degrade path is exercised too; the input-cap flag must survive it.
        let store_dir = std::env::temp_dir().join(format!("atlas-cortexmod-inputcap-{}", std::process::id()));
        std::env::set_var("SCRIBE_KG_STORE_DIR", &store_dir);

        let big: Vec<String> = (0..500).map(|i| format!("src/f{i}.rs")).collect();
        let tool = CortexScope { config: test_config() };
        let out = tool
            .execute(json!({"project_id": "TERM", "changed_files": big}))
            .await
            .expect("oversized input must degrade/scope, not error");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["truncated"], true, "oversized input file list must set truncated:true");

        std::env::remove_var("SCRIBE_KG_STORE_DIR");
    }

    // --- cortex_review (stub) --------------------------------------------------

    #[tokio::test]
    async fn test_review_rejects_unknown_project_id() {
        let tool = CortexReview { config: test_config() };
        let err = tool
            .execute(json!({"project_id": "NOPE", "changed_files": "a.rs"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn test_review_returns_pending_pointer_for_valid_input() {
        let tool = CortexReview { config: test_config() };
        let out = tool
            .execute(json!({"project_id": "LUM", "changed_files": "src/lib.rs"}))
            .await
            .expect("valid input must succeed with a pending pointer, not an error");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["status"], "pending");
        assert_eq!(v["item"], "CXEG-04");
        assert_eq!(v["project_id"], "LUM");
        assert_eq!(v["risk_score_threshold"], 7.0);
    }

    // --- cortex_audit (stub, still SSRF-gated) ---------------------------------

    #[tokio::test]
    async fn test_audit_rejects_non_public_url_before_stub_response() {
        // test fixture: RFC 1918 private-range address (SSRF-guard test)
        let tool = CortexAudit { config: test_config() };
        let err = tool
            .execute(json!({"url": "https://<internal-ip>/internal"})) // pii-test-fixture
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn test_audit_rejects_ssh_scheme_url() {
        let tool = CortexAudit { config: test_config() };
        let err = tool
            .execute(json!({"url": "ssh://<email>/owner/repo"})) // pii-test-fixture
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn test_audit_returns_pending_pointer_for_valid_url() {
        let tool = CortexAudit { config: test_config() };
        let out = tool
            .execute(json!({"url": "https://github.com/octocat/Hello-World"}))
            .await
            .expect("valid url must succeed with a pending pointer, not an error");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["status"], "pending");
        assert_eq!(v["item"], "CXEG-11");
        assert_eq!(v["url"], "https://github.com/octocat/Hello-World");
    }

    // --- registration -----------------------------------------------------------

    #[test]
    fn test_cortex_tools_registered() {
        let mut registry = ToolRegistry::new();
        register(&mut registry);
        // 4 "real" tools (cortex_scope + cortex_house_style live,
        // cortex_review/cortex_audit still pending) + 7 deprecation aliases
        // = 11 names, the pre-CXEG-01 10-name surface plus CXEG-06's new
        // `cortex_house_style` (an intentional, additive MCP-listing change).
        assert_eq!(registry.len(), 11);
        for name in [
            "cortex_scope",
            "cortex_house_style",
            "cortex_review",
            "cortex_audit",
            "cortex_stats",
            "cortex_build",
            "cortex_architecture",
            "cortex_deps",
            "cortex_recent",
            "cortex_community",
            "cortex_flows",
        ] {
            assert!(registry.contains(name), "missing tool {name}");
        }
    }

    // --- cortex_house_style (CXEG-06) ------------------------------------

    #[tokio::test]
    async fn test_house_style_rejects_unknown_project_id() {
        let tool = CortexHouseStyle {
            config: test_config(),
            cache: Arc::new(house_style::HouseStyleCache::new()),
        };
        let err = tool.execute(json!({"project_id": "NOPE"})).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn test_house_style_rejects_non_integer_community() {
        let tool = CortexHouseStyle {
            config: test_config(),
            cache: Arc::new(house_style::HouseStyleCache::new()),
        };
        let err = tool
            .execute(json!({"project_id": "TERM", "community": "not-a-number"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn test_house_style_degrades_to_configured_false_without_a_stored_graph() {
        let store_dir = std::env::temp_dir().join(format!("atlas-cortexmod-housestyle-{}", std::process::id()));
        std::env::set_var("SCRIBE_KG_STORE_DIR", &store_dir);

        let tool = CortexHouseStyle {
            config: test_config(),
            cache: Arc::new(house_style::HouseStyleCache::new()),
        };
        let out = tool
            .execute(json!({"project_id": "TERM"}))
            .await
            .expect("no stored graph must degrade, not error");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["configured"], false);
        assert_eq!(v["project_id"], "TERM");

        std::env::remove_var("SCRIBE_KG_STORE_DIR");
    }
}
