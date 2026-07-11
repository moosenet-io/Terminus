//! Remote client onboarding workflow (MESH-12), built on MESH-06's
//! [`crate::mesh::principal::PrincipalMap`]/[`crate::mesh::principal::PrincipalResolver`],
//! MESH-11's onboarding-workflow shape (see [`crate::mesh::onboarding`]'s
//! module doc — this module deliberately mirrors its "dry-run, emit
//! config for the operator to persist" convention), and TCLI-01/TCLI-02's
//! embedded CA / leaf-cert issuance (`crate::pki::ca`, `crate::pki::enroll`).
//!
//! ## What this item delivers
//! A single workflow (core tool `mesh_onboard_client`) that takes an
//! operator/agent through onboarding a new REMOTE client to the mesh over
//! the tailnet: mint/record its identity, map it to a canonical
//! [`crate::mesh::Principal`] name, seed a LEAST-PRIVILEGE allowlist grant
//! for that name, and return a ready-to-use client connection profile.
//!
//! ## Two identity mechanisms
//! - [`ClientMechanism::MtlsCert`] — mints a fresh short-lived leaf
//!   certificate via the embedded CA (`crate::pki::ca()`,
//!   `crate::pki::enroll::issue_leaf_cert`), with the cert's CN set to the
//!   requested canonical `name` itself. This reuses the SAME cert-issuance
//!   code TCLI-02's `/enroll` HTTP route uses — it does not reimplement
//!   `rcgen` params. Unlike the HTTP route, this call site is NOT gated by
//!   `TERMINUS_ENROLLMENT_SHARED_SECRET_<IDENTITY>` — see
//!   [`crate::pki::enroll::issue_leaf_cert`]'s doc for why that's correct
//!   here (this workflow is only reachable via terminus-rs's own
//!   already-authenticated tool dispatch, not a fresh unauthenticated
//!   request).
//! - [`ClientMechanism::Tailnet`] — records a tailnet login (+ optional
//!   ACL tags) → canonical name mapping ONLY. No cert is issued; the mapping
//!   is enforced the first time that login/tag actually connects and a
//!   [`crate::mesh::PrincipalResolver::resolve`] call consults it (MESH-06
//!   edge case: "tailnet login not yet seen by WhoIs" is still a valid
//!   mapping to record ahead of time).
//!
//! ## What this deliberately does NOT do
//! - Never mutates `TERMINUS_MESH_PRINCIPAL_MAP_JSON`,
//!   `TERMINUS_GATEWAY_ALLOWLIST_JSON`, any file, or any other live config —
//!   same convention as [`crate::mesh::onboarding::onboard_upstream`]. On
//!   success this workflow *emits* the validated JSON snippets for the
//!   operator to merge into those two env vars via the sanctioned config
//!   path (edit + restart/reload); it never writes them itself.
//! - Never default-allows. The seeded grant
//!   ([`LEAST_PRIVILEGE_CLIENT_GRANT_TOOLS`]) is always a small, explicit
//!   allow-list — never `["*"]` and never an `AllowDeny` grant either (that
//!   shape is reserved for the LHEG-07 `lumina`/`harmony` scaffold, a much
//!   broader posture than a brand-new remote client should start with). A
//!   default-allow seed is a hard review failure per this item's acceptance
//!   criteria.
//! - Never contacts a live server. `MtlsCert` issuance is entirely local
//!   (this node's own embedded CA); `Tailnet` records a mapping without
//!   dialing anything. Neither path enrolls against, nor contacts, any real
//!   `terminus-primary`/gateway deployment — see this crate's test
//!   conventions (in-process CA only, RFC-2606 example hosts).
//! - Never prints CA private key material. The client's OWN freshly-minted
//!   private key IS returned (the client must hold it locally to use the
//!   cert — that's not a leak, it's the point), but the CA's private key
//!   never leaves `crate::pki::ca::CertificateAuthority` (see that type's
//!   doc — no accessor for it exists outside `crate::pki` internals).

use serde_json::{json, Value};
use thiserror::Error;

use super::principal::PrincipalMap;
use crate::gateway_framework::{AllowlistPolicy, Grant};
use crate::pki::enroll::{is_valid_identity, issue_leaf_cert, EnrollError};

/// Tool names seeded for every newly-onboarded remote client — a minimal,
/// read-only local-core subset. Deliberately small and explicit (never
/// `"*"`, never an allow/deny wildcard grant): a brand-new client identity
/// should start with the least access that lets it prove connectivity and
/// introspect its own workspace, with everything else (including anything
/// GitHub/Gitea/secrets/ops-shaped) requiring a separate, deliberate grant
/// expansion by an operator. Mirrors the read-only carve-outs
/// `crate::gateway_framework`'s own [`crate::gateway_framework::DEFAULT_SENSITIVE_DENY_PREFIXES`]
/// doc already calls out as safe broad utility for the `lumina`/`harmony`
/// scaffold (`dev_read_file`, `dev_list_workspaces`, `dev_open_workspace`) —
/// reused here as the starting grant for every OTHER new principal, not just
/// those two.
pub const LEAST_PRIVILEGE_CLIENT_GRANT_TOOLS: &[&str] =
    &["dev_read_file", "dev_list_workspaces", "dev_open_workspace"];

/// Which identity mechanism a [`OnboardClientRequest`] uses.
#[derive(Debug, Clone)]
pub enum ClientMechanism {
    /// Mint a fresh client cert via the embedded CA, CN == the requested
    /// canonical name.
    MtlsCert,
    /// Record a tailnet login (+ optional ACL tags) → canonical name mapping.
    /// No cert is issued.
    Tailnet { login: String, tags: Vec<String> },
}

/// Input to [`onboard_client`]: the desired canonical identity name and how
/// to establish it.
#[derive(Debug, Clone)]
pub struct OnboardClientRequest {
    pub name: String,
    pub mechanism: ClientMechanism,
}

/// The mechanism-specific portion of a successful [`OnboardClientReport`].
/// Never holds CA private key material — see the module doc.
#[derive(Debug, Clone)]
pub enum ClientMechanismReport {
    MtlsCert {
        /// PEM-encoded leaf certificate, CN == the onboarded name.
        cert_pem: String,
        /// PEM-encoded private key for the issued cert. The CLIENT'S OWN
        /// key — this is what the client legitimately must hold locally to
        /// use the cert, not a leak (see the module doc).
        key_pem: String,
        /// The CA's own PEM certificate, for the client to pin locally.
        ca_cert_pem: String,
        expires_at: i64,
    },
    Tailnet {
        login: String,
        tags: Vec<String>,
    },
}

/// The result of a successful onboarding workflow run. Nothing in this
/// struct is CA private key material (see the module doc); the client's own
/// freshly-minted private key (mTLS mechanism only) is intentionally
/// present — the client must hold it.
#[derive(Debug, Clone)]
pub struct OnboardClientReport {
    pub name: String,
    pub mechanism: ClientMechanismReport,
    /// The validated JSON snippet an operator merges into
    /// `TERMINUS_MESH_PRINCIPAL_MAP_JSON` (under the matching top-level key
    /// — `cert_cn` or `tailnet_login`/`tailnet_tag`). This workflow emits
    /// it; it never writes it anywhere itself.
    pub principal_map_entry: Value,
    /// The validated JSON snippet an operator merges into
    /// `TERMINUS_GATEWAY_ALLOWLIST_JSON` under the key `name`. Always a
    /// plain allow-list built from [`LEAST_PRIVILEGE_CLIENT_GRANT_TOOLS`] —
    /// never a `"*"` entry. This workflow emits it; it never writes it
    /// anywhere itself.
    pub grant_entry: Value,
    /// The tool names the seeded grant allows, for direct inspection
    /// (mirrors `grant_entry`'s `"allow"` array).
    pub seeded_grant_tools: Vec<String>,
    /// A ready-to-use client connection profile: gateway hostname (from env,
    /// if configured), transport, and identity. Never includes CA private
    /// material; for the mTLS mechanism the client's own cert/key are
    /// carried separately on [`ClientMechanismReport::MtlsCert`], not
    /// duplicated into this profile.
    pub client_profile: Value,
    /// Non-fatal notices (e.g. the gateway MagicDNS name isn't configured
    /// yet, or the target identity already has an existing allowlist entry
    /// that this seed would sit alongside).
    pub warnings: Vec<String>,
}

/// Everything that can stop an onboarding attempt, each naming exactly
/// what's wrong and never a secret value.
#[derive(Debug, Error)]
pub enum OnboardClientError {
    /// The candidate `name` (or, for the tailnet mechanism, `login`) shape
    /// itself is invalid.
    #[error("invalid client onboarding request: {0}")]
    InvalidRequest(String),
    /// The requested canonical name already maps to an existing principal
    /// in `TERMINUS_MESH_PRINCIPAL_MAP_JSON` (cert CN, tailnet login, or
    /// tailnet tag) — MESH-12 edge case: reject rather than silently
    /// re-target an existing principal's identity.
    #[error("identity name \"{0}\" is already mapped to an existing principal")]
    NameCollision(String),
    /// The specific transport identity (cert CN, or tailnet login) the
    /// request would map is already claimed by a DIFFERENT canonical name.
    #[error("{kind} \"{value}\" is already mapped to principal \"{owner}\"")]
    MechanismCollision {
        kind: &'static str,
        value: String,
        owner: String,
    },
    /// The embedded CA (`crate::pki::ca()`) could not be bootstrapped.
    #[error("embedded CA unavailable: {0}")]
    CaUnavailable(String),
    /// `rcgen` failed to issue the leaf certificate.
    #[error("failed to issue client certificate: {0}")]
    CertIssuance(String),
}

/// Run the client onboarding workflow end to end: validate shape, check for
/// name/mechanism collisions against `existing_map`, establish the identity
/// (mint a cert, or just validate the tailnet mapping shape), seed a
/// least-privilege grant, and build the principal-map + allowlist JSON
/// snippets an operator would merge in. Never mutates `existing_map`,
/// `existing_allowlist`, or anything else — see the module doc.
pub fn onboard_client(
    existing_map: &PrincipalMap,
    existing_allowlist: &AllowlistPolicy,
    req: OnboardClientRequest,
) -> Result<OnboardClientReport, OnboardClientError> {
    let name = req.name.trim().to_string();
    if name.is_empty() {
        return Err(OnboardClientError::InvalidRequest(
            "name must not be empty".to_string(),
        ));
    }
    if existing_map.name_in_use(&name) {
        return Err(OnboardClientError::NameCollision(name));
    }

    let mut warnings = Vec::new();
    if existing_allowlist.has_any_entry(&name) {
        warnings.push(format!(
            "identity \"{name}\" already has an existing TERMINUS_GATEWAY_ALLOWLIST_JSON entry; \
             merging \"grant_entry\" as-is would overwrite it -- review before merging"
        ));
    }

    let (mechanism, principal_map_entry) = match req.mechanism {
        ClientMechanism::MtlsCert => mint_cert_mechanism(existing_map, &name)?,
        ClientMechanism::Tailnet { login, tags } => {
            tailnet_mechanism(existing_map, &name, login, tags)?
        }
    };

    let grant_entry = json!({ "allow": LEAST_PRIVILEGE_CLIENT_GRANT_TOOLS });
    let seeded_grant_tools: Vec<String> =
        LEAST_PRIVILEGE_CLIENT_GRANT_TOOLS.iter().map(|s| s.to_string()).collect();

    let gateway_magicdns_name = crate::config::gateway_magicdns_name();
    if gateway_magicdns_name.is_none() {
        warnings.push(
            "TERMINUS_MESH_GATEWAY_MAGICDNS_NAME is not configured -- the emitted client \
             profile carries a placeholder; set it before distributing this profile to the \
             client"
                .to_string(),
        );
    }
    let client_profile = json!({
        "identity": name,
        "transport": "mtls",
        "gateway_magicdns_name": gateway_magicdns_name
            .unwrap_or_else(|| "<TERMINUS_MESH_GATEWAY_MAGICDNS_NAME not configured>".to_string()),
        "gateway_mtls_port": crate::config::mtls_primary_port(),
    });

    Ok(OnboardClientReport {
        name,
        mechanism,
        principal_map_entry,
        grant_entry,
        seeded_grant_tools,
        client_profile,
        warnings,
    })
}

fn mint_cert_mechanism(
    existing_map: &PrincipalMap,
    name: &str,
) -> Result<(ClientMechanismReport, Value), OnboardClientError> {
    if !is_valid_identity(name) {
        return Err(OnboardClientError::InvalidRequest(format!(
            "\"{name}\" does not match the allowed identity naming pattern \
             (lowercase alphanumerics/hyphens, 2-63 chars, must not start/end with a hyphen)"
        )));
    }
    if let Some(owner) = existing_map.cert_cn_owner(name) {
        return Err(OnboardClientError::MechanismCollision {
            kind: "cert CN",
            value: name.to_string(),
            owner: owner.to_string(),
        });
    }

    let ca = crate::pki::ca().map_err(|e| OnboardClientError::CaUnavailable(e.to_string()))?;
    let (cert_pem, key_pem, _serial) = issue_leaf_cert(ca, name).map_err(|e| match e {
        EnrollError::CertIssuance(msg) => OnboardClientError::CertIssuance(msg),
        other => OnboardClientError::CertIssuance(other.to_string()),
    })?;

    let ttl_hours = crate::config::enrollment_cert_ttl_hours();
    let expires_at = chrono::Utc::now().timestamp() + ttl_hours * 3600;

    let report = ClientMechanismReport::MtlsCert {
        cert_pem,
        key_pem,
        ca_cert_pem: ca.cert_pem().to_string(),
        expires_at,
    };
    let mut cert_cn_entry = serde_json::Map::new();
    cert_cn_entry.insert(name.to_string(), Value::String(name.to_string()));
    let entry = json!({ "cert_cn": Value::Object(cert_cn_entry) });
    Ok((report, entry))
}

fn tailnet_mechanism(
    existing_map: &PrincipalMap,
    name: &str,
    login: String,
    tags: Vec<String>,
) -> Result<(ClientMechanismReport, Value), OnboardClientError> {
    let login = login.trim().to_string();
    if login.is_empty() {
        return Err(OnboardClientError::InvalidRequest(
            "tailnet mechanism requires a non-empty login".to_string(),
        ));
    }
    if let Some(owner) = existing_map.tailnet_login_owner(&login) {
        return Err(OnboardClientError::MechanismCollision {
            kind: "tailnet login",
            value: login,
            owner: owner.to_string(),
        });
    }
    let tags: Vec<String> = tags.into_iter().map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).collect();
    for tag in &tags {
        if let Some(owner) = existing_map.tailnet_tag_owner(tag) {
            return Err(OnboardClientError::MechanismCollision {
                kind: "tailnet tag",
                value: tag.clone(),
                owner: owner.to_string(),
            });
        }
    }

    let mut tailnet_tag_entry = serde_json::Map::new();
    for tag in &tags {
        tailnet_tag_entry.insert(tag.clone(), Value::String(name.to_string()));
    }
    let mut tailnet_login_entry = serde_json::Map::new();
    tailnet_login_entry.insert(login.clone(), Value::String(name.to_string()));
    let entry = json!({
        "tailnet_login": Value::Object(tailnet_login_entry),
        "tailnet_tag": Value::Object(tailnet_tag_entry),
    });

    Ok((ClientMechanismReport::Tailnet { login, tags }, entry))
}

// ---------------------------------------------------------------------------
// Core tool: mesh_onboard_client
// ---------------------------------------------------------------------------

use async_trait::async_trait;

use crate::error::ToolError;
use crate::mesh::principal::PrincipalResolver;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

/// Terminus CORE tool wrapping [`onboard_client`] — the workflow an
/// operator/agent calls to onboard a new remote client to the mesh: mint or
/// map its identity, seed a least-privilege allowlist grant, and emit a
/// connection profile. Never mutates live config and never prints CA
/// private material — see the module doc.
pub struct MeshOnboardClient;

#[async_trait]
impl RustTool for MeshOnboardClient {
    fn name(&self) -> &str {
        "mesh_onboard_client"
    }

    fn description(&self) -> &str {
        "Onboard a new remote client to the terminus mesh over the tailnet: mint a short-lived \
         client cert via the embedded CA (mechanism \"mtls_cert\") or record a tailnet \
         login/tag mapping (mechanism \"tailnet\"), map it to a canonical Principal name, seed \
         a LEAST-PRIVILEGE allowlist grant for that name (never default-allow), and return a \
         client connection profile. Never mutates live config: on success it emits the \
         validated JSON snippets for an operator to merge into \
         TERMINUS_MESH_PRINCIPAL_MAP_JSON and TERMINUS_GATEWAY_ALLOWLIST_JSON via the \
         sanctioned config path."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Desired canonical Principal identity name for the new client (e.g. \"dev-box-claude-code\")."
                },
                "mechanism": {
                    "type": "string",
                    "enum": ["mtls_cert", "tailnet"],
                    "description": "How to establish the client's identity: \"mtls_cert\" mints a fresh client cert; \"tailnet\" records a tailnet login/tag mapping only."
                },
                "tailnet_login": {
                    "type": "string",
                    "description": "Tailnet login to map (required when mechanism is \"tailnet\")."
                },
                "tailnet_tags": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional tailnet ACL tags to also map (mechanism \"tailnet\" only)."
                }
            },
            "required": ["name", "mechanism"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let name = args
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgument("name is required".into()))?
            .to_string();
        let mechanism_raw = args
            .get("mechanism")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgument("mechanism is required".into()))?;

        let mechanism = match mechanism_raw {
            "mtls_cert" => ClientMechanism::MtlsCert,
            "tailnet" => {
                let login = args
                    .get("tailnet_login")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        ToolError::InvalidArgument(
                            "tailnet_login is required when mechanism is \"tailnet\"".into(),
                        )
                    })?
                    .to_string();
                let tags = args
                    .get("tailnet_tags")
                    .and_then(Value::as_array)
                    .map(|arr| arr.iter().filter_map(Value::as_str).map(str::to_string).collect())
                    .unwrap_or_default();
                ClientMechanism::Tailnet { login, tags }
            }
            other => {
                return Err(ToolError::InvalidArgument(format!(
                    "unknown mechanism \"{other}\" (expected \"mtls_cert\" or \"tailnet\")"
                )))
            }
        };

        let req = OnboardClientRequest { name, mechanism };

        let resolver = PrincipalResolver::from_env().map_err(|e| {
            ToolError::Execution(format!("could not load the current principal map: {e}"))
        })?;
        let allowlist = AllowlistPolicy::from_env();

        let report = onboard_client(resolver.map(), &allowlist, req).map_err(|e| match e {
            OnboardClientError::InvalidRequest(_) => ToolError::InvalidArgument(e.to_string()),
            OnboardClientError::NameCollision(_) | OnboardClientError::MechanismCollision { .. } => {
                ToolError::Conflict(e.to_string())
            }
            OnboardClientError::CaUnavailable(_) => ToolError::NotConfigured(e.to_string()),
            OnboardClientError::CertIssuance(_) => ToolError::Execution(e.to_string()),
        })?;

        Ok(report_to_json(&report).to_string())
    }
}

fn report_to_json(report: &OnboardClientReport) -> Value {
    let mechanism = match &report.mechanism {
        ClientMechanismReport::MtlsCert { cert_pem, key_pem, ca_cert_pem, expires_at } => json!({
            "kind": "mtls_cert",
            "cert_pem": cert_pem,
            "key_pem": key_pem,
            "ca_cert_pem": ca_cert_pem,
            "expires_at": expires_at,
        }),
        ClientMechanismReport::Tailnet { login, tags } => json!({
            "kind": "tailnet",
            "login": login,
            "tags": tags,
        }),
    };
    json!({
        "name": report.name,
        "mechanism": mechanism,
        "seeded_grant_tools": report.seeded_grant_tools,
        "warnings": report.warnings,
        "dry_run_config": true,
        "note": "Nothing was written. To finish onboarding this client, merge \"principal_map_entry\" \
                 into TERMINUS_MESH_PRINCIPAL_MAP_JSON and \"grant_entry\" into \
                 TERMINUS_GATEWAY_ALLOWLIST_JSON (keyed by \"name\") via the sanctioned config path, \
                 then restart/reload. The mTLS cert/key (if minted) are already live-issued and \
                 usable by the client immediately -- only the mesh-side mapping/grant config is \
                 not yet applied.",
        "principal_map_entry": report.principal_map_entry,
        "grant_entry": report.grant_entry,
        "client_profile": report.client_profile,
    })
}

pub fn register(registry: &mut ToolRegistry) {
    if let Err(e) = registry.register(Box::new(MeshOnboardClient)) {
        tracing::error!("mesh::client_onboarding: failed to register tool: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn empty_map() -> PrincipalMap {
        PrincipalMap::default()
    }

    fn empty_allowlist() -> AllowlistPolicy {
        AllowlistPolicy::new(std::collections::HashMap::new())
    }

    // ── mTLS mechanism: happy path ──────────────────────────────────────────

    #[test]
    #[serial]
    fn onboards_a_new_client_via_mtls_cert() {
        let store_path = std::env::temp_dir().join(format!(
            "mesh12-onboard-client-mtls-{}.json",
            std::process::id()
        ));
        std::env::set_var("TERMINUS_CA_STORE_PATH", store_path.to_string_lossy().to_string());

        let req = OnboardClientRequest {
            name: "dev-box-claude-code".to_string(),
            mechanism: ClientMechanism::MtlsCert,
        };
        let report = onboard_client(&empty_map(), &empty_allowlist(), req)
            .expect("onboarding a fresh client via mtls_cert should succeed");

        match &report.mechanism {
            ClientMechanismReport::MtlsCert { cert_pem, key_pem, ca_cert_pem, expires_at } => {
                assert!(cert_pem.contains("BEGIN CERTIFICATE"));
                assert!(key_pem.contains("PRIVATE KEY"));
                assert!(ca_cert_pem.contains("BEGIN CERTIFICATE"));
                assert!(*expires_at > 0);
            }
            other => panic!("expected MtlsCert mechanism report, got {other:?}"),
        }
        assert_eq!(report.principal_map_entry["cert_cn"]["dev-box-claude-code"], "dev-box-claude-code");
        assert_eq!(
            report.seeded_grant_tools,
            LEAST_PRIVILEGE_CLIENT_GRANT_TOOLS
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        );

        std::env::remove_var("TERMINUS_CA_STORE_PATH");
        std::fs::remove_file(&store_path).ok();
    }

    // ── Tailnet mechanism: happy path, including unseen-by-WhoIs login ─────

    #[test]
    fn onboards_a_new_client_via_tailnet_mapping_even_if_login_never_seen_by_whois() {
        let req = OnboardClientRequest {
            name: "moose-laptop".to_string(),
            mechanism: ClientMechanism::Tailnet {
                login: "<email>".to_string(), // pii-test-fixture
                tags: vec!["tag:remote-client".to_string()],
            },
        };
        let report = onboard_client(&empty_map(), &empty_allowlist(), req)
            .expect("a tailnet login never yet seen by WhoIs must still be recordable");

        match &report.mechanism {
            ClientMechanismReport::Tailnet { login, tags } => {
                assert_eq!(login, "<email>");
                assert_eq!(tags, &vec!["tag:remote-client".to_string()]);
            }
            other => panic!("expected Tailnet mechanism report, got {other:?}"),
        }
        assert_eq!(
            report.principal_map_entry["tailnet_login"]["<email>"],
            "moose-laptop"
        );
        assert_eq!(
            report.principal_map_entry["tailnet_tag"]["tag:remote-client"],
            "moose-laptop"
        );
    }

    // ── Least-privilege grant: never default-allow, sensitive tools denied ──

    #[test]
    fn seeded_grant_is_least_privilege_never_default_allow() {
        let req = OnboardClientRequest {
            name: "least-priv-client".to_string(),
            mechanism: ClientMechanism::Tailnet {
                login: "<email>".to_string(), // pii-test-fixture
                tags: vec![],
            },
        };
        let report = onboard_client(&empty_map(), &empty_allowlist(), req).expect("should onboard");

        let allow = report.grant_entry["allow"]
            .as_array()
            .expect("grant_entry.allow is an array");
        assert!(
            !allow.iter().any(|v| v == "*"),
            "a seeded grant must never contain a \"*\" wildcard entry"
        );

        // Build the actual Grant a policy would use and assert it denies
        // sensitive/moose-scoped actions by default.
        let grant = Grant::from(
            allow
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect::<Vec<_>>(),
        );
        let mut entries = std::collections::HashMap::new();
        entries.insert("least-priv-client".to_string(), grant);
        let policy = AllowlistPolicy::new(entries);

        for sensitive in [
            "github_push_repo",
            "gitea_cargo_publish",
            "infisical_get_secret",
            "ansible_run_playbook",
            "approval_grant",
            "dev_write_file",
            "dev_run_command",
        ] {
            assert!(
                !policy.is_allowed("least-priv-client", sensitive),
                "new client must NOT be allowed to call sensitive tool {sensitive}"
            );
        }
        // The seeded read-only subset itself must remain allowed.
        for allowed in LEAST_PRIVILEGE_CLIENT_GRANT_TOOLS {
            assert!(
                policy.is_allowed("least-priv-client", allowed),
                "seeded grant must allow its own least-privilege tool {allowed}"
            );
        }
    }

    // ── Edge case: requested name collides with an existing principal ──────

    #[test]
    fn requested_name_colliding_with_existing_principal_is_rejected() {
        let existing = serde_json::from_str::<PrincipalMap>(
            r#"{"cert_cn": {"already-enrolled.example.test": "existing-client"}}"#,
        )
        .expect("valid fixture");
        let req = OnboardClientRequest {
            name: "existing-client".to_string(),
            mechanism: ClientMechanism::Tailnet {
                login: "<email>".to_string(), // pii-test-fixture
                tags: vec![],
            },
        };
        let err = onboard_client(&existing, &empty_allowlist(), req)
            .expect_err("a name already mapped to an existing principal must be rejected");
        assert!(matches!(err, OnboardClientError::NameCollision(n) if n == "existing-client"));
    }

    #[test]
    fn tailnet_login_already_mapped_to_a_different_principal_is_rejected() {
        let existing = serde_json::from_str::<PrincipalMap>(
            r#"{"tailnet_login": {"<email>": "someone-else"}}"#,
        )
        .expect("valid fixture");
        let req = OnboardClientRequest {
            name: "new-client".to_string(),
            mechanism: ClientMechanism::Tailnet {
                login: "<email>".to_string(), // pii-test-fixture
                tags: vec![],
            },
        };
        let err = onboard_client(&existing, &empty_allowlist(), req)
            .expect_err("a tailnet login already owned by a different principal must be rejected");
        match err {
            OnboardClientError::MechanismCollision { kind, value, owner } => {
                assert_eq!(kind, "tailnet login");
                assert_eq!(value, "<email>");
                assert_eq!(owner, "someone-else");
            }
            other => panic!("expected MechanismCollision, got {other:?}"),
        }
    }

    #[test]
    fn invalid_identity_name_is_rejected_for_mtls_mechanism() {
        let req = OnboardClientRequest {
            name: "Not A Valid Name!".to_string(),
            mechanism: ClientMechanism::MtlsCert,
        };
        let err = onboard_client(&empty_map(), &empty_allowlist(), req)
            .expect_err("a malformed identity name must be rejected before touching the CA");
        assert!(matches!(err, OnboardClientError::InvalidRequest(_)));
    }

    #[test]
    fn empty_tailnet_login_is_rejected() {
        let req = OnboardClientRequest {
            name: "some-client".to_string(),
            mechanism: ClientMechanism::Tailnet { login: "  ".to_string(), tags: vec![] },
        };
        let err = onboard_client(&empty_map(), &empty_allowlist(), req)
            .expect_err("an empty/blank tailnet login must be rejected");
        assert!(matches!(err, OnboardClientError::InvalidRequest(_)));
    }

    // ── No secret / CA private-key leakage in the tool's JSON output ───────

    #[tokio::test]
    #[serial]
    async fn tool_json_output_never_contains_ca_private_key_material() {
        let store_path = std::env::temp_dir().join(format!(
            "mesh12-onboard-client-tool-{}.json",
            std::process::id()
        ));
        std::env::set_var("TERMINUS_CA_STORE_PATH", store_path.to_string_lossy().to_string());
        std::env::remove_var("TERMINUS_MESH_PRINCIPAL_MAP_JSON");
        std::env::remove_var("TERMINUS_GATEWAY_ALLOWLIST_JSON");

        let ca = crate::pki::ca().expect("CA bootstraps");
        let ca_key_pem = ca.key_pem();

        let tool = MeshOnboardClient;
        let args = json!({ "name": "dev-box-claude-code-2", "mechanism": "mtls_cert" });
        let output = tool.execute(args).await.expect("tool call should succeed");

        assert!(!output.contains(&ca_key_pem), "tool output must never contain the CA's own private key");
        assert!(output.contains("\"dry_run_config\":true"));
        assert!(output.contains("\"kind\":\"mtls_cert\""));

        std::env::remove_var("TERMINUS_CA_STORE_PATH");
        std::fs::remove_file(&store_path).ok();
    }

    #[test]
    fn tool_metadata_matches_registration_conventions() {
        let tool = MeshOnboardClient;
        assert_eq!(tool.name(), "mesh_onboard_client");
        assert!(!tool.description().is_empty());
        assert_eq!(tool.parameters()["type"], "object");
    }
}
