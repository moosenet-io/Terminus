-- S132 / RMCP-06 — the account-level operator flag.
--
-- Additive and idempotent, same posture as the RMCP-01 core migration: applied
-- to the live database via `pg_ddl` as part of the deploy, never at service
-- startup.
--
-- ## Why this column exists
-- RMCP-06 refuses a bare `*` tool-group pattern from a delegated author: a
-- federation user must not be able to grant themselves the whole fleet. The
-- first cut of that rule took the caller's word for who the actor was — the
-- store accepted an `owner_kind` argument. Review (gpt56) correctly rejected
-- that: an authorization rule a caller supplies the input to is advisory, and a
-- delegated caller passing "I am the operator" would store a `*` that the read
-- path then honours for the life of the row.
--
-- The authority therefore has to live somewhere the caller does not control.
-- This is the narrowest thing that works: one boolean on the account, read
-- inside the SAME transaction as the write it authorizes (the same fix RMCP-01
-- landed for cross-account group assignment, after a forgeable marker token was
-- rejected for exactly this reason).
--
-- It DEFAULTS TO FALSE, which is the fail-closed direction: an account that
-- predates this migration, or one created without anybody thinking about it, is
-- delegated. Operator-ness is only ever acquired by an explicit UPDATE, which
-- is an operator action against the database itself.
ALTER TABLE rmcp_account
    ADD COLUMN IF NOT EXISTS is_operator boolean NOT NULL DEFAULT false;

-- Partial index: operator accounts are a handful among many, and every read of
-- this column asks "is THIS account an operator", so the false rows are dead
-- weight in a full index.
CREATE INDEX IF NOT EXISTS rmcp_account_operator_idx
    ON rmcp_account (id) WHERE is_operator;
