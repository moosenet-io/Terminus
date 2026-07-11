//! Unified caller `Principal` model (MESH-06).
//!
//! ## Why this exists
//! terminus-rs currently has THREE separate ways a caller's identity can
//! show up on a request, none of which talk to each other:
//! - [`crate::pki::mtls::ClientIdentity`] — the mTLS client cert's Subject
//!   CN, extracted by `crate::pki::mtls::run_listener` (TCLI-03).
//! - [`crate::mesh::TailnetIdentity`] — the tailnet WhoIs result (MESH-05),
//!   inserted per-connection by the (feature-gated) tailnet listener.
//! - The named PAT identity model (`PLANE_PAT_<NAME>` /
//!   `GITEA_PAT_<NAME>` / `GITHUB_PAT_<NAME>`, see `crate::plane`'s module
//!   doc and `crate::forge::gitea_family`) — a human-chosen name
//!   (`lumina`/`claude`/`moose`/`harmony`/...) that selects which downstream
//!   credential a call is authenticated with.
//!
//! Today these are reconciled ad hoc (a hard-coded `sub="lumina"` pin plus
//! an `X-Terminus-Client-Identity` header workaround — see the S109/LHEG
//! sprint history) rather than through one model. [`Principal`] is that one
//! model: a single canonical `name` — in the SAME string space
//! [`crate::plane::PlaneClient::for_identity`] /
//! `crate::forge::gitea_family`'s `GITEA_PAT_<NAME>` lookup already use — that
//! drives BOTH `crate::gateway_framework`'s allowlist/RBAC decision and the
//! downstream PAT selection, resolved from whichever transport identity
//! (or identities) are present on the request via a config-driven mapping.
//!
//! ## What this item delivers (and what it does not)
//! MESH-06 delivers the model, the [`PrincipalResolver`] mapping/resolution
//! logic, and [`crate::gateway_framework::GatewayFramework::guard`] accepting
//! a [`Principal`] instead of a raw `Option<&ClientIdentity>`. It does NOT
//! wire the resolver into the live request path (calling `resolve()` with
//! both a real `ClientIdentity` extension and a real `TailnetIdentity`
//! extension pulled off one request, replacing the `sub="lumina"` pin) —
//! that live wiring is MESH-07. Existing call sites in `crate::mcp_server`
//! keep working unchanged in *behavior* here via [`Principal::from`]'s
//! direct cert-CN-as-name conversion (see that impl's doc) — no resolver
//! mapping is consulted on the request path yet.
//!
//! ## Precedence, fail-closed
//! [`PrincipalResolver::resolve`] takes an optional cert identity and an
//! optional tailnet identity and returns exactly one canonical name, or an
//! [`AuthError`]:
//! 1. A present [`crate::pki::mtls::ClientIdentity`] is checked FIRST and
//!    EXCLUSIVELY — if its CN maps to a canonical name, that's the result
//!    (`PrincipalSource::MtlsCert`, or `PrincipalSource::Both` when a
//!    tailnet identity also happened to be present — it's carried on the
//!    resolved [`Principal`] for observability, but never changes which
//!    name wins). If the CN does NOT map, resolution fails-closed
//!    (`AuthError::UnmappedIdentity`) — a present-but-unmapped cert is
//!    never silently downgraded to consulting the tailnet identity instead;
//!    an operator who wants tailnet-only resolution for a given caller
//!    should not present a cert for that call at all.
//! 2. Only when NO cert identity is presented is the tailnet identity
//!    consulted: login first, then tags (first match wins, mapping order is
//!    not guaranteed — see [`PrincipalMap`]'s doc). Unmapped ⇒
//!    `AuthError::UnmappedIdentity`.
//! 3. Neither presented ⇒ `AuthError::NoIdentityPresented`.
//!
//! This makes mTLS the strictly stronger signal (matching TCLI-03's own
//! two-layer fail-closed handshake validation) while still allowing a
//! tailnet-only deployment shape for callers that never present a client
//! cert at all.
//!
//! ## Config surface (non-secret, `std::env::var` — see `crate::mesh`'s
//! module doc for why this crate has no separate `SecretManager::get()`)
//! `TERMINUS_MESH_PRINCIPAL_MAP_JSON` — a JSON object:
//! ```json
//! {
//!   "cert_cn": { "harmony-primary.example.test": "harmony" },
//!   "tailnet_login": { "<email>": "moose" },  // pii-test-fixture
//!   "tailnet_tag": { "tag:ci": "claude" }
//! }
//! ```
//! All three keys are optional (default to an empty map); an absent/blank
//! env var yields an entirely empty [`PrincipalMap`] — every `resolve()`
//! call then fails-closed with `AuthError::UnmappedIdentity`, never panics
//! and never falls back to trusting the raw transport identity as-is (that
//! would defeat the point of a canonical, config-driven mapping). A
//! malformed (present but not valid JSON, or wrong shape) value is a hard
//! `Err` from [`PrincipalResolver::from_env`] — a config typo should be
//! loud, not silently downgraded to an empty (deny-everyone) map the way
//! `crate::gateway_framework::AllowlistPolicy::from_env` degrades (that
//! type guards a runtime allow-decision that must never panic the whole
//! process on a typo; this type guards process *construction*, where a
//! loud startup failure is preferable to a running-but-silently-locked-out
//! gateway).
//!
//! Canonical `name` values are free-form strings but MUST match the
//! lowercase name space `PLANE_PAT_<NAME>`/`GITEA_PAT_<NAME>`/
//! `GITHUB_PAT_<NAME>` already use (`crate::plane`'s `for_identity`
//! lowercases its lookup key — see that module) so a resolved
//! [`Principal::name`] can be handed straight to `for_identity()` downstream
//! without any translation step. This module does not itself validate a
//! configured name against a live `PLANE_PAT_<NAME>` set (that set is
//! per-process/per-deployment and may not even be Plane — Gitea/GitHub have
//! their own); see [`Principal`]'s doc for the documented edge case where a
//! resolved name has no provisioned PAT at all.

use std::collections::HashMap;

use serde::Deserialize;
use thiserror::Error;

use crate::mesh::TailnetIdentity;
use crate::pki::mtls::ClientIdentity;

/// Which transport identity (or identities) [`PrincipalResolver::resolve`]
/// used to produce a [`Principal`]. Carried for observability/audit only —
/// never changes downstream authz behavior on its own (that's `name`'s job).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrincipalSource {
    /// Resolved from the mTLS client cert CN alone.
    MtlsCert,
    /// Resolved from the tailnet WhoIs identity alone (no cert presented).
    Tailnet,
    /// A cert CN resolved the name AND a tailnet identity was also present
    /// on the same request (carried on [`Principal::tailnet`] for
    /// observability) — the cert still exclusively decided `name` per the
    /// documented precedence.
    Both,
}

/// A single reconciled caller identity: one canonical `name` in the
/// `PLANE_PAT_<NAME>`/`GITEA_PAT_<NAME>`/`GITHUB_PAT_<NAME>` string space,
/// plus which transport identity/identities produced it.
///
/// ## Edge case: a resolved name with no provisioned PAT
/// [`PrincipalResolver::resolve`] only consults the
/// `TERMINUS_MESH_PRINCIPAL_MAP_JSON` mapping — it never checks whether a
/// `PLANE_PAT_<NAME>`/`GITEA_PAT_<NAME>`/`GITHUB_PAT_<NAME>` secret actually
/// exists for the resolved name (that would require probing multiple
/// unrelated secret namespaces, and this type has no opinion on which
/// downstream API a given call is even for). A `Principal` for a mapped-but-
/// unprovisioned name resolves successfully (RBAC/allowlist decisions still
/// apply normally); the missing-credential failure surfaces later, at the
/// point a downstream client's own `for_identity()`-equivalent call fails —
/// exactly like `crate::plane::PlaneClient::for_identity`'s existing
/// `ToolError::InvalidArgument` for an unconfigured name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    name: String,
    source: PrincipalSource,
    tailnet: Option<TailnetIdentity>,
    cert_cn: Option<String>,
}

impl Principal {
    /// Build a `Principal` directly from an already-known canonical name,
    /// bypassing [`PrincipalResolver`] entirely. Used by
    /// [`Principal::from`]'s `&ClientIdentity` conversion (existing
    /// `crate::mcp_server` call sites — see this module's doc for why those
    /// aren't yet routed through the resolver) and by tests. Not the
    /// production resolution path — that's always
    /// [`PrincipalResolver::resolve`].
    pub fn new(name: impl Into<String>, source: PrincipalSource) -> Self {
        Self {
            name: name.into(),
            source,
            tailnet: None,
            cert_cn: None,
        }
    }

    /// Attach the cert CN that (directly, or via the resolver) contributed
    /// to this principal. Builder-style, for construction call sites.
    pub fn with_cert_cn(mut self, cn: impl Into<String>) -> Self {
        self.cert_cn = Some(cn.into());
        self
    }

    /// Attach the tailnet identity that (directly, or via the resolver)
    /// contributed to, or was merely present alongside, this principal.
    /// Builder-style, for construction call sites.
    pub fn with_tailnet(mut self, tailnet: TailnetIdentity) -> Self {
        self.tailnet = Some(tailnet);
        self
    }

    /// The canonical identity name — same string space as
    /// `PLANE_PAT_<NAME>`/`GITEA_PAT_<NAME>`/`GITHUB_PAT_<NAME>` (lowercase
    /// by convention; this type does not itself force lowercasing, since
    /// [`PrincipalMap`] entries are operator-authored and compared exactly
    /// as configured — see [`PrincipalResolver::from_env`]'s doc). Feeds
    /// both `crate::gateway_framework::AllowlistPolicy` lookups and
    /// downstream `for_identity(name)`-shaped calls directly.
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn source(&self) -> PrincipalSource {
        self.source
    }

    pub fn tailnet(&self) -> Option<&TailnetIdentity> {
        self.tailnet.as_ref()
    }

    pub fn cert_cn(&self) -> Option<&str> {
        self.cert_cn.as_deref()
    }
}

/// Direct, resolver-bypassing conversion: a bare mTLS cert CN becomes a
/// `Principal` whose `name` IS the CN verbatim. This is deliberately the
/// SAME behavior `crate::gateway_framework::GatewayFramework::guard` had
/// before this item (it used to take `identity.as_str()` directly as the
/// allowlist/audit key) — preserved here so existing `crate::mcp_server`
/// call sites keep working unmodified in behavior after their `guard()`
/// call's argument type changes from `Option<&ClientIdentity>` to
/// `Option<&Principal>`. Wiring those call sites through
/// [`PrincipalResolver::resolve`] instead (so a CN like
/// `harmony-primary.example.test` maps to the canonical name `harmony`
/// rather than being used as-is) is MESH-07's job, not this conversion's.
impl From<&ClientIdentity> for Principal {
    fn from(id: &ClientIdentity) -> Self {
        Principal::new(id.as_str(), PrincipalSource::MtlsCert).with_cert_cn(id.as_str())
    }
}

/// Errors from [`PrincipalResolver::resolve`] or
/// [`PrincipalResolver::from_env`]. Every variant's `Display` is
/// log-safe — a cert CN or tailnet login/tag is not secret material (it's
/// the same identity string `crate::pki::mtls`/`crate::mesh::identity`
/// already log), but no raw JSON payload or secret VALUE is ever
/// interpolated.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum AuthError {
    /// Neither a cert identity nor a tailnet identity was presented at all.
    #[error("no transport identity (mTLS cert or tailnet WhoIs) presented on this request")]
    NoIdentityPresented,
    /// A transport identity was presented but the configured
    /// `TERMINUS_MESH_PRINCIPAL_MAP_JSON` mapping has no entry for it —
    /// fail-closed, never falls back to trusting the raw identity string.
    #[error("{0}")]
    UnmappedIdentity(String),
    /// `TERMINUS_MESH_PRINCIPAL_MAP_JSON` is set but not valid JSON, or not
    /// the expected `{"cert_cn": {...}, "tailnet_login": {...},
    /// "tailnet_tag": {...}}` shape.
    #[error("TERMINUS_MESH_PRINCIPAL_MAP_JSON is not valid JSON: {0}")]
    InvalidMapJson(String),
}

/// The parsed `TERMINUS_MESH_PRINCIPAL_MAP_JSON` shape: three independent
/// lookup tables, each mapping a raw transport identity string to a
/// canonical [`Principal::name`]. All three default to empty when the key
/// is absent from the JSON object, so an operator only needs to author the
/// table(s) they actually use.
///
/// When a [`TailnetIdentity`] carries multiple `tags`, `tailnet_tag` is
/// checked in the `tags` `Vec`'s own order (first configured match wins) —
/// tags on a single tailnet node are not otherwise ordered/prioritized by
/// `libtailscale` itself, so this is "first tag that has a mapping entry",
/// not a claim about ACL tag precedence.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PrincipalMap {
    #[serde(default)]
    cert_cn: HashMap<String, String>,
    #[serde(default)]
    tailnet_login: HashMap<String, String>,
    #[serde(default)]
    tailnet_tag: HashMap<String, String>,
}

impl PrincipalMap {
    /// The canonical name currently mapped from cert CN `cn`, if any.
    /// Read-only lookup — used by [`crate::mesh::client_onboarding`] (MESH-12)
    /// to detect a CN collision before minting a new client cert, without
    /// exposing the underlying map for mutation.
    pub fn cert_cn_owner(&self, cn: &str) -> Option<&str> {
        self.cert_cn.get(cn).map(String::as_str)
    }

    /// The canonical name currently mapped from tailnet login `login`, if
    /// any. See [`Self::cert_cn_owner`]'s doc.
    pub fn tailnet_login_owner(&self, login: &str) -> Option<&str> {
        self.tailnet_login.get(login).map(String::as_str)
    }

    /// The canonical name currently mapped from tailnet tag `tag`, if any.
    /// See [`Self::cert_cn_owner`]'s doc.
    pub fn tailnet_tag_owner(&self, tag: &str) -> Option<&str> {
        self.tailnet_tag.get(tag).map(String::as_str)
    }

    /// Whether `name` is already the canonical target of ANY entry in this
    /// map (cert CN, tailnet login, or tailnet tag) — used by
    /// [`crate::mesh::client_onboarding::onboard_client`] to reject a
    /// requested identity name that collides with an already-onboarded
    /// principal (MESH-12 edge case).
    pub fn name_in_use(&self, name: &str) -> bool {
        self.cert_cn.values().any(|v| v == name)
            || self.tailnet_login.values().any(|v| v == name)
            || self.tailnet_tag.values().any(|v| v == name)
    }
}

/// Resolves inbound transport identities (mTLS cert CN, tailnet WhoIs) to a
/// single canonical [`Principal`] via a config-driven [`PrincipalMap`]. See
/// this module's doc for the full precedence rule and config surface.
#[derive(Debug, Clone, Default)]
pub struct PrincipalResolver {
    map: PrincipalMap,
}

impl PrincipalResolver {
    /// Build a resolver directly from an already-parsed map — for tests and
    /// for callers that already have the data in hand.
    pub fn new(map: PrincipalMap) -> Self {
        Self { map }
    }

    /// Borrow the underlying [`PrincipalMap`] — read-only lookups for
    /// callers (e.g. [`crate::mesh::client_onboarding`], MESH-12) that need
    /// to check for collisions before proposing a new mapping entry, without
    /// duplicating `TERMINUS_MESH_PRINCIPAL_MAP_JSON` parsing themselves.
    pub fn map(&self) -> &PrincipalMap {
        &self.map
    }

    /// Build a resolver from `TERMINUS_MESH_PRINCIPAL_MAP_JSON`. An
    /// absent/blank env var yields an entirely empty map (every `resolve()`
    /// call then fails-closed — see this module's doc for why that's
    /// distinct from `AllowlistPolicy::from_env`'s "degrade on malformed
    /// JSON" convention: malformed JSON here is instead a hard `Err`).
    pub fn from_env() -> Result<Self, AuthError> {
        match env_nonempty("TERMINUS_MESH_PRINCIPAL_MAP_JSON") {
            Some(raw) => {
                let map: PrincipalMap = serde_json::from_str(&raw)
                    .map_err(|e| AuthError::InvalidMapJson(e.to_string()))?;
                Ok(Self::new(map))
            }
            None => Ok(Self::default()),
        }
    }

    /// `true` when at least one of the three `TERMINUS_MESH_PRINCIPAL_MAP_JSON`
    /// lookup tables (`cert_cn`/`tailnet_login`/`tailnet_tag`) has at least
    /// one entry — i.e. an operator has actually configured a mapping.
    /// MESH-07's live-request wiring (`crate::mcp_server::handle_mcp`) uses
    /// this to decide precedence: a configured map means strict
    /// resolve-or-fail-closed (`resolve()`); an entirely unconfigured
    /// resolver (the default for every deployment that predates MESH-07, and
    /// for `terminus_personal`, which never sets
    /// `TERMINUS_MESH_PRINCIPAL_MAP_JSON`) means the legacy
    /// `Principal::from(&ClientIdentity)` passthrough is used instead, so a
    /// single-identity deployment with no map authored keeps working exactly
    /// as it did before MESH-07 rather than being mass-denied. See this
    /// module's doc and `crate::mcp_server`'s module doc for the full
    /// precedence rule.
    pub fn is_configured(&self) -> bool {
        !self.map.cert_cn.is_empty() || !self.map.tailnet_login.is_empty() || !self.map.tailnet_tag.is_empty()
    }

    /// Resolve one request's transport identity/identities to a single
    /// canonical [`Principal`]. See this module's doc for the full,
    /// fail-closed precedence rule: cert (if present) decides exclusively;
    /// tailnet is consulted only when no cert is presented; neither present
    /// or neither mapped ⇒ `Err`.
    pub fn resolve(
        &self,
        cert: Option<&ClientIdentity>,
        tailnet: Option<&TailnetIdentity>,
    ) -> Result<Principal, AuthError> {
        if let Some(cert_identity) = cert {
            let cn = cert_identity.as_str();
            return match self.map.cert_cn.get(cn) {
                Some(name) => {
                    let source = if tailnet.is_some() { PrincipalSource::Both } else { PrincipalSource::MtlsCert };
                    let mut principal = Principal::new(name.clone(), source).with_cert_cn(cn);
                    if let Some(t) = tailnet {
                        principal = principal.with_tailnet(t.clone());
                    }
                    Ok(principal)
                }
                None => Err(AuthError::UnmappedIdentity(format!(
                    "cert CN '{cn}' has no entry in TERMINUS_MESH_PRINCIPAL_MAP_JSON's \"cert_cn\" map"
                ))),
            };
        }

        if let Some(t) = tailnet {
            if let Some(name) = self.map.tailnet_login.get(&t.login) {
                return Ok(Principal::new(name.clone(), PrincipalSource::Tailnet).with_tailnet(t.clone()));
            }
            for tag in &t.tags {
                if let Some(name) = self.map.tailnet_tag.get(tag) {
                    return Ok(Principal::new(name.clone(), PrincipalSource::Tailnet).with_tailnet(t.clone()));
                }
            }
            return Err(AuthError::UnmappedIdentity(format!(
                "tailnet login '{}' (tags: {:?}) has no entry in TERMINUS_MESH_PRINCIPAL_MAP_JSON's \
                 \"tailnet_login\"/\"tailnet_tag\" maps",
                t.login, t.tags
            )));
        }

        Err(AuthError::NoIdentityPresented)
    }
}

/// Read an env var, trimmed; `None` when unset or blank. Same small,
/// self-contained convention as `crate::mesh::registry`'s copy — see that
/// module's doc for why this crate duplicates this helper per-module rather
/// than sharing one.
fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cert(cn: &str) -> ClientIdentity {
        ClientIdentity(cn.to_string())
    }

    fn tailnet(login: &str, tags: &[&str]) -> TailnetIdentity {
        TailnetIdentity {
            login: login.to_string(),
            node: "caller-node.tailnetname.ts.net".to_string(), // pii-test-fixture
            tags: tags.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn resolver_with(cert_cn: &[(&str, &str)], tailnet_login: &[(&str, &str)], tailnet_tag: &[(&str, &str)]) -> PrincipalResolver {
        PrincipalResolver::new(PrincipalMap {
            cert_cn: cert_cn.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            tailnet_login: tailnet_login.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            tailnet_tag: tailnet_tag.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        })
    }

    // ── CN-only ──────────────────────────────────────────────────────────

    #[test]
    fn cn_only_maps_to_canonical_name() {
        let resolver = resolver_with(&[("harmony-primary.example.test", "harmony")], &[], &[]);
        let cid = cert("harmony-primary.example.test");
        let principal = resolver.resolve(Some(&cid), None).expect("mapped CN should resolve");
        assert_eq!(principal.name(), "harmony");
        assert_eq!(principal.source(), PrincipalSource::MtlsCert);
        assert_eq!(principal.cert_cn(), Some("harmony-primary.example.test"));
        assert!(principal.tailnet().is_none());
    }

    #[test]
    fn unmapped_cn_is_denied_fail_closed() {
        let resolver = resolver_with(&[("known.example.test", "claude")], &[], &[]);
        let cid = cert("unknown.example.test");
        let err = resolver.resolve(Some(&cid), None).expect_err("unmapped CN must be denied");
        assert!(matches!(err, AuthError::UnmappedIdentity(_)));
    }

    // ── Tailnet-only ─────────────────────────────────────────────────────

    #[test]
    fn tailnet_login_only_maps_to_canonical_name() {
        let resolver = resolver_with(&[], &[("<email>", "moose")], &[]);  // pii-test-fixture
        let t = tailnet("<email>", &[]);  // pii-test-fixture
        let principal = resolver.resolve(None, Some(&t)).expect("mapped tailnet login should resolve");
        assert_eq!(principal.name(), "moose");
        assert_eq!(principal.source(), PrincipalSource::Tailnet);
        assert!(principal.cert_cn().is_none());
        assert_eq!(principal.tailnet(), Some(&t));
    }

    #[test]
    fn tailnet_tag_only_maps_to_canonical_name_when_login_unmapped() {
        let resolver = resolver_with(&[], &[], &[("tag:ci", "claude")]);
        let t = tailnet("<email>", &["tag:ci"]);  // pii-test-fixture
        let principal = resolver.resolve(None, Some(&t)).expect("mapped tailnet tag should resolve");
        assert_eq!(principal.name(), "claude");
        assert_eq!(principal.source(), PrincipalSource::Tailnet);
    }

    #[test]
    fn tailnet_login_wins_over_tag_when_both_mapped() {
        let resolver = resolver_with(&[], &[("<email>", "moose")], &[("tag:ci", "claude")]);  // pii-test-fixture
        let t = tailnet("<email>", &["tag:ci"]);  // pii-test-fixture
        let principal = resolver.resolve(None, Some(&t)).expect("should resolve");
        assert_eq!(principal.name(), "moose");
    }

    #[test]
    fn unmapped_tailnet_identity_is_denied_fail_closed() {
        let resolver = resolver_with(&[], &[("<email>", "moose")], &[("tag:known", "claude")]);  // pii-test-fixture
        let t = tailnet("<email>", &["tag:unknown"]);  // pii-test-fixture
        let err = resolver.resolve(None, Some(&t)).expect_err("unmapped tailnet identity must be denied");
        assert!(matches!(err, AuthError::UnmappedIdentity(_)));
    }

    // ── Both present: precedence ────────────────────────────────────────

    #[test]
    fn cn_wins_over_tailnet_when_both_present_and_mapped() {
        let resolver = resolver_with(
            &[("harmony-primary.example.test", "harmony")],
            &[("<email>", "moose")],  // pii-test-fixture
            &[],
        );
        let cid = cert("harmony-primary.example.test");
        let t = tailnet("<email>", &[]);  // pii-test-fixture
        let principal = resolver.resolve(Some(&cid), Some(&t)).expect("should resolve");
        assert_eq!(principal.name(), "harmony", "cert CN must win over a conflicting tailnet mapping");
        assert_eq!(principal.source(), PrincipalSource::Both);
        // The tailnet identity is still carried for observability, even
        // though it didn't decide `name`.
        assert_eq!(principal.tailnet(), Some(&t));
    }

    #[test]
    fn cn_present_but_unmapped_is_denied_even_when_tailnet_is_mapped() {
        // CN is checked EXCLUSIVELY when present -- an unmapped cert never
        // silently falls back to a mapped tailnet identity.
        let resolver = resolver_with(&[], &[("<email>", "moose")], &[]);  // pii-test-fixture
        let cid = cert("unmapped-cn.example.test");
        let t = tailnet("<email>", &[]);  // pii-test-fixture
        let err = resolver
            .resolve(Some(&cid), Some(&t))
            .expect_err("unmapped CN must deny even with a mapped tailnet identity present");
        assert!(matches!(err, AuthError::UnmappedIdentity(_)));
    }

    // ── Neither present ──────────────────────────────────────────────────

    #[test]
    fn neither_identity_present_is_denied() {
        let resolver = resolver_with(&[("cn", "name")], &[("login", "name")], &[]);
        let err = resolver.resolve(None, None).expect_err("no identity at all must be denied");
        assert!(matches!(err, AuthError::NoIdentityPresented));
    }

    // ── is_configured (MESH-07 legacy-passthrough precedence signal) ──────

    #[test]
    fn is_configured_false_for_default_empty_resolver() {
        let resolver = PrincipalResolver::default();
        assert!(!resolver.is_configured());
    }

    #[test]
    fn is_configured_true_when_any_table_has_an_entry() {
        assert!(resolver_with(&[("cn", "name")], &[], &[]).is_configured());
        assert!(resolver_with(&[], &[("login", "name")], &[]).is_configured());
        assert!(resolver_with(&[], &[], &[("tag:ci", "name")]).is_configured());
    }

    // ── from_env ─────────────────────────────────────────────────────────

    #[test]
    fn from_env_absent_yields_empty_map_and_fail_closed_resolve() {
        std::env::remove_var("TERMINUS_MESH_PRINCIPAL_MAP_JSON");
        let resolver = PrincipalResolver::from_env().expect("absent env var must not error");
        let cid = cert("anything.example.test");
        let err = resolver.resolve(Some(&cid), None).expect_err("empty map must deny everything");
        assert!(matches!(err, AuthError::UnmappedIdentity(_)));
    }

    #[test]
    #[serial_test::serial]
    fn from_env_parses_configured_map() {
        std::env::set_var(
            "TERMINUS_MESH_PRINCIPAL_MAP_JSON",
            r#"{"cert_cn": {"harmony-primary.example.test": "harmony"}, "tailnet_login": {"<email>": "moose"}}"#,  // pii-test-fixture
        );
        let resolver = PrincipalResolver::from_env().expect("valid JSON should parse");
        let cid = cert("harmony-primary.example.test");
        let principal = resolver.resolve(Some(&cid), None).expect("should resolve from env-configured map");
        assert_eq!(principal.name(), "harmony");
        std::env::remove_var("TERMINUS_MESH_PRINCIPAL_MAP_JSON");
    }

    #[test]
    #[serial_test::serial]
    fn from_env_malformed_json_is_a_hard_error() {
        std::env::set_var("TERMINUS_MESH_PRINCIPAL_MAP_JSON", "not valid json {{{");
        let err = PrincipalResolver::from_env().expect_err("malformed JSON must be a hard error, not a silent empty map");
        assert!(matches!(err, AuthError::InvalidMapJson(_)));
        std::env::remove_var("TERMINUS_MESH_PRINCIPAL_MAP_JSON");
    }

    // ── Principal::from(&ClientIdentity) direct conversion ───────────────

    #[test]
    fn principal_from_client_identity_uses_raw_cn_as_name() {
        let cid = cert("raw-cn-as-name");
        let principal = Principal::from(&cid);
        assert_eq!(principal.name(), "raw-cn-as-name");
        assert_eq!(principal.source(), PrincipalSource::MtlsCert);
        assert_eq!(principal.cert_cn(), Some("raw-cn-as-name"));
    }

    // ── Resolved name is a valid PAT-identity-shaped string ──────────────

    #[test]
    fn resolved_name_is_lowercase_pat_identity_shaped() {
        let resolver = resolver_with(&[("cert.example.test", "claude")], &[], &[]);
        let cid = cert("cert.example.test");
        let principal = resolver.resolve(Some(&cid), None).expect("should resolve");
        // A PAT identity name is looked up case-insensitively via
        // `PLANE_PAT_<NAME>`'s lowercased key (see `crate::plane`'s
        // `scan_named_identities`/`for_identity`) -- assert the configured
        // name round-trips as a plausible identity string (non-empty,
        // already-lowercase, no embedded whitespace).
        let name = principal.name();
        assert!(!name.is_empty());
        assert_eq!(name, name.to_ascii_lowercase());
        assert!(!name.contains(char::is_whitespace));
    }
}
