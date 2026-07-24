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
//!     synthetic BMP generator for the sanity test. SUITE-VQA (S125) wires this
//!     module into the fleet as the `vision_qa` suite: on top of the caption
//!     path it adds a vision-QA path (image + question → short answer) scored by
//!     lenient accuracy / caption similarity / hallucination / latency / VRAM,
//!     driven by `runner::run_vision_qa_suite` over an `INTAKE_CORPUS_DIR`
//!     manifest, calling Chord's `/v1/chat/completions` image content part via
//!     `infer::vision_infer_with_metrics`. Catalog family `TEST_TYPE_VISION_QA`.
//!   - [`image_generation`]    — `task_category = "image_generation"`: DIFFERENT
//!     metric shape (no tokens/accuracy) — success bool, time-to-image ms,
//!     VRAM peak MB, plus a SCAFFOLDED CLIP prompt-adherence metric (emitted
//!     only when a CLIP score is measured; NOT measured on this box → cleanly
//!     absent). SUITE-IMG (S125): now WIRED — a `TEST_TYPE_IMAGE_GENERATION`
//!     const + catalog cell, a `run_image_generation_suite` driver in
//!     `runner.rs`, and a live backend call through
//!     `intake::infer::imagegen_with_metrics` (Chord's OpenAI-compatible
//!     `/v1/images/generations` route, sd-turbo diffusers behind it). Prompts
//!     load from `INTAKE_CORPUS_DIR/image_generation.json` with an in-source
//!     default set. Unit tests still exercise the scoring/write path against a
//!     mock `GenerationOutcome` and the backend parse-path via httpmock.
//!   - [`voice_transcription`] — `task_category = "voice_transcription"`: ASR
//!     transcript vs reference, scored by word-error-rate (WER).
//!   - [`tts`]                  — `task_category = "tts"` (S125 SUITE-TTS):
//!     text-to-speech via Chord `/v1/audio/speech`, scored END-TO-END by an
//!     STT-loopback (synthesize → transcribe → WER vs the input text) plus a
//!     scaffold acoustic MOS-proxy and a Real-Time Factor (RTF). Like
//!     `diffusion`, emits both a QUALITY dimension (`tts_intelligibility`) and a
//!     PERFORMANCE dimension (`tts_performance`).
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
//!   - [`reranking`]            — `task_category = "reranking"` (SUITE-RRK):
//!     cross-encoder reranking probe. Emits nDCG UPLIFT over a bi-encoder
//!     baseline (`ndcg_uplift`), the reranked/baseline nDCG, and rerank
//!     `latency_ms`. The live backend call goes through
//!     `intake::infer::rerank_with_metrics`'s `kind == "openai"` arm
//!     (`POST /v1/rerank`, bge-reranker-v2-m3); scoring (pure nDCG) is
//!     unit-tested against a mock ordering. Corpus via `INTAKE_CORPUS_DIR`.
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
pub mod reranking;
pub mod text_similarity;
pub mod tool_routing;
pub mod tts;
pub mod voice_transcription;
