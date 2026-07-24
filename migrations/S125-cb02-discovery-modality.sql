-- S125 CB-02 (TERM #519): per-candidate MODALITY on the discovery "brochure"
-- (`model_discovery_candidate`, DISC-01, see `S114-disc01-brochure.sql`).
--
-- WHY: the brochure's coarse `category` (FleetCategory: tool_router | writer_slm
-- | assistant | coder | embedding | visual | voice | diffusion) records WHICH
-- HF listing bucket a candidate was found under, but it cannot say which
-- profiling SUITE would characterize the model — `visual` lumps vision-QA VLMs
-- together with text-to-image generators, `voice` means ASR/STT only (no TTS),
-- and rerankers / document-parsers have no bucket at all. This column adds the
-- finer axis: the classified `modality` whose MINT suite (embedding_retrieval |
-- reranking | vision_qa | tool_routing | document_parsing | image_generation |
-- stt | tts) a fleet sweep should auto-route the candidate to.
--
-- VALUES ('modality', see `src/intake/discovery/schema.rs` `Modality::as_str()`):
--   'embedding' | 'rerank' | 'vlm' | 'tool_routing' | 'document_parsing' |
--   'image_gen' | 'stt' | 'tts' | 'text_generation'
-- NULL = the HF listing carried no signal the CB-02 classifier
-- (`Modality::classify`) recognized; DISC-06's daily refresh recomputes it, and
-- a later non-NULL classification is never erased by a subsequent NULL
-- re-observation (COALESCE in `upsert.rs`).
--
-- Applied OUT-OF-BAND by an operator, NOT by the harness (matching the DISC-01
-- convention — `src/intake/storage.rs` only INSERTs/SELECTs, never issues DDL).
-- Additive, idempotent, non-destructive: `ADD COLUMN IF NOT EXISTS` +
-- `CREATE INDEX IF NOT EXISTS`, so re-applying is a safe no-op, and existing
-- rows simply carry a NULL modality until the next discovery refresh reclassifies
-- them. Depends only on `model_discovery_candidate` existing (the DISC-01
-- migration); does not touch any other table.

ALTER TABLE model_discovery_candidate
    ADD COLUMN IF NOT EXISTS modality TEXT;

-- Query axis a fleet sweep filters on to auto-target a suite by modality.
CREATE INDEX IF NOT EXISTS idx_discovery_candidate_modality
    ON model_discovery_candidate (modality);
