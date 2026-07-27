//! Ask-4 Phase 1 (brochure → assistant sweep bridge): a PURE candidate
//! SELECTOR plus a NOMINATION BRIDGE that lets high-potential discovered models
//! flow into the existing MINT assistant sweep with no rewrite of the sweep
//! itself.
//!
//! ## What this is (and is NOT)
//! This module is the read/rank/map half of the "auto pull→test→promote" loop:
//!   1. [`select_discovery_candidates`] ranks brochure rows
//!      ([`DiscoveryCandidate`], from [`super::storage::read_brochure`]) into an
//!      ORDERED, capped shortlist of assistant-relevant models — pure and
//!      deterministic, mirroring `assistant::runner::select_gap_models`'s
//!      "data-in / data-out, unit-testable without a DB" style.
//!   2. [`nominations_from_selected`] maps each selected candidate into the
//!      runtime [`Nomination`] the existing `AssistantSweepRunner` already
//!      consumes, so the sweep needs no new code path to acquire/measure a
//!      brochure-sourced model.
//!   3. [`merge_discovery_nominations`] folds those synthesized nominations into
//!      the curated `nominations.json` set the sweep already reads (INTEGRATION
//!      OPTION i — see the run-path comment in `assistant::runner::run_mode`),
//!      curated entries winning any id collision.
//!
//! ## Phase-1 BOUNDARY (deliberate, do not "fix" here)
//! This phase performs NO internet pull and touches NO new secrets. It only
//! reads the already-persisted brochure and hands ids to the EXISTING acquire
//! path. If a selected candidate is not already in Chord cold storage, the
//! existing `ShellAcquirer`/`chord_pull` path resolves it to a clean
//! `Skipped`/`NonViable` (fail-soft) — the sweep records the skip and moves on.
//! HF → cold-storage INGESTION (making an un-stored candidate acquirable) is
//! explicitly DEFERRED to Phase 2. See the TODO in [`nominations_from_selected`].
//!
//! ## Size floor rationale
//! The default min-size floor is ≥ 7B so the selector never spends a scarce
//! sweep slot on a model the DOWNSTREAM Chord dynamic proxy would reject anyway:
//! the proxy auto-promotes the top assistant score only above a hard, un-
//! disable-able 5GB size gate (`CHORD_ALIAS_MIN_SIZE_BYTES`). A 7B model at
//! Q4-class weights is comfortably above that 5GB gate, so nothing selected here
//! can ever be "swept but un-promotable purely on size". The floor is env-
//! tunable UP (never silently below the intent) via
//! `INTAKE_ASSISTANT_DISCOVERY_MIN_SIZE_B`.

use std::collections::BTreeSet;

use super::schema::{DiscoveryCandidate, FleetCategory, Modality};
use crate::intake::assistant::acquire::{AcquisitionPath, Gfx1151Class, Nomination};

/// Default per-run cap on brochure-selected candidates. Deliberately small — a
/// brochure sweep should trickle a few high-signal models into the fleet per
/// window, not bulk-enqueue the whole discovery table. Env: `INTAKE_ASSISTANT_DISCOVERY_MAX`.
pub const DEFAULT_DISCOVERY_MAX: usize = 5;

/// Default minimum parameter size (billions) a candidate must have to be
/// selected. ≥ the Chord dynamic-proxy 5GB promotion gate (see the module doc).
/// Env: `INTAKE_ASSISTANT_DISCOVERY_MIN_SIZE_B`.
pub const DEFAULT_DISCOVERY_MIN_SIZE_B: f64 = 7.0;

/// Configuration for [`select_discovery_candidates`]. Every field has a safe,
/// conservative default (see [`DiscoverySelectConfig::default`]); [`DiscoverySelectConfig::from_env`]
/// overlays the env-tunable knobs. Pure data — no I/O.
#[derive(Debug, Clone)]
pub struct DiscoverySelectConfig {
    /// Max candidates returned (top-N by score). Default [`DEFAULT_DISCOVERY_MAX`].
    pub top_n: usize,
    /// Minimum `size_b` a candidate must have. A candidate with `None`/`< floor`
    /// size is DROPPED. Default [`DEFAULT_DISCOVERY_MIN_SIZE_B`].
    pub min_size_b: f64,
    /// Coarse discovery-listing roles considered assistant-relevant. Default
    /// `{Assistant}`.
    pub allowed_categories: BTreeSet<FleetCategory>,
    /// Profiling modalities considered assistant-relevant. Default
    /// `{TextGeneration}` (a plain chat/coder/writer LLM — what the assistant
    /// seven-dimension suite measures). Specialized modalities (embedding, vlm,
    /// tts, …) belong to OTHER sweeps and are excluded.
    pub allowed_modalities: BTreeSet<Modality>,
    /// Whether a candidate whose `modality` is `None` (unclassified) but whose
    /// category IS allowed is kept. Default `true`: an assistant-category listing
    /// with no modality signal is a plausible text LLM, and the downstream smoke
    /// gate + proxy quality floor still protect against a bad pick.
    pub allow_unclassified_modality: bool,
    /// gfx1151 runnability classes (`gfx1151_class` strings) allowed. Default
    /// `{"confirmed","experimental"}`. `"unknown"` is added only when
    /// [`allow_unknown_gfx`](Self::allow_unknown_gfx) is set.
    pub allowed_gfx_classes: BTreeSet<String>,
    /// Whether the `"unknown"` gfx1151 class is allowed. Default `false` — an
    /// unknown-runnability model is not auto-selected unless explicitly opted in
    /// via `INTAKE_ASSISTANT_DISCOVERY_ALLOW_UNKNOWN_GFX`.
    pub allow_unknown_gfx: bool,
}

impl Default for DiscoverySelectConfig {
    fn default() -> Self {
        let mut allowed_categories = BTreeSet::new();
        allowed_categories.insert(FleetCategory::Assistant);
        let mut allowed_modalities = BTreeSet::new();
        allowed_modalities.insert(Modality::TextGeneration);
        let mut allowed_gfx_classes = BTreeSet::new();
        allowed_gfx_classes.insert("confirmed".to_string());
        allowed_gfx_classes.insert("experimental".to_string());
        DiscoverySelectConfig {
            top_n: DEFAULT_DISCOVERY_MAX,
            min_size_b: DEFAULT_DISCOVERY_MIN_SIZE_B,
            allowed_categories,
            allowed_modalities,
            allow_unclassified_modality: true,
            allowed_gfx_classes,
            allow_unknown_gfx: false,
        }
    }
}

impl DiscoverySelectConfig {
    /// Overlay the env-tunable knobs on the conservative [`Default`]. Only the
    /// numeric cap/floor and the unknown-gfx opt-in are env-driven; the
    /// category/modality allowlists stay code-defined (they encode WHICH sweep
    /// this is, not an operator tuning knob). None of these values is secret-
    /// shaped, so plain `std::env::var` is correct here (matches
    /// `acquire::vram_ceiling_gb` / `intake::gap_max_from_env`); no
    /// `SecretManager` involvement, per S7.
    pub fn from_env() -> Self {
        let mut cfg = DiscoverySelectConfig::default();
        cfg.top_n = parse_discovery_max(
            std::env::var("INTAKE_ASSISTANT_DISCOVERY_MAX")
                .ok()
                .as_deref(),
        );
        cfg.min_size_b = parse_discovery_min_size_b(
            std::env::var("INTAKE_ASSISTANT_DISCOVERY_MIN_SIZE_B")
                .ok()
                .as_deref(),
        );
        cfg.allow_unknown_gfx = crate::intake::parse_only_stale(
            std::env::var("INTAKE_ASSISTANT_DISCOVERY_ALLOW_UNKNOWN_GFX")
                .ok()
                .as_deref(),
        );
        if cfg.allow_unknown_gfx {
            cfg.allowed_gfx_classes.insert("unknown".to_string());
        }
        cfg
    }

    /// The effective gfx-class allowlist, including `"unknown"` iff opted in.
    fn effective_gfx_classes(&self) -> BTreeSet<String> {
        let mut set = self.allowed_gfx_classes.clone();
        if self.allow_unknown_gfx {
            set.insert("unknown".to_string());
        }
        set
    }
}

/// Parse the discovery per-run cap (`INTAKE_ASSISTANT_DISCOVERY_MAX`). Clamped
/// to at least `1` (a cap of `0`/negative would select nothing); a missing/
/// unparseable value falls back to [`DEFAULT_DISCOVERY_MAX`]. Pure over input.
pub fn parse_discovery_max(raw: Option<&str>) -> usize {
    raw.and_then(|s| s.trim().parse::<i64>().ok())
        .filter(|n| *n >= 1)
        .map(|n| n as usize)
        .unwrap_or(DEFAULT_DISCOVERY_MAX)
}

/// Parse the discovery size floor (`INTAKE_ASSISTANT_DISCOVERY_MIN_SIZE_B`).
/// A missing/unparseable/non-positive value falls back to
/// [`DEFAULT_DISCOVERY_MIN_SIZE_B`] — the floor can be tuned UP but never
/// silently to zero (which would let a tiny, proxy-rejectable model through).
/// Pure over input.
pub fn parse_discovery_min_size_b(raw: Option<&str>) -> f64 {
    raw.and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|n| n.is_finite() && *n > 0.0)
        .unwrap_or(DEFAULT_DISCOVERY_MIN_SIZE_B)
}

/// PURELY rank the brochure into an ORDERED, capped shortlist of assistant-
/// relevant candidates. Deterministic; no I/O. Mirrors
/// `assistant::runner::select_gap_models`'s data-in/data-out contract.
///
/// Filtering (a candidate must pass ALL to be considered):
///   1. `category` ∈ `cfg.allowed_categories`;
///   2. `modality` ∈ `cfg.allowed_modalities`, OR (`modality` is `None` AND
///      `cfg.allow_unclassified_modality`);
///   3. `size_b` is `Some(v)` with `v >= cfg.min_size_b` (a `None`/too-small
///      size is DROPPED — the module-doc size-floor invariant vs the proxy gate);
///   4. `gfx1151_class` ∈ the effective gfx allowlist (`"unknown"` only if
///      opted in).
///
/// Ranking: `discovery_score` DESC (a `None`/NaN score sorts LAST), stable
/// tiebreak by `model_name` ASC. Then the top `cfg.top_n` are returned.
pub fn select_discovery_candidates(
    candidates: Vec<DiscoveryCandidate>,
    cfg: &DiscoverySelectConfig,
) -> Vec<DiscoveryCandidate> {
    let gfx_allow = cfg.effective_gfx_classes();

    let mut kept: Vec<DiscoveryCandidate> = candidates
        .into_iter()
        .filter(|c| cfg.allowed_categories.contains(&c.category))
        .filter(|c| match c.modality {
            Some(m) => cfg.allowed_modalities.contains(&m),
            None => cfg.allow_unclassified_modality,
        })
        .filter(|c| matches!(c.size_b, Some(v) if v >= cfg.min_size_b))
        .filter(|c| gfx_allow.contains(&c.gfx1151_class))
        .collect();

    // Score DESC (None/NaN last), stable tiebreak model_name ASC. A NaN score is
    // treated as "no usable score" so it can never sort ABOVE a real one.
    kept.sort_by(|a, b| {
        let sb = score_key(b.discovery_score);
        let sa = score_key(a.discovery_score);
        sb.partial_cmp(&sa)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.model_name.cmp(&b.model_name))
    });

    kept.truncate(cfg.top_n);
    kept
}

/// Sort key for `discovery_score`: a real finite score maps to itself; `None`
/// or a NaN maps to negative infinity so it sorts LAST under a DESC compare.
fn score_key(score: Option<f64>) -> f64 {
    match score {
        Some(v) if v.is_finite() => v,
        _ => f64::NEG_INFINITY,
    }
}

/// Map a brochure `gfx1151_class` string into the runtime [`Gfx1151Class`].
/// Unlike the schema enums' strict `from_str`, an unrecognized value here maps
/// to [`Gfx1151Class::Unknown`] (a SAFE conservative default: the sweep runner
/// treats `Unknown` as "run the bounded smoke test before committing the full
/// suite", so a mis-tagged class can never over-claim runnability). Selection
/// has already gated on the ALLOWED class strings, so in practice this only ever
/// sees `"confirmed"`/`"experimental"`/(opt-in) `"unknown"`.
fn map_gfx_class(s: &str) -> Gfx1151Class {
    match s {
        "confirmed" => Gfx1151Class::Confirmed,
        "experimental" => Gfx1151Class::Experimental,
        _ => Gfx1151Class::Unknown,
    }
}

/// NOMINATION BRIDGE: turn selected brochure candidates into the runtime
/// [`Nomination`] records the EXISTING `AssistantSweepRunner` consumes — no
/// change to the sweep. Deterministic; preserves input order (already ranked).
///
/// Mapping:
///   - `id`             ← `model_name` (byte-identical; the chord-registry /
///                        fleet-catalog join key, per `acquire.rs`);
///   - `size_b`         ← `size_b` (always `Some` post-selection; defensively 0.0
///                        if absent);
///   - `gfx1151_class`  ← [`map_gfx_class`] of the candidate's class string;
///   - `acquisition`    ← `HfFetch` when an `hf_repo` is present, else
///                        `OllamaPull`. NB (ACQ-01): BOTH tags route through
///                        Chord's cold-storage promotion (`chord_pull`), NOT the
///                        internet — the tag is descriptive, not a fetch mode;
///   - `hf_repo`        ← the candidate's `hf_repo` (audit/Phase-2 metadata);
///   - `backends`       ← empty ⇒ derived from the gfx class by
///                        `Nomination::backend_strategy` (GPU-then-CPU) — the
///                        same default a hand-authored nomination gets;
///   - `rationale`      ← `"auto-selected from brochure discovery_score=…"`.
///
/// TODO(Phase 2 — HF→cold-storage ingestion): a candidate whose weights are not
/// yet in Chord cold storage will fail-soft here (the existing acquire path logs
/// it `Skipped`/`NonViable`). Phase 2 adds the internet pull that stages an
/// un-stored candidate into cold storage BEFORE the sweep runs; this Phase-1
/// bridge deliberately does NOT pull.
pub fn nominations_from_selected(selected: &[DiscoveryCandidate]) -> Vec<Nomination> {
    selected
        .iter()
        .map(|c| {
            let hf_repo = if c.hf_repo.trim().is_empty() {
                None
            } else {
                Some(c.hf_repo.clone())
            };
            let acquisition = if hf_repo.is_some() {
                AcquisitionPath::HfFetch
            } else {
                AcquisitionPath::OllamaPull
            };
            let rationale = match c.discovery_score {
                Some(v) if v.is_finite() => {
                    format!("auto-selected from brochure discovery_score={v:.1}")
                }
                _ => "auto-selected from brochure discovery_score=none".to_string(),
            };
            Nomination {
                id: c.model_name.clone(),
                size_b: c.size_b.unwrap_or(0.0),
                gfx1151_class: map_gfx_class(&c.gfx1151_class),
                acquisition,
                yarn_capable: false,
                yarn: None,
                hf_repo,
                backends: Vec::new(),
                rationale,
            }
        })
        .collect()
}

/// Fold synthesized discovery nominations into the curated set the sweep reads.
/// CURATED entries win any id collision (author priority is authoritative — a
/// hand-tuned nomination is never overridden by an auto-selected one), and
/// curated order is preserved at the FRONT; new discovered nominations are
/// appended after, in their (already ranked) order, skipping any id already
/// present. Pure — unit-testable without a DB.
pub fn merge_discovery_nominations(
    curated: Vec<Nomination>,
    discovered: Vec<Nomination>,
) -> Vec<Nomination> {
    let existing: BTreeSet<String> = curated.iter().map(|n| n.id.clone()).collect();
    let mut out = curated;
    let mut appended: BTreeSet<String> = BTreeSet::new();
    for n in discovered {
        if existing.contains(&n.id) || !appended.insert(n.id.clone()) {
            continue;
        }
        out.push(n);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    /// Minimal candidate builder — assistant category, text-generation modality,
    /// confirmed gfx, a given size and score.
    fn cand(
        name: &str,
        size_b: Option<f64>,
        score: Option<f64>,
        category: FleetCategory,
        modality: Option<Modality>,
        gfx: &str,
    ) -> DiscoveryCandidate {
        DiscoveryCandidate {
            model_name: name.to_string(),
            hf_repo: format!("org/{name}"),
            category,
            status: super::super::schema::CandidateStatus::ColdStored,
            modality,
            gfx1151_class: gfx.to_string(),
            size_b,
            vram_footprint_gb: None,
            discovery_source: "hf_trending".to_string(),
            discovery_score: score,
            discovered_at: Utc::now(),
            last_seen_at: Utc::now(),
            fetched_at: None,
            marked_for_fleet_at: None,
            evicted_at: None,
            retained_profile: None,
            rationale: None,
        }
    }

    /// A plain assistant/text candidate with the common fields defaulted.
    fn asst(name: &str, size_b: f64, score: f64) -> DiscoveryCandidate {
        cand(
            name,
            Some(size_b),
            Some(score),
            FleetCategory::Assistant,
            Some(Modality::TextGeneration),
            "confirmed",
        )
    }

    #[test]
    fn empty_input_selects_nothing() {
        let out = select_discovery_candidates(vec![], &DiscoverySelectConfig::default());
        assert!(out.is_empty());
    }

    #[test]
    fn ranks_by_score_desc_and_caps_top_n() {
        let cands = vec![
            asst("a", 8.0, 10.0),
            asst("b", 8.0, 90.0),
            asst("c", 8.0, 50.0),
            asst("d", 8.0, 70.0),
        ];
        let mut cfg = DiscoverySelectConfig::default();
        cfg.top_n = 2;
        let out = select_discovery_candidates(cands, &cfg);
        let ids: Vec<&str> = out.iter().map(|c| c.model_name.as_str()).collect();
        assert_eq!(ids, vec!["b", "d"], "top-2 by score desc");
    }

    #[test]
    fn ties_break_by_model_name_asc_deterministically() {
        // Same score → stable, deterministic model_name ASC ordering.
        let cands = vec![
            asst("zebra", 8.0, 42.0),
            asst("alpha", 8.0, 42.0),
            asst("mike", 8.0, 42.0),
        ];
        let out = select_discovery_candidates(cands, &DiscoverySelectConfig::default());
        let ids: Vec<&str> = out.iter().map(|c| c.model_name.as_str()).collect();
        assert_eq!(ids, vec!["alpha", "mike", "zebra"]);
    }

    #[test]
    fn size_floor_boundary_is_inclusive_and_drops_below() {
        // Default floor 7.0: exactly 7.0 kept, 6.999 dropped, None dropped.
        let cands = vec![
            asst("exactly_floor", 7.0, 50.0),
            asst("just_below", 6.999, 99.0),
            cand(
                "no_size",
                None,
                Some(99.0),
                FleetCategory::Assistant,
                Some(Modality::TextGeneration),
                "confirmed",
            ),
        ];
        let out = select_discovery_candidates(cands, &DiscoverySelectConfig::default());
        let ids: Vec<&str> = out.iter().map(|c| c.model_name.as_str()).collect();
        assert_eq!(ids, vec!["exactly_floor"]);
    }

    #[test]
    fn filters_out_non_assistant_categories_and_specialized_modalities() {
        let cands = vec![
            asst("keep_me", 8.0, 50.0),
            cand(
                "an_embedder",
                Some(8.0),
                Some(99.0),
                FleetCategory::Embedding,
                Some(Modality::Embedding),
                "confirmed",
            ),
            cand(
                "a_coder_by_category",
                Some(8.0),
                Some(99.0),
                FleetCategory::Coder,
                Some(Modality::TextGeneration),
                "confirmed",
            ),
            cand(
                "assistant_but_vlm",
                Some(8.0),
                Some(99.0),
                FleetCategory::Assistant,
                Some(Modality::Vlm),
                "confirmed",
            ),
        ];
        let out = select_discovery_candidates(cands, &DiscoverySelectConfig::default());
        let ids: Vec<&str> = out.iter().map(|c| c.model_name.as_str()).collect();
        assert_eq!(
            ids,
            vec!["keep_me"],
            "only assistant-category text models survive"
        );
    }

    #[test]
    fn all_filtered_yields_empty() {
        // Everything fails at least one gate → empty, not a panic.
        let cands = vec![
            asst("too_small", 1.0, 99.0),
            cand(
                "wrong_cat",
                Some(8.0),
                Some(99.0),
                FleetCategory::Voice,
                Some(Modality::Tts),
                "confirmed",
            ),
        ];
        let out = select_discovery_candidates(cands, &DiscoverySelectConfig::default());
        assert!(out.is_empty());
    }

    #[test]
    fn unclassified_modality_kept_by_default_but_droppable() {
        let make = || {
            vec![cand(
                "unclassified",
                Some(8.0),
                Some(50.0),
                FleetCategory::Assistant,
                None,
                "confirmed",
            )]
        };
        // Default: kept.
        let out = select_discovery_candidates(make(), &DiscoverySelectConfig::default());
        assert_eq!(out.len(), 1);
        // Opt out: dropped.
        let mut cfg = DiscoverySelectConfig::default();
        cfg.allow_unclassified_modality = false;
        let out = select_discovery_candidates(make(), &cfg);
        assert!(out.is_empty());
    }

    #[test]
    fn unknown_gfx_dropped_unless_opted_in() {
        let make = || {
            vec![cand(
                "unknown_gfx",
                Some(8.0),
                Some(50.0),
                FleetCategory::Assistant,
                Some(Modality::TextGeneration),
                "unknown",
            )]
        };
        // Default: dropped.
        let out = select_discovery_candidates(make(), &DiscoverySelectConfig::default());
        assert!(out.is_empty());
        // Opted in: kept.
        let mut cfg = DiscoverySelectConfig::default();
        cfg.allow_unknown_gfx = true;
        let out = select_discovery_candidates(make(), &cfg);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn missing_score_sorts_last_never_above_a_real_score() {
        let cands = vec![
            cand(
                "no_score",
                Some(8.0),
                None,
                FleetCategory::Assistant,
                Some(Modality::TextGeneration),
                "confirmed",
            ),
            asst("low_but_real", 8.0, 1.0),
        ];
        let out = select_discovery_candidates(cands, &DiscoverySelectConfig::default());
        let ids: Vec<&str> = out.iter().map(|c| c.model_name.as_str()).collect();
        assert_eq!(ids, vec!["low_but_real", "no_score"]);
    }

    #[test]
    fn nan_score_treated_as_no_score() {
        let cands = vec![
            cand(
                "nan_score",
                Some(8.0),
                Some(f64::NAN),
                FleetCategory::Assistant,
                Some(Modality::TextGeneration),
                "confirmed",
            ),
            asst("real", 8.0, 5.0),
        ];
        let out = select_discovery_candidates(cands, &DiscoverySelectConfig::default());
        let ids: Vec<&str> = out.iter().map(|c| c.model_name.as_str()).collect();
        assert_eq!(ids, vec!["real", "nan_score"]);
    }

    // ---- nomination bridge ----

    #[test]
    fn bridge_maps_fields_and_derives_acquisition() {
        let selected = vec![asst("qwen3:8b", 8.0, 88.0)];
        let noms = nominations_from_selected(&selected);
        assert_eq!(noms.len(), 1);
        let n = &noms[0];
        assert_eq!(n.id, "qwen3:8b");
        assert_eq!(n.size_b, 8.0);
        assert_eq!(n.gfx1151_class, Gfx1151Class::Confirmed);
        assert_eq!(n.acquisition, AcquisitionPath::HfFetch);
        assert_eq!(n.hf_repo.as_deref(), Some("org/qwen3:8b"));
        assert!(
            n.backends.is_empty(),
            "empty ⇒ derived GPU-then-CPU strategy"
        );
        assert!(n.rationale.contains("discovery_score=88.0"));
        assert!(!n.yarn_capable);
    }

    #[test]
    fn bridge_uses_ollama_pull_when_no_hf_repo() {
        let mut c = asst("local:8b", 8.0, 10.0);
        c.hf_repo = "   ".to_string(); // blank / whitespace-only
        let noms = nominations_from_selected(&[c]);
        assert_eq!(noms[0].acquisition, AcquisitionPath::OllamaPull);
        assert_eq!(noms[0].hf_repo, None);
    }

    #[test]
    fn bridge_maps_gfx_class_and_defaults_unknown() {
        let mut c = asst("exp:8b", 8.0, 10.0);
        c.gfx1151_class = "experimental".to_string();
        assert_eq!(
            nominations_from_selected(&[c])[0].gfx1151_class,
            Gfx1151Class::Experimental
        );
        let mut c = asst("weird:8b", 8.0, 10.0);
        c.gfx1151_class = "totally-bogus".to_string();
        assert_eq!(
            nominations_from_selected(&[c])[0].gfx1151_class,
            Gfx1151Class::Unknown,
            "unrecognized class defaults to the safe Unknown (smoke-tested) class"
        );
    }

    #[test]
    fn bridge_derived_backend_strategy_is_gpu_then_cpu() {
        use crate::intake::assistant::BackendTag;
        let noms = nominations_from_selected(&[asst("m:8b", 8.0, 1.0)]);
        assert_eq!(
            noms[0].backend_strategy(),
            vec![(BackendTag::Gpu, "llama-gpu"), (BackendTag::Cpu, "ollama")]
        );
    }

    #[test]
    fn bridge_rationale_handles_missing_score() {
        let c = cand(
            "unscored:8b",
            Some(8.0),
            None,
            FleetCategory::Assistant,
            Some(Modality::TextGeneration),
            "confirmed",
        );
        assert!(nominations_from_selected(&[c])[0]
            .rationale
            .contains("discovery_score=none"));
    }

    // ---- merge ----

    fn nom(id: &str) -> Nomination {
        Nomination {
            id: id.to_string(),
            size_b: 8.0,
            gfx1151_class: Gfx1151Class::Confirmed,
            acquisition: AcquisitionPath::OllamaPull,
            yarn_capable: false,
            yarn: None,
            hf_repo: None,
            backends: Vec::new(),
            rationale: "curated".to_string(),
        }
    }

    #[test]
    fn merge_with_empty_discovered_is_identity_byte_for_byte() {
        // The invariant guaranteeing gap_only / full-sweep behavior is UNCHANGED
        // when discovery-select is off (it yields no synthesized nominations):
        // merging an empty discovered list returns the curated set unchanged.
        let curated = vec![nom("a"), nom("b"), nom("c")];
        let merged = merge_discovery_nominations(curated.clone(), vec![]);
        let ids: Vec<&str> = merged.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
        // Curated records are untouched (rationale preserved).
        assert!(merged.iter().all(|n| n.rationale == "curated"));
    }

    #[test]
    fn merge_curated_wins_id_collision_and_appends_new() {
        let curated = vec![nom("a"), nom("b")];
        let discovered = nominations_from_selected(&[
            asst("b", 8.0, 99.0), // collides with curated "b" → curated wins
            asst("z", 8.0, 50.0), // new → appended
        ]);
        let merged = merge_discovery_nominations(curated, discovered);
        let ids: Vec<&str> = merged.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b", "z"]);
        // The surviving "b" is the CURATED one, not the auto-selected one.
        let b = merged.iter().find(|n| n.id == "b").unwrap();
        assert_eq!(b.rationale, "curated");
    }

    #[test]
    fn merge_dedups_discovered_against_itself() {
        let discovered = nominations_from_selected(&[asst("dup", 8.0, 10.0)]);
        let mut twice = discovered.clone();
        twice.extend(discovered);
        let merged = merge_discovery_nominations(vec![], twice);
        let ids: Vec<&str> = merged.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, vec!["dup"], "a duplicate discovered id is collapsed");
    }

    // ---- env parsing ----

    #[test]
    fn parse_discovery_max_defaults_and_clamps() {
        assert_eq!(parse_discovery_max(None), DEFAULT_DISCOVERY_MAX);
        assert_eq!(parse_discovery_max(Some("bogus")), DEFAULT_DISCOVERY_MAX);
        assert_eq!(parse_discovery_max(Some("0")), DEFAULT_DISCOVERY_MAX);
        assert_eq!(parse_discovery_max(Some("-3")), DEFAULT_DISCOVERY_MAX);
        assert_eq!(parse_discovery_max(Some("3")), 3);
    }

    #[test]
    fn parse_discovery_min_size_defaults_and_rejects_nonpositive() {
        assert_eq!(
            parse_discovery_min_size_b(None),
            DEFAULT_DISCOVERY_MIN_SIZE_B
        );
        assert_eq!(
            parse_discovery_min_size_b(Some("nope")),
            DEFAULT_DISCOVERY_MIN_SIZE_B
        );
        assert_eq!(
            parse_discovery_min_size_b(Some("0")),
            DEFAULT_DISCOVERY_MIN_SIZE_B
        );
        assert_eq!(
            parse_discovery_min_size_b(Some("-1")),
            DEFAULT_DISCOVERY_MIN_SIZE_B
        );
        assert_eq!(parse_discovery_min_size_b(Some("13")), 13.0);
        assert_eq!(parse_discovery_min_size_b(Some(" 9.5 ")), 9.5);
    }
}
