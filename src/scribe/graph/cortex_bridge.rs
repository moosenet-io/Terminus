//! Cortex ↔ Atlas (KG) bridge (KGRULE-05).
//!
//! A small, dependency-light helper that turns a KG scope (`scope_kind`,
//! `scope_ref` — see `findings_store::ScopeKind`) into a best-effort Cortex
//! risk score, so KGRULE-02's rule crystallization can prioritize high-risk
//! recurring findings. This module does NOT talk SSH itself — it reuses the
//! existing `crate::cortex` tool implementation (config, shell-quoting,
//! degrade behavior, everything) verbatim, exactly as `crate::cortex`
//! reuses `crucible`/`sentinel`'s SSH-exec mechanics.
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
//! - `CORTEX_SSH_HOST` is unset (Cortex not configured at all) — checked
//!   BEFORE constructing anything, so this path makes **zero** SSH attempts
//!   and returns immediately.
//! - `scope_kind` is `"community"` or `"global"` — Cortex is a per-file/
//!   per-symbol (post-hoc risk / blast-radius) tool, it has no notion of a
//!   community or global-scope risk score, so these are skipped without
//!   even checking configuration.
//! - The underlying `cortex_review` tool call errors for any reason (SSH
//!   unreachable, auth failure, remote non-zero exit, `NotConfigured`
//!   surfacing late, task join failure, …).
//! - The tool's response is valid JSON but carries no numeric `risk`/`score`
//!   field (including the documented `{"raw": "<stdout>"}` shape `crate::cortex`
//!   returns for non-JSON remote output).
//! - The tool's response isn't valid JSON at all (defensive; `crate::cortex`
//!   should never actually produce this, but this module never trusts that
//!   invariant with an `unwrap`/`expect`).
//!
//! ## Which Cortex entry point this uses, and why
//!
//! `cortex_review` ("post-change risk assessment... Returns: risk_score
//! (0-10), risk_signals, blast_radius, token_reduction_pct" per its own
//! tool description in `crate::cortex`) is the one Cortex tool whose
//! documented purpose is a risk *score* for a given set of files — exactly
//! the "post-hoc risk on the scope" this bridge needs. `cortex_scope` is
//! blast-radius for a change that hasn't happened yet (no risk score at
//! all), and the other eight tools return stats/architecture/dependency
//! shapes with no risk field. Both `scope_kind == "path"` (a file path) and
//! `scope_kind == "node"` (a symbol) are passed through as `cortex_review`'s
//! `changed_files` argument — that field is a free-text comma-separated
//! list in the underlying tool, so a single file path or a single symbol
//! name both round-trip through it unchanged; `crate::cortex` places no
//! further structural requirement on the string.
//!
//! `repo` is fixed to `"lumina-terminus"` — this crate IS lumina-terminus,
//! so there's no ambiguity to parameterize, and the value is one of
//! `crate::cortex`'s own two known-repo enum values (not a host/IP/infra
//! literal — see `crate::cortex::KNOWN_REPOS`).
//!
//! ## Risk extraction
//!
//! `extract_risk` is a pure function (no I/O, no async, fully unit-tested)
//! that looks for a numeric `risk` or `score` field, first at the top level
//! of the parsed JSON and then one level deeper under a `result` key (in
//! case a future/alternate Cortex response nests its payload). A found
//! value is clamped to `[0.0, 1.0]` — Cortex's own documented `risk_score`
//! range is `0-10`, not `0-1`, but this bridge's contract (and KGRULE-02's
//! consumption of it) is a normalized `0.0..=1.0` risk signal, so any
//! numeric value found is clamped into that range rather than left
//! unbounded or silently rescaled (rescaling would assume a specific,
//! unverified upstream scale — see `crate::cortex`'s own extensive
//! "NOT verified" notes about response shapes). A non-numeric value at the
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

/// True when `CORTEX_SSH_HOST` is set to a non-empty value — mirrors
/// `CortexConfig::from_env`'s own `ssh_host` parsing exactly (see
/// `src/cortex/mod.rs`) without depending on a `CortexConfig` instance.
fn cortex_host_configured() -> bool {
    std::env::var("CORTEX_SSH_HOST")
        .ok()
        .filter(|s| !s.is_empty())
        .is_some()
}

/// Best-effort Cortex risk score for a KG scope. Returns `None` (never
/// panics, never errors) whenever Cortex is unconfigured, the scope kind
/// has no Cortex analog, or the remote call/parse fails in any way. See the
/// module-level doc comment for the full degrade contract.
pub async fn cortex_risk_for_scope(scope_kind: &str, scope_ref: &str) -> Option<f32> {
    if !scope_kind_supported(scope_kind) {
        return None;
    }
    if !cortex_host_configured() {
        // No SSH attempt at all — checked before touching crate::cortex.
        return None;
    }

    // Reuse the existing Cortex tool implementation verbatim (its config,
    // shell-quoting, SSH-exec, and degrade behavior) rather than
    // duplicating any of it here. A scratch registry containing only the
    // Cortex tools is the cheapest way to reach `cortex_review`'s `execute`
    // from outside `crate::cortex`, since its tool structs' fields are
    // private to that module.
    let mut registry = ToolRegistry::new();
    crate::cortex::register(&mut registry);

    let payload = serde_json::json!({
        "repo": "lumina-terminus",
        "changed_files": scope_ref,
    });

    let result = registry.call("cortex_review", payload).await?;
    let text = result.ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    extract_risk(&value)
}

/// Pure JSON → risk extraction. Looks for a numeric `risk` or `score` field
/// at the top level, then one level deeper under `result`. Any value found
/// is clamped to `[0.0, 1.0]`. Missing/non-numeric/unparsable → `None`.
fn extract_risk(v: &Value) -> Option<f32> {
    fn numeric_field(v: &Value, key: &str) -> Option<f32> {
        v.get(key).and_then(Value::as_f64).map(|f| f as f32)
    }

    let found = numeric_field(v, "risk")
        .or_else(|| numeric_field(v, "score"))
        .or_else(|| v.get("result").and_then(|r| numeric_field(r, "risk")))
        .or_else(|| v.get("result").and_then(|r| numeric_field(r, "score")));

    found.map(|f| f.clamp(0.0, 1.0))
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

    // --- cortex_risk_for_scope: env-gated, no SSH when unconfigured --------

    #[tokio::test]
    #[serial]
    async fn test_cortex_unconfigured_returns_none_fast_no_ssh() {
        if std::env::var("CORTEX_SSH_HOST")
            .ok()
            .filter(|s| !s.is_empty())
            .is_some()
        {
            // A real Cortex host is configured in this process's env — this
            // test specifically exercises the "unconfigured" path, so skip
            // rather than false-fail (or worse, actually attempt SSH).
            return;
        }
        std::env::remove_var("CORTEX_SSH_HOST");

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            cortex_risk_for_scope("path", "src/x.rs"),
        )
        .await
        .expect("must return fast — no SSH attempt should ever be made");

        assert_eq!(result, None);
    }

    #[tokio::test]
    #[serial]
    async fn test_community_and_global_scope_kinds_skip_before_env_check() {
        // Even if CORTEX_SSH_HOST happened to be set, these scope kinds
        // must short-circuit to None without attempting a call.
        assert_eq!(cortex_risk_for_scope("community", "cluster-1").await, None);
        assert_eq!(cortex_risk_for_scope("global", "").await, None);
    }
}
