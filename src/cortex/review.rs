//! CXEG-04: `cortex_review`'s real Atlas-backed risk-scoring implementation.
//!
//! Turns a post-change diff into a `risk_score` (0-10) + named `risk_signals`
//! by combining two independent sources, both reused (not reimplemented,
//! S9 single-source):
//!
//! - **Structural**: CXEG-03's [`crate::cortex::metrics::compute_signals`]
//!   over the diff's touched nodes (`centrality_spike`,
//!   `community_boundary_crossing`, `semantic_duplication`,
//!   `complexity_spike`, `fan_out_explosion`).
//! - **Recurrence**: KGFIND-01's
//!   [`crate::scribe::graph::findings_store::FindingsStore`] — the SAME store
//!   the `kg_findings` query tool reads, queried in-process here (not a
//!   second findings access path) for findings whose scope falls within this
//!   change's touched nodes/files/communities, aggregated by `category`.
//!
//! ## Scoring (`score`)
//! [`score`] is a PURE, sync, unit-testable function: `(signals,
//! recurrence) -> RiskScore`. Every structural [`EleganceSignal`] contributes
//! `weight(kind) * severity` points; every recurring finding category
//! contributes `weight_recurrence * log2(1 + total_occurrences)` points (log-
//! scaled so one pathological, heavily-recurring scope cannot alone pin the
//! score at the ceiling). The raw (pre-clamp) sum of every contribution's
//! `points` is clamped to `[0, 10]` for `risk_score`; `contributions` is
//! returned in full so a caller can always reconstruct the raw score by
//! summing `points` — nothing is hidden or lossy.
//!
//! ## Bands and recommendation
//! `band` is `"low"` (`< risk_band_elevated_cut`, default 4.0), `"elevated"`
//! (`>= risk_band_elevated_cut` and `< risk_score_threshold`), or `"high"`
//! (`>= risk_score_threshold`, default 7.0) — both cut-points come from
//! [`crate::cortex::CortexConfig`] (tunable in CXEG-10 calibration).
//! [`recommendation_for`] only ever ESCALATES review rigor for a high band;
//! it never recommends auto-rejection (that is explicitly out of scope here —
//! CXEG-08's job, if/when built).
//!
//! ## Degrade contract
//! - **Graph unconfigured/unbuilt for `project_id`**: [`compute_review`]
//!   returns a `"configured": false` response (mirrors `cortex_scope`'s own
//!   degrade shape) with a `band` of `"unknown"`, an empty `risk_signals`/
//!   `contributions`, and `findings: "unavailable"` — never an error.
//! - **Findings store unconfigured/unreachable/erroring** (but the Atlas
//!   graph itself loaded fine): the structural half still runs and is
//!   returned in full; `findings` is labeled `"unavailable"` and the
//!   recurrence contribution is simply absent (0 points) rather than the
//!   whole call failing.
//! - **Findings store reachable but no matching rows**: `findings` is
//!   labeled `"empty"` (distinct from `"unavailable"`) — a caller must not
//!   read "no recurrence found" as "the recurrence signal was skipped."
//! - **Findings store reachable with matches**: `findings` is `"ok"`.

use std::collections::{HashMap, HashSet};

use serde::Serialize;
use serde_json::{json, Value};

use crate::cortex::metrics::{compute_signals, round4, EleganceSignal, SignalKind};
use crate::cortex::CortexConfig;
use crate::scribe::graph::findings_store::FindingsStore;
use crate::scribe::graph::store::GraphStore;
use crate::scribe::ScribeConfig;

/// A single, fully-transparent contributor to the raw (pre-clamp) risk score.
/// `source` is either a structural signal's `kind` (`"centrality_spike"`,
/// ...) or `"recurrence:<category>"` for a KGFIND recurrence bucket.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Contribution {
    pub source: String,
    pub weight: f64,
    pub points: f64,
}

/// [`score`]'s result: the clamped/rounded `value`, its `band`, and the full
/// `contributions` list a caller can sum to reconstruct the raw pre-clamp
/// score.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RiskScore {
    pub value: f64,
    pub band: &'static str,
    pub contributions: Vec<Contribution>,
}

/// Log2(1+n) magnitude for a recurrence bucket's total occurrence count —
/// deliberately sub-linear so a single pathologically-recurring finding
/// bucket cannot alone pin the score at the ceiling (a bucket recurring 1000
/// times contributes only ~10x a bucket recurring once, not 1000x).
fn recurrence_magnitude(total_occurrences: i64) -> f64 {
    (1.0 + total_occurrences.max(0) as f64).log2()
}

/// The band cut-points come from `config`: `< risk_band_elevated_cut` is
/// `"low"`, `< risk_score_threshold` is `"elevated"`, otherwise `"high"`.
/// Both comparisons are `>=` at their lower bound (consistent with
/// `CortexConfig::risk_score_threshold`'s own documented "at or above which"
/// semantics), so the boundary value itself always resolves to the HIGHER
/// band, deterministically.
fn band_for(value: f64, config: &CortexConfig) -> &'static str {
    if value >= config.risk_score_threshold {
        "high"
    } else if value >= config.risk_band_elevated_cut {
        "elevated"
    } else {
        "low"
    }
}

/// A transparent, never-auto-rejecting recommendation string for a band.
/// `"high"` only ever escalates review rigor (extra reviewer, closer read of
/// the flagged signals) — it is explicitly NOT an auto-reject; that is out of
/// this item's scope (a hypothetical future CXEG-08, if ever built).
fn recommendation_for(band: &str) -> &'static str {
    match band {
        "high" => "escalate review rigor: request an additional reviewer and a closer read of the flagged risk_signals before merge — treat this as a signal to raise scrutiny, not as a merge gate.",
        "elevated" => "apply standard review rigor with attention to the flagged risk_signals; no escalation required.",
        "low" => "standard review rigor is sufficient; no elevated risk signals or recurring findings detected for this change.",
        _ => "insufficient data to assess risk for this change.",
    }
}

/// The pure, deterministic scoring function: turns CXEG-03 structural
/// signals plus a per-category recurrence-count summary into a [`RiskScore`].
/// No I/O — fully unit-testable with synthetic inputs. `recurrence` is
/// `(category, total_occurrences)` pairs; order does not affect the result
/// (contributions are built from a stably-sorted-by-category copy), so
/// callers do not need to pre-sort.
pub fn score(signals: &[EleganceSignal], recurrence: &[(String, i64)], config: &CortexConfig) -> RiskScore {
    let mut contributions: Vec<Contribution> = Vec::new();

    // Structural: `signals` already arrives sorted by `(kind, anchor_node)`
    // (metrics::sort_signals), so iterating in input order is deterministic.
    for s in signals {
        let weight = risk_weight_for(s.kind, config);
        contributions.push(Contribution {
            source: s.kind.as_str().to_string(),
            weight,
            points: round4(weight * s.severity),
        });
    }

    // Recurrence: sort by category for determinism regardless of caller order.
    let mut rec_sorted: Vec<&(String, i64)> = recurrence.iter().collect();
    rec_sorted.sort_by(|a, b| a.0.cmp(&b.0));
    for (category, total_occurrences) in rec_sorted {
        let magnitude = recurrence_magnitude(*total_occurrences);
        contributions.push(Contribution {
            source: format!("recurrence:{category}"),
            weight: config.risk_weight_recurrence,
            points: round4(config.risk_weight_recurrence * magnitude),
        });
    }

    let raw: f64 = contributions.iter().map(|c| c.points).sum();
    let value = round4(raw.clamp(0.0, 10.0));
    let band = band_for(value, config);

    RiskScore { value, band, contributions }
}

fn risk_weight_for(kind: SignalKind, config: &CortexConfig) -> f64 {
    match kind {
        SignalKind::CentralitySpike => config.risk_weight_centrality_spike,
        SignalKind::CommunityBoundaryCrossing => config.risk_weight_community_boundary_crossing,
        SignalKind::SemanticDuplication => config.risk_weight_semantic_duplication,
        SignalKind::ComplexitySpike => config.risk_weight_complexity_spike,
        SignalKind::FanOutExplosion => config.risk_weight_fan_out_explosion,
    }
}

/// Query the KGFIND-01 [`FindingsStore`] (the SAME store `kg_findings`
/// reads — no second access path, S9) for findings whose scope falls within
/// this change: `scope_kind = "node"` rows whose `scope_ref` is a touched
/// node id, `"path"` rows whose `scope_ref` is a touched file, and
/// `"community"` rows whose `scope_ref` is an affected community id.
/// `FindingsStore::list` has no `scope_ref` filter, so matching is done
/// client-side against the touched sets after listing each scope kind's
/// bucket.
///
/// Degrades to `(vec![], "unavailable")` on ANY store error (unconfigured,
/// unreachable, or a query failure mid-way) — recurrence is always a
/// best-effort enrichment, never a hard failure for `cortex_review`. Returns
/// `(vec![], "empty")` when the store is reachable but nothing in scope
/// matched (distinct from "unavailable" — a caller must not read "no
/// recurrence" as "recurrence wasn't even checked"). Returns
/// `(totals, "ok")` otherwise, `totals` sorted by category for determinism.
async fn touched_recurrence(project_id: &str, touched_node_ids: &[String], changed_files: &[String], communities: &HashSet<u32>) -> (Vec<(String, i64)>, &'static str) {
    let store = match FindingsStore::from_env().await {
        Ok(s) => s,
        Err(_) => return (Vec::new(), "unavailable"),
    };

    let node_ids: HashSet<&str> = touched_node_ids.iter().map(String::as_str).collect();
    let path_set: HashSet<&str> = changed_files.iter().map(String::as_str).collect();
    let community_refs: HashSet<String> = communities.iter().map(|c| c.to_string()).collect();

    let mut totals: HashMap<String, i64> = HashMap::new();

    match store.list(project_id, Some("node"), None, None).await {
        Ok(rows) => {
            for r in rows {
                if node_ids.contains(r.scope_ref.as_str()) {
                    *totals.entry(r.category).or_insert(0) += r.occurrences as i64;
                }
            }
        }
        Err(_) => return (Vec::new(), "unavailable"),
    }

    match store.list(project_id, Some("path"), None, None).await {
        Ok(rows) => {
            for r in rows {
                if path_set.contains(r.scope_ref.as_str()) {
                    *totals.entry(r.category).or_insert(0) += r.occurrences as i64;
                }
            }
        }
        Err(_) => return (Vec::new(), "unavailable"),
    }

    if !community_refs.is_empty() {
        match store.list(project_id, Some("community"), None, None).await {
            Ok(rows) => {
                for r in rows {
                    if community_refs.contains(&r.scope_ref) {
                        *totals.entry(r.category).or_insert(0) += r.occurrences as i64;
                    }
                }
            }
            Err(_) => return (Vec::new(), "unavailable"),
        }
    }

    if totals.is_empty() {
        return (Vec::new(), "empty");
    }

    let mut out: Vec<(String, i64)> = totals.into_iter().collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    (out, "ok")
}

/// The `"configured": false` degrade response, mirroring `cortex_scope`'s
/// own `unavailable_response` shape: no Atlas graph is stored/loadable for
/// `project_id`, so neither structural signals nor a graph-scoped recurrence
/// lookup can run. `band: "unknown"` and `findings: "unavailable"` are both
/// deliberate labels (never silently `"low"`/`0.0` as if a real assessment
/// ran) — this is "we don't know," not "this change is safe."
fn unavailable_response(project_id: &str, changed_files: &[String], input_truncated: bool) -> Value {
    let mut response = json!({
        "configured": false,
        "project_id": project_id,
        "changed_files": changed_files,
        "risk_score": 0.0,
        "band": "unknown",
        "risk_signals": [],
        "contributions": [],
        "findings": "unavailable",
        "recommendation": recommendation_for("unknown"),
    });
    if input_truncated {
        response["truncated"] = json!(true);
    }
    response
}

/// Compute the full `cortex_review` response for `project_id` +
/// `changed_files`. Loads the project's Atlas graph the same way
/// `cortex_scope` (CXEG-02) does; on a missing/unloadable graph, degrades to
/// [`unavailable_response`] rather than erroring. On a live graph: resolves
/// touched node ids (current nodes whose `path` is in `changed_files`),
/// computes CXEG-03's structural signals over them
/// (`metrics::compute_signals`, the full async pipeline including
/// `semantic_duplication`), looks up KGFIND recurrence for the touched
/// node/path/community scopes ([`touched_recurrence`]), and combines both via
/// [`score`].
pub async fn compute_review(project_id: &str, changed_files: &[String], config: &CortexConfig, input_truncated: bool) -> Value {
    let store = GraphStore::from_config(&ScribeConfig::from_env());
    let graph = match store.load(project_id) {
        Ok(Some(g)) => g,
        Ok(None) | Err(_) => return unavailable_response(project_id, changed_files, input_truncated),
    };

    let changed: HashSet<&str> = changed_files.iter().map(|s| s.as_str()).collect();
    let mut touched_ids: Vec<String> = graph.current_nodes().filter(|n| changed.contains(n.path.as_str())).map(|n| n.id.clone()).collect();
    touched_ids.sort();

    let signals = compute_signals(&touched_ids, &graph, project_id, config).await;

    let mut communities: HashSet<u32> = HashSet::new();
    for id in &touched_ids {
        if let Some(n) = graph.get_node(id) {
            if let Some(c) = n.cluster {
                communities.insert(c);
            }
        }
    }

    let (recurrence, findings_status) = touched_recurrence(project_id, &touched_ids, changed_files, &communities).await;

    let risk = score(&signals, &recurrence, config);
    let recommendation = recommendation_for(risk.band);

    let mut response = json!({
        "configured": true,
        "project_id": project_id,
        "changed_files": changed_files,
        "risk_score": risk.value,
        "band": risk.band,
        "risk_signals": signals,
        "contributions": risk.contributions,
        "findings": findings_status,
        "recommendation": recommendation,
    });
    if input_truncated {
        response["truncated"] = json!(true);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scribe::graph::model::{Confidence, EdgeKind, KgEdge, KgNode, KnowledgeGraph, NodeKind};

    fn cfg() -> CortexConfig {
        CortexConfig {
            risk_score_threshold: 7.0,
            enable_tier_b: true,
            enable_tier_c: false,
            elegance_advisory_only: true,
            dup_cosine: 0.85,
            atlas_database_url: None,
            max_blast_nodes: crate::cortex::scope::DEFAULT_MAX_BLAST_NODES,
            tier_b_percentile: 90.0,
            house_style_exemplars_k: crate::cortex::house_style::DEFAULT_EXEMPLARS_K,
            risk_weight_centrality_spike: 2.0,
            risk_weight_complexity_spike: 1.5,
            risk_weight_fan_out_explosion: 1.5,
            risk_weight_community_boundary_crossing: 2.5,
            risk_weight_semantic_duplication: 10.0,
            risk_weight_recurrence: 1.0,
            risk_band_elevated_cut: 4.0,
            audit_clone_timeout_secs: 60,
            audit_max_clone_bytes: 200_000_000,
            crystallize_min_recurrence: crate::cortex::crystallize::DEFAULT_MIN_RECURRENCE,
            escalation_enabled: true,
            escalation_add_provider: "agy".to_string(),
        }
    }

    fn sig(kind: SignalKind, severity: f64) -> EleganceSignal {
        EleganceSignal {
            kind,
            severity,
            anchor_node: "crate::a::foo".to_string(),
            anchor_file: "src/a.rs".to_string(),
            why: "test signal".to_string(),
            evidence: json!({}),
        }
    }

    // ── score: bands + explainability ───────────────────────────────────

    #[test]
    fn score_is_low_for_no_signals_no_recurrence() {
        let out = score(&[], &[], &cfg());
        assert_eq!(out.value, 0.0);
        assert_eq!(out.band, "low");
        assert!(out.contributions.is_empty());
    }

    #[test]
    fn score_is_high_for_severe_structural_and_recurrence() {
        let signals = vec![
            sig(SignalKind::CentralitySpike, 2.0),
            sig(SignalKind::FanOutExplosion, 3.0),
            sig(SignalKind::CommunityBoundaryCrossing, 1.0),
        ];
        let recurrence = vec![("panic_in_execute".to_string(), 50i64)];
        let out = score(&signals, &recurrence, &cfg());
        assert_eq!(out.band, "high");
        assert!(out.value >= cfg().risk_score_threshold, "{out:?}");
        assert!(out.value <= 10.0);
    }

    #[test]
    fn score_contributions_reconstruct_raw_pre_clamp_score() {
        // Deliberately construct signals whose raw sum exceeds 10 (the clamp
        // ceiling), so `value` (clamped) and `sum(points)` (raw) diverge --
        // contributions must still reconstruct the UNCLAMPED raw score.
        let signals = vec![
            sig(SignalKind::CentralitySpike, 5.0),
            sig(SignalKind::FanOutExplosion, 5.0),
        ];
        let recurrence = vec![("dup_logic".to_string(), 1000i64)];
        let out = score(&signals, &recurrence, &cfg());
        let raw: f64 = out.contributions.iter().map(|c| c.points).sum();
        assert!(raw > 10.0, "fixture must exceed the clamp ceiling: {raw}");
        assert_eq!(out.value, 10.0, "value must be clamped to the ceiling");
        // Re-derive the individual contribution math to confirm no hidden term.
        let expected_centrality = round4(cfg().risk_weight_centrality_spike * 5.0);
        let expected_fanout = round4(cfg().risk_weight_fan_out_explosion * 5.0);
        let expected_recurrence = round4(cfg().risk_weight_recurrence * recurrence_magnitude(1000));
        assert_eq!((expected_centrality + expected_fanout + expected_recurrence - raw).abs() < 1e-6, true);
    }

    #[test]
    fn score_is_deterministic() {
        let signals = vec![sig(SignalKind::CentralitySpike, 1.2), sig(SignalKind::ComplexitySpike, 0.5)];
        let recurrence = vec![("a".to_string(), 3i64), ("b".to_string(), 7i64)];
        let a = score(&signals, &recurrence, &cfg());
        let b = score(&signals, &recurrence, &cfg());
        assert_eq!(a, b);
    }

    #[test]
    fn score_recurrence_order_does_not_affect_result() {
        let recurrence_a = vec![("z".to_string(), 4i64), ("a".to_string(), 2i64)];
        let recurrence_b = vec![("a".to_string(), 2i64), ("z".to_string(), 4i64)];
        let a = score(&[], &recurrence_a, &cfg());
        let b = score(&[], &recurrence_b, &cfg());
        assert_eq!(a, b);
    }

    #[test]
    fn band_boundary_is_deterministic_and_favors_higher_band() {
        let c = cfg();
        assert_eq!(band_for(c.risk_band_elevated_cut, &c), "elevated", "exactly at the elevated cut must read elevated, not low");
        assert_eq!(band_for(c.risk_score_threshold, &c), "high", "exactly at the high threshold must read high, not elevated");
        assert_eq!(band_for(c.risk_band_elevated_cut - 0.0001, &c), "low");
        assert_eq!(band_for(c.risk_score_threshold - 0.0001, &c), "elevated");
    }

    #[test]
    fn recommendation_never_recommends_rejection() {
        for band in ["low", "elevated", "high", "unknown"] {
            let rec = recommendation_for(band);
            assert!(!rec.to_lowercase().contains("reject"), "{band}: {rec}");
            assert!(!rec.is_empty());
        }
        assert!(recommendation_for("high").to_lowercase().contains("escalate"));
    }

    // ── recurrence_magnitude: log-scaled, sub-linear ────────────────────

    #[test]
    fn recurrence_magnitude_is_log_scaled_not_linear() {
        let one = recurrence_magnitude(1);
        let thousand = recurrence_magnitude(1000);
        // log2(1001) ~= 9.97, log2(2) = 1 -- ~10x growth for 1000x more
        // occurrences, never anywhere near linear (1000x).
        assert!(thousand / one < 15.0, "thousand={thousand} one={one}");
        assert!(thousand > one);
    }

    #[test]
    fn recurrence_magnitude_zero_occurrences_is_zero() {
        assert_eq!(recurrence_magnitude(0), 0.0);
    }

    // ── touched_recurrence: degrade without a configured store ──────────

    #[tokio::test]
    #[serial_test::serial]
    async fn touched_recurrence_unavailable_without_dsn() {
        if std::env::var("ATLAS_DATABASE_URL").is_ok() {
            return; // a real DSN is live in this process; skip
        }
        let (rec, status) = touched_recurrence("TERM", &["crate::a::foo".to_string()], &["src/a.rs".to_string()], &HashSet::new()).await;
        assert!(rec.is_empty());
        assert_eq!(status, "unavailable");
    }

    // ── compute_review: graph-unavailable degrade ───────────────────────

    #[tokio::test]
    #[serial_test::serial]
    async fn compute_review_degrades_when_project_has_no_graph() {
        let store_dir = std::env::temp_dir().join(format!("atlas-cortexreview-nograph-{}", std::process::id()));
        std::env::set_var("SCRIBE_KG_STORE_DIR", &store_dir);

        let out = compute_review("NOPE", &["src/a.rs".to_string()], &cfg(), false).await;
        assert_eq!(out["configured"], false);
        assert_eq!(out["band"], "unknown");
        assert_eq!(out["risk_score"], 0.0);
        assert_eq!(out["findings"], "unavailable");
        assert!(out["risk_signals"].as_array().unwrap().is_empty());
        assert!(out["contributions"].as_array().unwrap().is_empty());
        assert!(!out["recommendation"].as_str().unwrap().to_lowercase().contains("reject"));

        std::env::remove_var("SCRIBE_KG_STORE_DIR");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn compute_review_propagates_input_truncated_on_degrade_path() {
        let store_dir = std::env::temp_dir().join(format!("atlas-cortexreview-nograph-trunc-{}", std::process::id()));
        std::env::set_var("SCRIBE_KG_STORE_DIR", &store_dir);

        let out = compute_review("NOPE", &["src/a.rs".to_string()], &cfg(), true).await;
        assert_eq!(out["truncated"], true);

        std::env::remove_var("SCRIBE_KG_STORE_DIR");
    }

    // ── compute_review: live graph, structural-only (no findings store) ─

    fn seed_hub_graph(store: &GraphStore, project_id: &str) {
        let mut g = KnowledgeGraph::new(project_id);
        let mut hub = KgNode::new("crate::hub::Hub", NodeKind::Struct, "Hub", "src/hub.rs");
        hub.rank = 0.9;
        hub.cluster = Some(1);
        g.insert_node(hub);
        for i in 0..20 {
            let mut leaf = KgNode::new(format!("crate::leaf::f{i}"), NodeKind::Function, format!("f{i}"), "src/leaf.rs");
            leaf.rank = 0.01;
            leaf.cluster = Some(1);
            g.insert_node(leaf);
        }
        for i in 0..15 {
            g.insert_edge(KgEdge::new("crate::hub::Hub", format!("crate::leaf::f{i}"), EdgeKind::Calls, Confidence::Extracted)).unwrap();
        }
        g.recompute_degrees();
        store.save(project_id, &g).unwrap();
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn compute_review_scores_high_for_a_hub_touching_change() {
        if std::env::var("ATLAS_DATABASE_URL").is_ok() {
            return; // avoid a live findings-store connection from a unit test
        }
        let store_dir = std::env::temp_dir().join(format!("atlas-cortexreview-hub-{}", std::process::id()));
        std::env::set_var("SCRIBE_KG_STORE_DIR", &store_dir);
        let gstore = GraphStore::from_config(&ScribeConfig::from_env());
        seed_hub_graph(&gstore, "TERM");

        let out = compute_review("TERM", &["src/hub.rs".to_string()], &cfg(), false).await;
        assert_eq!(out["configured"], true);
        assert_eq!(out["findings"], "unavailable", "no ATLAS_DATABASE_URL in this test env");
        let signals = out["risk_signals"].as_array().unwrap();
        assert!(!signals.is_empty(), "a touched hub must fire at least one structural signal: {out}");
        assert!(out["risk_score"].as_f64().unwrap() > 0.0);
        assert!(!out["recommendation"].as_str().unwrap().is_empty());

        let _ = std::fs::remove_dir_all(&store_dir);
        std::env::remove_var("SCRIBE_KG_STORE_DIR");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn compute_review_scores_low_for_a_clean_small_change() {
        if std::env::var("ATLAS_DATABASE_URL").is_ok() {
            return;
        }
        let store_dir = std::env::temp_dir().join(format!("atlas-cortexreview-clean-{}", std::process::id()));
        std::env::set_var("SCRIBE_KG_STORE_DIR", &store_dir);
        let gstore = GraphStore::from_config(&ScribeConfig::from_env());

        // A tiny, uniform graph -- nothing is a percentile outlier, so no
        // structural signal fires for a touched leaf node.
        let mut g = KnowledgeGraph::new("TERM");
        for i in 0..10 {
            let mut n = KgNode::new(format!("crate::m::f{i}"), NodeKind::Function, format!("f{i}"), "src/m.rs");
            n.rank = 0.1;
            n.cluster = Some(1);
            g.insert_node(n);
        }
        g.recompute_degrees();
        gstore.save("TERM", &g).unwrap();

        let out = compute_review("TERM", &["src/m.rs".to_string()], &cfg(), false).await;
        assert_eq!(out["configured"], true);
        assert_eq!(out["band"], "low", "{out}");
        assert_eq!(out["risk_score"], 0.0, "{out}");
        assert!(out["risk_signals"].as_array().unwrap().is_empty());

        let _ = std::fs::remove_dir_all(&store_dir);
        std::env::remove_var("SCRIBE_KG_STORE_DIR");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn compute_review_is_deterministic() {
        if std::env::var("ATLAS_DATABASE_URL").is_ok() {
            return;
        }
        let store_dir = std::env::temp_dir().join(format!("atlas-cortexreview-det-{}", std::process::id()));
        std::env::set_var("SCRIBE_KG_STORE_DIR", &store_dir);
        let gstore = GraphStore::from_config(&ScribeConfig::from_env());
        seed_hub_graph(&gstore, "TERM");

        let a = compute_review("TERM", &["src/hub.rs".to_string()], &cfg(), false).await;
        let b = compute_review("TERM", &["src/hub.rs".to_string()], &cfg(), false).await;
        assert_eq!(a, b);

        let _ = std::fs::remove_dir_all(&store_dir);
        std::env::remove_var("SCRIBE_KG_STORE_DIR");
    }
}
