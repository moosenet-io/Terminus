//! Cortex ↔ Atlas (KG) bridge (KGRULE-05).
//!
//! A small, dependency-light helper that turns a KG scope (`scope_kind`,
//! `scope_ref` — see `findings_store::ScopeKind`) into a best-effort Cortex
//! risk score, so KGRULE-02's rule crystallization can prioritize high-risk
//! recurring findings. As of CXEG-01 `crate::cortex` is Atlas-backed and no
//! longer an SSH relay: it holds no SSH transport, no shell-quoting, and no
//! remote fleet-host script. This bridge calls the in-process `cortex_review`
//! tool, which as of **CXEG-04** is a real Atlas-backed risk scorer (no
//! longer a pending stub) returning a `risk_score` (0-10) — so this bridge
//! now yields a real signal whenever a stored Atlas graph is available for
//! the project, and degrades cleanly to `None` otherwise.
//!
//! ## Degrade contract (read before calling this from new code)
//!
//! `cortex_risk_for_scope` NEVER panics and NEVER returns an `Err` — its
//! return type is `Option<f32>`, not a `Result`, specifically so callers
//! (KGRULE-02's crystallization flow) can attach it as a "nice to have"
//! without any error-handling branch. It returns `None` in every one of
//! these cases, and callers MUST treat `None` as "no signal available", not
//! as an error condition:
//!
//! - `scope_kind` is `"community"` or `"global"` — Cortex is a per-file/
//!   per-symbol (post-hoc risk / blast-radius) tool, it has no notion of a
//!   community or global-scope risk score, so these are skipped immediately.
//! - The underlying `cortex_review` tool call errors for any reason
//!   (`InvalidArgument`, task join failure, …).
//! - The response is a `cortex_review` **degrade** response
//!   (`"configured": false`, i.e. no Atlas graph is stored for the project,
//!   so `band` is `"unknown"` and `risk_score` is a placeholder `0.0`). That
//!   `0.0` is a "we don't know," not a real "zero risk," so the bridge treats
//!   it as no signal (`None`) rather than surfacing a misleading `Some(0.0)`.
//!   A `"configured": true` response — even one whose KGFIND `findings` are
//!   `"unavailable"`/`"empty"` — DOES carry a real (structural) `risk_score`
//!   and is surfaced as `Some(..)`.
//! - The tool's response is valid JSON but carries no numeric `risk_score`/
//!   `risk`/`score` field (defensive — a real `cortex_review` response always
//!   carries `risk_score`, but this module never assumes that invariant).
//! - The tool's response isn't valid JSON at all (defensive; `crate::cortex`
//!   should never actually produce this, but this module never trusts that
//!   invariant with an `unwrap`/`expect`).
//!
//! ## Which Cortex entry point this uses, and why
//!
//! `cortex_review` (post-change risk assessment, returning `risk_score`
//! (0-10) as of CXEG-04 — see its description in `crate::cortex`) is the one
//! Cortex tool whose purpose is a risk *score* for a given set of files —
//! exactly the "post-hoc risk on the scope" this bridge needs. `cortex_scope`
//! is blast-radius for a change that hasn't happened yet (no risk score at
//! all), and the 7 deprecated relay-tool aliases (`crate::cortex::deprecated`)
//! return only a `{"deprecated":true,...}` pointer, never a risk field. Both
//! `scope_kind == "path"` (a file path) and `scope_kind == "node"` (a
//! symbol) are passed through as `cortex_review`'s `changed_files` argument
//! — a single file path or a single symbol name both round-trip through it
//! unchanged (`cortex_review` derives the touched Atlas nodes for a path
//! from the graph; a bare symbol id that matches no file simply yields no
//! touched nodes, i.e. a low/zero structural score, not an error).
//!
//! `project_id` is fixed to `"TERM"` — this crate IS Terminus, so there's no
//! ambiguity to parameterize, and the value is one of `crate::cortex`'s own
//! `PROJECT_IDS` allow-list values (not a host/IP/infra literal — CXEG-01
//! replaced the old `KNOWN_REPOS` repo-name allowlist with this
//! `project_id` vocabulary).
//!
//! ## Risk extraction
//!
//! `extract_risk` is a pure function (no I/O, no async, fully unit-tested)
//! that looks for a numeric risk field, first at the top level of the parsed
//! JSON and then one level deeper under a `result` key (in case a future/
//! alternate Cortex response nests its payload). This bridge's contract (and
//! KGRULE-02's consumption of it) is a normalized `0.0..=1.0` risk signal, so:
//! - **`risk_score`** is Cortex's own DOCUMENTED field (`cortex_review` states
//!   its range is `0-10`), so it is the PRIMARY field and is RESCALED `0-10 ->
//!   0-1` by dividing by 10, then clamped. This is a rescale against Cortex's
//!   *documented* scale (not a guess).
//! - **`risk`/`score`** are accepted as fallbacks and treated as already-
//!   normalized `[0,1]` fractions — clamped as-is, never rescaled.
//! Everything ends clamped to `[0.0, 1.0]`. A non-numeric value at the
//! `risk`/`score` key (e.g. `{"risk": "high"}`) is treated the same as a
//! missing one: `None`, not a parse error.

use serde_json::Value;

use crate::registry::ToolRegistry;

/// Cortex has no notion of a graph-community or whole-project ("global")
/// risk score — it is strictly per-file/per-symbol. Skip these scope kinds
/// without even checking whether Cortex is configured.
fn scope_kind_supported(scope_kind: &str) -> bool {
    matches!(scope_kind, "path" | "node")
}

/// Best-effort Cortex risk score for a KG scope. Returns `None` (never
/// panics, never errors) whenever the scope kind has no Cortex analog, or
/// the call/parse fails or yields no risk field in any way (see the
/// module-level doc comment for the full degrade contract — as of CXEG-01,
/// `cortex_review` is a pending stub, so this always returns `None` today,
/// by design, until CXEG-04 lands).
pub async fn cortex_risk_for_scope(scope_kind: &str, scope_ref: &str) -> Option<f32> {
    if !scope_kind_supported(scope_kind) {
        return None;
    }

    // Reuse the existing Cortex tool implementation verbatim rather than
    // duplicating any of it here. A scratch registry containing only the
    // Cortex tools is the cheapest way to reach `cortex_review`'s `execute`
    // from outside `crate::cortex`, since its tool structs' fields are
    // private to that module.
    let mut registry = ToolRegistry::new();
    crate::cortex::register(&mut registry);

    let payload = serde_json::json!({
        "project_id": "TERM",
        "changed_files": scope_ref,
    });

    let result = registry.call("cortex_review", payload).await?;
    let text = result.ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;

    // CXEG-04: `cortex_review` is now real, but it degrades to a
    // `{"configured": false, "band": "unknown", "risk_score": 0.0, ...}`
    // response when no Atlas graph is stored for the project (see
    // `cortex::review::compute_review`'s degrade contract). That `0.0` is a
    // "we don't know," NOT a real "zero risk" — surfacing it as `Some(0.0)`
    // would falsely tell KGRULE-02 the scope is risk-free. So a degrade
    // response is treated as "no signal available" (`None`), exactly like a
    // tool error or a missing risk field. Only a `configured: true` response
    // (a real assessment ran against a stored graph) has its `risk_score`
    // extracted — even if its `findings` are `"unavailable"`/`"empty"`, the
    // structural half of that score IS a real signal.
    if value.get("configured") == Some(&Value::Bool(false)) {
        return None;
    }
    extract_risk(&value)
}

/// Pure JSON → risk extraction. Looks for a numeric `risk` or `score` field
/// at the top level, then one level deeper under `result`. Any value found
/// is clamped to `[0.0, 1.0]`. Missing/non-numeric/unparsable → `None`.
fn extract_risk(v: &Value) -> Option<f32> {
    fn numeric_field(v: &Value, key: &str) -> Option<f32> {
        v.get(key).and_then(Value::as_f64).map(|f| f as f32)
    }

    // Cortex's `cortex_review` documents its field as `risk_score` on a 0-10
    // scale — that's the PRIMARY field; `risk`/`score` are accepted as [0,1]
    // fraction fallbacks. Search the top level first, then one level under
    // `result`. `risk_score` is normalized 0-10 -> 0-1; the fraction fields are
    // clamped as-is. Everything ends clamped to [0,1].
    for obj in [Some(v), v.get("result")].into_iter().flatten() {
        if let Some(rs) = numeric_field(obj, "risk_score") {
            return Some((rs / 10.0).clamp(0.0, 1.0));
        }
        if let Some(frac) = numeric_field(obj, "risk").or_else(|| numeric_field(obj, "score")) {
            return Some(frac.clamp(0.0, 1.0));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use serial_test::serial;

    // --- extract_risk: pure, no I/O -----------------------------------------

    #[test]
    fn test_extract_risk_top_level_risk_field() {
        assert_eq!(extract_risk(&json!({"risk": 0.7})), Some(0.7));
    }

    #[test]
    fn test_extract_risk_top_level_score_field() {
        assert_eq!(extract_risk(&json!({"score": 0.3})), Some(0.3));
    }

    #[test]
    fn test_extract_risk_nested_under_result() {
        assert_eq!(extract_risk(&json!({"result": {"risk": 0.9}})), Some(0.9));
        assert_eq!(extract_risk(&json!({"result": {"score": 0.4}})), Some(0.4));
    }

    #[test]
    fn test_extract_risk_raw_wrapper_is_none() {
        assert_eq!(extract_risk(&json!({"raw": "Graph rebuilt: 42 nodes"})), None);
    }

    #[test]
    fn test_extract_risk_empty_object_is_none() {
        assert_eq!(extract_risk(&json!({})), None);
    }

    #[test]
    fn test_extract_risk_non_numeric_value_is_none() {
        assert_eq!(extract_risk(&json!({"risk": "high"})), None);
    }

    #[test]
    fn test_extract_risk_clamps_above_one() {
        assert_eq!(extract_risk(&json!({"risk": 7.0})), Some(1.0));
    }

    #[test]
    fn test_extract_risk_clamps_below_zero() {
        assert_eq!(extract_risk(&json!({"risk": -3.0})), Some(0.0));
    }

    #[test]
    fn test_extract_risk_prefers_risk_over_score_when_both_present() {
        assert_eq!(extract_risk(&json!({"risk": 0.2, "score": 0.9})), Some(0.2));
    }

    #[test]
    fn test_extract_risk_reads_cortex_risk_score_field_on_0_to_10_scale() {
        // cortex_review's documented field is `risk_score` (0-10) — normalize to
        // [0,1]. This is the field a REAL Cortex answer uses. Use approx compares
        // for non-exact f32 divisions (7.2/10, 8.0/10 aren't exact in f32).
        let approx = |v: Option<f32>, want: f32| {
            assert!(matches!(v, Some(x) if (x - want).abs() < 1e-6), "got {v:?}, want ~{want}");
        };
        approx(extract_risk(&json!({"risk_score": 7.2})), 0.72);
        assert_eq!(extract_risk(&json!({"risk_score": 10})), Some(1.0)); // exact
        assert_eq!(extract_risk(&json!({"risk_score": 0})), Some(0.0)); // exact
        assert_eq!(extract_risk(&json!({"result": {"risk_score": 5}})), Some(0.5)); // exact
        // precedence over the fraction fallbacks
        approx(extract_risk(&json!({"risk_score": 8.0, "risk": 0.1})), 0.8);
    }

    // --- scope_kind gating: pure ---------------------------------------------

    #[test]
    fn test_scope_kind_supported_path_and_node() {
        assert!(scope_kind_supported("path"));
        assert!(scope_kind_supported("node"));
    }

    #[test]
    fn test_scope_kind_supported_rejects_community_and_global() {
        assert!(!scope_kind_supported("community"));
        assert!(!scope_kind_supported("global"));
    }

    // --- cortex_risk_for_scope: no network of any kind, ever (CXEG-01/04) --

    #[tokio::test]
    #[serial]
    async fn test_cortex_review_degrade_without_graph_returns_none_fast() {
        // As of CXEG-04, cortex_review is REAL (not a pending stub), but with
        // no Atlas graph stored for the project it degrades to
        // {configured:false, band:"unknown", risk_score:0.0}. The bridge
        // treats that "we don't know" degrade as no signal (None), NOT a
        // Some(0.0) that would falsely read as "zero risk" — see
        // `cortex_risk_for_scope`'s configured-guard. An empty store dir is
        // forced so the degrade path is deterministic regardless of any
        // ambient SCRIBE_KG_STORE_DIR. Must return fast: GraphStore is a
        // local filesystem load and FindingsStore is NotConfigured without a
        // DSN, so no network is ever attempted.
        let store_dir = std::env::temp_dir()
            .join(format!("atlas-cortexbridge-nograph-{}", std::process::id()));
        std::env::set_var("SCRIBE_KG_STORE_DIR", &store_dir);

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            cortex_risk_for_scope("path", "src/x.rs"),
        )
        .await
        .expect("must return fast — no network attempt is ever made");

        std::env::remove_var("SCRIBE_KG_STORE_DIR");
        assert_eq!(result, None, "a configured:false degrade must yield no signal");
    }

    #[tokio::test]
    #[serial]
    async fn test_community_and_global_scope_kinds_skip_immediately() {
        assert_eq!(cortex_risk_for_scope("community", "cluster-1").await, None);
        assert_eq!(cortex_risk_for_scope("global", "").await, None);
    }
}
