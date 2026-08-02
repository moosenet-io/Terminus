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
//! tunable UP ONLY (a below-default override is clamped back up to the default —
//! never silently below the intent) via `INTAKE_ASSISTANT_DISCOVERY_MIN_SIZE_B`.

use std::collections::BTreeSet;

use super::schema::{CandidateStatus, DiscoveryCandidate, FleetCategory, Modality};
use crate::intake::assistant::acquire::{AcquisitionPath, Gfx1151Class, Nomination};

/// Default per-run cap on brochure-selected candidates. Deliberately small — a
/// brochure sweep should trickle a few high-signal models into the fleet per
/// window, not bulk-enqueue the whole discovery table. Env: `INTAKE_ASSISTANT_DISCOVERY_MAX`.
pub const DEFAULT_DISCOVERY_MAX: usize = 5;

/// Default minimum parameter size (billions) a candidate must have to be
/// selected. ≥ the Chord dynamic-proxy 5GB promotion gate (see the module doc).
/// Env: `INTAKE_ASSISTANT_DISCOVERY_MIN_SIZE_B`.
pub const DEFAULT_DISCOVERY_MIN_SIZE_B: f64 = 7.0;

/// Default MAX parameter size (billions). A candidate above this is DROPPED.
/// Set to ~100B: serving-feasible under this host's ~120GB GTT VRAM envelope
/// (~100B @ Q4 ≈ 60GB), while still bounded — a fits-but-huge model monopolizes
/// the shared idle-reaped GPU pool. Env: `INTAKE_ASSISTANT_DISCOVERY_MAX_SIZE_B`.
///
/// NB (serving vs. ingestion): GTT raises the SERVING ceiling to ~120GB, but the
/// INGESTION path (`ollama-create` converting safetensors → GGUF) is still bound
/// by ~31GB SYSTEM RAM and OOMs on ~20B+ non-GGUF repos (GTT does not help the
/// CPU-side conversion). So this size ceiling is the SERVING bound; the hard
/// ingestion backstop is the Phase-2a ingest step's own byte guard
/// (`CHORD_MODEL_INGEST_MAX_BYTES`), which favors GGUF or ≤~20B safetensors.
pub const DEFAULT_DISCOVERY_MAX_SIZE_B: f64 = 100.0;

/// Default recency half-life (days) for the `recency` fit component:
/// `recency = exp(-age_days / HALFLIFE)`. Env:
/// `INTAKE_DISCOVERY_RANK_RECENCY_HALFLIFE_DAYS`.
pub const DEFAULT_RECENCY_HALFLIFE_DAYS: f64 = 180.0;

/// Neutral recency for a candidate with NO usable date (low-but-nonzero, so an
/// undated model isn't fully zeroed but ranks below any dated recent one).
const NEUTRAL_RECENCY_NO_DATE: f64 = 0.3;

/// Size sweet-spot bounds (billions of params). `fit` plateaus at 1.0 across
/// `[LO, HI]`, tapers down toward `FLOOR` below `LO`, and down toward `CEIL`
/// above `HI` (a fits-but-huge model scores lower).
///
/// The taper endpoints are a deliberate COMPROMISE between two ceilings:
/// - SERVING: this host's ~120GB GTT VRAM envelope makes ~100B @ Q4 (≈60GB)
///   servable, so `CEIL` extends to ~100B (a model can still be selected there);
/// - INGESTION: the safetensors → GGUF conversion path is bound by ~31GB SYSTEM
///   RAM (GTT does NOT help CPU-side conversion) and OOMs on ~20B+ non-GGUF
///   repos — so very large non-GGUF models are impractical to actually pull.
///
/// Net: the plateau stays at the assistant sweet spot (~8–34B) and the top-end
/// taper is meaningful (a 100B model scores 0.2, a 70B ≈0.56 vs. 1.0 at ≤34B),
/// so the ranking still clearly PREFERS ≤~34B while allowing a strong larger
/// model through. The hard ingestion backstop remains the Phase-2a ingest byte
/// guard (`CHORD_MODEL_INGEST_MAX_BYTES`), not this soft score.
const SWEETSPOT_FLOOR_B: f64 = 7.0;
const SWEETSPOT_LO_B: f64 = 8.0;
const SWEETSPOT_HI_B: f64 = 34.0;
const SWEETSPOT_CEIL_B: f64 = 100.0;

/// The blended `fit_score` component weights — each env-configurable and
/// sanitized (NaN/negative/non-finite → the per-weight default). Popularity is a
/// deliberately WEAK tiebreak (default 0.10): "popularity is just a hype meter."
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RankWeights {
    /// Hardware fit (gfx class × size sweet-spot). Env: `INTAKE_DISCOVERY_RANK_W_HW`.
    pub w_hw: f64,
    /// Assistant suitability (instruct/chat prior). Env: `INTAKE_DISCOVERY_RANK_W_ASST`.
    pub w_asst: f64,
    /// Recency (exp decay). Env: `INTAKE_DISCOVERY_RANK_W_RECENCY`.
    pub w_recency: f64,
    /// Popularity (weak tiebreak). Env: `INTAKE_DISCOVERY_RANK_W_POP`.
    pub w_pop: f64,
}

impl Default for RankWeights {
    fn default() -> Self {
        RankWeights {
            w_hw: 0.35,
            w_asst: 0.30,
            w_recency: 0.25,
            w_pop: 0.10,
        }
    }
}

impl RankWeights {
    /// Overlay the env-tunable weights on the defaults, each sanitized
    /// independently (a NaN/negative/non-finite override falls back to that
    /// weight's default). None of these are secret-shaped, so plain
    /// `std::env::var` is correct (matches the existing knobs / S7).
    pub fn from_env() -> Self {
        let d = RankWeights::default();
        RankWeights {
            w_hw: parse_weight(env_opt("INTAKE_DISCOVERY_RANK_W_HW").as_deref(), d.w_hw),
            w_asst: parse_weight(env_opt("INTAKE_DISCOVERY_RANK_W_ASST").as_deref(), d.w_asst),
            w_recency: parse_weight(
                env_opt("INTAKE_DISCOVERY_RANK_W_RECENCY").as_deref(),
                d.w_recency,
            ),
            w_pop: parse_weight(env_opt("INTAKE_DISCOVERY_RANK_W_POP").as_deref(), d.w_pop),
        }
    }
}

/// Small helper: `std::env::var` as an `Option<String>` (non-secret knobs only).
fn env_opt(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

/// The default clearly-unusable license deny-set (substring match, lowercase).
/// Deliberately SMALL + lenient — only licenses that block redistribution /
/// fine-tuning outright. `cc-by-nc` (non-commercial) is intentionally NOT here
/// (allowed); only `cc-by-nc-nd` (no-derivatives) and outright proprietary/
/// closed terms are denied. Env-overridable via
/// `INTAKE_ASSISTANT_DISCOVERY_LICENSE_DENY` (comma-separated substrings).
pub fn default_license_deny() -> BTreeSet<String> {
    [
        "cc-by-nc-nd",
        "proprietary",
        "closed-source",
        "noncommercial-noderivatives",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

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

    // ---- Ask-4 practical hard filters + blended ranking (S127) ----
    /// Max `size_b` a candidate may have (hard filter). Default
    /// [`DEFAULT_DISCOVERY_MAX_SIZE_B`]. Env: `INTAKE_ASSISTANT_DISCOVERY_MAX_SIZE_B`.
    pub max_size_b: f64,
    /// Host VRAM ceiling (GB) for the footprint hard filter — a candidate whose
    /// estimated footprint exceeds this is dropped. Seeded from
    /// `acquire::vram_ceiling_gb()` (`INTAKE_VRAM_CEILING_GB`).
    pub vram_ceiling_gb: f64,
    /// Whether to KEEP a model whose `is_instruct` heuristic is `Some(false)`
    /// (a base/non-instruct model). Default `false` (drop base models); the
    /// escape hatch exists because `is_instruct` is heuristic. Env:
    /// `INTAKE_DISCOVERY_ALLOW_NONINSTRUCT`.
    pub allow_noninstruct: bool,
    /// Clearly-unusable license substrings (lowercase) — a candidate whose
    /// license CONTAINS any of these is dropped. Default [`default_license_deny`].
    /// Env: `INTAKE_ASSISTANT_DISCOVERY_LICENSE_DENY` (comma-separated).
    pub license_deny: BTreeSet<String>,
    /// Whether a candidate with NO license (unknown/absent) is allowed. Default
    /// `true` (lenient). Env: `INTAKE_ASSISTANT_DISCOVERY_ALLOW_UNKNOWN_LICENSE`.
    pub allow_unknown_license: bool,
    /// Whether to REQUIRE a confirmed pre-built GGUF (S127b). Default `true` —
    /// serveability is a HARD requirement for the live loop: the fleet serves
    /// GGUF via ollama/llama.cpp and the ingest path (`ollama pull hf.co/<repo>`)
    /// only accepts GGUF repos, so a non-GGUF model is neither ingestable nor
    /// serveable. When `true`, a candidate is dropped unless `has_gguf ==
    /// Some(true)` (an unmeasured `None` is fail-closed OUT — its serveability is
    /// unknown, so it is not selected). The escape hatch (`false`, which disables
    /// the filter entirely) exists for a future safetensors→GGUF conversion
    /// ingest path. Env: `INTAKE_DISCOVERY_REQUIRE_GGUF`.
    pub require_gguf: bool,
    /// Recency half-life (days) for the recency component. Default
    /// [`DEFAULT_RECENCY_HALFLIFE_DAYS`]. Env:
    /// `INTAKE_DISCOVERY_RANK_RECENCY_HALFLIFE_DAYS`.
    pub recency_halflife_days: f64,
    /// The blended `fit_score` component weights. See [`RankWeights`].
    pub weights: RankWeights,
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
            max_size_b: DEFAULT_DISCOVERY_MAX_SIZE_B,
            vram_ceiling_gb: DEFAULT_VRAM_CEILING_GB_FALLBACK,
            allow_noninstruct: false,
            license_deny: default_license_deny(),
            allow_unknown_license: true,
            // Serveability is a hard requirement for the live loop (GGUF-only
            // ingest); default ON, fail-closed on an unmeasured None.
            require_gguf: true,
            recency_halflife_days: DEFAULT_RECENCY_HALFLIFE_DAYS,
            weights: RankWeights::default(),
        }
    }
}

/// Fallback VRAM ceiling used by [`DiscoverySelectConfig::default`] when the
/// config is built WITHOUT env (tests). [`DiscoverySelectConfig::from_env`]
/// overlays the real `acquire::vram_ceiling_gb()`. Matches acquire's own
/// documented default (~120GB, this host's GTT serving envelope) so a defaulted
/// config never rejects a normally-sized model.
const DEFAULT_VRAM_CEILING_GB_FALLBACK: f64 = 120.0;

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
        // Ask-4 practical hard filters + ranking knobs (S127).
        cfg.max_size_b = parse_max_size_b(
            env_opt("INTAKE_ASSISTANT_DISCOVERY_MAX_SIZE_B").as_deref(),
            cfg.min_size_b,
        );
        cfg.vram_ceiling_gb = crate::intake::assistant::acquire::vram_ceiling_gb();
        cfg.allow_noninstruct = crate::intake::parse_only_stale(
            env_opt("INTAKE_DISCOVERY_ALLOW_NONINSTRUCT").as_deref(),
        );
        cfg.allow_unknown_license = parse_allow_unknown_license(
            env_opt("INTAKE_ASSISTANT_DISCOVERY_ALLOW_UNKNOWN_LICENSE").as_deref(),
        );
        cfg.require_gguf = parse_require_gguf(env_opt("INTAKE_DISCOVERY_REQUIRE_GGUF").as_deref());
        if let Some(deny) =
            parse_license_deny(env_opt("INTAKE_ASSISTANT_DISCOVERY_LICENSE_DENY").as_deref())
        {
            cfg.license_deny = deny;
        }
        cfg.recency_halflife_days =
            parse_halflife_days(env_opt("INTAKE_DISCOVERY_RANK_RECENCY_HALFLIFE_DAYS").as_deref());
        cfg.weights = RankWeights::from_env();
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
/// The floor can be tuned UP ONLY: a valid positive value BELOW
/// [`DEFAULT_DISCOVERY_MIN_SIZE_B`] is clamped UP to that default (so an operator
/// can never silently lower the size protection below the proxy's effective gate
/// — see the module-doc size-floor rationale). A missing/unparseable/
/// zero/negative/non-finite value also falls back to
/// [`DEFAULT_DISCOVERY_MIN_SIZE_B`]. Pure over input.
pub fn parse_discovery_min_size_b(raw: Option<&str>) -> f64 {
    raw.and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|n| n.is_finite() && *n > 0.0)
        // Tuned-UP-only: never below the default protective floor.
        .map(|n| n.max(DEFAULT_DISCOVERY_MIN_SIZE_B))
        .unwrap_or(DEFAULT_DISCOVERY_MIN_SIZE_B)
}

/// Sanitize a single blended-`fit_score` weight: a finite, non-negative value is
/// taken as-is; a NaN / negative / non-finite / unparseable override falls back
/// to `default`. Zero is a legal weight (disables that component). Pure.
pub fn parse_weight(raw: Option<&str>, default: f64) -> f64 {
    raw.and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|n| n.is_finite() && *n >= 0.0)
        .unwrap_or(default)
}

/// Parse the max-size ceiling (`INTAKE_ASSISTANT_DISCOVERY_MAX_SIZE_B`). A
/// missing/unparseable/non-positive value → [`DEFAULT_DISCOVERY_MAX_SIZE_B`].
/// Clamped to be at least `min_size_b` so a degenerate `max < min` config can't
/// silently drop everything. Pure.
pub fn parse_max_size_b(raw: Option<&str>, min_size_b: f64) -> f64 {
    let v = raw
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|n| n.is_finite() && *n > 0.0)
        .unwrap_or(DEFAULT_DISCOVERY_MAX_SIZE_B);
    v.max(min_size_b)
}

/// Parse the recency half-life (`INTAKE_DISCOVERY_RANK_RECENCY_HALFLIFE_DAYS`).
/// A missing/unparseable/non-positive/non-finite value →
/// [`DEFAULT_RECENCY_HALFLIFE_DAYS`]. Pure.
pub fn parse_halflife_days(raw: Option<&str>) -> f64 {
    raw.and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|n| n.is_finite() && *n > 0.0)
        .unwrap_or(DEFAULT_RECENCY_HALFLIFE_DAYS)
}

/// Parse the allow-unknown-license flag. Unlike most flags this DEFAULTS TRUE
/// (lenient): only an explicit falsey value (`0`/`false`/`no`/`off`) turns it
/// off. Pure.
pub fn parse_allow_unknown_license(raw: Option<&str>) -> bool {
    match raw.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        Some("0") | Some("false") | Some("no") | Some("off") => false,
        _ => true,
    }
}

/// Parse the require-GGUF flag (`INTAKE_DISCOVERY_REQUIRE_GGUF`, S127b). DEFAULTS
/// TRUE (serveability is a hard requirement — the fleet only ingests/serves
/// GGUF): only an explicit falsey value (`0`/`false`/`no`/`off`) turns it off
/// (opening the future safetensors-conversion escape hatch). Pure.
pub fn parse_require_gguf(raw: Option<&str>) -> bool {
    match raw.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        Some("0") | Some("false") | Some("no") | Some("off") => false,
        _ => true,
    }
}

/// Parse a comma-separated license deny-set override. Returns `None` (keep the
/// default set) when the value is absent/blank; an explicitly empty-but-present
/// override (e.g. a lone comma) yields an EMPTY set (deny nothing) — an operator
/// can thereby disable the license filter entirely. Entries are lowercased +
/// trimmed. Pure.
pub fn parse_license_deny(raw: Option<&str>) -> Option<BTreeSet<String>> {
    let raw = raw?;
    if raw.trim().is_empty() {
        return None; // blank → keep default
    }
    Some(
        raw.split(',')
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .collect(),
    )
}

// ---------------------------------------------------------------------------
// Blended practical fit_score (S127) — pure components, each unit-tested.
// ---------------------------------------------------------------------------

/// gfx1151-class runnability score for `hardware_fit`. `"no"` scores 0.0 (though
/// it is already excluded by the hard filter); `"confirmed"` 1.0, `"likely"`
/// 0.8, `"experimental"` 0.5, everything else (incl. `"unknown"`) 0.2. Pure.
pub fn gfx_class_score(gfx1151_class: &str) -> f64 {
    match gfx1151_class {
        "confirmed" => 1.0,
        "likely" => 0.8,
        "experimental" => 0.5,
        "no" => 0.0,
        _ => 0.2, // unknown / any unmapped string
    }
}

/// Size sweet-spot multiplier ∈ [0,1]: 1.0 across `[LO, HI]`, tapering toward
/// `FLOOR` below `LO` and toward `CEIL` above `HI` (a fits-but-huge model scores
/// lower). A non-finite/≤0 size scores 0.0. Pure.
pub fn size_sweetspot(size_b: f64) -> f64 {
    if !size_b.is_finite() || size_b <= 0.0 {
        return 0.0;
    }
    if (SWEETSPOT_LO_B..=SWEETSPOT_HI_B).contains(&size_b) {
        return 1.0;
    }
    if size_b < SWEETSPOT_LO_B {
        // Ramp FLOOR..LO → 0.7..1.0 (below FLOOR clamps to a small floor).
        let frac = (size_b - SWEETSPOT_FLOOR_B) / (SWEETSPOT_LO_B - SWEETSPOT_FLOOR_B);
        return (0.7 + 0.3 * frac).clamp(0.1, 1.0);
    }
    // size_b > HI: taper HI..CEIL → 1.0..0.2; above CEIL clamps low.
    let frac = (size_b - SWEETSPOT_HI_B) / (SWEETSPOT_CEIL_B - SWEETSPOT_HI_B);
    (1.0 - 0.8 * frac).clamp(0.1, 1.0)
}

/// Hardware fit = gfx-class score × size sweet-spot. Pure.
pub fn hardware_fit(gfx1151_class: &str, size_b: f64) -> f64 {
    gfx_class_score(gfx1151_class) * size_sweetspot(size_b)
}

/// Assistant-suitability metadata prior (no measured quality pre-test). Driven
/// by the persisted `is_instruct` heuristic: `Some(true)` (instruct/chat-tuned)
/// → 1.0, `None` (unknown) → 0.5 (nonzero neutral), `Some(false)` (base/generic
/// text-gen) → 0.4. Pure.
///
/// NB: the design's 3-tier "chat-tuned 1.0 / instruct 0.8 / generic 0.4" split
/// is served here by the single persisted `is_instruct` boolean — chat and
/// instruct both surface as `Some(true)` and share the top tuned tier (1.0);
/// only the base/unknown tiers are distinguished. See the module deviation note.
pub fn assistant_suitability(is_instruct: Option<bool>) -> f64 {
    match is_instruct {
        Some(true) => 1.0,
        None => 0.5,
        Some(false) => 0.4,
    }
}

/// Recency ∈ (0,1]: `exp(-age_days / halflife)`, from the newest of
/// `updated_at`/`published_at`. A future date clamps age to 0 (→ 1.0); NO usable
/// date yields the documented low-but-nonzero neutral
/// [`NEUTRAL_RECENCY_NO_DATE`]. Pure.
pub fn recency_score(
    published_at: Option<chrono::DateTime<chrono::Utc>>,
    updated_at: Option<chrono::DateTime<chrono::Utc>>,
    now: chrono::DateTime<chrono::Utc>,
    halflife_days: f64,
) -> f64 {
    // Prefer lastModified (updated_at); fall back to createdAt (published_at).
    let date = updated_at.or(published_at);
    let date = match date {
        Some(d) => d,
        None => return NEUTRAL_RECENCY_NO_DATE,
    };
    let age_days = (now - date).num_seconds() as f64 / 86_400.0;
    let age_days = age_days.max(0.0); // a future date → age 0 → recency 1.0
    let hl = if halflife_days.is_finite() && halflife_days > 0.0 {
        halflife_days
    } else {
        DEFAULT_RECENCY_HALFLIFE_DAYS
    };
    (-age_days / hl).exp().clamp(0.0, 1.0)
}

/// Popularity normalized to [0,1] from `discovery_score` (a 0..100 HF signal).
/// `None`/NaN → 0.0. This is the WEAK tiebreak the redesign demotes popularity
/// to. Pure.
pub fn popularity_norm(discovery_score: Option<f64>) -> f64 {
    match discovery_score {
        Some(v) if v.is_finite() => (v / 100.0).clamp(0.0, 1.0),
        _ => 0.0,
    }
}

/// The blended practical fit score ∈ [0, Σweights] (∈ [0,1] with default
/// weights, which sum to 1.0):
/// `w_hw·hardware_fit + w_asst·suitability + w_recency·recency + w_pop·popularity`.
/// Pure.
pub fn fit_score(
    c: &DiscoveryCandidate,
    cfg: &DiscoverySelectConfig,
    now: chrono::DateTime<chrono::Utc>,
) -> f64 {
    let size = c.size_b.unwrap_or(0.0);
    let hw = hardware_fit(&c.gfx1151_class, size);
    let asst = assistant_suitability(c.is_instruct);
    let rec = recency_score(c.published_at, c.updated_at, now, cfg.recency_halflife_days);
    let pop = popularity_norm(c.discovery_score);
    let w = cfg.weights;
    w.w_hw * hw + w.w_asst * asst + w.w_recency * rec + w.w_pop * pop
}

/// The candidate's estimated VRAM footprint (GB) for the size-ceiling hard
/// filter: the measured `vram_footprint_gb` when present, else the ~0.6 GB/B
/// Q4-class estimate (matching `acquire`). Pure.
fn footprint_gb(c: &DiscoveryCandidate) -> f64 {
    c.vram_footprint_gb
        .filter(|v| v.is_finite() && *v > 0.0)
        .unwrap_or_else(|| c.size_b.unwrap_or(0.0) * 0.6)
}

/// Whether a candidate's LICENSE clears the deny-set. `None` license → allowed
/// iff `allow_unknown`. A present license is dropped iff it CONTAINS any
/// deny-substring. Pure.
pub fn license_allowed(
    license: Option<&str>,
    deny: &BTreeSet<String>,
    allow_unknown: bool,
) -> bool {
    match license {
        None => allow_unknown,
        Some(l) => {
            let l = l.to_lowercase();
            !deny.iter().any(|d| l.contains(d.as_str()))
        }
    }
}

/// Whether a candidate clears the instruct-only hard filter. `Some(false)`
/// (base/non-instruct) is dropped UNLESS `allow_noninstruct`; `Some(true)` and
/// `None` (unknown — lenient, since the signal is heuristic) are kept. Pure.
pub fn instruct_allowed(is_instruct: Option<bool>, allow_noninstruct: bool) -> bool {
    if allow_noninstruct {
        return true;
    }
    !matches!(is_instruct, Some(false))
}

/// Whether a candidate clears the GGUF-serveability hard filter (S127b). This is
/// a PRACTICAL/SERVEABILITY criterion, NOT a quality one: the fleet serves GGUF
/// via ollama/llama.cpp and the ingest path (`ollama pull hf.co/<repo>`) only
/// accepts GGUF repos, so a non-GGUF model is neither ingestable nor serveable.
///
/// When `require_gguf` is `false` (the future safetensors-conversion escape
/// hatch) every candidate passes. When `true` (the default), ONLY a confirmed
/// GGUF passes: `Some(true)` is kept; `Some(false)` (safetensors-only) is
/// dropped; and `None` (unmeasured — serveability unknown) is dropped
/// FAIL-CLOSED — a model whose serveability can't be confirmed is never selected
/// into the live loop. Pure.
pub fn gguf_allowed(has_gguf: Option<bool>, require_gguf: bool) -> bool {
    if !require_gguf {
        return true;
    }
    has_gguf == Some(true)
}

/// PURELY rank the brochure into an ORDERED, capped shortlist of assistant-
/// relevant candidates. Deterministic; no I/O. Mirrors
/// `assistant::runner::select_gap_models`'s data-in/data-out contract.
///
/// Hard FILTERS (a candidate must pass ALL — disqualified before ranking):
///   0. `status` is nominatable ([`is_nominatable_status`]) — a fail-closed gate
///      that drops terminal/ineligible rows (`Rejected`/`Evicted`/`Swept`),
///      since [`super::storage::read_brochure`] returns ALL statuses;
///   1. `category` ∈ `cfg.allowed_categories`;
///   2. `modality` ∈ `cfg.allowed_modalities`, OR (`modality` is `None` AND
///      `cfg.allow_unclassified_modality`);
///   3. size FLOOR: `size_b` is `Some(finite v)` with `v >= cfg.min_size_b`;
///   4. size CEILING (S127): `v <= cfg.max_size_b` AND the estimated VRAM
///      footprint ≤ `cfg.vram_ceiling_gb` (a fits-but-huge model is dropped);
///   5. servable arch (S127): `gfx1151_class != "no"` — subsumes unservable-arch
///      + not-fits-VRAM (the derived `"no"` verdict), reinforced by the gfx
///      allowlist gate (`"no"` is never in it);
///   6. gfx allowlist: `gfx1151_class` ∈ the effective allowlist (`"unknown"`
///      only if opted in);
///   7. instruct-only (S127): drop `is_instruct == Some(false)` unless
///      `cfg.allow_noninstruct` (an unknown/`None` is kept — heuristic-lenient);
///   8. license (S127): drop a clearly-blocked license (`cfg.license_deny`); an
///      absent license is kept iff `cfg.allow_unknown_license`;
///   9. public (S127): `gated != Some(true)` (a gated repo can't be auto-ingested);
///  10. GGUF serveability (S127b): when `cfg.require_gguf` (default true), drop
///      any candidate without a confirmed GGUF (`has_gguf != Some(true)`) — a
///      PRACTICAL/serveability filter (the fleet serves GGUF via ollama/llama.cpp
///      and `ollama pull hf.co/<repo>` only accepts GGUF repos). An unmeasured
///      `None` is fail-closed OUT (serveability unknown → not selected).
///
/// Ranking (S127): by the blended practical [`fit_score`] DESC — hardware fit,
/// assistant suitability, and recency dominate; popularity is only `w_pop`
/// (default 0.10). Ties break by popularity DESC then `model_name` ASC (so two
/// equal-fit candidates order by the weak popularity signal, then deterministically
/// by name). The top `cfg.top_n` are returned. `now` anchors the recency decay.
pub fn select_discovery_candidates(
    candidates: Vec<DiscoveryCandidate>,
    cfg: &DiscoverySelectConfig,
    now: chrono::DateTime<chrono::Utc>,
) -> Vec<DiscoveryCandidate> {
    let gfx_allow = cfg.effective_gfx_classes();

    let mut kept: Vec<DiscoveryCandidate> = candidates
        .into_iter()
        // Fail-closed status gate FIRST: read_brochure returns all statuses, so a
        // terminal/ineligible one (Rejected/Evicted/Swept) must never be nominated.
        .filter(|c| is_nominatable_status(c.status))
        .filter(|c| cfg.allowed_categories.contains(&c.category))
        .filter(|c| match c.modality {
            Some(m) => cfg.allowed_modalities.contains(&m),
            None => cfg.allow_unclassified_modality,
        })
        // A `None`, non-finite (NaN/±∞), or below-floor size is DROPPED: a bogus
        // infinite size must never pass the floor gate on `∞ >= floor == true`.
        .filter(|c| matches!(c.size_b, Some(v) if v.is_finite() && v >= cfg.min_size_b))
        // Size CEILING + VRAM footprint (S127).
        .filter(|c| matches!(c.size_b, Some(v) if v <= cfg.max_size_b))
        .filter(|c| footprint_gb(c) <= cfg.vram_ceiling_gb)
        // Servable arch: a derived "no" is never selectable.
        .filter(|c| c.gfx1151_class != "no")
        .filter(|c| gfx_allow.contains(&c.gfx1151_class))
        // Instruct-only + license + public gates (S127).
        .filter(|c| instruct_allowed(c.is_instruct, cfg.allow_noninstruct))
        .filter(|c| {
            license_allowed(
                c.license.as_deref(),
                &cfg.license_deny,
                cfg.allow_unknown_license,
            )
        })
        .filter(|c| c.gated != Some(true))
        // GGUF serveability (S127b): require a confirmed pre-built GGUF (default
        // on); fail-closed on an unmeasured None.
        .filter(|c| gguf_allowed(c.has_gguf, cfg.require_gguf))
        .collect();

    // Rank by blended fit_score DESC; tiebreak popularity DESC then name ASC.
    // Precompute each candidate's fit once (cheap, and avoids recompute in the
    // O(n log n) comparator).
    let mut scored: Vec<(f64, f64, DiscoveryCandidate)> = kept
        .drain(..)
        .map(|c| {
            let fit = fit_score(&c, cfg, now);
            let pop = popularity_norm(c.discovery_score);
            (fit, pop, c)
        })
        .collect();
    scored.sort_by(|a, b| {
        // fit DESC
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            // popularity DESC (weak tiebreak)
            .then_with(|| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal))
            // deterministic final tiebreak: model_name ASC
            .then_with(|| a.2.model_name.cmp(&b.2.model_name))
    });

    scored
        .into_iter()
        .take(cfg.top_n)
        .map(|(_, _, c)| c)
        .collect()
}

/// Whether a brochure candidate in this lifecycle status may be NOMINATED into
/// the assistant sweep. FAIL-CLOSED by construction: an explicit, EXHAUSTIVE
/// match with NO wildcard arm, so a future [`CandidateStatus`] variant forces a
/// compile error here (a deliberate "decide, don't leak") rather than silently
/// flowing into the sweep.
///
/// This gate is necessary because [`super::storage::read_brochure`] returns rows
/// of ALL statuses (its SQL has no status predicate), so without it a terminal /
/// ineligible candidate could be re-nominated:
///
/// Allowed (pre-fleet, acquirable, or already in the pipeline):
/// - [`Discovered`](CandidateStatus::Discovered) — newly found; the core case;
/// - [`Fetching`](CandidateStatus::Fetching) — a fetch is already in flight;
/// - [`ColdStored`](CandidateStatus::ColdStored) — in the cold archive, so it is
///   actually acquirable NOW (the ideal nominate state);
/// - [`MarkedForFleet`](CandidateStatus::MarkedForFleet) — already queued for a sweep.
///
/// Dropped:
/// - [`Swept`](CandidateStatus::Swept) — already has a fleet cell; re-nominating
///   is redundant (the point is NEW models);
/// - [`Evicted`](CandidateStatus::Evicted) — archive copy pruned; not acquirable
///   and re-introduces churn;
/// - [`Rejected`](CandidateStatus::Rejected) — already failed the VRAM/gfx fit
///   check; a KNOWN-BAD model must never be swept-for-assistant.
fn is_nominatable_status(status: CandidateStatus) -> bool {
    match status {
        CandidateStatus::Discovered
        | CandidateStatus::Fetching
        | CandidateStatus::ColdStored
        | CandidateStatus::MarkedForFleet => true,
        CandidateStatus::Swept | CandidateStatus::Evicted | CandidateStatus::Rejected => false,
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
            published_at: None,
            updated_at: None,
            license: None,
            arch: None,
            // Default the practical-ranking fields to the "assistant-friendly"
            // state so the EXISTING filter/rank tests below stay focused on the
            // axis they assert; tests that exercise the new hard filters /
            // fit_score set these explicitly.
            is_instruct: Some(true),
            gated: Some(false),
            quant_dtype: None,
            // Serveable by default (has a GGUF) so the EXISTING filter/rank tests
            // aren't tripped by the S127b require-GGUF hard filter (default on);
            // the GGUF-filter tests below set this explicitly.
            has_gguf: Some(true),
            // Persisted score is irrelevant to the selector (it recomputes
            // fit_score transiently); default None in the test builder.
            fit_score: None,
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
        let out = select_discovery_candidates(
            vec![],
            &DiscoverySelectConfig::default(),
            chrono::Utc::now(),
        );
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
        let out = select_discovery_candidates(cands, &cfg, chrono::Utc::now());
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
        let out = select_discovery_candidates(
            cands,
            &DiscoverySelectConfig::default(),
            chrono::Utc::now(),
        );
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
        let out = select_discovery_candidates(
            cands,
            &DiscoverySelectConfig::default(),
            chrono::Utc::now(),
        );
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
        let out = select_discovery_candidates(
            cands,
            &DiscoverySelectConfig::default(),
            chrono::Utc::now(),
        );
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
        let out = select_discovery_candidates(
            cands,
            &DiscoverySelectConfig::default(),
            chrono::Utc::now(),
        );
        assert!(out.is_empty());
    }

    #[test]
    fn ineligible_statuses_are_dropped_and_eligible_kept() {
        // read_brochure returns ALL statuses; the fail-closed status gate must
        // drop known-bad / terminal ones and keep pre-fleet eligible ones, even
        // when every OTHER field (category/modality/size/gfx/score) passes.
        let with_status = |name: &str, status: CandidateStatus| {
            let mut c = asst(name, 8.0, 99.0);
            c.status = status;
            c
        };
        let cands = vec![
            with_status("rejected", CandidateStatus::Rejected),
            with_status("evicted", CandidateStatus::Evicted),
            with_status("swept", CandidateStatus::Swept),
            with_status("discovered", CandidateStatus::Discovered),
            with_status("cold_stored", CandidateStatus::ColdStored),
            with_status("fetching", CandidateStatus::Fetching),
            with_status("marked", CandidateStatus::MarkedForFleet),
        ];
        let mut cfg = DiscoverySelectConfig::default();
        cfg.top_n = 100;
        let out = select_discovery_candidates(cands, &cfg, chrono::Utc::now());
        let mut ids: Vec<&str> = out.iter().map(|c| c.model_name.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(
            ids,
            vec!["cold_stored", "discovered", "fetching", "marked"],
            "Rejected/Evicted/Swept dropped; pre-fleet eligible states kept"
        );
    }

    #[test]
    fn rejected_and_evicted_never_selected_even_with_top_score() {
        // A known-bad Rejected candidate with the HIGHEST score must not win a slot.
        let mut rejected = asst("known_bad", 70.0, 100.0);
        rejected.status = CandidateStatus::Rejected;
        let mut evicted = asst("pruned", 70.0, 100.0);
        evicted.status = CandidateStatus::Evicted;
        let good = asst("fresh", 8.0, 1.0);
        let out = select_discovery_candidates(
            vec![rejected, evicted, good],
            &DiscoverySelectConfig::default(),
            chrono::Utc::now(),
        );
        let ids: Vec<&str> = out.iter().map(|c| c.model_name.as_str()).collect();
        assert_eq!(ids, vec!["fresh"]);
    }

    #[test]
    fn is_nominatable_status_is_fail_closed_allow_list() {
        // Locks the eligibility decision (the exhaustive match is the fail-closed
        // guard; this pins the intended allow/deny split).
        assert!(is_nominatable_status(CandidateStatus::Discovered));
        assert!(is_nominatable_status(CandidateStatus::Fetching));
        assert!(is_nominatable_status(CandidateStatus::ColdStored));
        assert!(is_nominatable_status(CandidateStatus::MarkedForFleet));
        assert!(!is_nominatable_status(CandidateStatus::Swept));
        assert!(!is_nominatable_status(CandidateStatus::Evicted));
        assert!(!is_nominatable_status(CandidateStatus::Rejected));
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
        let out = select_discovery_candidates(
            make(),
            &DiscoverySelectConfig::default(),
            chrono::Utc::now(),
        );
        assert_eq!(out.len(), 1);
        // Opt out: dropped.
        let mut cfg = DiscoverySelectConfig::default();
        cfg.allow_unclassified_modality = false;
        let out = select_discovery_candidates(make(), &cfg, chrono::Utc::now());
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
        let out = select_discovery_candidates(
            make(),
            &DiscoverySelectConfig::default(),
            chrono::Utc::now(),
        );
        assert!(out.is_empty());
        // Opted in: kept.
        let mut cfg = DiscoverySelectConfig::default();
        cfg.allow_unknown_gfx = true;
        let out = select_discovery_candidates(make(), &cfg, chrono::Utc::now());
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
        let out = select_discovery_candidates(
            cands,
            &DiscoverySelectConfig::default(),
            chrono::Utc::now(),
        );
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
        let out = select_discovery_candidates(
            cands,
            &DiscoverySelectConfig::default(),
            chrono::Utc::now(),
        );
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
        // Tuned UP is honored.
        assert_eq!(parse_discovery_min_size_b(Some("13")), 13.0);
        assert_eq!(parse_discovery_min_size_b(Some(" 9.5 ")), 9.5);
    }

    #[test]
    fn parse_discovery_min_size_clamps_below_floor_up_to_default() {
        // A valid positive override BELOW the default is clamped UP to the floor —
        // an operator can never silently lower the size protection.
        assert_eq!(
            parse_discovery_min_size_b(Some("3")),
            DEFAULT_DISCOVERY_MIN_SIZE_B
        );
        assert_eq!(
            parse_discovery_min_size_b(Some("0.5")),
            DEFAULT_DISCOVERY_MIN_SIZE_B
        );
        // Exactly at the floor stays at the floor; above stays above.
        assert_eq!(
            parse_discovery_min_size_b(Some("7")),
            DEFAULT_DISCOVERY_MIN_SIZE_B
        );
        assert_eq!(parse_discovery_min_size_b(Some("9.5")), 9.5);
    }

    #[test]
    fn parse_discovery_min_size_rejects_non_finite_env_value() {
        // A non-finite env value falls back to the default, never becomes the floor.
        assert_eq!(
            parse_discovery_min_size_b(Some("inf")),
            DEFAULT_DISCOVERY_MIN_SIZE_B
        );
        assert_eq!(
            parse_discovery_min_size_b(Some("NaN")),
            DEFAULT_DISCOVERY_MIN_SIZE_B
        );
    }

    #[test]
    fn non_finite_candidate_size_is_dropped() {
        // A bogus infinite/NaN candidate size must NOT pass the floor gate
        // (`∞ >= 7.0` would otherwise be true), and must never be selected.
        let cands = vec![
            cand(
                "infinite_size",
                Some(f64::INFINITY),
                Some(99.0),
                FleetCategory::Assistant,
                Some(Modality::TextGeneration),
                "confirmed",
            ),
            cand(
                "nan_size",
                Some(f64::NAN),
                Some(99.0),
                FleetCategory::Assistant,
                Some(Modality::TextGeneration),
                "confirmed",
            ),
            asst("legit", 8.0, 5.0),
        ];
        let out = select_discovery_candidates(
            cands,
            &DiscoverySelectConfig::default(),
            chrono::Utc::now(),
        );
        let ids: Vec<&str> = out.iter().map(|c| c.model_name.as_str()).collect();
        assert_eq!(
            ids,
            vec!["legit"],
            "only the finite-sized candidate survives"
        );
    }

    // ======================================================================
    // Ask-4 practical ranking (S127): fit_score components, blended ranking,
    // and the new hard filters.
    // ======================================================================

    fn test_now() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339("2026-07-25T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    /// Candidate with the practical fields set, `updated_at` = `days_ago` before
    /// [`test_now`].
    fn practical(
        name: &str,
        size_b: f64,
        score: f64,
        gfx: &str,
        is_instruct: Option<bool>,
        days_ago: i64,
    ) -> DiscoveryCandidate {
        let mut c = asst(name, size_b, score);
        c.gfx1151_class = gfx.to_string();
        c.is_instruct = is_instruct;
        c.updated_at = Some(test_now() - chrono::Duration::days(days_ago));
        c.published_at = c.updated_at;
        c
    }

    // ---- gfx_class_score ----

    #[test]
    fn gfx_class_score_tiers() {
        assert_eq!(gfx_class_score("confirmed"), 1.0);
        assert_eq!(gfx_class_score("likely"), 0.8);
        assert_eq!(gfx_class_score("experimental"), 0.5);
        assert_eq!(gfx_class_score("no"), 0.0);
        assert_eq!(gfx_class_score("unknown"), 0.2);
        assert_eq!(gfx_class_score("something-else"), 0.2);
    }

    // ---- size_sweetspot ----

    #[test]
    fn size_sweetspot_plateaus_over_the_sweet_range() {
        assert_eq!(size_sweetspot(8.0), 1.0);
        assert_eq!(size_sweetspot(20.0), 1.0);
        assert_eq!(size_sweetspot(34.0), 1.0);
    }

    #[test]
    fn size_sweetspot_tapers_below_lo_and_above_hi() {
        // At the 7B floor → 0.7; a fits-but-huge 100B (the CEIL) → 0.2 (much lower).
        assert!((size_sweetspot(7.0) - 0.7).abs() < 1e-9);
        assert!((size_sweetspot(100.0) - 0.2).abs() < 1e-9);
        // A 70B model tapers to ~0.56 — still well below the ≤34B plateau (1.0),
        // so the ranking keeps preferring the assistant sweet spot.
        assert!(size_sweetspot(70.0) > 0.5 && size_sweetspot(70.0) < 0.6);
        // Monotonic: bigger past the plateau scores strictly lower.
        assert!(size_sweetspot(40.0) < size_sweetspot(34.0));
        assert!(size_sweetspot(70.0) < size_sweetspot(40.0));
        assert!(size_sweetspot(100.0) < size_sweetspot(70.0));
        // A midpoint below LO ramps between 0.7 and 1.0.
        assert!(size_sweetspot(7.5) > 0.7 && size_sweetspot(7.5) < 1.0);
        // Non-finite / non-positive → 0.0.
        assert_eq!(size_sweetspot(0.0), 0.0);
        assert_eq!(size_sweetspot(f64::NAN), 0.0);
    }

    // ---- assistant_suitability ----

    #[test]
    fn assistant_suitability_tiers() {
        assert_eq!(assistant_suitability(Some(true)), 1.0);
        assert_eq!(assistant_suitability(None), 0.5);
        assert_eq!(assistant_suitability(Some(false)), 0.4);
        // instruct/chat (true) outranks unknown outranks base.
        assert!(
            assistant_suitability(Some(true)) > assistant_suitability(None)
                && assistant_suitability(None) > assistant_suitability(Some(false))
        );
    }

    // ---- recency_score ----

    #[test]
    fn recency_decays_with_age_and_halves_at_the_halflife() {
        let now = test_now();
        let hl = 180.0;
        // age 0 → 1.0.
        assert!((recency_score(None, Some(now), now, hl) - 1.0).abs() < 1e-9);
        // The formula is exp(-age/hl), so at age == hl the value is 1/e (~0.368),
        // NOT 0.5 — `hl` is the exp-decay time constant, per the design formula
        // `recency = exp(-age_days / HALFLIFE)`.
        let one_tau = now - chrono::Duration::days(180);
        let e_inv = (-1.0f64).exp();
        assert!((recency_score(None, Some(one_tau), now, hl) - e_inv).abs() < 1e-3);
        let half = one_tau;
        // older is strictly smaller.
        let old = now - chrono::Duration::days(400);
        assert!(recency_score(None, Some(old), now, hl) < recency_score(None, Some(half), now, hl));
    }

    #[test]
    fn recency_no_date_is_the_documented_neutral_and_future_clamps_to_one() {
        let now = test_now();
        assert_eq!(recency_score(None, None, now, 180.0), 0.3);
        // A future date clamps age to 0 → recency 1.0 (never > 1).
        let future = now + chrono::Duration::days(30);
        assert!((recency_score(None, Some(future), now, 180.0) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn recency_prefers_updated_over_published() {
        let now = test_now();
        let old_pub = now - chrono::Duration::days(500);
        let fresh_upd = now - chrono::Duration::days(5);
        // updated_at (fresh) should dominate the stale published_at.
        let r = recency_score(Some(old_pub), Some(fresh_upd), now, 180.0);
        assert!(r > 0.9, "updated_at should win, got {r}");
    }

    // ---- popularity_norm ----

    #[test]
    fn popularity_norm_clamps_and_handles_none_nan() {
        assert!((popularity_norm(Some(50.0)) - 0.5).abs() < 1e-9);
        assert_eq!(popularity_norm(Some(150.0)), 1.0); // clamp high
        assert_eq!(popularity_norm(Some(-5.0)), 0.0); // clamp low
        assert_eq!(popularity_norm(None), 0.0);
        assert_eq!(popularity_norm(Some(f64::NAN)), 0.0);
    }

    // ---- fit_score composition ----

    #[test]
    fn fit_score_with_default_weights_is_the_weighted_sum() {
        let cfg = DiscoverySelectConfig::default();
        // 8B confirmed instruct, updated today, popularity 50.
        let c = practical("m", 8.0, 50.0, "confirmed", Some(true), 0);
        let got = fit_score(&c, &cfg, test_now());
        // 0.35*1.0 + 0.30*1.0 + 0.25*1.0 + 0.10*0.5 = 0.35+0.30+0.25+0.05 = 0.95.
        assert!((got - 0.95).abs() < 1e-9, "got {got}");
    }

    // ---- the headline behavior: popularity is a hype meter, not the ranking ----

    #[test]
    fn a_hugely_popular_but_old_model_ranks_below_a_recent_well_fit_one() {
        // A: max popularity, but 400 days stale.
        let popular_old = practical("popular_old", 8.0, 100.0, "confirmed", Some(true), 400);
        // B: modest popularity, recent + well-fit.
        let recent_fit = practical("recent_fit", 8.0, 20.0, "confirmed", Some(true), 10);
        let out = select_discovery_candidates(
            vec![popular_old, recent_fit],
            &DiscoverySelectConfig::default(),
            test_now(),
        );
        let ids: Vec<&str> = out.iter().map(|c| c.model_name.as_str()).collect();
        assert_eq!(
            ids,
            vec!["recent_fit", "popular_old"],
            "recency+fit must beat raw popularity — popularity is only the weak tiebreak"
        );
    }

    #[test]
    fn a_hugely_popular_but_oversized_model_is_dropped_entirely() {
        // 200B model: way over the 100B ceiling → filtered before ranking, even
        // at max popularity.
        let popular_huge = practical("popular_huge", 200.0, 100.0, "confirmed", Some(true), 0);
        let modest = practical("modest_fit", 8.0, 5.0, "confirmed", Some(true), 30);
        let out = select_discovery_candidates(
            vec![popular_huge, modest],
            &DiscoverySelectConfig::default(),
            test_now(),
        );
        let ids: Vec<&str> = out.iter().map(|c| c.model_name.as_str()).collect();
        assert_eq!(
            ids,
            vec!["modest_fit"],
            "oversized popular model is filtered out"
        );
    }

    #[test]
    fn two_equal_fit_candidates_order_by_popularity_then_name() {
        // Set w_pop = 0 so popularity does NOT enter fit_score — the two
        // candidates are then EXACTLY equal on fit, and the secondary popularity
        // tiebreak (then name ASC) is what genuinely decides the order.
        let mut cfg = DiscoverySelectConfig::default();
        cfg.weights.w_pop = 0.0;
        cfg.top_n = 10;
        // Later name but higher popularity → popularity tiebreak wins.
        let hi_pop = practical("zzz_hi_pop", 8.0, 90.0, "confirmed", Some(true), 10);
        let lo_pop = practical("aaa_lo_pop", 8.0, 10.0, "confirmed", Some(true), 10);
        let out = select_discovery_candidates(vec![lo_pop, hi_pop], &cfg, test_now());
        let ids: Vec<&str> = out.iter().map(|c| c.model_name.as_str()).collect();
        assert_eq!(
            ids,
            vec!["zzz_hi_pop", "aaa_lo_pop"],
            "equal fit → higher popularity wins the tiebreak despite a later name"
        );

        // Popularity identical too → deterministic final tiebreak by name ASC.
        let a = practical("bravo", 8.0, 42.0, "confirmed", Some(true), 10);
        let b = practical("alpha", 8.0, 42.0, "confirmed", Some(true), 10);
        let out = select_discovery_candidates(vec![a, b], &cfg, test_now());
        let ids: Vec<&str> = out.iter().map(|c| c.model_name.as_str()).collect();
        assert_eq!(ids, vec!["alpha", "bravo"], "full fit+pop tie → name ASC");
    }

    // ---- each new hard filter ----

    #[test]
    fn hard_filter_size_ceiling_drops_over_max() {
        // 40B is within the default 100B ceiling; 120B is over.
        let ok = practical("within", 40.0, 50.0, "confirmed", Some(true), 5);
        let over = practical("over", 120.0, 50.0, "confirmed", Some(true), 5);
        let out = select_discovery_candidates(
            vec![ok, over],
            &DiscoverySelectConfig::default(),
            test_now(),
        );
        let ids: Vec<&str> = out.iter().map(|c| c.model_name.as_str()).collect();
        assert_eq!(ids, vec!["within"]);
    }

    #[test]
    fn hard_filter_vram_footprint_over_ceiling_is_dropped() {
        // 30B is under the size ceiling but its measured footprint is huge.
        let mut c = practical("footprint_hog", 30.0, 50.0, "confirmed", Some(true), 5);
        c.vram_footprint_gb = Some(200.0); // > the 120GB fallback ceiling
        let out =
            select_discovery_candidates(vec![c], &DiscoverySelectConfig::default(), test_now());
        assert!(
            out.is_empty(),
            "a within-size but VRAM-busting model is dropped"
        );
    }

    #[test]
    fn hard_filter_unservable_arch_no_is_dropped() {
        // A candidate whose derived gfx class is "no" is never selectable.
        let no = practical("gpt_oss_ish", 8.0, 99.0, "no", Some(true), 1);
        let ok = practical("fine", 8.0, 1.0, "confirmed", Some(true), 1);
        let out = select_discovery_candidates(
            vec![no, ok],
            &DiscoverySelectConfig::default(),
            test_now(),
        );
        let ids: Vec<&str> = out.iter().map(|c| c.model_name.as_str()).collect();
        assert_eq!(ids, vec!["fine"]);
    }

    #[test]
    fn hard_filter_non_instruct_dropped_unless_opted_in() {
        let base = practical("base_model", 8.0, 99.0, "confirmed", Some(false), 1);
        // Default: dropped.
        let out = select_discovery_candidates(
            vec![base.clone()],
            &DiscoverySelectConfig::default(),
            test_now(),
        );
        assert!(out.is_empty(), "base/non-instruct dropped by default");
        // Opt in: kept.
        let mut cfg = DiscoverySelectConfig::default();
        cfg.allow_noninstruct = true;
        let out = select_discovery_candidates(vec![base], &cfg, test_now());
        assert_eq!(out.len(), 1);
        // An UNKNOWN (None) instruct is lenient-kept even by default.
        let unknown = practical("unknown_instruct", 8.0, 5.0, "confirmed", None, 1);
        let out = select_discovery_candidates(
            vec![unknown],
            &DiscoverySelectConfig::default(),
            test_now(),
        );
        assert_eq!(out.len(), 1, "unknown instruct is lenient-kept");
    }

    #[test]
    fn hard_filter_blocked_license_dropped_but_lenient_ones_kept() {
        let mut blocked = practical("nc_nd", 8.0, 99.0, "confirmed", Some(true), 1);
        blocked.license = Some("cc-by-nc-nd-4.0".to_string());
        let mut lenient = practical("apache", 8.0, 1.0, "confirmed", Some(true), 1);
        lenient.license = Some("apache-2.0".to_string());
        // cc-by-nc (non-commercial, NOT no-derivatives) is lenient-allowed.
        let mut nc = practical("nc_ok", 8.0, 2.0, "confirmed", Some(true), 1);
        nc.license = Some("cc-by-nc-4.0".to_string());
        let out = select_discovery_candidates(
            vec![blocked, lenient, nc],
            &DiscoverySelectConfig::default(),
            test_now(),
        );
        let mut ids: Vec<&str> = out.iter().map(|c| c.model_name.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec!["apache", "nc_ok"], "only cc-by-nc-nd is dropped");
    }

    #[test]
    fn hard_filter_unknown_license_kept_by_default_but_droppable() {
        let no_license = practical("no_license", 8.0, 5.0, "confirmed", Some(true), 1);
        assert!(no_license.license.is_none());
        // Default allow_unknown_license = true → kept.
        let out = select_discovery_candidates(
            vec![no_license.clone()],
            &DiscoverySelectConfig::default(),
            test_now(),
        );
        assert_eq!(out.len(), 1);
        // Turn it off → dropped.
        let mut cfg = DiscoverySelectConfig::default();
        cfg.allow_unknown_license = false;
        let out = select_discovery_candidates(vec![no_license], &cfg, test_now());
        assert!(out.is_empty());
    }

    #[test]
    fn hard_filter_gated_model_dropped() {
        let mut gated = practical("gated_repo", 8.0, 99.0, "confirmed", Some(true), 1);
        gated.gated = Some(true);
        let ok = practical("public_repo", 8.0, 1.0, "confirmed", Some(true), 1);
        let out = select_discovery_candidates(
            vec![gated, ok],
            &DiscoverySelectConfig::default(),
            test_now(),
        );
        let ids: Vec<&str> = out.iter().map(|c| c.model_name.as_str()).collect();
        assert_eq!(
            ids,
            vec!["public_repo"],
            "a gated repo can't be auto-ingested"
        );
    }

    #[test]
    fn hard_filter_requires_gguf_by_default_dropping_safetensors_and_unmeasured() {
        // GGUF present → kept; safetensors-only (Some(false)) → dropped; unmeasured
        // (None) → dropped FAIL-CLOSED. Default require_gguf = true.
        let mut gguf = practical("gguf_ok", 8.0, 50.0, "confirmed", Some(true), 1);
        gguf.has_gguf = Some(true);
        let mut safetensors = practical("safetensors_only", 8.0, 99.0, "confirmed", Some(true), 1);
        safetensors.has_gguf = Some(false);
        let mut unmeasured = practical("unmeasured", 8.0, 99.0, "confirmed", Some(true), 1);
        unmeasured.has_gguf = None;
        let out = select_discovery_candidates(
            vec![gguf, safetensors, unmeasured],
            &DiscoverySelectConfig::default(),
            test_now(),
        );
        let ids: Vec<&str> = out.iter().map(|c| c.model_name.as_str()).collect();
        assert_eq!(
            ids,
            vec!["gguf_ok"],
            "only a confirmed-GGUF candidate survives; safetensors-only + unmeasured are dropped"
        );
    }

    #[test]
    fn require_gguf_false_disables_the_filter_entirely() {
        // The escape hatch: with require_gguf off, safetensors-only AND unmeasured
        // candidates are selectable again (future safetensors→GGUF conversion path).
        let mut safetensors = practical("safetensors_only", 8.0, 50.0, "confirmed", Some(true), 1);
        safetensors.has_gguf = Some(false);
        let mut unmeasured = practical("unmeasured", 8.0, 10.0, "confirmed", Some(true), 1);
        unmeasured.has_gguf = None;
        let mut cfg = DiscoverySelectConfig::default();
        cfg.require_gguf = false;
        cfg.top_n = 10;
        let out = select_discovery_candidates(vec![safetensors, unmeasured], &cfg, test_now());
        let mut ids: Vec<&str> = out.iter().map(|c| c.model_name.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(
            ids,
            vec!["safetensors_only", "unmeasured"],
            "require_gguf=false lets non-GGUF candidates through"
        );
    }

    #[test]
    fn require_gguf_does_not_reorder_among_gguf_candidates() {
        // The GGUF filter is purely additive: among candidates that all HAVE a
        // GGUF, the fit_score ordering is exactly what it would be without it.
        let a = practical("popular_old", 8.0, 100.0, "confirmed", Some(true), 400);
        let b = practical("recent_fit", 8.0, 20.0, "confirmed", Some(true), 10);
        // (cand()/practical() default has_gguf = Some(true).)
        let mut cfg = DiscoverySelectConfig::default();
        cfg.top_n = 10;
        let out = select_discovery_candidates(vec![a, b], &cfg, test_now());
        let ids: Vec<&str> = out.iter().map(|c| c.model_name.as_str()).collect();
        assert_eq!(
            ids,
            vec!["recent_fit", "popular_old"],
            "GGUF filter is additive — ordering among GGUF candidates is unchanged"
        );
    }

    // ---- pure filter helpers ----

    #[test]
    fn gguf_allowed_helper() {
        // require on (default): only a confirmed GGUF passes; None fail-closed.
        assert!(gguf_allowed(Some(true), true));
        assert!(!gguf_allowed(Some(false), true));
        assert!(!gguf_allowed(None, true));
        // require off: everything passes (escape hatch).
        assert!(gguf_allowed(Some(true), false));
        assert!(gguf_allowed(Some(false), false));
        assert!(gguf_allowed(None, false));
    }

    #[test]
    fn license_allowed_helper() {
        let deny = default_license_deny();
        assert!(license_allowed(Some("apache-2.0"), &deny, true));
        assert!(license_allowed(Some("mit"), &deny, true));
        assert!(license_allowed(Some("llama3.1"), &deny, true));
        assert!(license_allowed(Some("cc-by-nc-4.0"), &deny, true));
        assert!(!license_allowed(Some("cc-by-nc-nd-4.0"), &deny, true));
        assert!(!license_allowed(Some("Proprietary"), &deny, true)); // case-insensitive
                                                                     // Unknown honors the flag.
        assert!(license_allowed(None, &deny, true));
        assert!(!license_allowed(None, &deny, false));
    }

    #[test]
    fn instruct_allowed_helper() {
        assert!(instruct_allowed(Some(true), false));
        assert!(instruct_allowed(None, false)); // lenient on unknown
        assert!(!instruct_allowed(Some(false), false));
        assert!(instruct_allowed(Some(false), true)); // opted in
    }

    // ---- env knob parsing / sanitization ----

    #[test]
    fn parse_weight_sanitizes_nan_negative_and_unparseable() {
        assert_eq!(parse_weight(None, 0.35), 0.35);
        assert_eq!(parse_weight(Some("bogus"), 0.35), 0.35);
        assert_eq!(parse_weight(Some("NaN"), 0.35), 0.35);
        assert_eq!(parse_weight(Some("-0.5"), 0.35), 0.35);
        assert_eq!(parse_weight(Some("inf"), 0.35), 0.35);
        // A valid non-negative override (incl. 0.0 = disable) is honored.
        assert_eq!(parse_weight(Some("0"), 0.35), 0.0);
        assert_eq!(parse_weight(Some("0.5"), 0.35), 0.5);
    }

    #[test]
    fn parse_max_size_b_defaults_and_clamps_to_min() {
        assert_eq!(parse_max_size_b(None, 7.0), DEFAULT_DISCOVERY_MAX_SIZE_B);
        assert_eq!(
            parse_max_size_b(Some("bogus"), 7.0),
            DEFAULT_DISCOVERY_MAX_SIZE_B
        );
        assert_eq!(parse_max_size_b(Some("120"), 7.0), 120.0);
        // A max below the floor is clamped UP to the floor (never drop-everything).
        assert_eq!(parse_max_size_b(Some("3"), 7.0), 7.0);
    }

    #[test]
    fn parse_halflife_days_defaults_and_rejects_nonpositive() {
        assert_eq!(parse_halflife_days(None), DEFAULT_RECENCY_HALFLIFE_DAYS);
        assert_eq!(
            parse_halflife_days(Some("0")),
            DEFAULT_RECENCY_HALFLIFE_DAYS
        );
        assert_eq!(
            parse_halflife_days(Some("-5")),
            DEFAULT_RECENCY_HALFLIFE_DAYS
        );
        assert_eq!(parse_halflife_days(Some("90")), 90.0);
    }

    #[test]
    fn parse_allow_unknown_license_defaults_true() {
        assert!(parse_allow_unknown_license(None));
        assert!(parse_allow_unknown_license(Some("1")));
        assert!(parse_allow_unknown_license(Some("anything")));
        assert!(!parse_allow_unknown_license(Some("false")));
        assert!(!parse_allow_unknown_license(Some("0")));
        assert!(!parse_allow_unknown_license(Some("off")));
    }

    #[test]
    fn parse_require_gguf_defaults_true_and_only_explicit_falsey_disables() {
        // Default ON (serveability is a hard requirement).
        assert!(parse_require_gguf(None));
        assert!(parse_require_gguf(Some("1")));
        assert!(parse_require_gguf(Some("true")));
        assert!(parse_require_gguf(Some("anything")));
        // Only an explicit falsey value opens the escape hatch.
        assert!(!parse_require_gguf(Some("false")));
        assert!(!parse_require_gguf(Some("0")));
        assert!(!parse_require_gguf(Some("no")));
        assert!(!parse_require_gguf(Some("off")));
        assert!(!parse_require_gguf(Some("  OFF  ")));
    }

    #[test]
    fn parse_license_deny_override_and_disable() {
        // Absent/blank → keep default (None sentinel).
        assert!(parse_license_deny(None).is_none());
        assert!(parse_license_deny(Some("   ")).is_none());
        // A custom set.
        let set = parse_license_deny(Some("foo, BAR ,baz")).unwrap();
        assert!(set.contains("foo") && set.contains("bar") && set.contains("baz"));
        // A lone comma → empty set = deny nothing (filter disabled).
        assert!(parse_license_deny(Some(",")).unwrap().is_empty());
    }

    #[test]
    fn flag_off_no_change_merge_empty_discovered_is_identity() {
        // The Ask-4 select pre-step is gated OFF by default at the runner
        // (discovery_select_from_env); when off, no synthesized nominations are
        // produced and the curated set is byte-for-byte unchanged. This asserts
        // the selector-output → merge boundary preserves that: an empty
        // discovered vector never mutates curated (see also
        // `merge_with_empty_discovered_is_identity_byte_for_byte`).
        let curated = vec![nom("a"), nom("b")];
        let merged = merge_discovery_nominations(curated.clone(), vec![]);
        assert_eq!(
            merged.iter().map(|n| n.id.clone()).collect::<Vec<_>>(),
            vec!["a".to_string(), "b".to_string()]
        );
    }
}
