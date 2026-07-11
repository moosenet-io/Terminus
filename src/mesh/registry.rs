//! Parse/validate the upstream mesh registry, and lazy per-upstream secret
//! resolution. See `crate::mesh` module doc for the overall design.

use std::collections::HashSet;

use serde::Deserialize;
use thiserror::Error;

/// Namespace prefixes must be short, DNS/tool-name-safe lowercase
/// alphanumeric strings — long enough to be meaningful, short enough to
/// prefix a tool name without blowing past typical MCP name-length limits.
const NAMESPACE_MIN_LEN: usize = 2;
const NAMESPACE_MAX_LEN: usize = 16;

/// How a call to an upstream is authenticated. Deliberately case-insensitive
/// on the wire (`"mtls"`, `"MTLS"`, `"Bearer"`, … all parse) since this value
/// is hand-authored into `TERMINUS_MESH_UPSTREAMS_JSON` by an operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamTransport {
    /// Mutual TLS — the same client-cert model `crate::pki` already issues
    /// for federated Terminus-to-Terminus traffic.
    Mtls,
    /// A bearer token presented in the `Authorization` header, resolved from
    /// `secret_key` at dial time (never at registry-load time).
    Bearer,
}

impl<'de> Deserialize<'de> for UpstreamTransport {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        match raw.to_ascii_lowercase().as_str() {
            "mtls" => Ok(UpstreamTransport::Mtls),
            "bearer" => Ok(UpstreamTransport::Bearer),
            other => Err(serde::de::Error::custom(format!(
                "unknown transport \"{other}\" (expected \"mtls\" or \"bearer\")"
            ))),
        }
    }
}

/// One upstream Terminus-shaped MCP server to federate.
///
/// Holds a *secret key NAME* only (`secret_key`) — never a credential value.
/// `#[derive(Debug)]` here is safe precisely because of that: there is no
/// field on this struct capable of holding a secret value, so a `{:?}` of an
/// `UpstreamServer` can never print one. See
/// [`UpstreamServer::resolve_secret`] for the one place (deliberately NOT
/// this struct, and NOT parse time) a secret value is ever read.
#[derive(Debug, Clone, Deserialize)]
pub struct UpstreamServer {
    /// Stable, unique identifier for this upstream (e.g. `"personal"`,
    /// `"harmony-fleet"`). Used for logging/lookup, never as the namespace
    /// prefix itself (those are independent, separately-unique fields).
    pub name: String,
    /// Reachable base URL for the upstream's MCP endpoint.
    pub url: String,
    /// How this upstream authenticates inbound calls from terminus-rs.
    pub transport: UpstreamTransport,
    /// Short lowercase-alphanumeric prefix this upstream's tools are
    /// namespaced under once federated (e.g. `personal_ledger_accounts`).
    pub namespace: String,
    /// NAME of the credential in this crate's runtime-secret-store
    /// convention (see the module doc) — never a literal token. `None` for
    /// transports (or deployments) that need no credential, e.g. an mTLS
    /// upstream whose identity is carried entirely by the client cert.
    #[serde(default)]
    pub secret_key: Option<String>,
    /// Whether this upstream participates in [`UpstreamRegistry::enabled_upstreams`].
    /// A `false` entry is still parsed/validated (so a temporarily-disabled
    /// upstream keeps its config visible/auditable), just excluded from
    /// dialing.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_enabled() -> bool {
    true
}

impl UpstreamServer {
    /// Resolve this upstream's credential VALUE from the process
    /// environment, by NAME (`secret_key`) — this crate's established
    /// "materialized into env at startup, plain env read after that IS the
    /// secret read" convention (see `crate::pki`'s module doc for why there
    /// is no separate `SecretManager::get()`/`vault::manager()` API here).
    ///
    /// Deliberately lazy: called by a later dial step (MESH-02), never by
    /// the registry loader — so a registry can be loaded/validated/inspected
    /// (e.g. logged, listed) with zero secret-store reads.
    ///
    /// Returns `None` when this upstream has no `secret_key` configured
    /// (e.g. a pure-mTLS upstream). Returns `Some(Err(_))` when a
    /// `secret_key` is configured but the named credential isn't currently
    /// resolvable (unset or blank in the process environment) — a
    /// config/provisioning problem the dial step should surface, not paper
    /// over.
    pub fn resolve_secret(&self) -> Option<Result<ResolvedSecret, MeshConfigError>> {
        let key = self.secret_key.as_ref()?;
        match std::env::var(key) {
            Ok(value) if !value.trim().is_empty() => Some(Ok(ResolvedSecret(value))),
            Ok(_) => Some(Err(MeshConfigError::SecretEmpty(key.clone()))),
            Err(_) => Some(Err(MeshConfigError::SecretMissing(key.clone()))),
        }
    }
}

/// A resolved credential value, held only long enough to be used for a dial.
/// Deliberately has no `Display` impl and a redacted `Debug` impl, so it can
/// never be accidentally logged/printed by a `{}`/`{:?}` format — the only
/// way to see the underlying value is the explicit [`ResolvedSecret::expose`]
/// call a dial step makes right before using it.
pub struct ResolvedSecret(String);

impl ResolvedSecret {
    /// Explicit, deliberately-named accessor for the underlying credential
    /// value — named `expose` (rather than e.g. `AsRef`/`Deref`) so every
    /// call site reads as an intentional secret access, not an accident.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for ResolvedSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("ResolvedSecret").field(&"<redacted>").finish()
    }
}

/// Errors from loading/validating the mesh registry, or from resolving a
/// per-upstream secret. Every variant names the offending field/value so a
/// misconfigured `TERMINUS_MESH_UPSTREAMS_JSON` is easy to fix — none of
/// them ever include a secret value (only names/keys, which are not
/// themselves secret).
#[derive(Debug, Error, PartialEq, Eq)]
pub enum MeshConfigError {
    #[error("TERMINUS_MESH_UPSTREAMS_JSON is not valid JSON: {0}")]
    InvalidJson(String),
    #[error("upstream entry at index {index} has an empty \"name\"")]
    EmptyName { index: usize },
    #[error("upstream \"{name}\" has an empty \"url\"")]
    EmptyUrl { name: String },
    #[error("duplicate upstream \"name\": \"{name}\"")]
    DuplicateName { name: String },
    #[error("duplicate upstream \"namespace\": \"{namespace}\" (used by both \"{first_owner}\" and \"{name}\")")]
    DuplicateNamespace {
        namespace: String,
        first_owner: String,
        name: String,
    },
    #[error(
        "upstream \"{name}\" has an invalid \"namespace\" \"{namespace}\" (must match ^[a-z0-9]{{{NAMESPACE_MIN_LEN},{NAMESPACE_MAX_LEN}}}$)"
    )]
    InvalidNamespace { name: String, namespace: String },
    #[error("secret \"{0}\" is not set in the process environment")]
    SecretMissing(String),
    #[error("secret \"{0}\" is set but empty")]
    SecretEmpty(String),
}

/// The validated set of upstream Terminus-shaped MCP servers to federate.
///
/// Construct via [`UpstreamRegistry::from_env`] in production, or
/// [`UpstreamRegistry::from_json`] directly in tests / for an
/// operator-supplied override. There is no public constructor that skips
/// validation — every `UpstreamRegistry` in existence has already passed
/// [`validate`].
#[derive(Debug, Clone, Default)]
pub struct UpstreamRegistry {
    upstreams: Vec<UpstreamServer>,
}

impl UpstreamRegistry {
    /// An empty, dormant registry — what every caller gets when the mesh
    /// feature isn't configured. Never an error: a dormant feature is not a
    /// misconfiguration.
    pub fn empty() -> Self {
        Self { upstreams: Vec::new() }
    }

    /// Build the registry from process environment config:
    /// `TERMINUS_MESH_ENABLED` (non-secret bool flag) gates the whole
    /// feature; `TERMINUS_MESH_UPSTREAMS_JSON` (non-secret structural JSON)
    /// is the entry list. Both are read via plain `std::env::var` — neither
    /// is a credential.
    ///
    /// - `TERMINUS_MESH_ENABLED` unset/false, or `TERMINUS_MESH_UPSTREAMS_JSON`
    ///   unset/blank ⇒ `Ok(Self::empty())`, never an error (dormant feature).
    /// - `TERMINUS_MESH_ENABLED` true with malformed/invalid
    ///   `TERMINUS_MESH_UPSTREAMS_JSON` ⇒ `Err` naming the offending field —
    ///   a misconfiguration is surfaced loudly rather than silently
    ///   downgraded to an empty registry.
    pub fn from_env() -> Result<Self, MeshConfigError> {
        if !mesh_enabled_from_env() {
            return Ok(Self::empty());
        }
        match env_nonempty("TERMINUS_MESH_UPSTREAMS_JSON") {
            Some(raw) => Self::from_json(&raw),
            None => Ok(Self::empty()),
        }
    }

    /// Parse + validate a registry from a raw JSON array string (what
    /// `TERMINUS_MESH_UPSTREAMS_JSON` holds). Never reads any `secret_key`'s
    /// VALUE — parsing only ever touches the key NAME strings in the JSON
    /// itself.
    pub fn from_json(json: &str) -> Result<Self, MeshConfigError> {
        let upstreams: Vec<UpstreamServer> =
            serde_json::from_str(json).map_err(|e| MeshConfigError::InvalidJson(e.to_string()))?;
        validate(&upstreams)?;
        Ok(Self { upstreams })
    }

    /// Every parsed entry, enabled or not (e.g. for an operator-facing
    /// listing/audit view).
    pub fn all(&self) -> &[UpstreamServer] {
        &self.upstreams
    }

    /// Entries with `enabled: true` — what a later dial step (MESH-02)
    /// actually federates against.
    pub fn enabled_upstreams(&self) -> impl Iterator<Item = &UpstreamServer> {
        self.upstreams.iter().filter(|u| u.enabled)
    }

    /// Look up an upstream by its (unique) namespace prefix.
    pub fn by_namespace(&self, namespace: &str) -> Option<&UpstreamServer> {
        self.upstreams.iter().find(|u| u.namespace == namespace)
    }

    pub fn len(&self) -> usize {
        self.upstreams.len()
    }

    pub fn is_empty(&self) -> bool {
        self.upstreams.is_empty()
    }
}

/// Read an env var, trimmed; `None` when unset or blank. Same convention as
/// `crate::config`'s private helper of the same name / `crate::federation`'s
/// copy — duplicated here rather than shared, per this crate's existing
/// practice of keeping each module's env reads small and self-contained.
fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

/// `TERMINUS_MESH_ENABLED` truthiness: `1`/`true`/`yes`/`on` (case
/// insensitive) is enabled; anything else, including unset/blank, is
/// disabled. A non-secret feature flag, read via plain `std::env::var`.
fn mesh_enabled_from_env() -> bool {
    env_nonempty("TERMINUS_MESH_ENABLED")
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

fn is_valid_namespace(namespace: &str) -> bool {
    let len = namespace.chars().count();
    (NAMESPACE_MIN_LEN..=NAMESPACE_MAX_LEN).contains(&len)
        && namespace.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

/// Validate a parsed entry list: unique `name`, unique `namespace`, every
/// `namespace` matches `^[a-z0-9]{2,16}$`, every `url` non-empty. Stops at
/// the first violation (entries are hand-authored config; one clear error at
/// a time is easier to act on than a batch).
fn validate(upstreams: &[UpstreamServer]) -> Result<(), MeshConfigError> {
    let mut seen_names: HashSet<String> = HashSet::new();
    let mut seen_namespaces: std::collections::HashMap<String, String> = std::collections::HashMap::new();

    for (index, u) in upstreams.iter().enumerate() {
        if u.name.trim().is_empty() {
            return Err(MeshConfigError::EmptyName { index });
        }
        if u.url.trim().is_empty() {
            return Err(MeshConfigError::EmptyUrl { name: u.name.clone() });
        }
        if !is_valid_namespace(&u.namespace) {
            return Err(MeshConfigError::InvalidNamespace {
                name: u.name.clone(),
                namespace: u.namespace.clone(),
            });
        }
        if !seen_names.insert(u.name.clone()) {
            return Err(MeshConfigError::DuplicateName { name: u.name.clone() });
        }
        if let Some(first_owner) = seen_namespaces.insert(u.namespace.clone(), u.name.clone()) {
            return Err(MeshConfigError::DuplicateNamespace {
                namespace: u.namespace.clone(),
                first_owner,
                name: u.name.clone(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    // Fixture URLs use RFC 2606 example/reserved-style names -- never a real
    // infrastructure hostname or IP (S1: no hardcoded infra values).
    const VALID_JSON: &str = r#"[
        {
            "name": "upstream-a",
            "url": "https://upstream-a.example.test:8443",
            "transport": "mtls",
            "namespace": "aaa",
            "enabled": true
        },
        {
            "name": "upstream-b",
            "url": "https://upstream-b.example.test:8443",
            "transport": "bearer",
            "namespace": "bbb",
            "secret_key": "TERMINUS_MESH_UPSTREAM_B_TOKEN",
            "enabled": false
        }
    ]"#;

    fn clear_mesh_env() {
        std::env::remove_var("TERMINUS_MESH_ENABLED");
        std::env::remove_var("TERMINUS_MESH_UPSTREAMS_JSON");
        std::env::remove_var("TERMINUS_MESH_UPSTREAM_B_TOKEN");
    }

    // ── Parsing / transport case-insensitivity ────────────────────────────

    #[test]
    fn parses_valid_registry() {
        let reg = UpstreamRegistry::from_json(VALID_JSON).expect("valid JSON should parse");
        assert_eq!(reg.len(), 2);
        let a = reg.by_namespace("aaa").expect("namespace aaa should exist");
        assert_eq!(a.name, "upstream-a");
        assert_eq!(a.transport, UpstreamTransport::Mtls);
        assert!(a.enabled);
        let b = reg.by_namespace("bbb").expect("namespace bbb should exist");
        assert_eq!(b.transport, UpstreamTransport::Bearer);
        assert!(!b.enabled);
        assert_eq!(b.secret_key.as_deref(), Some("TERMINUS_MESH_UPSTREAM_B_TOKEN"));
    }

    #[test]
    fn transport_parses_case_insensitively() {
        let json = r#"[{"name":"u","url":"https://u.example.test","transport":"BEARER","namespace":"ns1"}]"#;
        let reg = UpstreamRegistry::from_json(json).expect("uppercase transport should parse");
        assert_eq!(reg.all()[0].transport, UpstreamTransport::Bearer);
    }

    #[test]
    fn unknown_transport_is_rejected_with_clear_error() {
        let json = r#"[{"name":"u","url":"https://u.example.test","transport":"carrier-pigeon","namespace":"ns1"}]"#;
        let err = UpstreamRegistry::from_json(json).expect_err("bogus transport must be rejected");
        assert!(matches!(err, MeshConfigError::InvalidJson(_)));
    }

    #[test]
    fn entry_missing_enabled_defaults_to_true() {
        let json = r#"[{"name":"u","url":"https://u.example.test","transport":"mtls","namespace":"ns1"}]"#;
        let reg = UpstreamRegistry::from_json(json).expect("should parse");
        assert!(reg.all()[0].enabled);
    }

    // ── Validation rejections ──────────────────────────────────────────────

    #[test]
    fn rejects_duplicate_name() {
        let json = r#"[
            {"name":"dup","url":"https://a.example.test","transport":"mtls","namespace":"aaa"},
            {"name":"dup","url":"https://b.example.test","transport":"mtls","namespace":"bbb"}
        ]"#;
        let err = UpstreamRegistry::from_json(json).expect_err("duplicate name must be rejected");
        assert!(matches!(err, MeshConfigError::DuplicateName { name } if name == "dup"));
    }

    #[test]
    fn rejects_duplicate_namespace() {
        let json = r#"[
            {"name":"a","url":"https://a.example.test","transport":"mtls","namespace":"same"},
            {"name":"b","url":"https://b.example.test","transport":"mtls","namespace":"same"}
        ]"#;
        let err = UpstreamRegistry::from_json(json).expect_err("duplicate namespace must be rejected");
        assert!(matches!(err, MeshConfigError::DuplicateNamespace { namespace, .. } if namespace == "same"));
    }

    #[test]
    fn rejects_bad_namespace_charset() {
        let json = r#"[{"name":"a","url":"https://a.example.test","transport":"mtls","namespace":"Not_OK!"}]"#;
        let err = UpstreamRegistry::from_json(json).expect_err("bad namespace charset must be rejected");
        assert!(matches!(err, MeshConfigError::InvalidNamespace { .. }));
    }

    #[test]
    fn rejects_namespace_too_short() {
        let json = r#"[{"name":"a","url":"https://a.example.test","transport":"mtls","namespace":"x"}]"#;
        let err = UpstreamRegistry::from_json(json).expect_err("1-char namespace must be rejected");
        assert!(matches!(err, MeshConfigError::InvalidNamespace { .. }));
    }

    #[test]
    fn rejects_namespace_too_long() {
        let json = r#"[{"name":"a","url":"https://a.example.test","transport":"mtls","namespace":"abcdefghijklmnopq"}]"#;
        let err = UpstreamRegistry::from_json(json).expect_err("17-char namespace must be rejected");
        assert!(matches!(err, MeshConfigError::InvalidNamespace { .. }));
    }

    #[test]
    fn rejects_empty_url() {
        let json = r#"[{"name":"a","url":"","transport":"mtls","namespace":"aaa"}]"#;
        let err = UpstreamRegistry::from_json(json).expect_err("empty url must be rejected");
        assert!(matches!(err, MeshConfigError::EmptyUrl { name } if name == "a"));
    }

    #[test]
    fn rejects_missing_url_field() {
        let json = r#"[{"name":"a","transport":"mtls","namespace":"aaa"}]"#;
        let err = UpstreamRegistry::from_json(json).expect_err("missing url field must be rejected");
        assert!(matches!(err, MeshConfigError::InvalidJson(_)));
    }

    #[test]
    fn rejects_empty_name() {
        let json = r#"[{"name":"","url":"https://a.example.test","transport":"mtls","namespace":"aaa"}]"#;
        let err = UpstreamRegistry::from_json(json).expect_err("empty name must be rejected");
        assert!(matches!(err, MeshConfigError::EmptyName { index: 0 }));
    }

    #[test]
    fn malformed_json_is_a_clear_error_not_a_panic() {
        let err = UpstreamRegistry::from_json("not valid json {{{")
            .expect_err("malformed JSON must error, never panic");
        assert!(matches!(err, MeshConfigError::InvalidJson(_)));
    }

    // ── Secret handling: never read at parse time ──────────────────────────

    #[test]
    #[serial]
    fn secret_key_is_stored_but_not_resolved_at_parse_time() {
        clear_mesh_env();
        // Deliberately do NOT set TERMINUS_MESH_UPSTREAM_B_TOKEN. If parsing
        // ever read the secret's value, resolve_secret's own later call
        // would still see it missing (a value being unset doesn't prove
        // parse-time never touched it), so the real assertion is behavioral:
        // parsing a JSON string can never make a network/vault call at all,
        // and the struct only stores the key NAME, which we confirm here.
        let reg = UpstreamRegistry::from_json(VALID_JSON).expect("should parse with no secret set");
        let b = reg.by_namespace("bbb").unwrap();
        assert_eq!(b.secret_key.as_deref(), Some("TERMINUS_MESH_UPSTREAM_B_TOKEN"));
        clear_mesh_env();
    }

    #[test]
    #[serial]
    fn resolve_secret_reads_the_named_env_var_lazily() {
        clear_mesh_env();
        let reg = UpstreamRegistry::from_json(VALID_JSON).expect("should parse");
        let b = reg.by_namespace("bbb").unwrap();

        // Before the env var is set, resolution fails with SecretMissing.
        let before = b.resolve_secret().expect("secret_key is Some, so this must be Some");
        assert!(matches!(before, Err(MeshConfigError::SecretMissing(_))));

        // After setting it, resolution succeeds and exposes the value.
        std::env::set_var("TERMINUS_MESH_UPSTREAM_B_TOKEN", "fixture-token-value"); // pii-test-fixture
        let after = b.resolve_secret().expect("secret_key is Some, so this must be Some").expect("should resolve now");
        assert_eq!(after.expose(), "fixture-token-value");

        clear_mesh_env();
    }

    #[test]
    #[serial]
    fn resolve_secret_treats_blank_value_as_missing() {
        clear_mesh_env();
        std::env::set_var("TERMINUS_MESH_UPSTREAM_B_TOKEN", "   ");
        let reg = UpstreamRegistry::from_json(VALID_JSON).expect("should parse");
        let b = reg.by_namespace("bbb").unwrap();
        let result = b.resolve_secret().expect("secret_key is Some");
        assert!(matches!(result, Err(MeshConfigError::SecretEmpty(_))));
        clear_mesh_env();
    }

    #[test]
    fn resolve_secret_is_none_when_no_secret_key_configured() {
        let reg = UpstreamRegistry::from_json(VALID_JSON).expect("should parse");
        let a = reg.by_namespace("aaa").unwrap();
        assert!(a.secret_key.is_none());
        assert!(a.resolve_secret().is_none());
    }

    // ── Debug/Display never leaks a secret ──────────────────────────────────

    #[test]
    fn upstream_server_debug_never_prints_a_secret_value() {
        let reg = UpstreamRegistry::from_json(VALID_JSON).expect("should parse");
        let b = reg.by_namespace("bbb").unwrap();
        let debug_output = format!("{b:?}");
        // The struct only ever holds the key NAME, never a value -- so the
        // NAME is expected to appear (it's not secret), but nothing that
        // looks like a resolved credential can, because the struct has no
        // field capable of holding one.
        assert!(debug_output.contains("TERMINUS_MESH_UPSTREAM_B_TOKEN"));
        assert!(!debug_output.to_lowercase().contains("fixture-token-value"));
    }

    #[test]
    fn resolved_secret_debug_is_redacted() {
        let secret = ResolvedSecret("super-secret-value".to_string()); // pii-test-fixture
        let debug_output = format!("{secret:?}");
        assert!(!debug_output.contains("super-secret-value"));
        assert!(debug_output.contains("redacted"));
    }

    // ── enabled_upstreams / by_namespace lookups ────────────────────────────

    #[test]
    fn enabled_upstreams_excludes_disabled_entries() {
        let reg = UpstreamRegistry::from_json(VALID_JSON).expect("should parse");
        let enabled_names: Vec<&str> = reg.enabled_upstreams().map(|u| u.name.as_str()).collect();
        assert_eq!(enabled_names, vec!["upstream-a"]);
        // The disabled entry is still visible via `all()`.
        assert_eq!(reg.all().len(), 2);
    }

    #[test]
    fn by_namespace_returns_none_for_unknown_namespace() {
        let reg = UpstreamRegistry::from_json(VALID_JSON).expect("should parse");
        assert!(reg.by_namespace("zzz").is_none());
    }

    // ── Feature gating: TERMINUS_MESH_ENABLED / TERMINUS_MESH_UPSTREAMS_JSON ─

    #[test]
    #[serial]
    fn from_env_is_empty_when_mesh_disabled() {
        clear_mesh_env();
        std::env::set_var("TERMINUS_MESH_UPSTREAMS_JSON", VALID_JSON);
        // TERMINUS_MESH_ENABLED left unset.
        let reg = UpstreamRegistry::from_env().expect("disabled mesh must never error");
        assert!(reg.is_empty());
        clear_mesh_env();
    }

    #[test]
    #[serial]
    fn from_env_is_empty_when_enabled_but_no_json_configured() {
        clear_mesh_env();
        std::env::set_var("TERMINUS_MESH_ENABLED", "true");
        let reg = UpstreamRegistry::from_env().expect("enabled with no JSON must be empty, not an error");
        assert!(reg.is_empty());
        clear_mesh_env();
    }

    #[test]
    #[serial]
    fn from_env_parses_when_enabled_and_configured() {
        clear_mesh_env();
        std::env::set_var("TERMINUS_MESH_ENABLED", "1");
        std::env::set_var("TERMINUS_MESH_UPSTREAMS_JSON", VALID_JSON);
        let reg = UpstreamRegistry::from_env().expect("should parse");
        assert_eq!(reg.len(), 2);
        clear_mesh_env();
    }

    #[test]
    #[serial]
    fn from_env_surfaces_a_clear_error_when_enabled_with_malformed_json() {
        clear_mesh_env();
        std::env::set_var("TERMINUS_MESH_ENABLED", "on");
        std::env::set_var("TERMINUS_MESH_UPSTREAMS_JSON", "not valid json {{{");
        let err = UpstreamRegistry::from_env().expect_err("malformed JSON while enabled must error");
        assert!(matches!(err, MeshConfigError::InvalidJson(_)));
        clear_mesh_env();
    }
}
