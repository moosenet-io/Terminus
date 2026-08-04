-- S132 / RMCP-01 — OAuth 2.1 remote-MCP connector core schema.
--
-- Additive and idempotent: every statement is IF NOT EXISTS, so re-running the
-- migration is a no-op. Per the v4.6 DEPLOY rule, migrations are NOT applied at
-- service startup — this file is applied to the live database via `pg_ddl` as
-- part of the deploy, sequenced with or before the image swap.
--
-- Credential storage rule, enforced by column naming and by the store layer:
-- NOTHING in this schema holds a usable credential. Authorization codes and
-- refresh tokens are high-entropy machine-generated values stored as SHA-256
-- hashes; client secrets and account passwords are stored as argon2id PHC
-- strings. A dump of this schema yields nothing an attacker can present.

CREATE EXTENSION IF NOT EXISTS pgcrypto;

-- ---------------------------------------------------------------------------
-- Accounts — the humans who can consent. Distinct from the fleet's `Principal`
-- name space: an account MAPS to a principal (see RMCP-05), it is not one.
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS rmcp_account (
    id              uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    name            text NOT NULL UNIQUE,
    -- argon2id PHC string. Never a reversible encoding.
    password_hash   text NOT NULL,
    -- TOTP shared secret, encrypted at rest with a subkey derived from the
    -- OAuth signing key. NULL means this account has no second factor.
    totp_secret_enc bytea,
    disabled        boolean NOT NULL DEFAULT false,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now()
);

-- ---------------------------------------------------------------------------
-- Clients — one row per connector. `client_id` is the public identifier the
-- user pastes into Claude; `client_secret_hash` is NULL for a public client.
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS rmcp_client (
    id                        uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    client_id                 text NOT NULL UNIQUE,
    client_secret_hash        text,
    name                      text NOT NULL,
    redirect_uris             text[] NOT NULL DEFAULT '{}',
    grant_types               text[] NOT NULL DEFAULT '{authorization_code,refresh_token}',
    token_endpoint_auth_method text NOT NULL DEFAULT 'none',
    owner_account_id          uuid NOT NULL REFERENCES rmcp_account(id) ON DELETE CASCADE,
    -- 'operator' (minted in the GUI/CLI) or 'dcr' (RFC 7591 self-registration).
    -- A 'dcr' client holds no tool access until an operator scopes it.
    registration_source       text NOT NULL DEFAULT 'operator'
                              CHECK (registration_source IN ('operator', 'dcr')),
    disabled                  boolean NOT NULL DEFAULT false,
    created_at                timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS rmcp_client_owner_idx ON rmcp_client (owner_account_id);

-- ---------------------------------------------------------------------------
-- Tool groups — named pattern sets over the merged tool catalog (RMCP-06).
-- An EMPTY `patterns` array matches NOTHING. This is asserted in the store's
-- tests because the tempting bug is to read empty as "unrestricted".
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS rmcp_tool_group (
    id               uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    name             text NOT NULL UNIQUE,
    description      text NOT NULL DEFAULT '',
    patterns         text[] NOT NULL DEFAULT '{}',
    owner_account_id uuid NOT NULL REFERENCES rmcp_account(id) ON DELETE CASCADE,
    created_at       timestamptz NOT NULL DEFAULT now()
);

-- Which groups a client may draw on, and which federated servers it may see.
-- Absence of rows means the EMPTY set, never the full set (RMCP-07).
CREATE TABLE IF NOT EXISTS rmcp_client_scope (
    client_id     uuid NOT NULL REFERENCES rmcp_client(id) ON DELETE CASCADE,
    tool_group_id uuid NOT NULL REFERENCES rmcp_tool_group(id) ON DELETE CASCADE,
    PRIMARY KEY (client_id, tool_group_id)
);

CREATE TABLE IF NOT EXISTS rmcp_client_server (
    client_id uuid NOT NULL REFERENCES rmcp_client(id) ON DELETE CASCADE,
    namespace text NOT NULL,
    PRIMARY KEY (client_id, namespace)
);

-- ---------------------------------------------------------------------------
-- Authorization codes — single-use, short-lived, bound to six fields so a code
-- stolen in transit is useless without the matching verifier, client, redirect
-- and resource. `consumed_at` is set by an atomic conditional UPDATE (RMCP-04)
-- so two concurrent redemptions cannot both succeed.
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS rmcp_auth_code (
    code_hash      bytea PRIMARY KEY,
    client_id      uuid NOT NULL REFERENCES rmcp_client(id) ON DELETE CASCADE,
    account_id     uuid NOT NULL REFERENCES rmcp_account(id) ON DELETE CASCADE,
    redirect_uri   text NOT NULL,
    resource       text NOT NULL,
    code_challenge text NOT NULL,
    scope          text NOT NULL,
    issued_at      timestamptz NOT NULL DEFAULT now(),
    expires_at     timestamptz NOT NULL,
    consumed_at    timestamptz
);
CREATE INDEX IF NOT EXISTS rmcp_auth_code_expiry_idx ON rmcp_auth_code (expires_at);

-- ---------------------------------------------------------------------------
-- Refresh tokens — rotating, with a family id. Presenting an already-rotated
-- token is treated as theft: the whole family is revoked (RMCP-04).
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS rmcp_refresh_token (
    token_hash bytea PRIMARY KEY,
    family_id  uuid NOT NULL,
    client_id  uuid NOT NULL REFERENCES rmcp_client(id) ON DELETE CASCADE,
    account_id uuid NOT NULL REFERENCES rmcp_account(id) ON DELETE CASCADE,
    resource   text NOT NULL,
    scope      text NOT NULL,
    issued_at  timestamptz NOT NULL DEFAULT now(),
    expires_at timestamptz NOT NULL,
    -- Set when this token has been exchanged for a successor. A presentation
    -- of a token with a non-NULL `rotated_to` is a reuse event.
    rotated_to bytea,
    revoked_at timestamptz
);
CREATE INDEX IF NOT EXISTS rmcp_refresh_family_idx ON rmcp_refresh_token (family_id);

-- ---------------------------------------------------------------------------
-- Consents — what a human actually approved, per client and scope. Revoking a
-- consent revokes the token families issued under it (RMCP-11).
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS rmcp_consent (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    account_id uuid NOT NULL REFERENCES rmcp_account(id) ON DELETE CASCADE,
    client_id  uuid NOT NULL REFERENCES rmcp_client(id) ON DELETE CASCADE,
    scope      text NOT NULL,
    granted_at timestamptz NOT NULL DEFAULT now(),
    revoked_at timestamptz
);
CREATE UNIQUE INDEX IF NOT EXISTS rmcp_consent_live_idx
    ON rmcp_consent (account_id, client_id, scope)
    WHERE revoked_at IS NULL;

-- ---------------------------------------------------------------------------
-- Server ownership — which account administers a federated namespace
-- (RMCP-12). One owner per namespace, enforced by the primary key.
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS rmcp_server_owner (
    namespace        text PRIMARY KEY,
    owner_account_id uuid NOT NULL REFERENCES rmcp_account(id) ON DELETE CASCADE,
    granted_at       timestamptz NOT NULL DEFAULT now()
);
