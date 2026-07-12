//! KGRULE-02: crystallize candidate rules from recurring findings.
//!
//! `kg_rule_crystallize(project_id, min_occurrences?)` scans the
//! [`super::findings_store::FindingsStore`] for finding buckets whose
//! recurrence meets a threshold and mints CANDIDATE rules — always
//! `status=candidate`, `enforcement=advisory` (never active, never
//! blocking; that's KGRULE-03's adversarial promotion). Crystallization is
//! idempotent: [`super::rules_store::RulesStore::create_candidate`] dedups
//! per `(project_id, scope_kind, scope_ref, category)` bucket at the DB
//! layer regardless of what this module decides, so a duplicate mint is
//! never possible even if the pure decision below is ever wrong.
//!
//! Cortex risk is attached best-effort via
//! [`super::cortex_bridge::cortex_risk_for_scope`] — that helper already
//! never panics/errors (returns `None` on any failure or when Cortex is
//! unconfigured), so this module never needs its own Cortex error handling.

use serde_json::{json, Value};

use async_trait::async_trait;

use super::findings_store::{FindingRow, FindingsStore, ScopeKind};
use super::rules_store::{NewRule, RulesStore};
use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::{RustTool, ToolOutput};

/// Default minimum occurrence count for a finding bucket to crystallize into
/// a candidate rule, used when neither the tool argument nor
/// `KGRULE_CRYSTALLIZE_MIN_OCCURRENCES` is set. Mirrors the
/// `dedup_threshold()` env-read idiom in `findings_store.rs`.
pub const DEFAULT_MIN_OCCURRENCES: i32 = 3;

/// Resolve the crystallization threshold from `KGRULE_CRYSTALLIZE_MIN_OCCURRENCES`,
/// falling back to [`DEFAULT_MIN_OCCURRENCES`] when unset or unparsable.
fn min_occurrences_default() -> i32 {
    std::env::var("KGRULE_CRYSTALLIZE_MIN_OCCURRENCES")
        .ok()
        .and_then(|v| v.trim().parse::<i32>().ok())
        .unwrap_or(DEFAULT_MIN_OCCURRENCES)
}

/// A `(scope_kind, scope_ref, category)` bucket already covered by an
/// existing rule (active, in practice — see [`crystallize_candidates`]'s
/// doc comment on why `active` is what the tool call site supplies), used to
/// skip re-crystallizing a bucket that already has a rule. Kept as its own
/// small type (rather than a bare tuple) so the pure decision function's
/// signature stays self-documenting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExistingRuleBucket {
    pub scope_kind: String,
    pub scope_ref: String,
    pub category: String,
}

/// A decided crystallization seed: the pieces of a [`NewRule`] derived from a
/// [`FindingRow`], before Cortex risk (async, I/O) or the actual store write
/// are attached. Kept separate from `NewRule` so the DECISION step
/// ([`crystallize_candidates`]) stays pure and DB/Cortex-free.
#[derive(Debug, Clone, PartialEq)]
pub struct NewRuleSeed {
    pub project_id: String,
    pub scope_kind: String,
    pub scope_ref: String,
    pub category: String,
    pub guidance: String,
    pub provenance: Value,
    pub recurrence_at_creation: i32,
}

/// Pure guidance derivation: a concise imperative built from a finding's
/// category + description. Deterministic, non-empty, no I/O.
///
/// Shape: `"Address recurring {category}: {trimmed description}."` — always
/// includes the category (per KGRULE-02's test plan) and never panics on a
/// pathological (empty/whitespace-only) description.
pub fn derive_guidance(category: &str, description: &str) -> String {
    let category = category.trim();
    let description = description.trim();
    let category_label = if category.is_empty() { "issue" } else { category };
    if description.is_empty() {
        format!("Address recurring {category_label}.")
    } else {
        format!("Address recurring {category_label}: {description}.")
    }
}

/// Pure crystallization DECISION: given the finding rows, the buckets
/// already covered by an existing rule, and a threshold, decide which rows
/// should mint a new candidate rule seed. No DB, no Cortex, no async — fully
/// unit-testable.
///
/// A finding qualifies iff `occurrences >= min_occurrences` AND its
/// `(scope_kind, scope_ref, category)` bucket is not in `existing`. This is
/// a best-effort pre-filter for the tool's `created`/`skipped` tally and for
/// avoiding a wasted Cortex lookup — the actual duplicate-safety guarantee
/// is `RulesStore::create_candidate`'s own idempotent-per-bucket dedup at
/// the DB layer (KGRULE-01), which this function's caller always still goes
/// through.
pub fn crystallize_candidates(
    findings: &[FindingRow],
    existing: &[ExistingRuleBucket],
    min_occurrences: i32,
) -> Vec<NewRuleSeed> {
    findings
        .iter()
        .filter(|f| f.occurrences >= min_occurrences)
        .filter(|f| {
            !existing.iter().any(|e| {
                e.scope_kind == f.scope_kind && e.scope_ref == f.scope_ref && e.category == f.category
            })
        })
        .map(|f| NewRuleSeed {
            project_id: f.project_id.clone(),
            scope_kind: f.scope_kind.clone(),
            scope_ref: f.scope_ref.clone(),
            category: f.category.clone(),
            guidance: derive_guidance(&f.category, &f.description),
            provenance: json!({
                "finding_ids": [f.id.to_string()],
                "occurrences": f.occurrences,
                "source": "kg_rule_crystallize",
            }),
            recurrence_at_creation: f.occurrences,
        })
        .collect()
}

/// Parse a finding row's `scope_kind` string into the `ScopeKind` enum
/// `RulesStore`/`NewRule` expect, falling back to `Path` for a value the
/// store itself wouldn't have written (defensive; `kg_findings`'s CHECK
/// constraint means this should always parse in practice).
fn parse_scope_kind(s: &str) -> ScopeKind {
    ScopeKind::parse(s).unwrap_or(ScopeKind::Path)
}

// ── kg_rule_crystallize ───────────────────────────────────────────────────
pub struct KgRuleCrystallize;

#[async_trait]
impl RustTool for KgRuleCrystallize {
    fn name(&self) -> &str {
        "kg_rule_crystallize"
    }
    fn description(&self) -> &str {
        "Scan a project's Atlas knowledge-graph findings (kg_findings) for recurring (scope, \
category) buckets and mint CANDIDATE rules (advisory, status=candidate — never active or \
blocking) for buckets at or above the occurrence threshold. Attaches a best-effort Cortex risk \
score per scope when Cortex is configured. Idempotent — re-running never duplicates an existing \
candidate/active rule for the same bucket. Degrades to `configured:false` when the rules or \
findings store is unconfigured."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_id": {"type": "string", "description": "Plane project id, e.g. TERM"},
                "min_occurrences": {"type": "integer", "description": "minimum finding recurrence to crystallize (default from KGRULE_CRYSTALLIZE_MIN_OCCURRENCES, else 3)"}
            },
            "required": ["project_id"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.execute_structured(args).await?.text)
    }
    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let project_id = args
            .get("project_id")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                ToolError::InvalidArgument("'project_id' is required and must be a non-empty string".into())
            })?;
        let threshold = args
            .get("min_occurrences")
            .and_then(|v| v.as_i64())
            // Saturating clamp to a valid threshold [1, i32::MAX], never `as i32`:
            // a huge JSON int would wrap to a negative threshold that accepts
            // EVERY finding (minting candidates far below the intended
            // recurrence). A threshold below 1 is meaningless, so floor at 1.
            .map(|v| v.clamp(1, i32::MAX as i64) as i32)
            .unwrap_or_else(min_occurrences_default);

        let rules_store = match RulesStore::from_env().await {
            Ok(s) => s,
            Err(ToolError::NotConfigured(_)) => {
                return structured(json!({"configured": false, "project_id": project_id}));
            }
            Err(e) => {
                return structured(json!({
                    "configured": false, "project_id": project_id, "error": e.to_string(),
                }));
            }
        };
        let findings_store = match FindingsStore::from_env().await {
            Ok(s) => s,
            Err(ToolError::NotConfigured(_)) => {
                return structured(json!({"configured": false, "project_id": project_id}));
            }
            Err(e) => {
                return structured(json!({
                    "configured": false, "project_id": project_id, "error": e.to_string(),
                }));
            }
        };

        let findings = findings_store
            .list(&project_id, None, None, Some(threshold))
            .await?;

        // Buckets already covered by an active rule — best-effort pre-filter
        // (see crystallize_candidates' doc comment); RulesStore::create_candidate
        // is still the authoritative, DB-level idempotency guarantee below.
        let existing: Vec<ExistingRuleBucket> = rules_store
            .list_active(&project_id, None, None, None)
            .await?
            .into_iter()
            .map(|r| ExistingRuleBucket {
                scope_kind: r.scope_kind,
                scope_ref: r.scope_ref,
                category: r.category,
            })
            .collect();

        let seeds = crystallize_candidates(&findings, &existing, threshold);
        let skipped = (findings.len() as u32).saturating_sub(seeds.len() as u32);

        let mut created = 0u32;
        let mut candidates: Vec<Value> = Vec::with_capacity(seeds.len());

        for seed in seeds {
            // Best-effort Cortex risk — cortex_risk_for_scope never panics or
            // errors; `None` just means "no signal", crystallization proceeds.
            let cortex_risk =
                super::cortex_bridge::cortex_risk_for_scope(&seed.scope_kind, &seed.scope_ref)
                    .await;

            let new_rule = NewRule {
                project_id: seed.project_id.clone(),
                scope_kind: parse_scope_kind(&seed.scope_kind),
                scope_ref: seed.scope_ref.clone(),
                category: seed.category.clone(),
                guidance: seed.guidance.clone(),
                provenance: seed.provenance.clone(),
                recurrence_at_creation: Some(seed.recurrence_at_creation),
                cortex_risk,
            };

            let id = rules_store.create_candidate(new_rule).await?;
            created += 1;
            candidates.push(json!({
                "id": id.to_string(),
                "scope_kind": seed.scope_kind,
                "scope_ref": seed.scope_ref,
                "category": seed.category,
                "cortex_risk": cortex_risk,
            }));
        }

        structured(json!({
            "configured": true,
            "project_id": project_id,
            "created": created,
            "skipped": skipped,
            "candidates": candidates,
        }))
    }
}

fn structured(v: Value) -> Result<ToolOutput, ToolError> {
    let text = serde_json::to_string_pretty(&v).unwrap_or_else(|_| "{}".to_string());
    Ok(ToolOutput { text, structured: Some(v) })
}

/// Register the `kg_rule_crystallize` tool on the core registry.
pub fn register(registry: &mut ToolRegistry) {
    let _ = registry.register(Box::new(KgRuleCrystallize));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use uuid::Uuid;

    fn finding(occurrences: i32, category: &str, description: &str) -> FindingRow {
        let now = chrono::Utc::now();
        FindingRow {
            id: Uuid::new_v4(),
            project_id: "TERM".to_string(),
            category: category.to_string(),
            severity: "warning".to_string(),
            scope_kind: "path".to_string(),
            scope_ref: "src/lib.rs".to_string(),
            description: description.to_string(),
            provenance: json!([]),
            first_seen: now,
            last_seen: now,
            occurrences,
        }
    }

    // ── derive_guidance: pure, deterministic, non-empty ────────────────────

    #[test]
    fn derive_guidance_is_deterministic() {
        let a = derive_guidance("lint", "unused import");
        let b = derive_guidance("lint", "unused import");
        assert_eq!(a, b);
    }

    #[test]
    fn derive_guidance_is_non_empty() {
        assert!(!derive_guidance("lint", "unused import").is_empty());
        assert!(!derive_guidance("", "").is_empty());
    }

    #[test]
    fn derive_guidance_includes_category() {
        let g = derive_guidance("security", "SQL injection risk");
        assert!(g.contains("security"), "guidance must mention category: {g}");
        assert!(g.contains("SQL injection risk"), "guidance must mention description: {g}");
    }

    #[test]
    fn derive_guidance_handles_empty_category_and_description() {
        let g = derive_guidance("", "");
        assert!(!g.is_empty());
        assert!(g.contains("issue"));
    }

    #[test]
    fn derive_guidance_trims_whitespace() {
        let g = derive_guidance("  lint  ", "  unused import  ");
        assert_eq!(g, "Address recurring lint: unused import.");
    }

    // ── crystallize_candidates: pure DECISION, no DB ───────────────────────

    #[test]
    fn crystallize_candidates_selects_at_or_above_threshold() {
        let findings = vec![finding(2, "lint", "a"), finding(3, "lint", "b"), finding(5, "lint", "c")];
        let seeds = crystallize_candidates(&findings, &[], 3);
        assert_eq!(seeds.len(), 2);
        assert!(seeds.iter().all(|s| s.recurrence_at_creation >= 3));
    }

    #[test]
    fn crystallize_candidates_below_threshold_yields_none() {
        let findings = vec![finding(1, "lint", "a"), finding(2, "lint", "b")];
        let seeds = crystallize_candidates(&findings, &[], 3);
        assert!(seeds.is_empty());
    }

    #[test]
    fn crystallize_candidates_exact_boundary_included() {
        let findings = vec![finding(3, "lint", "a")];
        let seeds = crystallize_candidates(&findings, &[], 3);
        assert_eq!(seeds.len(), 1);
    }

    #[test]
    fn crystallize_candidates_carries_scope_and_category() {
        let findings = vec![finding(4, "security", "eval() call")];
        let seeds = crystallize_candidates(&findings, &[], 3);
        assert_eq!(seeds[0].scope_kind, "path");
        assert_eq!(seeds[0].scope_ref, "src/lib.rs");
        assert_eq!(seeds[0].category, "security");
        assert_eq!(seeds[0].project_id, "TERM");
    }

    #[test]
    fn crystallize_candidates_provenance_carries_finding_id_and_occurrences() {
        let f = finding(4, "lint", "x");
        let expected_id = f.id.to_string();
        let seeds = crystallize_candidates(&[f], &[], 3);
        let prov = &seeds[0].provenance;
        assert_eq!(prov["occurrences"], json!(4));
        assert_eq!(prov["finding_ids"], json!([expected_id]));
    }

    #[test]
    fn crystallize_candidates_empty_input_is_empty_output() {
        assert!(crystallize_candidates(&[], &[], 3).is_empty());
    }

    #[test]
    fn crystallize_candidates_skips_bucket_with_existing_rule() {
        let findings = vec![finding(5, "lint", "a"), finding(5, "security", "b")];
        let existing = vec![ExistingRuleBucket {
            scope_kind: "path".to_string(),
            scope_ref: "src/lib.rs".to_string(),
            category: "lint".to_string(),
        }];
        let seeds = crystallize_candidates(&findings, &existing, 3);
        assert_eq!(seeds.len(), 1);
        assert_eq!(seeds[0].category, "security");
    }

    #[test]
    fn crystallize_candidates_existing_rule_in_different_scope_ref_does_not_block() {
        let findings = vec![finding(5, "lint", "a")];
        let existing = vec![ExistingRuleBucket {
            scope_kind: "path".to_string(),
            scope_ref: "src/other.rs".to_string(),
            category: "lint".to_string(),
        }];
        let seeds = crystallize_candidates(&findings, &existing, 3);
        assert_eq!(seeds.len(), 1);
    }

    // ── min_occurrences_default ─────────────────────────────────────────────

    #[test]
    #[serial]
    fn min_occurrences_default_falls_back_when_unset() {
        if std::env::var("KGRULE_CRYSTALLIZE_MIN_OCCURRENCES").is_ok() {
            return;
        }
        assert_eq!(min_occurrences_default(), DEFAULT_MIN_OCCURRENCES);
    }

    // ── parse_scope_kind ─────────────────────────────────────────────────────

    #[test]
    fn parse_scope_kind_roundtrips_known_values() {
        assert_eq!(parse_scope_kind("node"), ScopeKind::Node);
        assert_eq!(parse_scope_kind("path"), ScopeKind::Path);
        assert_eq!(parse_scope_kind("community"), ScopeKind::Community);
        assert_eq!(parse_scope_kind("global"), ScopeKind::Global);
    }

    #[test]
    fn parse_scope_kind_defaults_to_path_for_unknown() {
        assert_eq!(parse_scope_kind("bogus"), ScopeKind::Path);
    }

    // ── kg_rule_crystallize tool: degrade ────────────────────────────────────

    #[tokio::test]
    #[serial]
    async fn crystallize_unconfigured_store_degrades_not_errors() {
        if std::env::var("ATLAS_DATABASE_URL").is_ok() {
            return;
        }
        let out = KgRuleCrystallize
            .execute_structured(json!({"project_id": "TERM"}))
            .await
            .unwrap();
        let v = out.structured.expect("structured payload");
        assert_eq!(v["configured"], false);
    }

    #[tokio::test]
    #[serial]
    async fn crystallize_missing_project_id_is_invalid_argument() {
        let err = KgRuleCrystallize
            .execute_structured(json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }
}
