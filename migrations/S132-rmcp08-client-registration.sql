-- S132 / RMCP-08 — client registration: operator minting and gated DCR.
--
-- Additive and idempotent, like every other S132 migration: each statement is
-- IF NOT EXISTS / ADD COLUMN IF NOT EXISTS, so re-running is a no-op. Per the
-- v4.6 DEPLOY rule this is applied to the live database by hand (`pg_ddl`),
-- sequenced with or before the image swap — never at service startup.
--
-- Two things are added, and nothing existing changes meaning:
--
--  1. `rmcp_client.version` — an optimistic-concurrency token for the
--     administration tools. Two operators editing one connector in two browser
--     tabs is an ordinary situation, and the failure it produces without this
--     column is silent: the second save overwrites the first's scoping with a
--     stale copy and reports success. A connector's scoping is an authorization
--     record, so "quietly reverted to an older, possibly WIDER set" is the
--     specific outcome worth refusing.
--
--  2. `rmcp_registration_token` — operator-issued INITIAL ACCESS TOKENS for RFC
--     7591 dynamic client registration. DCR is off by default; when it is on it
--     is never an unauthenticated write, and this table is what a registration
--     request must present. Only a SHA-256 digest is stored (the same treatment
--     as authorization codes and refresh tokens), so a dump of this schema
--     still yields nothing anyone can present.

-- ---------------------------------------------------------------------------
-- Optimistic concurrency for the client administration tools.
--
-- Existing rows default to 1, which is correct: any in-flight editor holding a
-- pre-migration view has no version to send, and the tools require one, so the
-- first write after the migration is made deliberately rather than inherited.
-- ---------------------------------------------------------------------------
ALTER TABLE rmcp_client ADD COLUMN IF NOT EXISTS version integer NOT NULL DEFAULT 1;

-- ---------------------------------------------------------------------------
-- Initial access tokens (RFC 7591 §3.1).
--
-- `uses_remaining` is the bounded-use counter, decremented by the same atomic
-- conditional UPDATE that reads the row, so a replayed token cannot be spent
-- twice by two concurrent requests. `expires_at` bounds it in time as well as
-- in count, because a token with uses left and no expiry is a standing
-- invitation that nobody remembers issuing.
--
-- `revoked_at` exists so the authority can be WITHDRAWN, not merely used up.
-- It is re-read on every registration attempt: a write-time check is
-- point-in-time, and an operator who revokes a leaked token needs the next
-- request to feel it.
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS rmcp_registration_token (
    -- SHA-256 of the issued token. Never the token itself.
    token_hash     bytea PRIMARY KEY,
    -- Which operator account issued it, for the trail. CASCADE because a token
    -- issued by a deleted account should not outlive the account.
    issued_by      uuid NOT NULL REFERENCES rmcp_account(id) ON DELETE CASCADE,
    -- Operator-chosen note ("laptop, one-off"). Never rendered into an audit
    -- record — that vocabulary is closed and carries no caller text.
    label          text NOT NULL DEFAULT '',
    -- Bounded use. A single-use token is the default the tool mints; more than
    -- one is available but must be asked for.
    uses_remaining integer NOT NULL,
    expires_at     timestamptz NOT NULL,
    created_at     timestamptz NOT NULL DEFAULT now(),
    revoked_at     timestamptz,
    CONSTRAINT rmcp_registration_token_uses_nonneg CHECK (uses_remaining >= 0)
);

-- Housekeeping reads only; correctness never depends on this index existing,
-- because every consuming read filters on expiry in its own predicate.
CREATE INDEX IF NOT EXISTS rmcp_registration_token_expiry_idx
    ON rmcp_registration_token (expires_at);
