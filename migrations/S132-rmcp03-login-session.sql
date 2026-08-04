-- S132 / RMCP-03 — durable single-use marker for a login session.
--
-- Additive and idempotent (IF NOT EXISTS throughout), and per the v4.6 DEPLOY
-- rule NOT applied at service startup: apply it via `pg_ddl` as part of the
-- deploy, sequenced with or before the image swap. `OauthStore::schema_ready`
-- requires this table, so a deploy that ships the code without the migration
-- reports the door unconfigured rather than running with a weakened guard.
--
-- ## Why this table exists
-- One authentication must yield at most ONE authorization code. The login
-- session is a signed cookie, and a signed token cannot express "already
-- used" — so a replayed consent form post would mint a second code for a
-- single human approval.
--
-- The first implementation kept the spent identifiers in a process-local
-- `HashMap`. Review round 1 (gpt56) correctly rejected that: it is the right
-- property enforced in the weaker place. Terminus runs behind a load balancer
-- with more than one replica, so the same signed session arriving at two
-- instances would be unspent at both, and each would issue a code. A guard
-- that holds only when there happens to be one replica is not a guard.
--
-- The claim is therefore a row insert with a PRIMARY KEY conflict as the
-- arbiter: `INSERT … ON CONFLICT DO NOTHING` affects one row for exactly one
-- caller, cluster-wide, with no lock held and no read-then-write window. That
-- is the same shape as `rmcp_auth_code`'s conditional-UPDATE consumption, and
-- for the same reason — the check and the claim must be one statement.
--
-- ## Why only a hash is stored
-- The stored value is the SHA-256 of the session's `jti`, never the `jti`
-- itself. The `jti` is a bearer-adjacent value carried inside a live cookie;
-- storing it verbatim would put presentable session material in a table whose
-- whole schema is otherwise built on the rule that a dump yields nothing
-- usable. A digest is enough — the claim is an equality lookup, which needs
-- determinism, not reversibility.

CREATE TABLE IF NOT EXISTS rmcp_login_session_use (
    -- SHA-256 of the session's `jti`. PRIMARY KEY is load-bearing: it is what
    -- makes the claim atomic across replicas, not merely a lookup index.
    jti_hash    bytea PRIMARY KEY,
    consumed_at timestamptz NOT NULL DEFAULT now(),
    -- When this row stops being useful. Retention is bounded by the login
    -- session's own TTL, so the table's size tracks the login rate over a few
    -- minutes rather than growing without limit. Purging is housekeeping only:
    -- correctness never depends on it having run, because a row that is still
    -- present always denies and a session whose row has been purged has long
    -- since expired as a token.
    expires_at  timestamptz NOT NULL
);

CREATE INDEX IF NOT EXISTS rmcp_login_session_use_expires_idx
    ON rmcp_login_session_use (expires_at);
