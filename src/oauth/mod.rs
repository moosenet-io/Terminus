//! RMCP — the OAuth 2.1 remote-MCP connector door (S132).
//!
//! ## Why this module exists
//! Terminus has three ways in today, and all three are private: the loopback
//! plain listener, the mTLS listener ([`crate::pki::mtls`]), and the tailnet
//! listener ([`crate::mesh::tailnet`]). Each binds a caller's identity to a
//! transport artifact — a client certificate CN, or a tailnet WhoIs result.
//! That works well for the fleet's own services and for machines the operator
//! enrolled by hand.
//!
//! It does not work for a hosted third-party client. Anthropic's Claude
//! surfaces reach an external MCP server over public HTTPS, authenticate with
//! OAuth 2.1, and arrive from a fixed egress range — they cannot present an
//! mTLS certificate this fleet issued, and they are not on the tailnet. This
//! module is the fourth door: an OAuth 2.1 authorization server plus
//! resource-server validation, whose output is a [`crate::mesh::Principal`]
//! that the EXISTING [`crate::gateway_framework`] authorization already
//! understands. The new door changes how a caller proves who they are; it does
//! not introduce a second way to decide what they may do.
//!
//! ## The scoping model, stated once
//! An internet-facing door onto 400 fleet tools is only safe if the door is
//! narrower than the room behind it. Every request through this module resolves
//! to an intersection (RMCP-07):
//!
//! ```text
//! effective = grant_of(account)          // what the HUMAN may do  (existing)
//!           ∩ tools_of(client.groups)    // what THIS connector may do
//!           ∩ namespaces(client.servers) // which federated servers it sees
//! ```
//!
//! The intersection can only ever REMOVE. There is deliberately no code path by
//! which a client scoping record grants a tool the account's own grant would
//! have denied — the same anti-widening discipline as
//! [`crate::gateway_framework`]'s guest clamp, and for the same reason: the
//! dangerous failure in an authorization change is never a spurious denial, it
//! is a silent widening that nobody notices until it is used.
//!
//! ## What RMCP-01 delivers
//! The persistence layer and nothing else. There is no HTTP surface here yet —
//! the metadata documents (RMCP-02), the authorize/token endpoints (RMCP-03,
//! RMCP-04), resource-server validation (RMCP-05) and the scoping resolver
//! (RMCP-07) each land as their own item on top of these types. This item is
//! deliberately unreachable from the network so the schema and its fail-closed
//! contracts can be reviewed on their own.
//!
//! ## Credential storage — nothing here is presentable
//! No table in this schema stores a usable credential:
//! - Authorization codes and refresh tokens are high-entropy machine-generated
//!   values, stored as SHA-256 hashes ([`secret_hash`]). They need no salt or
//!   work factor precisely because they are full-entropy and short-lived;
//!   argon2 on a 256-bit random value buys nothing and costs latency on the
//!   token endpoint, which has a 10-second budget.
//! - Client secrets and account passwords are stored as argon2id PHC strings,
//!   written by RMCP-03/RMCP-08 which own the verification path.
//!
//! ## Secret access (S7/S8)
//! This crate has no separate `SecretManager::get()` API; the runtime secret
//! store is materialized into the process environment at startup, so an env
//! read here IS the vault read. See [`crate::pki`]'s module docs for the full
//! rationale and [`crate::pg::conn`] for the established precedent this
//! mirrors. The connection URL is read in exactly one place
//! ([`OauthConfig::from_env`]) and is never logged, returned, or embedded in an
//! error.

pub mod model;
pub mod store;

use crate::error::ToolError;

/// Env var naming the Postgres connection this module's own data plane uses.
///
/// This is the S9-pg "application service owns its own data plane" case: the
/// OAuth store is Terminus's own state, not ad-hoc fleet-database access, so it
/// holds a pool rather than routing through the `pg_*` tools. Fleet queries by
/// an agent still go through those tools.
pub const DATABASE_URL_ENV: &str = "RMCP_DATABASE_URL";

/// Non-secret configuration for the OAuth door.
///
/// Deliberately does NOT derive `Debug`: the only field is a connection URL
/// with an embedded password, and a stray `{:?}` in a log line is exactly how
/// that leaks. Callers that want to describe this value get
/// [`OauthConfig::describe`], which names the source and never the value.
#[derive(Clone)]
pub struct OauthConfig {
    database_url: String,
}

impl OauthConfig {
    /// Read the configuration from the environment.
    ///
    /// Returns [`ToolError::NotConfigured`] when the URL is absent or blank —
    /// blank is treated as absent, matching `secrets_bootstrap`'s own rule that
    /// an empty materialized secret is a missing one rather than a valid empty
    /// credential. The error text names the VARIABLE, never its value.
    pub fn from_env() -> Result<Self, ToolError> {
        let database_url = std::env::var(DATABASE_URL_ENV)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .ok_or_else(|| {
                ToolError::NotConfigured(format!(
                    "{DATABASE_URL_ENV} not set — the RMCP OAuth door requires its own \
                     Postgres connection"
                ))
            })?;
        Ok(Self { database_url })
    }

    /// The connection URL, for the one caller that opens the pool.
    pub(crate) fn database_url(&self) -> &str {
        &self.database_url
    }

    /// A log-safe description. Names where the value came from; never the value.
    pub fn describe(&self) -> String {
        format!("RMCP OAuth store configured from {DATABASE_URL_ENV}")
    }
}

/// Hash a high-entropy machine-generated secret (an authorization code or a
/// refresh token) for storage and lookup.
///
/// SHA-256, unsalted and unstretched, and that is the correct choice here — not
/// a shortcut. These values are 256-bit random strings this server generated;
/// there is no dictionary to attack and no low-entropy input to stretch, so a
/// work factor would only add latency to the token endpoint. A salt would break
/// the property this function exists for: the store looks a token UP by its
/// hash, which requires the mapping to be deterministic.
///
/// Passwords and client secrets are the opposite case — attacker-chosen or
/// human-chosen, hence argon2id — and deliberately do NOT come through here.
pub fn secret_hash(secret: &str) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(secret.as_bytes());
    hasher.finalize().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stored hash must never equal the plaintext it was derived from. This
    /// is the property a schema dump depends on, so it is asserted rather than
    /// assumed.
    #[test]
    fn secret_hash_is_not_the_plaintext() {
        let secret = "<REDACTED-SECRET>";
        let hashed = secret_hash(secret);
        assert_ne!(hashed.as_slice(), secret.as_bytes());
        assert_eq!(hashed.len(), 32, "SHA-256 digests are 32 bytes");
    }

    /// Lookup by hash requires determinism — a salted hash would silently break
    /// every `find_*_by_hash` in the store.
    #[test]
    fn secret_hash_is_deterministic() {
        assert_eq!(secret_hash("same-input"), secret_hash("same-input"));
        assert_ne!(secret_hash("one-input"), secret_hash("another-input"));
    }

    /// A blank materialized secret is a MISSING one. If this ever returned
    /// `Ok`, the pool would be opened against an empty URL and fail later with
    /// a confusing connection error instead of a clear config error here.
    #[test]
    fn blank_database_url_is_treated_as_absent() {
        // Exercises the same filter `from_env` applies, without mutating
        // process-global environment state that would race other tests.
        let blank: Option<String> = Some("   ".to_string())
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());
        assert!(blank.is_none(), "a whitespace-only URL must read as absent");
    }

    /// The config's own description must not be a channel for the URL.
    #[test]
    fn describe_never_contains_the_url() {
        let cfg = OauthConfig {
            database_url: "postgres://user:<email>/db".to_string(),
        };
        let described = cfg.describe();
        assert!(!described.contains("hunter2"));
        assert!(!described.contains("postgres://"));
    }
}
