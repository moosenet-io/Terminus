-- S114 DISC-01 (TERM #251): the discovery "brochure" — a standing registry of
-- HuggingFace model CANDIDATES for the gfx1151 fleet, distinct from the Model
-- Fleet Catalog (`model_fleet_catalog`/`model_fleet_catalog_cell`, MINT2-07/08,
-- see `S112-mint07-fleet-catalog.sql`).
--
-- WHY THIS TABLE IS SEPARATE FROM THE FLEET CATALOG: the fleet catalog answers
-- "what has been TESTED, and how did it score?" for models already in the
-- fleet. This table answers an earlier question: "what's a CANDIDATE — newly
-- available on HuggingFace, not yet acquired or tested?" The two relate ONLY
-- by a `model_name` join; this migration never touches
-- `model_fleet_catalog`/`model_fleet_catalog_cell`, and vice versa. See
-- `src/intake/discovery/schema.rs` for the naming-footgun note (this registry
-- is always the "brochure," never "catalog" — Chord's `src/catalog.rs` is a
-- third, unrelated thing: the MCP *tool* catalog).
--
-- LIFECYCLE (`status`, enforced by `src/intake/discovery/schema.rs`'s
-- `CandidateStatus`): discovered -> fetching -> cold_stored ->
-- marked_for_fleet -> swept -> evicted (evicted re-enters at discovered ONLY,
-- if a pruned model reappears in a later HF listing); discovered/fetching can
-- also terminate at rejected (failed the VRAM/gfx1151 fit check, never
-- fetched). DISC-03 is the only write API permitted to move `status`; this
-- migration only creates the storage.
--
-- Applied OUT-OF-BAND by an operator, NOT by the harness code (matching the
-- `model_fleet_catalog` MINT2-07 convention — `src/intake/storage.rs` is
-- authoritative that the harness only INSERTs/SELECTs, never issues DDL).
-- Additive, idempotent, non-destructive: `CREATE TABLE IF NOT EXISTS` +
-- `CREATE INDEX IF NOT EXISTS`, so re-applying is a safe no-op. `IF NOT EXISTS`
-- also means this migration does not depend on `model_fleet_catalog` (or any
-- other migration) having run first, even if migrations run out of order on a
-- fresh, un-migrated database.

CREATE TABLE IF NOT EXISTS model_discovery_candidate (
    -- Matches `model_fleet_catalog.model_name` byte-for-byte (S83 join
    -- convention `acquire.rs` documents) so a brochure row and a fleet-catalog
    -- card for the same model share one identity.
    model_name          TEXT        NOT NULL,
    hf_repo              TEXT        NOT NULL,
    -- 'tool_router' | 'writer_slm' | 'assistant' | 'coder' | 'embedding' |
    -- 'visual' | 'voice' — see `FleetCategory::as_str()`.
    category             TEXT        NOT NULL,
    -- 'discovered' | 'fetching' | 'cold_stored' | 'marked_for_fleet' | 'swept'
    -- | 'evicted' | 'rejected' — see `CandidateStatus::as_str()`.
    status                TEXT        NOT NULL,
    -- `acquire.rs::Gfx1151Class` value ('confirmed' | 'experimental' |
    -- 'unknown'), kept as plain text here — DISC-05 owns the enum.
    gfx1151_class        TEXT        NOT NULL,
    size_b                DOUBLE PRECISION,
    vram_footprint_gb   DOUBLE PRECISION,
    -- Free text: which DISC-04 signal found it (e.g. 'hf_trending').
    discovery_source    TEXT        NOT NULL,
    -- The numeric signal DISC-05 computed (HF likes/downloads/trending, or a
    -- real leaderboard score once available — a documented placeholder, see
    -- the S114 spec's open question 3).
    discovery_score      DOUBLE PRECISION,
    discovered_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- Bumped every refresh a still-listed candidate is re-observed, so
    -- staleness is queryable (`WHERE last_seen_at < now() - interval '...'`).
    last_seen_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    fetched_at             TIMESTAMPTZ,
    marked_for_fleet_at  TIMESTAMPTZ,
    evicted_at             TIMESTAMPTZ,
    -- NULL until an eviction populates it (DISC-13, via DISC-03's
    -- `record_eviction` — the ONLY permitted write site). Invariant
    -- (application-layer enforced, not a DB CHECK — no precedent for a
    -- cross-column CHECK in this migration style): populated iff
    -- status = 'evicted'. This JSONB blob is what survives pruning — the
    -- "keep their profile data" requirement's actual storage.
    retained_profile      JSONB,
    -- Free text, mirrors `Nomination::rationale` (a fetch failure reason, a
    -- classification rationale, …).
    rationale              TEXT,
    -- Simple PK on model_name: one brochure row per candidate model. A
    -- `model_name` collision on a fresh discovery (DISC-06 re-observing an
    -- already-known candidate) is an idempotent upsert at this constraint
    -- level, not a silent duplicate — DISC-03 owns the upsert logic that
    -- relies on this.
    PRIMARY KEY (model_name)
);

-- Query axes DISC-02's brochure read tool filters on: status, category, and
-- staleness (last_seen_at).
CREATE INDEX IF NOT EXISTS idx_discovery_candidate_status
    ON model_discovery_candidate (status);
CREATE INDEX IF NOT EXISTS idx_discovery_candidate_category
    ON model_discovery_candidate (category);
CREATE INDEX IF NOT EXISTS idx_discovery_candidate_last_seen
    ON model_discovery_candidate (last_seen_at);
