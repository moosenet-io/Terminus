//! S84 Assistant Intake Profiling — foundation (ASMT-01).
//!
//! This module is the BASE that all six dimension runners (ASMT-02..07) and the
//! consolidated runner (ASMT-09) write through. It owns:
//!   - the assistant-profile DB schema + idempotent migration ([`schema`]),
//!   - the 3-judge panel harness ([`judges`]) that scores a model's output on a
//!     set of traits via provider OAuth CLIs,
//!   - the shared types every dimension runner uses ([`ModelId`],
//!     [`DimensionScore`], [`PanelResult`], [`BackendTag`]).
//!
//! ## Model identity (CRITICAL — byte-identical to S83/MINT)
//! S83/MINT does NOT normalize model names: it stores the model name verbatim as
//! `model_profiles.model_name` (the same string used as the chord
//! model-registry KEY, e.g. `"qwen3:8b"`). To stay byte-identical we REUSE that
//! exact string as our [`ModelId`] — see [`ModelId::from_registry_key`], which is
//! a pass-through (no lowering, no trimming) precisely because S83 does none.
//! Inventing a new normalization here would silently break the
//! `model_dual_profile` join.
//!
//! ## Backend tag (`gpu` | `cpu`)
//! The hardware a model was profiled on is S83/P5's `ResolvedBackend.hardware`
//! (`infer::resolve_backend`), serialized lowercase as `"gpu"` / `"cpu"`. We
//! carry the same two-value tag so a model profiled on both devices yields two
//! distinct assistant rows, matching the both-hardware sizing comparison.

pub mod acquire;
pub mod dim1_conversation;
pub mod dim5_prompted;
pub mod dim2_toolchain;
pub mod dim3_memory;
pub mod dim4_ocean;
pub mod dim6_embeddings;
pub mod dim7_yarn_depth;
pub mod fleet;
pub mod judges;
pub mod reporting;
pub mod runner;
pub mod schema;

use std::fmt;

/// A model identifier, byte-identical to what S83/MINT stores in
/// `model_profiles.model_name` (which is the chord registry key, verbatim).
///
/// We intentionally do NOT normalize: S83 stores the raw name, so any divergence
/// (lowercasing, trimming, tag stripping) would break the `model_dual_profile`
/// join. Construct from the registry key / S83 model_name with
/// [`ModelId::from_registry_key`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ModelId(String);

impl ModelId {
    /// Build from the chord model-registry key (== S83 `model_profiles.model_name`).
    ///
    /// Pass-through by design: S83 performs NO normalization, so neither do we.
    pub fn from_registry_key(key: impl Into<String>) -> Self {
        ModelId(key.into())
    }

    /// The raw string, exactly as stored by S83.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume into the inner `String`.
    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Display for ModelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for ModelId {
    fn from(s: &str) -> Self {
        ModelId::from_registry_key(s)
    }
}

impl From<String> for ModelId {
    fn from(s: String) -> Self {
        ModelId::from_registry_key(s)
    }
}

/// Hardware the model was profiled on, matching S83/P5 `ResolvedBackend.hardware`
/// (`"gpu"` | `"cpu"`). Stored as the `backend_tag` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BackendTag {
    Gpu,
    Cpu,
}

impl BackendTag {
    /// Lowercase wire string, byte-identical to P5's `Hardware` serde.
    pub fn as_str(self) -> &'static str {
        match self {
            BackendTag::Gpu => "gpu",
            BackendTag::Cpu => "cpu",
        }
    }

    /// Parse the lowercase tag P5 emits (`"gpu"` / `"cpu"`). Anything else ⇒ None.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "gpu" => Some(BackendTag::Gpu),
            "cpu" => Some(BackendTag::Cpu),
            _ => None,
        }
    }
}

impl fmt::Display for BackendTag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One aggregated score for a single (dimension, trait/metric) of a model on a
/// backend — the unit written into `assistant_dimension_score` by ASMT-02..07.
///
/// Built from a [`PanelResult`] via [`PanelResult::into_dimension_scores`], or
/// directly by a runner that computes a non-judge metric (e.g. a latency number).
#[derive(Debug, Clone, PartialEq)]
pub struct DimensionScore {
    /// Model under test (S83-identical id).
    pub model_id: ModelId,
    /// Hardware tag the score was measured on.
    pub backend_tag: BackendTag,
    /// Dimension this metric belongs to (e.g. `"instruction_following"`).
    pub dimension: String,
    /// The specific metric / trait within the dimension (e.g. `"concision"`).
    pub metric: String,
    /// Aggregated value. For judge traits this is the mean over complying judges.
    pub value: f64,
    /// Sample standard deviation across complying judges; `None` when only one
    /// judge complied (low confidence) or the metric isn't judge-derived.
    pub std_dev: Option<f64>,
    /// Which judge produced this — `"panel"` for an aggregated judge metric, or a
    /// single judge id when only one complied, or a non-judge source label.
    pub judge: String,
    /// True when only one judge complied (mean over n=1, SD undefined).
    pub low_confidence: bool,
    /// Optional raw audit JSON (redacted). `None` for non-judge metrics.
    pub raw_json: Option<String>,
}

/// Per-judge outcome for one panel invocation.
#[derive(Debug, Clone, PartialEq)]
pub enum JudgeOutcome {
    /// Judge returned a contract-valid trait map.
    Scored {
        judge: String,
        /// trait -> integer score in [1,5].
        traits: std::collections::BTreeMap<String, i64>,
    },
    /// Judge failed the contract twice, errored, or hit an auth failure. Excluded
    /// from aggregation. `raw` holds redacted output for audit.
    Abstained {
        judge: String,
        reason: String,
        raw: Option<String>,
    },
}

impl JudgeOutcome {
    pub fn judge_id(&self) -> &str {
        match self {
            JudgeOutcome::Scored { judge, .. } => judge,
            JudgeOutcome::Abstained { judge, .. } => judge,
        }
    }

    pub fn complied(&self) -> bool {
        matches!(self, JudgeOutcome::Scored { .. })
    }
}

/// Result of running the full panel over one item (one model output + one trait
/// set). The aggregation contract:
///   - per-trait mean + sample SD over the COMPLYING judges (n = 2 or 3),
///   - n == 1 ⇒ value kept, `low_confidence = true`, SD = `None`,
///   - n == 0 ⇒ item `unscored` (see [`PanelResult::aggregate`]).
#[derive(Debug, Clone, PartialEq)]
pub struct PanelResult {
    /// Dimension being scored (for downstream `DimensionScore` rows).
    pub dimension: String,
    /// Per-judge outcomes (always 3 entries: one per provider in panel order).
    pub outcomes: Vec<JudgeOutcome>,
    /// Per-trait aggregate over complying judges. Empty ⇒ item unscored.
    pub aggregates: std::collections::BTreeMap<String, TraitAggregate>,
    /// Number of judges that complied (0..=3).
    pub complying: usize,
    /// Set when the whole item is unscored (all judges abstained), with the
    /// combined reason.
    pub unscored_reason: Option<String>,
    /// Operator warnings emitted during the run (e.g. auth failures), one per
    /// affected judge. Never causes a crash.
    pub warnings: Vec<String>,
}

/// Aggregate for a single trait across complying judges.
#[derive(Debug, Clone, PartialEq)]
pub struct TraitAggregate {
    /// Mean over complying judges.
    pub mean: f64,
    /// Sample standard deviation (n-1 denominator); `None` when n == 1.
    pub std_dev: Option<f64>,
    /// Number of judges contributing to this trait.
    pub n: usize,
    /// True when n == 1 (mean over a single judge).
    pub low_confidence: bool,
}

impl PanelResult {
    /// True when no judge complied — the item is `unscored`.
    pub fn is_unscored(&self) -> bool {
        self.unscored_reason.is_some()
    }

    /// Flatten the per-trait aggregates into storage-ready [`DimensionScore`]
    /// rows for one (model, backend). `raw_json` carries a redacted audit blob of
    /// the per-judge outcomes so a row is auditable without re-running the panel.
    pub fn into_dimension_scores(
        &self,
        model_id: &ModelId,
        backend_tag: BackendTag,
    ) -> Vec<DimensionScore> {
        let raw = self.audit_json();
        self.aggregates
            .iter()
            .map(|(trait_name, agg)| DimensionScore {
                model_id: model_id.clone(),
                backend_tag,
                dimension: self.dimension.clone(),
                metric: trait_name.clone(),
                value: agg.mean,
                std_dev: agg.std_dev,
                judge: if agg.n == 1 {
                    // single complying judge → name it for attribution
                    self.outcomes
                        .iter()
                        .find(|o| o.complied())
                        .map(|o| o.judge_id().to_string())
                        .unwrap_or_else(|| "panel".to_string())
                } else {
                    "panel".to_string()
                },
                low_confidence: agg.low_confidence,
                raw_json: Some(raw.clone()),
            })
            .collect()
    }

    /// Redacted audit JSON of all per-judge outcomes (no prompts, no secrets;
    /// abstain raw output is already redacted upstream).
    pub fn audit_json(&self) -> String {
        let outcomes: Vec<serde_json::Value> = self
            .outcomes
            .iter()
            .map(|o| match o {
                JudgeOutcome::Scored { judge, traits } => serde_json::json!({
                    "judge": judge,
                    "status": "scored",
                    "traits": traits,
                }),
                JudgeOutcome::Abstained { judge, reason, raw } => serde_json::json!({
                    "judge": judge,
                    "status": "abstained",
                    "reason": reason,
                    "raw": raw,
                }),
            })
            .collect();
        serde_json::json!({
            "dimension": self.dimension,
            "complying": self.complying,
            "unscored_reason": self.unscored_reason,
            "outcomes": outcomes,
        })
        .to_string()
    }

    /// Aggregate per-judge outcomes into the trait mean + sample SD contract.
    ///
    /// `traits_expected` is the ordered set of traits the prompt asked for; a
    /// trait missing from a judge's compliant output simply isn't counted for
    /// that judge (the validator already guarantees required keys, so this is
    /// defensive). Returns a fully-populated [`PanelResult`].
    pub fn aggregate(
        dimension: impl Into<String>,
        outcomes: Vec<JudgeOutcome>,
        warnings: Vec<String>,
    ) -> PanelResult {
        let dimension = dimension.into();
        let complying: Vec<&JudgeOutcome> = outcomes.iter().filter(|o| o.complied()).collect();
        let n_complying = complying.len();

        if n_complying == 0 {
            let reasons: Vec<String> = outcomes
                .iter()
                .filter_map(|o| match o {
                    JudgeOutcome::Abstained { judge, reason, .. } => {
                        Some(format!("{judge}: {reason}"))
                    }
                    _ => None,
                })
                .collect();
            return PanelResult {
                dimension,
                aggregates: std::collections::BTreeMap::new(),
                complying: 0,
                unscored_reason: Some(format!("all judges abstained ({})", reasons.join("; "))),
                outcomes,
                warnings,
            };
        }

        // Collect every trait any complying judge scored.
        let mut by_trait: std::collections::BTreeMap<String, Vec<f64>> =
            std::collections::BTreeMap::new();
        for o in &complying {
            if let JudgeOutcome::Scored { traits, .. } = o {
                for (k, v) in traits {
                    by_trait.entry(k.clone()).or_default().push(*v as f64);
                }
            }
        }

        let mut aggregates = std::collections::BTreeMap::new();
        for (trait_name, values) in by_trait {
            let (mean, sd) = mean_and_sample_sd(&values);
            let n = values.len();
            aggregates.insert(
                trait_name,
                TraitAggregate {
                    mean,
                    std_dev: sd,
                    n,
                    low_confidence: n == 1,
                },
            );
        }

        PanelResult {
            dimension,
            aggregates,
            complying: n_complying,
            unscored_reason: None,
            outcomes,
            warnings,
        }
    }
}

/// Mean and **sample** standard deviation (n-1 denominator) of `values`.
/// Returns `(mean, None)` when `values.len() <= 1` (SD undefined for n=1).
/// Panics-free: empty input returns `(0.0, None)`.
pub fn mean_and_sample_sd(values: &[f64]) -> (f64, Option<f64>) {
    let n = values.len();
    if n == 0 {
        return (0.0, None);
    }
    let mean = values.iter().sum::<f64>() / n as f64;
    if n == 1 {
        return (mean, None);
    }
    let var = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n as f64 - 1.0);
    (mean, Some(var.sqrt()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn scored(judge: &str, pairs: &[(&str, i64)]) -> JudgeOutcome {
        let traits = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), *v))
            .collect::<BTreeMap<_, _>>();
        JudgeOutcome::Scored {
            judge: judge.to_string(),
            traits,
        }
    }

    fn abstained(judge: &str) -> JudgeOutcome {
        JudgeOutcome::Abstained {
            judge: judge.to_string(),
            reason: "invalid json (x2)".to_string(),
            raw: Some("[redacted]".to_string()),
        }
    }

    #[test]
    fn model_id_is_pass_through_byte_identical() {
        // S83 stores the raw registry key — we must NOT normalize.
        for raw in ["qwen3:8b", "Qwen3:8B", "  spaced  ", "gpt-oss:20b"] {
            assert_eq!(ModelId::from_registry_key(raw).as_str(), raw);
        }
    }

    #[test]
    fn backend_tag_round_trips_p5_wire() {
        assert_eq!(BackendTag::Gpu.as_str(), "gpu");
        assert_eq!(BackendTag::Cpu.as_str(), "cpu");
        assert_eq!(BackendTag::parse("gpu"), Some(BackendTag::Gpu));
        assert_eq!(BackendTag::parse("cpu"), Some(BackendTag::Cpu));
        assert_eq!(BackendTag::parse("tpu"), None);
    }

    #[test]
    fn mean_sd_three_judges() {
        // Spec: judges [3,4,5] → mean 4.0, SD 1.0.
        let (mean, sd) = mean_and_sample_sd(&[3.0, 4.0, 5.0]);
        assert!((mean - 4.0).abs() < 1e-9);
        assert!((sd.unwrap() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn mean_sd_two_judges() {
        // n=2: [3,5] → mean 4.0, SD sqrt(((1)^2+(1)^2)/1) = sqrt(2).
        let (mean, sd) = mean_and_sample_sd(&[3.0, 5.0]);
        assert!((mean - 4.0).abs() < 1e-9);
        assert!((sd.unwrap() - 2f64.sqrt()).abs() < 1e-9);
    }

    #[test]
    fn mean_sd_one_judge_is_low_confidence() {
        // n=1: value kept, SD None.
        let (mean, sd) = mean_and_sample_sd(&[4.0]);
        assert_eq!(mean, 4.0);
        assert_eq!(sd, None);
    }

    #[test]
    fn aggregate_three_complying() {
        let outcomes = vec![
            scored("claude", &[("clarity", 3), ("tone", 4)]),
            scored("gemini", &[("clarity", 4), ("tone", 4)]),
            scored("codex", &[("clarity", 5), ("tone", 4)]),
        ];
        let pr = PanelResult::aggregate("instruction_following", outcomes, vec![]);
        assert_eq!(pr.complying, 3);
        assert!(!pr.is_unscored());
        let clarity = &pr.aggregates["clarity"];
        assert!((clarity.mean - 4.0).abs() < 1e-9);
        assert!((clarity.std_dev.unwrap() - 1.0).abs() < 1e-9);
        assert_eq!(clarity.n, 3);
        assert!(!clarity.low_confidence);
        // tone all 4 → SD 0.0
        assert!((pr.aggregates["tone"].std_dev.unwrap() - 0.0).abs() < 1e-9);
    }

    #[test]
    fn aggregate_one_complying_is_low_confidence() {
        let outcomes = vec![
            scored("claude", &[("clarity", 4)]),
            abstained("gemini"),
            abstained("codex"),
        ];
        let pr = PanelResult::aggregate("tone", outcomes, vec![]);
        assert_eq!(pr.complying, 1);
        let agg = &pr.aggregates["clarity"];
        assert_eq!(agg.mean, 4.0);
        assert_eq!(agg.std_dev, None);
        assert!(agg.low_confidence);
        // attribution names the single judge, not "panel"
        let scores = pr.into_dimension_scores(&ModelId::from("qwen3:8b"), BackendTag::Cpu);
        assert_eq!(scores.len(), 1);
        assert_eq!(scores[0].judge, "claude");
        assert!(scores[0].low_confidence);
        assert_eq!(scores[0].std_dev, None);
    }

    #[test]
    fn aggregate_all_abstain_is_unscored() {
        let outcomes = vec![abstained("claude"), abstained("gemini"), abstained("codex")];
        let pr = PanelResult::aggregate("creativity", outcomes, vec![]);
        assert_eq!(pr.complying, 0);
        assert!(pr.is_unscored());
        assert!(pr.aggregates.is_empty());
        assert!(pr.unscored_reason.as_ref().unwrap().contains("all judges abstained"));
        // no rows produced for an unscored item
        assert!(pr
            .into_dimension_scores(&ModelId::from("m"), BackendTag::Gpu)
            .is_empty());
    }

    #[test]
    fn into_dimension_scores_uses_panel_label_for_multi() {
        let outcomes = vec![
            scored("claude", &[("clarity", 3)]),
            scored("gemini", &[("clarity", 5)]),
        ];
        let pr = PanelResult::aggregate("tone", outcomes, vec![]);
        let scores = pr.into_dimension_scores(&ModelId::from("qwen3:8b"), BackendTag::Gpu);
        assert_eq!(scores[0].judge, "panel");
        assert_eq!(scores[0].dimension, "tone");
        assert_eq!(scores[0].metric, "clarity");
        assert_eq!(scores[0].backend_tag, BackendTag::Gpu);
        assert!(scores[0].raw_json.is_some());
    }
}
