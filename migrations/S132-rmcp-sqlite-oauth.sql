-- S132 / RMCP-SQLITE — the OAuth 2.1 remote-MCP door's data plane, on SQLite.
--
-- This file REPLACES the four Postgres migrations it was ported from
-- (`S132-rmcp01-oauth-core.sql`, `S132-rmcp03-login-session.sql`,
-- `S132-rmcp06-account-operator-flag.sql`, `S132-rmcp08-client-registration.sql`),
-- which are deleted rather than kept.
--
-- ## Why one file rather than four
--
-- The four were sequenced additively because each was written against a LIVE
-- Postgres database that already held rows: RMCP-06 could only add a column to
-- an `rmcp_account` that existed, and RMCP-08 could only widen an `rmcp_client`
-- that was already deployed. None of that ever happened — nothing was deployed
-- and there is no data anywhere. Carrying four files forward would preserve the
-- shape of a migration history that has no history in it, and would mean the
-- `is_operator` and `version` columns are described in one place and created in
-- another. One file states the schema this code actually needs, once.
--
-- The readiness check in `OauthStore::schema_ready` is UNCHANGED in intent and
-- still probes every table AND the two later-added columns. A single file is
-- not an atomic file: a run interrupted partway leaves exactly the partially
-- applied state that check exists to catch, so it still earns its keep.
--
-- ## What this file is applied to
--
-- A SQLite database FILE, named by `RMCP_SQLITE_PATH`. There is no server, no
-- role and no credential — which is the point of the port: the door's
-- credential material (argon2id password and client-secret hashes, SHA-256
-- refresh-token and authorization-code digests, consent records) lives in its
-- own file rather than in a shared fleet database, and the deployment needs no
-- `RMCP_DATABASE_URL` secret at all.
--
-- Per the v4.6 DEPLOY rule migrations are NOT applied at service startup. Apply
-- this file with `sqlite3 <path> < S132-rmcp-sqlite-oauth.sql` as part of the
-- deploy, sequenced with or before the image swap.
--
-- Every statement is `IF NOT EXISTS`, so re-running is a no-op.
--
-- ## Credential storage rule, unchanged from the Postgres schema
--
-- NOTHING here holds a usable credential. Authorization codes, refresh tokens
-- and registration tokens are high-entropy machine-generated values stored as
-- SHA-256 digests (BLOB); client secrets and account passwords are argon2id PHC
-- strings (TEXT). A copy of this file yields nothing an attacker can present.
--
-- The file itself is nonetheless credential-adjacent — it is the authorization
-- database — so it belongs on persistent storage with 0600 permissions owned by
-- the service user. See `RMCP_SQLITE_PATH` in `.env.example` for the full
-- operational contract (location, permissions, backup, and the single-writer
-- constraint).
--
-- ---------------------------------------------------------------------------
-- ## Type mapping, and why each choice was made
--
-- **`uuid` → BLOB (16 raw bytes).** This is `sqlx`'s native `Uuid` mapping for
-- SQLite, applied automatically by the type system at every bind and every
-- decode. The alternative — TEXT holding the canonical hyphenated form — reads
-- better in an ad-hoc `sqlite3` session, and was rejected anyway: `sqlx` binds a
-- bare `Uuid` as BLOB, so a TEXT schema would need a wrapper type remembered at
-- ~140 call sites, and forgetting it once produces a bind that matches NO ROW.
-- Under this store's standing rule that absence is the empty set, a silent
-- zero-row lookup is indistinguishable from a legitimate deny — the most
-- dangerous possible bug in this port, and one no test would obviously catch.
-- BLOB removes the second representation entirely, so there is nothing to drift
-- to. `hex(id)` and `X'…'` literals cover the hand-query case.
--
-- **`timestamptz` → INTEGER (unix epoch SECONDS).** Two properties decide this.
-- First, comparison: integers compare numerically and totally. TEXT ISO-8601
-- would compare lexicographically, which is only correct while every writer
-- emits byte-identical formatting — and SQLite's own `datetime('now')` emits
-- `YYYY-MM-DD HH:MM:SS` while `sqlx` emits RFC3339 with fractional seconds and a
-- `+00:00` offset, so the two would MIS-compare at equal instants. A comparison
-- that looks right and is subtly weaker is this sprint's recurring defect.
-- Second, the clock: `unixepoch()` is SQLite's own clock in exactly these units,
-- so the store's rule that THE DATABASE'S CLOCK IS THE ONLY CLOCK survives
-- verbatim — every expiry is still `expires_at > unixepoch()` evaluated in SQL,
-- never against a process clock that may have drifted.
--
-- No timestamp is ever BOUND from Rust; every one is written by `unixepoch()`
-- here or in the store's SQL. That matters, because `sqlx` ENCODES a
-- `DateTime<Utc>` as RFC3339 TEXT while DECODING an INTEGER column natively as
-- unix seconds. Reading is supported; writing would silently introduce the TEXT
-- representation this column has just ruled out. The store contains no
-- timestamp bind, and `no_timestamp_is_bound_from_rust` in `store.rs` fails the
-- build if one is ever added.
--
-- **`bytea` → BLOB.** Direct. These are SHA-256 digests; the hashes stay hashes.
--
-- **`boolean` → INTEGER 0/1.** SQLite has no boolean type; `sqlx` maps `bool` to
-- INTEGER, and `NOT disabled` / `WHERE is_operator` work unchanged.
--
-- **`text[]` → a JSON array in a TEXT column.** This is the one genuine schema
-- change, and the alternative considered was a child table. These three columns
-- (`rmcp_client.redirect_uris`, `rmcp_client.grant_types`,
-- `rmcp_tool_group.patterns`) are VALUES, not entities: no query selects a row
-- BY an element, and every read and every write is wholesale. `update_tool_group`
-- in particular replaces a pattern list ENTIRELY and documents why — "a
-- partially applied permission change is a state nobody chose". A child table
-- would convert that single-column assignment into a delete-plus-N-inserts
-- whose partial application is precisely the state the current design rules out
-- by construction. A JSON column keeps it one atomic assignment.
--
-- The cost is that the RMCP-06 bounds (128 patterns per group, 4096 per
-- resolution) are no longer expressible as a column constraint. They were never
-- enforced by the Postgres `text[]` either — `validate_group` /
-- `validate_patterns` enforce them at the write gate, and still do. What is NEW
-- is that the store is now a file an operator can hand-edit, so the write gate
-- is no longer the only way in: `model.rs` therefore bounds the list on DECODE
-- as well, and a row exceeding it fails to decode rather than resolving. That is
-- the same doctrine this item applies to authority — a write-time check is
-- point-in-time, so anything that can be tampered with must be re-checked on the
-- read path.
--
-- `json_valid()` + `json_type() = 'array'` CHECK constraints keep a malformed
-- value out at the storage layer, and the DEFAULT is `'[]'` — the empty set,
-- never a wildcard.
--
-- **`gen_random_uuid()` → generated in Rust at insert.** SQLite has no
-- equivalent, and `randomblob(16)` would produce a value that is random but not
-- a valid RFC 4122 v4 UUID (no version or variant bits). The store calls
-- `Uuid::new_v4()` and binds it, so every id in this schema is a real v4 UUID
-- exactly as before. These columns therefore carry NO default: an insert that
-- forgot the id fails on NOT NULL rather than silently storing something
-- shaped wrong.
--
-- **`now()` in DDL defaults → `(unixepoch())`.** Same clock, same units.
-- ---------------------------------------------------------------------------
--
-- ## Foreign keys are NOT optional here, and SQLite turns them OFF by default
--
-- Several behaviours this door relies on are expressed as `ON DELETE CASCADE` —
-- deleting a tool group REVOKES it from every client that drew on it, and
-- deleting an account takes its clients, groups, codes, tokens and consents with
-- it. In Postgres that enforcement was unconditional. In SQLite `PRAGMA
-- foreign_keys` defaults to OFF and is PER-CONNECTION, so a pool that does not
-- set it leaves every cascade silently inert and every reference unchecked.
-- `OauthStore::connect` sets it on every pooled connection, and
-- `foreign_keys_are_enforced_on_every_pooled_connection` in `store.rs` proves it
-- against a live database rather than trusting the option was passed.

-- ---------------------------------------------------------------------------
-- Accounts — the humans who can consent. Distinct from the fleet's `Principal`
-- name space: an account MAPS to a principal (see RMCP-05), it is not one.
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS rmcp_account (
    -- v4 UUID as 16 raw bytes, generated in Rust. No default: see the type note.
    id              BLOB PRIMARY KEY NOT NULL,
    name            TEXT NOT NULL UNIQUE,
    -- argon2id PHC string. Never a reversible encoding.
    password_hash   TEXT NOT NULL,
    -- TOTP shared secret, encrypted at rest with a subkey derived from the
    -- OAuth signing key. NULL means this account has no second factor.
    totp_secret_enc BLOB,
    disabled        INTEGER NOT NULL DEFAULT 0,
    -- RMCP-06's operator flag. DEFAULTS TO FALSE, which is the fail-closed
    -- direction: an account created without anybody thinking about it is
    -- delegated, and operator-ness is only ever acquired by an explicit UPDATE.
    -- It is the only source of truth for whether an author may write a bare `*`
    -- pattern, and it is read inside the SAME transaction as the write it
    -- authorizes — never taken from a caller's argument.
    is_operator     INTEGER NOT NULL DEFAULT 0,
    created_at      INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at      INTEGER NOT NULL DEFAULT (unixepoch())
);

-- Partial index: operator accounts are a handful among many, and every read of
-- this column asks "is THIS account an operator", so the false rows are dead
-- weight in a full index.
CREATE INDEX IF NOT EXISTS rmcp_account_operator_idx
    ON rmcp_account (id) WHERE is_operator;

-- ---------------------------------------------------------------------------
-- Clients — one row per connector. `client_id` is the public identifier the
-- user pastes into Claude; `client_secret_hash` is NULL for a public client.
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS rmcp_client (
    id                         BLOB PRIMARY KEY NOT NULL,
    client_id                  TEXT NOT NULL UNIQUE,
    client_secret_hash         TEXT,
    name                       TEXT NOT NULL,
    redirect_uris              TEXT NOT NULL DEFAULT '[]'
                               CHECK (json_valid(redirect_uris)
                                      AND json_type(redirect_uris) = 'array'),
    grant_types                TEXT NOT NULL
                               DEFAULT '["authorization_code","refresh_token"]'
                               CHECK (json_valid(grant_types)
                                      AND json_type(grant_types) = 'array'),
    token_endpoint_auth_method TEXT NOT NULL DEFAULT 'none',
    owner_account_id           BLOB NOT NULL
                               REFERENCES rmcp_account(id) ON DELETE CASCADE,
    -- 'operator' (minted in the GUI/CLI) or 'dcr' (RFC 7591 self-registration).
    -- A 'dcr' client holds no tool access until an operator scopes it.
    registration_source        TEXT NOT NULL DEFAULT 'operator'
                               CHECK (registration_source IN ('operator', 'dcr')),
    disabled                   INTEGER NOT NULL DEFAULT 0,
    created_at                 INTEGER NOT NULL DEFAULT (unixepoch()),
    -- RMCP-08's optimistic-concurrency token. Two operators editing one
    -- connector in two browser tabs is ordinary, and without this the second
    -- save overwrites the first's scoping with a stale — possibly WIDER — set
    -- and reports success.
    version                    INTEGER NOT NULL DEFAULT 1
);
CREATE INDEX IF NOT EXISTS rmcp_client_owner_idx ON rmcp_client (owner_account_id);

-- ---------------------------------------------------------------------------
-- Tool groups — named pattern sets over the merged tool catalog (RMCP-06).
-- An EMPTY `patterns` array matches NOTHING. This is asserted in the store's
-- tests because the tempting bug is to read empty as "unrestricted".
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS rmcp_tool_group (
    id               BLOB PRIMARY KEY NOT NULL,
    name             TEXT NOT NULL UNIQUE,
    description      TEXT NOT NULL DEFAULT '',
    patterns         TEXT NOT NULL DEFAULT '[]'
                     CHECK (json_valid(patterns) AND json_type(patterns) = 'array'),
    owner_account_id BLOB NOT NULL REFERENCES rmcp_account(id) ON DELETE CASCADE,
    created_at       INTEGER NOT NULL DEFAULT (unixepoch())
);

-- Which groups a client may draw on, and which federated servers it may see.
-- Absence of rows means the EMPTY set, never the full set (RMCP-07).
CREATE TABLE IF NOT EXISTS rmcp_client_scope (
    client_id     BLOB NOT NULL REFERENCES rmcp_client(id) ON DELETE CASCADE,
    tool_group_id BLOB NOT NULL REFERENCES rmcp_tool_group(id) ON DELETE CASCADE,
    PRIMARY KEY (client_id, tool_group_id)
);

CREATE TABLE IF NOT EXISTS rmcp_client_server (
    client_id BLOB NOT NULL REFERENCES rmcp_client(id) ON DELETE CASCADE,
    namespace TEXT NOT NULL,
    PRIMARY KEY (client_id, namespace)
);

-- ---------------------------------------------------------------------------
-- Authorization codes — single-use, short-lived, bound to six fields so a code
-- stolen in transit is useless without the matching verifier, client, redirect
-- and resource. `consumed_at` is set by an atomic conditional UPDATE (RMCP-04)
-- so two concurrent redemptions cannot both succeed.
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS rmcp_auth_code (
    code_hash      BLOB PRIMARY KEY NOT NULL,
    client_id      BLOB NOT NULL REFERENCES rmcp_client(id) ON DELETE CASCADE,
    account_id     BLOB NOT NULL REFERENCES rmcp_account(id) ON DELETE CASCADE,
    redirect_uri   TEXT NOT NULL,
    resource       TEXT NOT NULL,
    code_challenge TEXT NOT NULL,
    scope          TEXT NOT NULL,
    issued_at      INTEGER NOT NULL DEFAULT (unixepoch()),
    expires_at     INTEGER NOT NULL,
    consumed_at    INTEGER
);
CREATE INDEX IF NOT EXISTS rmcp_auth_code_expiry_idx ON rmcp_auth_code (expires_at);

-- ---------------------------------------------------------------------------
-- Refresh tokens — rotating, with a family id. Presenting an already-rotated
-- token is treated as theft: the whole family is revoked (RMCP-04).
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS rmcp_refresh_token (
    token_hash BLOB PRIMARY KEY NOT NULL,
    family_id  BLOB NOT NULL,
    client_id  BLOB NOT NULL REFERENCES rmcp_client(id) ON DELETE CASCADE,
    account_id BLOB NOT NULL REFERENCES rmcp_account(id) ON DELETE CASCADE,
    resource   TEXT NOT NULL,
    scope      TEXT NOT NULL,
    issued_at  INTEGER NOT NULL DEFAULT (unixepoch()),
    expires_at INTEGER NOT NULL,
    -- Set when this token has been exchanged for a successor. A presentation
    -- of a token with a non-NULL `rotated_to` is a reuse event.
    rotated_to BLOB,
    revoked_at INTEGER
);
CREATE INDEX IF NOT EXISTS rmcp_refresh_family_idx ON rmcp_refresh_token (family_id);

-- ---------------------------------------------------------------------------
-- Consents — what a human actually approved, per client and scope. Revoking a
-- consent revokes the token families issued under it (RMCP-11).
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS rmcp_consent (
    id         BLOB PRIMARY KEY NOT NULL,
    account_id BLOB NOT NULL REFERENCES rmcp_account(id) ON DELETE CASCADE,
    client_id  BLOB NOT NULL REFERENCES rmcp_client(id) ON DELETE CASCADE,
    scope      TEXT NOT NULL,
    granted_at INTEGER NOT NULL DEFAULT (unixepoch()),
    revoked_at INTEGER
);
-- Partial unique index on LIVE rows only, so re-consenting after a revocation
-- is possible while a double-submit of the consent form cannot produce two
-- approvals to revoke separately. SQLite supports partial indexes, so this
-- carries over unchanged — it is what makes `record_consent` idempotent.
CREATE UNIQUE INDEX IF NOT EXISTS rmcp_consent_live_idx
    ON rmcp_consent (account_id, client_id, scope)
    WHERE revoked_at IS NULL;

-- ---------------------------------------------------------------------------
-- Server ownership — which account administers a federated namespace
-- (RMCP-12). One owner per namespace, enforced by the primary key.
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS rmcp_server_owner (
    namespace        TEXT PRIMARY KEY NOT NULL,
    owner_account_id BLOB NOT NULL REFERENCES rmcp_account(id) ON DELETE CASCADE,
    granted_at       INTEGER NOT NULL DEFAULT (unixepoch())
);

-- ---------------------------------------------------------------------------
-- Login sessions (RMCP-03) — the durable single-use marker for one login.
--
-- One authentication must yield at most ONE authorization code. The login
-- session is a signed cookie, and a signed token cannot express "already used",
-- so a replayed consent-form post would mint a second code for a single human
-- approval. The claim is `INSERT … ON CONFLICT DO NOTHING`: the PRIMARY KEY is
-- the arbiter, so exactly one caller sees one row affected, with no lock held
-- and no read-then-write window.
--
-- ## ⚠ THE SCOPE OF THAT GUARANTEE CHANGED WITH THIS PORT — READ THIS
--
-- RMCP-03's first implementation kept spent identifiers in a process-local
-- `HashMap`, and review round 1 rejected it: Terminus behind a load balancer
-- with more than one replica would see the same signed session unspent at both,
-- and each would issue a code. Moving the marker into Postgres made the claim
-- CLUSTER-WIDE, because every replica shared one database.
--
-- A SQLite file does not have that property. The claim is atomic across every
-- connection to ONE FILE, which is strictly what the primary key promises — and
-- that is exactly as strong as the Postgres version IF AND ONLY IF every replica
-- writes the same file. Two replicas with their own files reproduce the
-- process-local `HashMap` defect precisely, and SILENTLY: nothing errors, each
-- replica simply believes it is the first to spend the session.
--
-- Therefore this door is SINGLE-WRITER BY CONSTRUCTION. Run exactly one
-- instance against one file on local persistent storage. Do NOT place the file
-- on NFS to "share" it between replicas: SQLite's locking depends on POSIX
-- advisory locks that NFS implements unreliably, so that configuration trades a
-- visible constraint for silent corruption. If the door ever needs to be
-- horizontally scaled, this table is the thing that has to move — not the
-- deployment topology.
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS rmcp_login_session_use (
    -- SHA-256 of the session's `jti`, never the `jti` itself: it is a
    -- bearer-adjacent value carried inside a live cookie, and this schema's
    -- standing rule is that no table holds anything presentable. A digest is
    -- enough — the claim is an equality lookup, which needs determinism, not
    -- reversibility. PRIMARY KEY is load-bearing: it is what makes the claim
    -- atomic, not merely a lookup index.
    jti_hash    BLOB PRIMARY KEY NOT NULL,
    consumed_at INTEGER NOT NULL DEFAULT (unixepoch()),
    -- When this row stops being useful. Purging is housekeeping only:
    -- correctness never depends on it having run, because a row that is still
    -- present always denies and a session whose row has been purged has long
    -- since expired as a token.
    expires_at  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS rmcp_login_session_use_expires_idx
    ON rmcp_login_session_use (expires_at);

-- ---------------------------------------------------------------------------
-- Initial access tokens (RFC 7591 §3.1, RMCP-08).
--
-- `uses_remaining` is the bounded-use counter, decremented inside the same
-- write transaction that read the row, so a replayed token cannot be spent
-- twice by two concurrent requests. `expires_at` bounds it in time as well as
-- in count, because a token with uses left and no expiry is a standing
-- invitation nobody remembers issuing.
--
-- `revoked_at` exists so the authority can be WITHDRAWN, not merely used up. It
-- is re-read on every registration attempt — and so, separately, is the ISSUING
-- ACCOUNT's operator status: a write-time check is point-in-time, and a token
-- minted by an operator who was later demoted must stop working at redemption,
-- not at expiry.
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS rmcp_registration_token (
    -- SHA-256 of the issued token. Never the token itself.
    token_hash     BLOB PRIMARY KEY NOT NULL,
    -- Which operator account issued it, for the trail. CASCADE because a token
    -- issued by a deleted account should not outlive the account.
    issued_by      BLOB NOT NULL REFERENCES rmcp_account(id) ON DELETE CASCADE,
    -- Operator-chosen note ("laptop, one-off"). Never rendered into an audit
    -- record — that vocabulary is closed and carries no caller text.
    label          TEXT NOT NULL DEFAULT '',
    -- Bounded use. A single-use token is the default the tool mints; more than
    -- one is available but must be asked for.
    uses_remaining INTEGER NOT NULL,
    expires_at     INTEGER NOT NULL,
    created_at     INTEGER NOT NULL DEFAULT (unixepoch()),
    revoked_at     INTEGER,
    CONSTRAINT rmcp_registration_token_uses_nonneg CHECK (uses_remaining >= 0)
);
CREATE INDEX IF NOT EXISTS rmcp_registration_token_expiry_idx
    ON rmcp_registration_token (expires_at);
