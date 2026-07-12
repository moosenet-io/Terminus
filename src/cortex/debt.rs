//! CXEG-12: consistency-debt trend — a READ-ONLY aggregation over the
//! KGFIND corpus.
//!
//! `cortex_consistency_debt` answers one question: across everything the
//! fleet's review gates have already recorded, is house-style debt growing
//! or shrinking, and which subsystems are accruing it? It is deliberately
//! the LIGHTEST thing that works:
//!
//! - **No new store.** It reuses the SAME KGFIND-01 `FindingsStore`
//!   (`crate::scribe::graph::findings_store`) every other finding-shaped
//!   Cortex tool already reads (`cortex_review`'s recurrence lookup,
//!   `cortex_crystallize`'s candidate selection, `cortex_waive`'s waiver
//!   ledger) — the same `list()` query path, no parallel SQL, no new table
//!   (S9).
//! - **Read-only.** This module never calls `FindingsStore::record` and
//!   never writes `crystallize_state`. It only lists and aggregates.
//! - **Per-community, per-category.** The three categories this trend
//!   tracks are `consistency` (CXEG-07's Tier-C lens), `elegance` (CXEG-04's
//!   structural signals when captured as findings), and `waiver` (CXEG-08's
//!   `cortex_waive` — over-waiving is itself debt worth surfacing, exactly
//!   as `cortex_waive`'s own module doc already says). A finding's
//!   `scope_kind`/`scope_ref` (`node`/`path`/`community`/`global`) is
//!   resolved to a Leiden community bucket using the SAME Atlas graph
//!   lookups `cortex_scope`/`cortex_review` already use
//!   (`GraphStore::load`, `KnowledgeGraph::get_node`/`current_nodes`) — no
//!   second graph-walk implementation.
//!
//! ## Degrade contract — never an error
//!
//! - No `ATLAS_DATABASE_URL` configured, or the findings store is
//!   otherwise unreachable: `{"configured": false, ...}`, never a tool
//!   error — mirrors `cortex_scope`/`cortex_review`'s own posture.
//! - No stored Atlas graph for the project: the rollup still runs (findings
//!   are real either way), but every `node`/`path`-scoped finding falls
//!   back to the `"unmapped"` community bucket and `graph_available` is
//!   `false` — never fabricates a community for an ungraphed project.
//! - No matching findings at all: `rollups: []`, `totals: {}` — a clean
//!   empty result, not an error.

use async_trait::async_trait;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

use crate::error::ToolError;
use crate::scribe::graph::findings_store::FindingsStore;
use crate::scribe::graph::model::KnowledgeGraph;
use crate::scribe::graph::store::GraphStore;
use crate::scribe::ScribeConfig;
use crate::tool::RustTool;

use super::{require_str, validate_project_id, PROJECT_IDS};

/// The finding categories this trend rolls up. Anything else in
/// `kg_findings` (e.g. a correctness-panel `"bug"` finding) is out of scope
/// for a HOUSE-STYLE debt trend and is excluded.
const DEBT_CATEGORIES: &[&str] = &["consistency", "elegance", "waiver"];

/// A community bucket a finding rolls up into. `Community(id)` for a
/// resolved Leiden cluster; `ProjectWide` for a `scope_kind: global`
/// finding (nothing to localize — e.g. most `cortex_waive` entries);
/// `Unmapped` for a `node`/`path`-scoped finding that could not be resolved
/// to a cluster (no stored graph, or the node/path is no longer current).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum CommunityBucket {
    Community(u32),
    ProjectWide,
    Unmapped,
}

impl CommunityBucket {
    fn to_json(&self) -> Value {
        match self {
            CommunityBucket::Community(id) => json!(id),
            CommunityBucket::ProjectWide => json!("project-wide"),
            CommunityBucket::Unmapped => json!("unmapped"),
        }
    }
}

/// Resolve a single finding's `(scope_kind, scope_ref)` to a community
/// bucket. `graph` is `None` when the project has no stored Atlas graph
/// (or it failed to load) — every `node`/`path` lookup then falls back to
/// `Unmapped` rather than guessing.
fn resolve_bucket(scope_kind: &str, scope_ref: &str, graph: Option<&KnowledgeGraph>) -> CommunityBucket {
    match scope_kind {
        "global" => CommunityBucket::ProjectWide,
        "community" => scope_ref
            .parse::<u32>()
            .map(CommunityBucket::Community)
            .unwrap_or(CommunityBucket::Unmapped),
        "node" => graph
            .and_then(|g| g.get_node(scope_ref))
            .and_then(|n| n.cluster)
            .map(CommunityBucket::Community)
            .unwrap_or(CommunityBucket::Unmapped),
        "path" => graph
            .and_then(|g| g.current_nodes().find(|n| n.path == scope_ref))
            .and_then(|n| n.cluster)
            .map(CommunityBucket::Community)
            .unwrap_or(CommunityBucket::Unmapped),
        _ => CommunityBucket::Unmapped,
    }
}

#[derive(Debug, Clone, Default)]
struct Rollup {
    distinct_findings: u64,
    total_occurrences: i64,
    first_seen: Option<chrono::DateTime<chrono::Utc>>,
    last_seen: Option<chrono::DateTime<chrono::Utc>>,
}

impl Rollup {
    fn absorb(&mut self, occurrences: i32, first_seen: chrono::DateTime<chrono::Utc>, last_seen: chrono::DateTime<chrono::Utc>) {
        self.distinct_findings += 1;
        self.total_occurrences += occurrences as i64;
        self.first_seen = Some(self.first_seen.map_or(first_seen, |f| f.min(first_seen)));
        self.last_seen = Some(self.last_seen.map_or(last_seen, |l| l.max(last_seen)));
    }

    fn to_json(&self) -> Value {
        json!({
            "distinct_findings": self.distinct_findings,
            "total_occurrences": self.total_occurrences,
            "first_seen": self.first_seen.map(|d| d.to_rfc3339()),
            "last_seen": self.last_seen.map(|d| d.to_rfc3339()),
        })
    }
}

/// Compute the consistency-debt rollup for `project_id`. Never errors on a
/// missing/unreachable findings store or a missing Atlas graph -- both
/// degrade per the module doc's contract; only a genuinely invalid
/// `project_id` (checked by the caller before this runs) is out of scope
/// here.
pub async fn compute_debt(project_id: &str) -> Value {
    let store = match FindingsStore::from_env().await {
        Ok(s) => s,
        Err(_) => {
            return json!({
                "configured": false,
                "project_id": project_id,
                "message": "no Atlas KGFIND findings store configured (ATLAS_DATABASE_URL unset or unreachable) -- run with a configured store to see the consistency-debt trend",
            });
        }
    };

    // No server-side "category IN (...)" filter on `FindingsStore::list`
    // (S9 -- reuse the exact query path `kg_findings`/`cortex_review` use,
    // never a parallel SQL statement), so list once per debt category and
    // merge client-side, mirroring `cortex_review::touched_recurrence`'s
    // own per-scope-kind listing pattern.
    let mut rows = Vec::new();
    for category in DEBT_CATEGORIES.iter().copied() {
        match store.list(project_id, None, Some(category), None).await {
            Ok(mut r) => rows.append(&mut r),
            Err(_) => {
                return json!({
                    "configured": false,
                    "project_id": project_id,
                    "message": "Atlas KGFIND findings store is configured but unreachable -- consistency-debt trend unavailable this call",
                });
            }
        }
    }

    let graph_store = GraphStore::from_config(&ScribeConfig::from_env());
    let graph = graph_store.load(project_id).ok().flatten();
    let graph_available = graph.is_some();
    let generation = graph.as_ref().map(|g| g.build_seq);

    let mut buckets: BTreeMap<(CommunityBucket, String), Rollup> = BTreeMap::new();
    let mut totals: BTreeMap<String, Rollup> = BTreeMap::new();

    for row in &rows {
        let bucket = resolve_bucket(&row.scope_kind, &row.scope_ref, graph.as_ref());
        buckets
            .entry((bucket, row.category.clone()))
            .or_default()
            .absorb(row.occurrences, row.first_seen, row.last_seen);
        totals
            .entry(row.category.clone())
            .or_default()
            .absorb(row.occurrences, row.first_seen, row.last_seen);
    }

    // Deterministic ordering: community ids ascending, then "project-wide",
    // then "unmapped" (`CommunityBucket`'s derived `Ord` already sorts this
    // way -- `Community(id)` < `ProjectWide` < `Unmapped`), then category
    // ascending within a bucket. `BTreeMap` iteration is already ordered by
    // key, so this falls out for free rather than needing a separate sort.
    let rollups: Vec<Value> = buckets
        .iter()
        .map(|((bucket, category), rollup)| {
            let mut obj = Map::new();
            obj.insert("community".to_string(), bucket.to_json());
            obj.insert("category".to_string(), json!(category));
            if let Value::Object(inner) = rollup.to_json() {
                obj.extend(inner);
            }
            Value::Object(obj)
        })
        .collect();

    let totals_json: Map<String, Value> = totals.iter().map(|(cat, r)| (cat.clone(), r.to_json())).collect();

    json!({
        "configured": true,
        "project_id": project_id,
        "graph_available": graph_available,
        "generation": generation,
        "rollups": rollups,
        "totals": Value::Object(totals_json),
    })
}

// ---------------------------------------------------------------------------
// Tool: cortex_consistency_debt (CXEG-12)
// ---------------------------------------------------------------------------

pub struct CortexConsistencyDebt;

#[async_trait]
impl RustTool for CortexConsistencyDebt {
    fn name(&self) -> &str {
        "cortex_consistency_debt"
    }

    fn description(&self) -> &str {
        "Read-only consistency-debt trend: aggregates recurring \
         category:consistency|elegance|waiver findings from the Atlas KGFIND \
         corpus (the same store cortex_review/cortex_crystallize/cortex_waive \
         already use -- no new store, no writes) into per-community, \
         per-category rollups, so the fleet can see whether house-style debt \
         is growing or shrinking and which subsystems accrue it. \
         project_id: one of TERM/LUM/HARM/CHRD/RAIL. A node/path-scoped \
         finding is resolved to its Leiden community via the project's \
         stored Atlas graph; a global-scoped finding (most waivers) rolls up \
         under 'project-wide'; a finding that can't be resolved (no stored \
         graph, or an invalidated node/path) rolls up under 'unmapped' -- \
         never fabricated. Degrades to configured:false (never an error) \
         when no Atlas KGFIND findings store is configured/reachable."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "string", "description": "One of TERM/LUM/HARM/CHRD/RAIL", "enum": PROJECT_IDS }
            },
            "required": ["project_id"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let project_id = require_str(&args, "project_id")?;
        validate_project_id(project_id)?;

        let response = compute_debt(project_id).await;
        serde_json::to_string_pretty(&response).map_err(|e| ToolError::Execution(format!("JSON render error: {e}")))
    }
}

pub fn register(registry: &mut crate::registry::ToolRegistry) {
    let _ = registry.register(Box::new(CortexConsistencyDebt));
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── resolve_bucket: pure, no I/O ─────────────────────────────────────

    #[test]
    fn global_scope_is_project_wide() {
        assert_eq!(resolve_bucket("global", "TERM", None), CommunityBucket::ProjectWide);
    }

    #[test]
    fn community_scope_parses_the_ref_directly() {
        assert_eq!(resolve_bucket("community", "3", None), CommunityBucket::Community(3));
    }

    #[test]
    fn community_scope_with_unparseable_ref_is_unmapped() {
        assert_eq!(resolve_bucket("community", "not-a-number", None), CommunityBucket::Unmapped);
    }

    #[test]
    fn node_scope_without_a_graph_is_unmapped() {
        assert_eq!(resolve_bucket("node", "crate::foo::bar", None), CommunityBucket::Unmapped);
    }

    #[test]
    fn path_scope_without_a_graph_is_unmapped() {
        assert_eq!(resolve_bucket("path", "src/foo.rs", None), CommunityBucket::Unmapped);
    }

    #[test]
    fn unknown_scope_kind_is_unmapped() {
        assert_eq!(resolve_bucket("bogus", "x", None), CommunityBucket::Unmapped);
    }

    #[test]
    fn bucket_ordering_is_communities_then_project_wide_then_unmapped() {
        let mut buckets = vec![
            CommunityBucket::Unmapped,
            CommunityBucket::ProjectWide,
            CommunityBucket::Community(5),
            CommunityBucket::Community(1),
        ];
        buckets.sort();
        assert_eq!(
            buckets,
            vec![
                CommunityBucket::Community(1),
                CommunityBucket::Community(5),
                CommunityBucket::ProjectWide,
                CommunityBucket::Unmapped,
            ]
        );
    }

    // ── Rollup: pure aggregation ──────────────────────────────────────────

    #[test]
    fn rollup_absorbs_multiple_findings_deterministically() {
        let mut r = Rollup::default();
        let t1 = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z").unwrap().with_timezone(&chrono::Utc);
        let t2 = chrono::DateTime::parse_from_rfc3339("2026-02-01T00:00:00Z").unwrap().with_timezone(&chrono::Utc);
        r.absorb(3, t1, t1);
        r.absorb(5, t2, t2);
        assert_eq!(r.distinct_findings, 2);
        assert_eq!(r.total_occurrences, 8);
        assert_eq!(r.first_seen, Some(t1));
        assert_eq!(r.last_seen, Some(t2));
    }

    // ── compute_debt: degrade without a configured store ─────────────────

    #[tokio::test]
    #[serial_test::serial]
    async fn compute_debt_degrades_without_a_configured_findings_store() {
        if std::env::var("ATLAS_DATABASE_URL").is_ok() {
            return; // a real DSN is live in this process; skip
        }
        let out = compute_debt("TERM").await;
        assert_eq!(out["configured"], false);
        assert_eq!(out["project_id"], "TERM");
    }

    // ── cortex_consistency_debt tool: argument validation ─────────────────

    #[tokio::test]
    async fn tool_rejects_unknown_project_id() {
        let err = CortexConsistencyDebt.execute(json!({"project_id": "NOPE"})).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn tool_rejects_missing_project_id() {
        let err = CortexConsistencyDebt.execute(json!({})).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn tool_degrades_cleanly_without_a_configured_store() {
        if std::env::var("ATLAS_DATABASE_URL").is_ok() {
            return; // a real DSN is live in this process; skip
        }
        let out = CortexConsistencyDebt.execute(json!({"project_id": "TERM"})).await.expect("must degrade, not error");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["configured"], false);
    }
}
