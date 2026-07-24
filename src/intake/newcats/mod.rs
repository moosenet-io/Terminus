//! New model-benchmarking categories (MINT new-model-types extension).
//!
//! Four additive measurement harnesses, sibling in spirit to
//! `assistant::dim1_conversation` .. `dim6_embeddings` but each scoring a
//! DIFFERENT modality/capability than the assistant-persona dims. All four
//! write through [`super::assistant::schema::insert_dimension_score_with_category`]
//! with a `task_category` distinct from `"assistant"`/`"coder"`, so existing
//! reporting/reconciliation over `assistant_dimension_score` is untouched
//! (queries that don't filter `task_category` will now also see these rows —
//! by design, see `schema::migrate`'s doc comment on the column).
//!
//! ## Modules
//!   - [`document_parsing`]    — `task_category = "document_parsing"`: doc/form
//!     text → structured output, scored by field-level accuracy.
//!   - [`image_parsing`]       — `task_category = "image_parsing"`: image →
//!     description, scored by caption similarity. Ships a dependency-free
//!     synthetic BMP generator for the sanity test.
//!   - [`image_generation`]    — `task_category = "image_generation"`: DIFFERENT
//!     metric shape (no tokens/accuracy) — success bool, time-to-image ms,
//!     VRAM peak MB. No generation backend exists on this box yet; only the
//!     scoring/write path is exercised (against a mock result).
//!   - [`voice_transcription`] — `task_category = "voice_transcription"`: ASR
//!     transcript vs reference, scored by word-error-rate (WER).
//!   - [`diffusion`]           — `task_category = "diffusion"` (MINT-DIFF-01):
//!     diffusion-language-model (DiffusionGemma/dgem) probe. Unlike the other
//!     four, emits TWO dimensions per use-case: use-case QUALITY
//!     (`diffusion_use_case`/`use_case_success`, word-overlap vs a reference
//!     answer) AND PERFORMANCE (`diffusion_performance`: `time_to_output_ms`,
//!     `vram_peak_mb`, `blocks_per_sec` — never a token/sec number, since
//!     diffusion generates in fixed canvas blocks, not a token stream). The
//!     live backend call goes through `intake::infer::infer_with_metrics`'s
//!     `kind == "daemon"` arm; this module's tests exercise scoring only.
//!   - [`embedding_retrieval`] — `task_category = "embedding_retrieval"`
//!     (SUITE-EMB): the IR-quality suite for TEXT-EMBEDDING models. Promotes the
//!     `assistant::dim6_embeddings` precursor into a fleet-wired suite — reuses
//!     its precision/recall/MRR/nDCG + public-vs-domain-delta machinery and adds
//!     the newcats surface (`INTAKE_CORPUS_DIR` loader, throughput metric,
//!     `score_and_write`). Backend seam = Chord `/v1/embeddings` via
//!     `infer::embed_with_metrics`'s `openai_embed` arm.
//!   - [`tool_routing`]       — `task_category = "tool_routing"` (S125 SUITE-TOOL):
//!     the first-class tool-routing / function-calling profiler. Unlike the four
//!     synthetic-corpus siblings it REUSES the `agent` suite's scenario corpus
//!     (`agent-scenarios.json`) + tool-catalog builder + multi-step scorer, but
//!     routes through Chord's OpenAI-compatible `/v1/chat/completions` `tools`
//!     path (`infer::tool_infer_with_metrics`) and scores discrete metrics
//!     (`correct_tool_at_1`, `parameter_validity`, `decoy_rejection`,
//!     `multi_step_success`). The legacy `agent` suite is left untouched.
//!
//! ## Shared scoring primitives
//! [`text_similarity`] holds the small, dependency-free string-similarity
//! helpers (`levenshtein`, `token_jaccard`, `word_error_rate`) shared by
//! `document_parsing`, `image_parsing`, and `voice_transcription`, so the three
//! text-comparison categories don't each reinvent edit distance.
//!
//! ## Testability pattern
//! Each category defines a small trait for "call the backend" (mirroring
//! `assistant::dim4_ocean::OceanModel`), so unit tests inject a synthetic/mock
//! implementation and exercise the SCORING logic without a live network call —
//! consistent with `document_parsing`/`image_parsing` being fully exercisable
//! tonight with synthetic data, independent of whether any real vision/ASR/
//! image-gen backend is reachable from this box.

pub mod diffusion;
pub mod document_parsing;
pub mod embedding_retrieval;
pub mod image_generation;
pub mod image_parsing;
pub mod text_similarity;
pub mod tool_routing;
pub mod voice_transcription;
