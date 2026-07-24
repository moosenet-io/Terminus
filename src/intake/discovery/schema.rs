//! DISC-01 (S114, TERM #251): the brochure's storage schema — types + SQL only.
//!
//! WHY THIS EXISTS — a distinct lifecycle stage from the Model Fleet Catalog:
//! [`crate::intake::catalog`] (`model_fleet_catalog`/`model_fleet_catalog_cell`,
//! MINT2-07/08) answers "what has been TESTED, and how did it score?" for
//! models already in the fleet. This module's `model_discovery_candidate` table
//! answers a different, earlier question: "what's a CANDIDATE — newly available
//! on HuggingFace, not yet acquired or tested?" The two relate ONLY by a
//! `model_name` join; brochure fields never get added to the fleet-catalog cell
//! table, and fleet-catalog fields never get added here (see the S114 grounding
//! summary's "naming footgun avoided" note — this registry is always called the
//! **brochure**, never "catalog," to keep it distinct from both Terminus's model
//! fleet catalog and Chord's unrelated MCP *tool* catalog).
//!
//! This item is STORAGE ONLY: [`FleetCategory`]/[`CandidateStatus`] (mirroring
//! [`crate::intake::catalog::CoverageStatus`]'s explicit-enum-with-`as_str()`
//! pattern, plus a `from_str()` this module adds since callers — DISC-02's tool
//! args, DISC-03's transition API — need to parse an untrusted string back into
//! an enum, which `CoverageStatus` itself never needed to do), the
//! [`DiscoveryCandidate`] row type, and the migration SQL live here. No business
//! logic: DISC-03 owns the upsert/transition API that actually writes rows.

use crate::error::ToolError;

/// Which fleet category a candidate targets. Snake_case `as_str()`/`from_str()`
/// round-trip; an unrecognized string is a clean [`ToolError::InvalidArgument`],
/// never a silent default — every caller (DISC-02's tool filter, DISC-03's
/// upsert path) must handle "I don't know that category" explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FleetCategory {
    ToolRouter,
    WriterSlm,
    Assistant,
    Coder,
    Embedding,
    Visual,
    Voice,
    /// MINT-DIFF-01: diffusion language models (DiffusionGemma/dgem-shaped) —
    /// generate via a fixed-canvas-block daemon, not Ollama's token-stream
    /// wire protocol. See `crate::intake::newcats::diffusion`.
    Diffusion,
}

impl FleetCategory {
    /// All variants, in the spec's documented order — used by tests and by any
    /// caller that needs to enumerate every category (e.g. DISC-06's per-
    /// category refresh loop).
    pub const ALL: [FleetCategory; 8] = [
        FleetCategory::ToolRouter,
        FleetCategory::WriterSlm,
        FleetCategory::Assistant,
        FleetCategory::Coder,
        FleetCategory::Embedding,
        FleetCategory::Visual,
        FleetCategory::Voice,
        FleetCategory::Diffusion,
    ];

    /// The stable snake_case key persisted to `model_discovery_candidate.category`.
    pub fn as_str(&self) -> &'static str {
        match self {
            FleetCategory::ToolRouter => "tool_router",
            FleetCategory::WriterSlm => "writer_slm",
            FleetCategory::Assistant => "assistant",
            FleetCategory::Coder => "coder",
            FleetCategory::Embedding => "embedding",
            FleetCategory::Visual => "visual",
            FleetCategory::Voice => "voice",
            FleetCategory::Diffusion => "diffusion",
        }
    }

    /// Parse a persisted/queried category string. An unrecognized value is a
    /// clean [`ToolError::InvalidArgument`] naming the bad input — never a
    /// silent default to some "unknown" variant, matching DISC-01's acceptance
    /// criteria.
    pub fn from_str(s: &str) -> Result<Self, ToolError> {
        match s {
            "tool_router" => Ok(FleetCategory::ToolRouter),
            "writer_slm" => Ok(FleetCategory::WriterSlm),
            "assistant" => Ok(FleetCategory::Assistant),
            "coder" => Ok(FleetCategory::Coder),
            "embedding" => Ok(FleetCategory::Embedding),
            "visual" => Ok(FleetCategory::Visual),
            "voice" => Ok(FleetCategory::Voice),
            "diffusion" => Ok(FleetCategory::Diffusion),
            other => Err(ToolError::InvalidArgument(format!(
                "unrecognized fleet category '{other}' (expected one of: tool_router, \
                 writer_slm, assistant, coder, embedding, visual, voice, diffusion)"
            ))),
        }
    }
}

/// A candidate's lifecycle status. `Discovered` is the entry state (DISC-06
/// found it, nothing fetched yet); `Evicted` is the only terminal-but-
/// re-enterable state (DISC-06 documents the one allowed `Evicted` →
/// `Discovered` re-entry transition when a pruned model reappears in a later
/// HF listing — enforced by DISC-03, not this module).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CandidateStatus {
    /// Found by DISC-06's discovery refresh, not yet fetched.
    Discovered,
    /// DISC-08's fetch is in flight (also the concurrency guard — see DISC-08).
    Fetching,
    /// Present in the cold archive, not yet marked for a fleet sweep.
    ColdStored,
    /// DISC-11 flipped it: sweep queued/running.
    MarkedForFleet,
    /// The fleet catalog now has a run/stale cell for this model.
    Swept,
    /// DISC-13 pruned the archive copy; `retained_profile` is populated.
    Evicted,
    /// Failed the VRAM/gfx1151 fit check — never fetched.
    Rejected,
}

impl CandidateStatus {
    /// All variants, in the spec's documented lifecycle order.
    pub const ALL: [CandidateStatus; 7] = [
        CandidateStatus::Discovered,
        CandidateStatus::Fetching,
        CandidateStatus::ColdStored,
        CandidateStatus::MarkedForFleet,
        CandidateStatus::Swept,
        CandidateStatus::Evicted,
        CandidateStatus::Rejected,
    ];

    /// The stable snake_case key persisted to `model_discovery_candidate.status`.
    pub fn as_str(&self) -> &'static str {
        match self {
            CandidateStatus::Discovered => "discovered",
            CandidateStatus::Fetching => "fetching",
            CandidateStatus::ColdStored => "cold_stored",
            CandidateStatus::MarkedForFleet => "marked_for_fleet",
            CandidateStatus::Swept => "swept",
            CandidateStatus::Evicted => "evicted",
            CandidateStatus::Rejected => "rejected",
        }
    }

    /// Parse a persisted/queried status string. An unrecognized value is a
    /// clean [`ToolError::InvalidArgument`] naming the bad input — never a
    /// silent default, matching DISC-01's acceptance criteria.
    pub fn from_str(s: &str) -> Result<Self, ToolError> {
        match s {
            "discovered" => Ok(CandidateStatus::Discovered),
            "fetching" => Ok(CandidateStatus::Fetching),
            "cold_stored" => Ok(CandidateStatus::ColdStored),
            "marked_for_fleet" => Ok(CandidateStatus::MarkedForFleet),
            "swept" => Ok(CandidateStatus::Swept),
            "evicted" => Ok(CandidateStatus::Evicted),
            "rejected" => Ok(CandidateStatus::Rejected),
            other => Err(ToolError::InvalidArgument(format!(
                "unrecognized candidate status '{other}' (expected one of: discovered, \
                 fetching, cold_stored, marked_for_fleet, swept, evicted, rejected)"
            ))),
        }
    }

    /// The statuses a candidate may legally move to FROM this one, per DISC-03's
    /// `transition_status` enforcement. Pure — unit-testable without a DB. This
    /// is a storage-schema-level DECLARATION of the state machine; DISC-03 is
    /// the only code path that actually calls it to gate a write.
    ///
    /// `Evicted` is documented as re-enterable to `Discovered` ONLY (DISC-06's
    /// one allowed re-entry transition, when a pruned model reappears in a
    /// fresh HF listing) — every other terminal-looking edge here is exactly
    /// what DISC-03's own doc comment enumerates.
    pub fn valid_transitions(&self) -> &'static [CandidateStatus] {
        match self {
            CandidateStatus::Discovered => {
                &[CandidateStatus::Fetching, CandidateStatus::Rejected]
            }
            CandidateStatus::Fetching => {
                &[CandidateStatus::ColdStored, CandidateStatus::Discovered]
            }
            CandidateStatus::ColdStored => &[CandidateStatus::MarkedForFleet],
            CandidateStatus::MarkedForFleet => &[CandidateStatus::Swept],
            CandidateStatus::Swept => &[CandidateStatus::Evicted],
            CandidateStatus::Evicted => &[CandidateStatus::Discovered],
            CandidateStatus::Rejected => &[],
        }
    }
}

/// The profiling MODALITY of a discovery candidate (CB-02, S125, TERM #519) —
/// the finer capability axis that says WHICH profiling suite would characterize
/// this model, so a fleet sweep can auto-route a candidate to the right suite.
///
/// Distinct from [`FleetCategory`], the coarse discovery-LISTING role. Where
/// `FleetCategory` answers "which HF listing bucket did we find this under?"
/// (`visual`/`voice`/`embedding`/…), `Modality` answers the question the sweep
/// actually needs: "given its `pipeline_tag`/`tags`, which ONE profiling suite
/// (`embedding_retrieval` / `reranking` / `vision_qa` / `tool_routing` /
/// `document_parsing` / `image_generation` / `stt` / `tts`) should measure it?"
///
/// It deliberately SPLITS the coarse roles the brochure could not distinguish
/// before:
/// - the coarse `visual` role → [`Modality::Vlm`] (image-analysis / vision-QA)
///   vs [`Modality::ImageGen`] (text-to-image), and
/// - the coarse `voice` role (which meant ASR/STT only) → [`Modality::Stt`]
///   (speech-to-text) vs [`Modality::Tts`] (text-to-speech),
/// and adds [`Modality::Rerank`] + [`Modality::DocumentParsing`], which had no
/// coarse-category home at all (a reranker or an OCR/doc model is often listed
/// under `embedding`/`visual` yet needs an entirely different suite).
///
/// Populated by [`Modality::classify`] from an HF listing's `pipeline_tag` +
/// `tags` at discovery time. A plain chat/coder/writer LLM classifies as
/// [`Modality::TextGeneration`] — profiled by the classic
/// coder/assistant/agent test_types, NOT a newcats specialized suite, so
/// [`Modality::suite`] is `None` for it. A genuinely unrecognizable listing
/// yields `None` (persisted as SQL `NULL`) rather than a silent wrong default —
/// matching this module's "never silently default an enum" ethos.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Modality {
    /// Text embeddings → `embedding_retrieval` suite.
    Embedding,
    /// Cross-encoder / listwise reranking → `reranking` suite.
    Rerank,
    /// Vision-language / image-analysis / visual-QA → `vision_qa` suite.
    Vlm,
    /// Tool-use / function-calling / router LLM → `tool_routing` suite.
    ToolRouting,
    /// OCR / document / DocVQA parsing → `document_parsing` suite.
    DocumentParsing,
    /// Text-to-image generation → `image_generation` suite.
    ImageGen,
    /// Automatic speech recognition (speech-to-text) → `stt` suite.
    Stt,
    /// Text-to-speech synthesis → `tts` suite.
    Tts,
    /// A plain generative text LLM (chat/coder/writer/diffusion-LLM) with no
    /// specialized newcats suite — profiled by the classic
    /// coder/assistant/agent test_types. [`Modality::suite`] returns `None`.
    TextGeneration,
}

impl Modality {
    /// All variants, in a stable documented order — used by tests and any
    /// caller that needs to enumerate every modality.
    pub const ALL: [Modality; 9] = [
        Modality::Embedding,
        Modality::Rerank,
        Modality::Vlm,
        Modality::ToolRouting,
        Modality::DocumentParsing,
        Modality::ImageGen,
        Modality::Stt,
        Modality::Tts,
        Modality::TextGeneration,
    ];

    /// The stable snake_case key persisted to `model_discovery_candidate.modality`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Modality::Embedding => "embedding",
            Modality::Rerank => "rerank",
            Modality::Vlm => "vlm",
            Modality::ToolRouting => "tool_routing",
            Modality::DocumentParsing => "document_parsing",
            Modality::ImageGen => "image_gen",
            Modality::Stt => "stt",
            Modality::Tts => "tts",
            Modality::TextGeneration => "text_generation",
        }
    }

    /// The MINT profiling suite (`task_category` / `test_type`) whose harness
    /// would profile a candidate of this modality — the actual auto-route
    /// target a fleet sweep uses. `None` for [`Modality::TextGeneration`],
    /// which has no specialized newcats suite (it is covered by the classic
    /// coder/assistant/agent test_types instead). Suite names match the MINT
    /// suite guide (`embedding_retrieval`/`reranking`/`vision_qa`/`tool_routing`/
    /// `document_parsing`/`image_generation`/`stt`/`tts`).
    pub fn suite(&self) -> Option<&'static str> {
        match self {
            Modality::Embedding => Some("embedding_retrieval"),
            Modality::Rerank => Some("reranking"),
            Modality::Vlm => Some("vision_qa"),
            Modality::ToolRouting => Some("tool_routing"),
            Modality::DocumentParsing => Some("document_parsing"),
            Modality::ImageGen => Some("image_generation"),
            Modality::Stt => Some("stt"),
            Modality::Tts => Some("tts"),
            Modality::TextGeneration => None,
        }
    }

    /// Parse a persisted/queried modality string. An unrecognized value is a
    /// clean [`ToolError::InvalidArgument`] naming the bad input — never a
    /// silent default, matching this module's [`FleetCategory`]/
    /// [`CandidateStatus`] convention. (SQL `NULL` — an unclassified candidate
    /// — is represented as `Option::None` by the storage layer and never
    /// reaches this function.)
    pub fn from_str(s: &str) -> Result<Self, ToolError> {
        match s {
            "embedding" => Ok(Modality::Embedding),
            "rerank" => Ok(Modality::Rerank),
            "vlm" => Ok(Modality::Vlm),
            "tool_routing" => Ok(Modality::ToolRouting),
            "document_parsing" => Ok(Modality::DocumentParsing),
            "image_gen" => Ok(Modality::ImageGen),
            "stt" => Ok(Modality::Stt),
            "tts" => Ok(Modality::Tts),
            "text_generation" => Ok(Modality::TextGeneration),
            other => Err(ToolError::InvalidArgument(format!(
                "unrecognized modality '{other}' (expected one of: embedding, rerank, vlm, \
                 tool_routing, document_parsing, image_gen, stt, tts, text_generation)"
            ))),
        }
    }

    /// Classify an HF listing into a profiling [`Modality`] from its
    /// `pipeline_tag` (HF's canonical task label) plus its free-text `tags`.
    /// Pure and deterministic — the CB-02 classifier heuristic, unit-tested
    /// without any network.
    ///
    /// Precedence is deliberate: strong TAG signals win first, because a
    /// reranker or an OCR/doc model is frequently listed under a coarser
    /// bucket (`embedding`/`visual`) whose `pipeline_tag` would otherwise
    /// mis-route it. The canonical `pipeline_tag` is consulted next, and a few
    /// tag-only fallbacks last. An input that matches nothing returns `None`
    /// (an honest "unclassified", persisted as `NULL`) rather than guessing.
    pub fn classify(pipeline_tag: Option<&str>, tags: &[String]) -> Option<Modality> {
        let pt = pipeline_tag.map(|s| s.trim().to_lowercase());
        let pt = pt.as_deref();
        // Tokenize a tag/needle on any non-alphanumeric separator (space, hyphen,
        // underscore, slash, dot, …) into whole lowercase word tokens.
        fn tag_tokens(s: &str) -> Vec<String> {
            s.to_lowercase()
                .split(|c: char| !c.is_alphanumeric())
                .filter(|t| !t.is_empty())
                .map(|t| t.to_string())
                .collect()
        }
        // Word-boundary tag match. A coarse `contains(substr)` false-positives on
        // short signals (e.g. "socratic" contains "ocr", "disaster" contains
        // "asr"); instead we require the needle's token SEQUENCE to appear as a
        // contiguous run of WHOLE tokens in some tag. For a single-token needle of
        // length ≥ 4 we also accept a token that BEGINS with it (so "rerank"
        // matches "reranker"/"reranking", "agent" matches "agentic") — never a
        // mid-word substring, and never for the 3-char traps (ocr/asr/tts/vqa),
        // which must match a whole token exactly.
        let has = |needle: &str| {
            let nt = tag_tokens(needle);
            if nt.is_empty() {
                return false;
            }
            tags.iter().any(|tag| {
                let tt = tag_tokens(tag);
                if nt.len() == 1 {
                    let n = nt[0].as_str();
                    tt.iter()
                        .any(|t| t == n || (n.len() >= 4 && t.starts_with(n)))
                } else {
                    tt.windows(nt.len()).any(|w| w == nt.as_slice())
                }
            })
        };

        // 1. Strong tag overrides — capabilities a coarse category hides.
        if has("rerank") || has("cross-encoder") || has("cross_encoder") {
            return Some(Modality::Rerank);
        }
        if has("ocr")
            || has("docvqa")
            || has("document-question-answering")
            || has("document-parsing")
            || has("document_parsing")
        {
            return Some(Modality::DocumentParsing);
        }

        // 2. HF's canonical pipeline_tag.
        match pt {
            Some("feature-extraction") | Some("sentence-similarity") => Some(Modality::Embedding),
            Some("text-ranking") => Some(Modality::Rerank),
            Some("automatic-speech-recognition") => Some(Modality::Stt),
            Some("text-to-speech") | Some("text-to-audio") => Some(Modality::Tts),
            Some("text-to-image") => Some(Modality::ImageGen),
            Some("document-question-answering") => Some(Modality::DocumentParsing),
            Some("image-text-to-text")
            | Some("visual-question-answering")
            | Some("image-to-text") => Some(Modality::Vlm),
            Some("text-generation") | Some("text2text-generation") => {
                // Refine a plain text LLM: a tool/function-calling/router model
                // is a tool_routing candidate; everything else is generic
                // text-generation (no specialized suite).
                if has("function-calling")
                    || has("function_call")
                    || has("tool-use")
                    || has("tool_use")
                    || has("tool-calling")
                    || has("agent")
                {
                    Some(Modality::ToolRouting)
                } else {
                    Some(Modality::TextGeneration)
                }
            }
            // 3. No / unrecognized pipeline_tag: last-ditch tag fallbacks.
            _ => {
                if has("text-to-speech") || has("tts") {
                    Some(Modality::Tts)
                } else if has("automatic-speech-recognition")
                    || has("speech-recognition")
                    || has("asr")
                {
                    Some(Modality::Stt)
                } else if has("text-to-image")
                    || has("stable-diffusion")
                    || has("diffusers")
                {
                    Some(Modality::ImageGen)
                } else {
                    None
                }
            }
        }
    }
}

/// One `model_discovery_candidate` row. Mirrors the table's columns 1:1;
/// timestamps are `None` until the corresponding lifecycle event sets them
/// (DISC-03 owns every write; this is a plain owned struct with no DB access).
#[derive(Debug, Clone, PartialEq)]
pub struct DiscoveryCandidate {
    /// Primary key. Matches `model_fleet_catalog.model_name` byte-for-byte per
    /// the S83 join convention `acquire.rs` documents.
    pub model_name: String,
    pub hf_repo: String,
    pub category: FleetCategory,
    pub status: CandidateStatus,
    /// The profiling [`Modality`] this candidate was classified into (CB-02) —
    /// the finer axis that lets a fleet sweep auto-route it to the right suite
    /// (see [`Modality::suite`]). `None` when the HF listing carried no signal
    /// the classifier recognized (persisted as SQL `NULL`); DISC-06's daily
    /// refresh recomputes it via [`Modality::classify`] and a later
    /// (non-`NULL`) classification never gets erased by a subsequent `NULL`
    /// re-observation (see `upsert.rs`).
    pub modality: Option<Modality>,
    /// `acquire.rs::Gfx1151Class::as_str()` value — kept as a plain string here
    /// (not a second copy of that enum) since this module never branches on it;
    /// DISC-05's classifier owns the enum-to-string conversion.
    pub gfx1151_class: String,
    pub size_b: Option<f64>,
    pub vram_footprint_gb: Option<f64>,
    /// Free text: which DISC-04 signal found it (e.g. `"hf_trending"`).
    pub discovery_source: String,
    /// The numeric signal DISC-05 computed (HF likes/downloads/trending, or a
    /// real leaderboard score once available — see the spec's open question 3).
    pub discovery_score: Option<f64>,
    pub discovered_at: chrono::DateTime<chrono::Utc>,
    /// Bumped every refresh a still-listed candidate is re-observed, so
    /// staleness is queryable.
    pub last_seen_at: chrono::DateTime<chrono::Utc>,
    pub fetched_at: Option<chrono::DateTime<chrono::Utc>>,
    pub marked_for_fleet_at: Option<chrono::DateTime<chrono::Utc>>,
    pub evicted_at: Option<chrono::DateTime<chrono::Utc>>,
    /// `NULL` until an eviction populates it (DISC-13, via DISC-03's
    /// `record_eviction` — the ONLY permitted write site). Invariant:
    /// populated ⟺ `status == Evicted` (see `model.rs`'s EDGE CASES doc / the
    /// migration's comment — enforced at the application layer by DISC-03,
    /// not a DB CHECK, since the existing migration style has no precedent for
    /// a cross-column CHECK and this crate's convention is to enforce such
    /// invariants at the write-API layer, matching `record_eviction`'s
    /// "the ONLY call site" design in DISC-03).
    pub retained_profile: Option<serde_json::Value>,
    /// Free text, mirrors `Nomination::rationale` — DISC-08's failure reason,
    /// DISC-05's classification rationale, etc.
    pub rationale: Option<String>,
}

/// The migration SQL, applied out-of-band by an operator (matching
/// `model_fleet_catalog`'s MINT2-07 convention — `src/intake/storage.rs` is
/// authoritative that the harness only INSERTs/SELECTs, never issues DDL). This
/// constant exists so a test can assert the SQL text is well-formed / contains
/// the expected guards without needing a live Postgres; the canonical copy that
/// an operator actually applies lives in `migrations/` (see
/// `S114-disc01-brochure.sql`), kept byte-identical to this constant.
pub const MODEL_DISCOVERY_CANDIDATE_MIGRATION_SQL: &str = include_str!(
    "../../../migrations/S114-disc01-brochure.sql"
);

/// CB-02 (S125, TERM #519) additive migration: adds the nullable `modality`
/// column (+ its query index) to `model_discovery_candidate`, so each candidate
/// carries the profiling modality whose suite would characterize it (see
/// [`Modality`]). Additive/idempotent (`ADD COLUMN IF NOT EXISTS` +
/// `CREATE INDEX IF NOT EXISTS`), applied out-of-band by an operator exactly
/// like the DISC-01 migration above. Kept byte-identical to the canonical copy
/// in `migrations/`; the const exists so a test can assert its shape without a
/// live Postgres.
pub const MODEL_DISCOVERY_MODALITY_MIGRATION_SQL: &str = include_str!(
    "../../../migrations/S125-cb02-discovery-modality.sql"
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fleet_category_round_trips_every_variant() {
        for cat in FleetCategory::ALL {
            let s = cat.as_str();
            let parsed = FleetCategory::from_str(s).expect("round trip");
            assert_eq!(parsed, cat, "round trip failed for {s}");
        }
    }

    #[test]
    fn fleet_category_rejects_unrecognized_string() {
        let err = FleetCategory::from_str("not_a_category").unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[test]
    fn candidate_status_round_trips_every_variant() {
        for status in CandidateStatus::ALL {
            let s = status.as_str();
            let parsed = CandidateStatus::from_str(s).expect("round trip");
            assert_eq!(parsed, status, "round trip failed for {s}");
        }
    }

    #[test]
    fn candidate_status_rejects_unrecognized_string() {
        let err = CandidateStatus::from_str("not_a_status").unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[test]
    fn candidate_status_as_str_values_are_snake_case_and_stable() {
        // Locks the exact persisted strings (a rename here is a migration, not
        // a refactor) so a future edit doesn't accidentally reshuffle them.
        assert_eq!(CandidateStatus::Discovered.as_str(), "discovered");
        assert_eq!(CandidateStatus::Fetching.as_str(), "fetching");
        assert_eq!(CandidateStatus::ColdStored.as_str(), "cold_stored");
        assert_eq!(CandidateStatus::MarkedForFleet.as_str(), "marked_for_fleet");
        assert_eq!(CandidateStatus::Swept.as_str(), "swept");
        assert_eq!(CandidateStatus::Evicted.as_str(), "evicted");
        assert_eq!(CandidateStatus::Rejected.as_str(), "rejected");
    }

    #[test]
    fn evicted_re_enters_only_to_discovered() {
        assert_eq!(
            CandidateStatus::Evicted.valid_transitions(),
            &[CandidateStatus::Discovered]
        );
    }

    #[test]
    fn rejected_is_terminal() {
        assert!(CandidateStatus::Rejected.valid_transitions().is_empty());
    }

    // ---- Modality (CB-02) ----

    #[test]
    fn modality_round_trips_every_variant() {
        for m in Modality::ALL {
            let s = m.as_str();
            assert_eq!(Modality::from_str(s).expect("round trip"), m, "round trip for {s}");
        }
    }

    #[test]
    fn modality_rejects_unrecognized_string() {
        let err = Modality::from_str("not_a_modality").unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[test]
    fn modality_as_str_values_are_stable_snake_case() {
        // Locks the persisted strings — a rename here is a migration, not a refactor.
        assert_eq!(Modality::Embedding.as_str(), "embedding");
        assert_eq!(Modality::Rerank.as_str(), "rerank");
        assert_eq!(Modality::Vlm.as_str(), "vlm");
        assert_eq!(Modality::ToolRouting.as_str(), "tool_routing");
        assert_eq!(Modality::DocumentParsing.as_str(), "document_parsing");
        assert_eq!(Modality::ImageGen.as_str(), "image_gen");
        assert_eq!(Modality::Stt.as_str(), "stt");
        assert_eq!(Modality::Tts.as_str(), "tts");
        assert_eq!(Modality::TextGeneration.as_str(), "text_generation");
    }

    #[test]
    fn modality_suite_maps_specialized_variants_and_none_for_text_generation() {
        assert_eq!(Modality::Embedding.suite(), Some("embedding_retrieval"));
        assert_eq!(Modality::Rerank.suite(), Some("reranking"));
        assert_eq!(Modality::Vlm.suite(), Some("vision_qa"));
        assert_eq!(Modality::ToolRouting.suite(), Some("tool_routing"));
        assert_eq!(Modality::DocumentParsing.suite(), Some("document_parsing"));
        assert_eq!(Modality::ImageGen.suite(), Some("image_generation"));
        assert_eq!(Modality::Stt.suite(), Some("stt"));
        assert_eq!(Modality::Tts.suite(), Some("tts"));
        // The only variant with no specialized newcats suite.
        assert_eq!(Modality::TextGeneration.suite(), None);
    }

    #[test]
    fn every_specialized_modality_has_a_suite() {
        for m in Modality::ALL {
            if m == Modality::TextGeneration {
                continue;
            }
            assert!(m.suite().is_some(), "{} must map to a suite", m.as_str());
        }
    }

    fn t(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn classify_pipeline_tag_maps_the_canonical_tasks() {
        assert_eq!(Modality::classify(Some("feature-extraction"), &[]), Some(Modality::Embedding));
        assert_eq!(Modality::classify(Some("sentence-similarity"), &[]), Some(Modality::Embedding));
        assert_eq!(Modality::classify(Some("text-ranking"), &[]), Some(Modality::Rerank));
        assert_eq!(
            Modality::classify(Some("automatic-speech-recognition"), &[]),
            Some(Modality::Stt)
        );
        assert_eq!(Modality::classify(Some("text-to-speech"), &[]), Some(Modality::Tts));
        assert_eq!(Modality::classify(Some("text-to-image"), &[]), Some(Modality::ImageGen));
        assert_eq!(
            Modality::classify(Some("document-question-answering"), &[]),
            Some(Modality::DocumentParsing)
        );
    }

    #[test]
    fn classify_splits_the_coarse_visual_role_into_vlm_and_image_gen() {
        // image-analysis / vision-QA VLM ...
        assert_eq!(Modality::classify(Some("image-text-to-text"), &[]), Some(Modality::Vlm));
        assert_eq!(
            Modality::classify(Some("visual-question-answering"), &[]),
            Some(Modality::Vlm)
        );
        // ... vs a text-to-image generator — the split CB-02 introduces.
        assert_eq!(Modality::classify(Some("text-to-image"), &[]), Some(Modality::ImageGen));
    }

    #[test]
    fn classify_splits_the_coarse_voice_role_into_stt_and_tts() {
        assert_eq!(
            Modality::classify(Some("automatic-speech-recognition"), &[]),
            Some(Modality::Stt)
        );
        assert_eq!(Modality::classify(Some("text-to-speech"), &[]), Some(Modality::Tts));
    }

    #[test]
    fn classify_tags_override_a_coarser_pipeline_tag() {
        // A reranker listed under the coarse `embedding` bucket
        // (pipeline_tag=feature-extraction) is still routed to reranking.
        assert_eq!(
            Modality::classify(Some("feature-extraction"), &t(&["cross-encoder", "reranker"])),
            Some(Modality::Rerank)
        );
        // An OCR/doc model listed under the coarse `visual` bucket goes to
        // document_parsing, not plain vision_qa.
        assert_eq!(
            Modality::classify(Some("image-text-to-text"), &t(&["ocr", "document"])),
            Some(Modality::DocumentParsing)
        );
    }

    #[test]
    fn classify_refines_text_generation_into_tool_routing_by_tags() {
        assert_eq!(
            Modality::classify(Some("text-generation"), &t(&["function-calling"])),
            Some(Modality::ToolRouting)
        );
        // A plain chat/coder LLM stays generic text-generation.
        assert_eq!(
            Modality::classify(Some("text-generation"), &t(&["chat"])),
            Some(Modality::TextGeneration)
        );
    }

    #[test]
    fn classify_unrecognized_listing_is_none_not_a_guess() {
        assert_eq!(Modality::classify(None, &[]), None);
        assert_eq!(Modality::classify(Some("some-future-task"), &[]), None);
    }

    /// CB-02 b2fix (finding 3): short signals match on WORD boundaries, so a
    /// substring trap like "socratic" (contains "ocr") or "disaster" (contains
    /// "asr") does NOT misclassify — while real whole-token tags still do.
    #[test]
    fn classify_does_not_substring_false_positive_on_short_signals() {
        // "socratic" contains "ocr" as a substring but is not an OCR/doc model.
        assert_eq!(
            Modality::classify(Some("text-generation"), &t(&["socratic", "chat"])),
            Some(Modality::TextGeneration),
            "a substring 'ocr' inside 'socratic' must not route to DocumentParsing"
        );
        // No pipeline tag + only substring-trap tags → honest None, not a guess.
        assert_eq!(Modality::classify(None, &t(&["socratic"])), None);
        assert_eq!(Modality::classify(None, &t(&["disaster"])), None); // contains "asr"
        assert_eq!(Modality::classify(None, &t(&["mattstseason"])), None); // contains "tts"
        // Real whole-token tags still classify correctly.
        assert_eq!(Modality::classify(None, &t(&["ocr"])), Some(Modality::DocumentParsing));
        assert_eq!(
            Modality::classify(None, &t(&["document-question-answering"])),
            Some(Modality::DocumentParsing)
        );
        assert_eq!(Modality::classify(None, &t(&["asr"])), Some(Modality::Stt));
        // A ≥4-char signal still tolerates a morphological suffix (reranker).
        assert_eq!(Modality::classify(None, &t(&["reranker"])), Some(Modality::Rerank));
    }

    #[test]
    fn modality_migration_sql_adds_the_column_and_index() {
        let sql = MODEL_DISCOVERY_MODALITY_MIGRATION_SQL;
        assert!(sql.contains("ALTER TABLE model_discovery_candidate"));
        assert!(sql.contains("ADD COLUMN IF NOT EXISTS modality"));
        assert!(sql.contains("idx_discovery_candidate_modality"));
    }

    #[test]
    fn migration_sql_creates_the_table_with_unique_model_name() {
        let sql = MODEL_DISCOVERY_CANDIDATE_MIGRATION_SQL;
        assert!(sql.contains("CREATE TABLE IF NOT EXISTS model_discovery_candidate"));
        assert!(sql.contains("PRIMARY KEY (model_name)"));
        assert!(sql.contains("idx_discovery_candidate_status"));
        assert!(sql.contains("idx_discovery_candidate_category"));
        assert!(sql.contains("idx_discovery_candidate_last_seen"));
    }
}
