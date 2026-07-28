-- S127 Ask-4 (TERM): PRACTICAL-ranking metadata on the discovery "brochure"
-- (`model_discovery_candidate`, DISC-01, see `S114-disc01-brochure.sql`).
--
-- WHY: the original candidate ranking sorted purely on HuggingFace popularity
-- (`discovery_score` = downloads/likes/trending). Popularity is a hype meter, not
-- a fitness signal: it says nothing about whether a model actually RUNS on the
-- gfx1151 host, whether it is an assistant-suitable instruct/chat model, or how
-- recent it is. This migration adds the practical/hardware metadata the redesigned
-- blended `fit_score` (and the new hard servability/suitability filters) need —
-- all of it derivable from the model-info blob the MEASURE step already fetches
-- (public `/api/models/{repo}`, NO token, NO weight download):
--
--   published_at   HF `createdAt`   — recency lower bound.
--   updated_at     HF `lastModified`— primary recency signal.
--   license        HF cardData.license (fallback a `license:*` tag), lowercase.
--   arch           HF config.architectures[0] (fallback model_type/library_name)
--                  — drives gfx1151_class derivation (`classify_gfx1151`).
--   is_instruct    heuristic bool from pipeline_tag + tags + repo-id markers.
--   gated          HF model-info `gated` flag — a gated repo can't be auto-ingested.
--   quant_dtype    dominant key of safetensors.parameters — refines VRAM estimate.
--
-- NULL = the HF blob carried no signal the enrich step recognized (fail-soft per
-- field); a later non-NULL enrichment is never erased by a subsequent NULL
-- re-observation (COALESCE in `upsert.rs`). `arch`-derived `gfx1151_class` is
-- stored in the EXISTING `gfx1151_class` column (a derived value replaces the
-- 'unknown' sentinel; see `upsert.rs`), so no new gfx column is needed here.
--
-- Applied OUT-OF-BAND by an operator, NOT by the harness (matching the DISC-01 /
-- CB-02 convention — `src/intake/storage.rs` only INSERTs/SELECTs, never issues
-- DDL). Additive, idempotent, non-destructive: `ADD COLUMN IF NOT EXISTS` +
-- `CREATE INDEX IF NOT EXISTS`, so re-applying is a safe no-op, and existing rows
-- simply carry NULLs until the next MEASURE pass enriches them. Depends only on
-- `model_discovery_candidate` existing (the DISC-01 migration); touches no other
-- table.

ALTER TABLE model_discovery_candidate
    ADD COLUMN IF NOT EXISTS published_at TIMESTAMPTZ;
ALTER TABLE model_discovery_candidate
    ADD COLUMN IF NOT EXISTS updated_at   TIMESTAMPTZ;
ALTER TABLE model_discovery_candidate
    ADD COLUMN IF NOT EXISTS license      TEXT;
ALTER TABLE model_discovery_candidate
    ADD COLUMN IF NOT EXISTS arch         TEXT;
ALTER TABLE model_discovery_candidate
    ADD COLUMN IF NOT EXISTS is_instruct  BOOLEAN;
ALTER TABLE model_discovery_candidate
    ADD COLUMN IF NOT EXISTS gated        BOOLEAN;
ALTER TABLE model_discovery_candidate
    ADD COLUMN IF NOT EXISTS quant_dtype  TEXT;

-- Query axis the license hard-filter and audit reporting filter on.
CREATE INDEX IF NOT EXISTS idx_discovery_candidate_license
    ON model_discovery_candidate (license);
