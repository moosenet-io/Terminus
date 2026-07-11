//! Upstream onboarding workflow (MESH-11), built on MESH-01's
//! [`crate::mesh::registry::UpstreamRegistry`], MESH-02's
//! [`crate::mesh::client::UpstreamClient`], and MESH-03's
//! [`crate::mesh::merge::namespaced`].
//!
//! Before this item, adding a new upstream Terminus to the mesh meant an
//! operator hand-authoring one more entry into `TERMINUS_MESH_UPSTREAMS_JSON`
//! and hoping the namespace didn't collide, the transport was reachable, and
//! the secret (if any) actually resolved — no first-class way to check any of
//! that *before* committing the change. This module adds exactly that check:
//! probe the candidate, discover its tool catalog, verify namespace safety
//! and trust readiness, and preview the namespaced catalog delta the merge
//! step (MESH-03) would produce — all WITHOUT writing anything anywhere.
//!
//! ## What this deliberately does NOT do
//! - Never mutates `TERMINUS_MESH_UPSTREAMS_JSON`, any file, or any other
//!   live config. [`onboard_upstream`] is read-only end to end: on success it
//!   *emits* the validated JSON entry for the operator to append via the
//!   sanctioned config path (editing `TERMINUS_MESH_UPSTREAMS_JSON` and
//!   restarting/reloading, same as any other mesh config change) — it never
//!   writes that entry itself.
//! - Never prints a secret VALUE. A Bearer candidate's trust step only
//!   confirms the named `secret_key` resolves via
//!   [`crate::mesh::registry::UpstreamServer::resolve_secret`] (the same
//!   lazy, env-backed resolution [`crate::mesh::client::UpstreamClient`]
//!   already uses) — the resolved [`crate::mesh::registry::ResolvedSecret`]
//!   is dropped immediately after the check, never placed in
//!   [`OnboardingReport`] or logged.
//! - Never contacts a real remote server on its own initiative beyond the
//!   single candidate URL the caller explicitly supplied — no crawling, no
//!   speculative probing of other hosts.
//!
//! ## mTLS trust
//! Mesh peers share one embedded-CA trust domain (see `client`'s module doc):
//! there is no separate handshake with the candidate to "enroll" here, the
//! same way [`crate::mesh::client::UpstreamClient::from_upstream`] mints a
//! short-lived CLIENT leaf cert against [`crate::pki::ca`] and pins that same
//! CA as the trust root for the candidate's presented server cert. Driving
//! that existing `/enroll`-issued trust flow is therefore exactly what
//! building the candidate's [`UpstreamClient`] already does — this module's
//! mTLS trust step is confirming that construction succeeds (i.e. this
//! node's embedded CA is bootstrapped and can mint the client identity the
//! candidate will need to trust), surfacing
//! [`crate::mesh::client::UpstreamClientError::TlsConfig`] as
//! [`OnboardingError::TrustFailed`] if not.
//!
//! ## Bearer trust
//! Confirmed via the same `UpstreamClient::from_upstream` call: a Bearer
//! candidate whose `secret_key` doesn't resolve from the process environment
//! surfaces [`crate::mesh::client::UpstreamClientError::SecretUnavailable`]
//! (itself built from [`crate::mesh::registry::MeshConfigError`], which only
//! ever names the missing/empty key, never a value) as
//! [`OnboardingError::TrustFailed`] — onboarding blocks with a clear message,
//! no secret printed, per this item's acceptance criteria.

use serde_json::{json, Value};
use thiserror::Error;

use super::client::{ToolMeta, UpstreamClient, UpstreamClientError};
use super::merge::namespaced;
use super::registry::{MeshConfigError, UpstreamRegistry, UpstreamServer, UpstreamTransport};

/// Namespace suffixing ceiling for collision suggestions — mirrors
/// `registry::NAMESPACE_MAX_LEN`. Duplicated locally rather than made `pub`
/// on that module, matching this crate's established convention of small,
/// self-contained per-module constants (see e.g. `registry::env_nonempty`'s
/// own doc comment on why it's duplicated rather than shared).
const NAMESPACE_MAX_LEN: usize = 16;

/// How many alternate-namespace suggestions [`OnboardingError::NamespaceCollision`]
/// offers at most.
const MAX_SUGGESTIONS: usize = 3;

/// One onboarding candidate's input: everything an operator supplies to try
/// onboarding a new upstream. Deliberately mirrors
/// [`UpstreamServer`]'s fields (this *becomes* an `UpstreamServer` once
/// validated) rather than introducing a parallel shape.
#[derive(Debug, Clone)]
pub struct OnboardingRequest {
    pub name: String,
    pub url: String,
    pub transport: UpstreamTransport,
    pub namespace: String,
    /// NAME of the credential (never a value) — required for
    /// [`UpstreamTransport::Bearer`], ignored for
    /// [`UpstreamTransport::Mtls`] (mTLS trust is carried entirely by the
    /// client cert, see the module doc).
    pub secret_key: Option<String>,
}

/// Which trust mechanism this candidate uses, and whether it's ready.
/// Never holds a secret value — see the module doc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustStatus {
    /// mTLS: this node's embedded CA bootstrapped successfully and minted a
    /// client identity the candidate can trust (see the module doc's "mTLS
    /// trust" section).
    MtlsReady,
    /// Bearer: the named `secret_key` resolved from the process environment.
    /// Carries only the key NAME, never the resolved value.
    BearerResolved { secret_key: String },
}

/// The result of a successful (or at least non-fatally-blocked) onboarding
/// dry-run. Nothing in this struct is ever a secret value.
#[derive(Debug, Clone)]
pub struct OnboardingReport {
    pub name: String,
    pub namespace: String,
    pub transport: UpstreamTransport,
    pub trust: TrustStatus,
    /// `true` when `GET /healthz` succeeded. Best-effort/non-fatal: a
    /// candidate can still onboard on a failed health probe as long as
    /// `tools/list` itself succeeded (some minimal MCP servers have no
    /// `/healthz` route at all) — see [`UpstreamClient::health_probe`]'s own
    /// "never returns an Err" contract.
    pub health_probe_ok: bool,
    pub discovered_tools: Vec<ToolMeta>,
    /// The namespaced `tools/list` delta the MESH-03 merge step would add
    /// (`<namespace>__<tool>` for each discovered tool) — a PREVIEW only,
    /// nothing is merged/committed by this workflow.
    pub catalog_delta: Vec<String>,
    /// Non-fatal notices (e.g. "zero tools discovered").
    pub warnings: Vec<String>,
    /// The validated JSON entry for the operator to append to
    /// `TERMINUS_MESH_UPSTREAMS_JSON` via the sanctioned config path. This
    /// workflow emits it; it never writes it anywhere itself.
    pub emitted_entry: Value,
}

/// Everything that can stop an onboarding attempt before (or during) the
/// probe, each naming exactly what's wrong and never a secret value.
#[derive(Debug, Error)]
pub enum OnboardingError {
    /// The candidate's `name`/`url`/`namespace` shape itself is invalid
    /// (reuses [`UpstreamRegistry`]'s own single-entry validation, so this
    /// covers empty name/url and malformed namespace charset/length exactly
    /// as strictly as the registry the entry would eventually join).
    #[error("invalid onboarding request: {0}")]
    InvalidRequest(String),
    /// The proposed `name` is already used by a currently-configured
    /// upstream.
    #[error("upstream name \"{0}\" is already in use")]
    DuplicateName(String),
    /// The proposed `namespace` is already used by a currently-configured
    /// upstream; `suggestions` are free alternatives of the same shape.
    #[error(
        "namespace \"{namespace}\" is already used by upstream \"{taken_by}\"; try one of: {suggestions:?}"
    )]
    NamespaceCollision {
        namespace: String,
        taken_by: String,
        suggestions: Vec<String>,
    },
    /// Trust could not be established: an mTLS candidate whose local CA
    /// bootstrap failed, or a Bearer candidate whose named secret doesn't
    /// resolve from the process environment. The message (from
    /// [`UpstreamClientError`]'s `Display`) never includes a secret value —
    /// only the upstream name and the key NAME, if any.
    #[error("could not establish trust for candidate upstream \"{0}\": {1}")]
    TrustFailed(String, String),
    /// The candidate was reachable enough to build a client, but probing it
    /// (`tools/list`) failed outright — unreachable host, handshake failure,
    /// non-2xx rejection, or an unparseable response. Nothing is written on
    /// this path.
    #[error("candidate upstream \"{0}\" could not be probed: {1}")]
    Unreachable(String, String),
}

/// Run the onboarding dry-run workflow end to end: validate shape, check for
/// namespace/name collisions against `existing`, establish/confirm trust,
/// probe + discover the candidate's tool catalog, and build the namespaced
/// delta + the config entry an operator would append. Never mutates
/// `existing` or anything else — see the module doc.
pub async fn onboard_upstream(
    existing: &UpstreamRegistry,
    req: OnboardingRequest,
) -> Result<OnboardingReport, OnboardingError> {
    let candidate = validate_shape(&req)?;

    if let Some(taken) = existing.all().iter().find(|u| u.name == candidate.name) {
        return Err(OnboardingError::DuplicateName(taken.name.clone()));
    }
    if let Some(taken) = existing.by_namespace(&candidate.namespace) {
        return Err(OnboardingError::NamespaceCollision {
            namespace: candidate.namespace.clone(),
            taken_by: taken.name.clone(),
            suggestions: suggest_free_namespaces(&candidate.namespace, existing),
        });
    }

    let client = UpstreamClient::from_upstream(&candidate)
        .map_err(|e| OnboardingError::TrustFailed(candidate.name.clone(), e.to_string()))?;

    let trust = match candidate.transport {
        UpstreamTransport::Mtls => TrustStatus::MtlsReady,
        UpstreamTransport::Bearer => TrustStatus::BearerResolved {
            // Safe to unwrap: `validate_shape` already rejected a Bearer
            // request with no `secret_key`, and `UpstreamClient::from_upstream`
            // above would already have failed with `SecretUnavailable` (caught
            // as `TrustFailed`) had the named secret not resolved.
            secret_key: candidate.secret_key.clone().unwrap_or_default(),
        },
    };

    let health_probe_ok = client.health_probe().await;

    let discovered_tools = client
        .list_tools()
        .await
        .map_err(|e| classify_probe_failure(&candidate.name, e))?;

    let mut warnings = Vec::new();
    if discovered_tools.is_empty() {
        warnings.push(format!(
            "upstream \"{}\" is reachable but exports zero tools",
            candidate.name
        ));
    }
    if !health_probe_ok {
        warnings.push(format!(
            "upstream \"{}\" did not respond successfully to GET /healthz (tools/list still succeeded, so onboarding continues)",
            candidate.name
        ));
    }

    let catalog_delta = discovered_tools
        .iter()
        .map(|t| namespaced(&candidate.namespace, &t.name))
        .collect();

    let emitted_entry = emit_entry(&candidate);

    Ok(OnboardingReport {
        name: candidate.name,
        namespace: candidate.namespace,
        transport: candidate.transport,
        trust,
        health_probe_ok,
        discovered_tools,
        catalog_delta,
        warnings,
        emitted_entry,
    })
}

/// A transport-level probe failure (unreachable, timeout, bad response) is
/// always [`OnboardingError::Unreachable`] — distinct from
/// [`OnboardingError::TrustFailed`], which is specifically "the client
/// couldn't even be constructed" (missing secret / broken local CA). By the
/// time [`UpstreamClient::list_tools`] runs, the client already built
/// successfully, so any [`UpstreamClientError`] surfacing here is a
/// reachability problem, not a trust one — including a `TlsConfig` variant
/// (an mTLS handshake rejection at DIAL time, as opposed to at client
/// CONSTRUCTION time, which is caught earlier as `TrustFailed`).
fn classify_probe_failure(name: &str, e: UpstreamClientError) -> OnboardingError {
    OnboardingError::Unreachable(name.to_string(), e.to_string())
}

/// Validate the candidate's shape by routing it through
/// [`UpstreamRegistry::from_json`]'s own single-entry validation (unique
/// name/namespace among the ONE entry is trivially satisfied; the useful
/// checks it performs here are non-empty name/url and namespace
/// charset/length) — reusing that logic rather than duplicating the regex,
/// so this workflow can never drift from what the registry itself would
/// eventually accept.
fn validate_shape(req: &OnboardingRequest) -> Result<UpstreamServer, OnboardingError> {
    if matches!(req.transport, UpstreamTransport::Bearer)
        && req.secret_key.as_deref().map(str::trim).unwrap_or("").is_empty()
    {
        return Err(OnboardingError::InvalidRequest(
            "transport \"bearer\" requires a non-empty secret_key (credential NAME, never a value)"
                .to_string(),
        ));
    }

    let transport_str = match req.transport {
        UpstreamTransport::Mtls => "mtls",
        UpstreamTransport::Bearer => "bearer",
    };
    let single_entry = json!([{
        "name": req.name,
        "url": req.url,
        "transport": transport_str,
        "namespace": req.namespace,
        "secret_key": req.secret_key,
        "enabled": true,
    }]);

    let parsed = UpstreamRegistry::from_json(&single_entry.to_string())
        .map_err(|e: MeshConfigError| OnboardingError::InvalidRequest(e.to_string()))?;
    Ok(parsed.all()[0].clone())
}

/// Build the validated JSON entry the operator appends to
/// `TERMINUS_MESH_UPSTREAMS_JSON` — the same shape [`UpstreamServer`]
/// deserializes from. Only ever includes the `secret_key` NAME, never a
/// value (this struct/module never reads one).
fn emit_entry(candidate: &UpstreamServer) -> Value {
    let transport_str = match candidate.transport {
        UpstreamTransport::Mtls => "mtls",
        UpstreamTransport::Bearer => "bearer",
    };
    json!({
        "name": candidate.name,
        "url": candidate.url,
        "transport": transport_str,
        "namespace": candidate.namespace,
        "secret_key": candidate.secret_key,
        "enabled": true,
    })
}

/// Suggest up to [`MAX_SUGGESTIONS`] free alternative namespaces by
/// appending `2`, `3`, `4`, … to `base` (truncated so the result never
/// exceeds [`NAMESPACE_MAX_LEN`]), skipping any already taken in `existing`.
fn suggest_free_namespaces(base: &str, existing: &UpstreamRegistry) -> Vec<String> {
    let mut suggestions = Vec::with_capacity(MAX_SUGGESTIONS);
    for n in 2..100u32 {
        if suggestions.len() >= MAX_SUGGESTIONS {
            break;
        }
        let suffix = n.to_string();
        let truncate_at = NAMESPACE_MAX_LEN.saturating_sub(suffix.len());
        let mut candidate: String = base.chars().take(truncate_at).collect();
        candidate.push_str(&suffix);
        if existing.by_namespace(&candidate).is_none() && candidate != base {
            suggestions.push(candidate);
        }
    }
    suggestions
}

// ---------------------------------------------------------------------------
// Core tool: mesh_onboard_upstream
// ---------------------------------------------------------------------------

use async_trait::async_trait;

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::RustTool;

/// Terminus CORE tool wrapping [`onboard_upstream`] — a read-only dry-run
/// workflow an operator/agent calls to try onboarding a candidate upstream
/// before hand-editing `TERMINUS_MESH_UPSTREAMS_JSON`. See the module doc for
/// the full "what this deliberately does NOT do" list: this tool never
/// mutates live config and never prints a secret value.
pub struct MeshOnboardUpstream;

#[async_trait]
impl RustTool for MeshOnboardUpstream {
    fn name(&self) -> &str {
        "mesh_onboard_upstream"
    }

    fn description(&self) -> &str {
        "Dry-run onboarding workflow for a new upstream Terminus mesh peer: probes the \
         candidate (MCP initialize + tools/list, plus a best-effort /healthz check), \
         discovers its tool catalog, checks the proposed namespace/name for collisions \
         against the currently-configured mesh registry, confirms trust readiness (mTLS: \
         this node's embedded CA; bearer: the named secret_key resolves from the process \
         environment -- never prints its value), and previews the namespaced tools/list \
         delta the merge step would add. Never mutates any live config: on success it \
         emits the validated JSON entry for an operator to append to \
         TERMINUS_MESH_UPSTREAMS_JSON via the sanctioned config path."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Stable unique identifier for this upstream (e.g. \"personal\")."
                },
                "url": {
                    "type": "string",
                    "description": "Reachable base URL for the candidate's MCP endpoint."
                },
                "transport": {
                    "type": "string",
                    "enum": ["mtls", "bearer"],
                    "description": "How to authenticate calls to this upstream."
                },
                "namespace": {
                    "type": "string",
                    "description": "Proposed short lowercase-alphanumeric namespace prefix (2-16 chars)."
                },
                "secret_key": {
                    "type": "string",
                    "description": "NAME of the credential in the process environment (never a value) -- required when transport is \"bearer\"."
                }
            },
            "required": ["name", "url", "transport", "namespace"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let name = args
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgument("name is required".into()))?
            .to_string();
        let url = args
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgument("url is required".into()))?
            .to_string();
        let namespace = args
            .get("namespace")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgument("namespace is required".into()))?
            .to_string();
        let transport_raw = args
            .get("transport")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArgument("transport is required".into()))?;
        let transport = match transport_raw.to_ascii_lowercase().as_str() {
            "mtls" => UpstreamTransport::Mtls,
            "bearer" => UpstreamTransport::Bearer,
            other => {
                return Err(ToolError::InvalidArgument(format!(
                    "unknown transport \"{other}\" (expected \"mtls\" or \"bearer\")"
                )))
            }
        };
        let secret_key = args
            .get("secret_key")
            .and_then(Value::as_str)
            .map(str::to_string);

        let req = OnboardingRequest { name, url, transport, namespace, secret_key };

        let existing = UpstreamRegistry::from_env().map_err(|e| {
            ToolError::Execution(format!("could not load the current mesh registry: {e}"))
        })?;

        let report = onboard_upstream(&existing, req).await.map_err(|e| match e {
            OnboardingError::InvalidRequest(_) => ToolError::InvalidArgument(e.to_string()),
            OnboardingError::DuplicateName(_) | OnboardingError::NamespaceCollision { .. } => {
                ToolError::Conflict(e.to_string())
            }
            OnboardingError::TrustFailed(_, _) => ToolError::NotConfigured(e.to_string()),
            OnboardingError::Unreachable(_, _) => ToolError::Http(e.to_string()),
        })?;

        Ok(report_to_json(&report).to_string())
    }
}

fn report_to_json(report: &OnboardingReport) -> Value {
    let trust = match &report.trust {
        TrustStatus::MtlsReady => json!({"kind": "mtls", "ready": true}),
        TrustStatus::BearerResolved { secret_key } => {
            json!({"kind": "bearer", "ready": true, "secret_key": secret_key})
        }
    };
    let transport_str = match report.transport {
        UpstreamTransport::Mtls => "mtls",
        UpstreamTransport::Bearer => "bearer",
    };
    json!({
        "name": report.name,
        "namespace": report.namespace,
        "transport": transport_str,
        "trust": trust,
        "health_probe_ok": report.health_probe_ok,
        "discovered_tool_count": report.discovered_tools.len(),
        "discovered_tools": report.discovered_tools.iter().map(|t| t.name.clone()).collect::<Vec<_>>(),
        "catalog_delta": report.catalog_delta,
        "warnings": report.warnings,
        "dry_run": true,
        "note": "Nothing was written. To onboard this upstream for real, append \"emitted_entry\" \
                 to TERMINUS_MESH_UPSTREAMS_JSON via the sanctioned config path and reload/restart.",
        "emitted_entry": report.emitted_entry,
    })
}

pub fn register(registry: &mut ToolRegistry) {
    if let Err(e) = registry.register(Box::new(MeshOnboardUpstream)) {
        tracing::error!("mesh::onboarding: failed to register tool: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::MockServer;
    use serial_test::serial;

    fn initialize_response() -> Value {
        json!({"jsonrpc": "2.0", "id": 1, "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "mock-upstream", "version": "0.0.0"}
        }})
    }

    fn mock_tools_list(server: &MockServer, tools: Value) {
        server.mock(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/mcp")
                .json_body_partial(r#"{"method": "initialize"}"#);
            then.status(200).header("Mcp-Session-Id", "s1").json_body(initialize_response());
        });
        server.mock(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/mcp")
                .json_body_partial(r#"{"method": "tools/list"}"#);
            then.status(200).json_body(json!({
                "jsonrpc": "2.0", "id": 2,
                "result": {"tools": tools}
            }));
        });
    }

    fn mock_healthz_ok(server: &MockServer) {
        server.mock(|when, then| {
            when.method(httpmock::Method::GET).path("/healthz");
            then.status(200).json_body(json!({"ok": true}));
        });
    }

    // ── Happy path: discovers tools, dry-runs the namespaced delta ─────────

    #[tokio::test]
    #[serial]
    async fn onboards_a_healthy_bearer_upstream_and_produces_a_dry_run_delta() {
        std::env::set_var("MESH_ONBOARD_TEST_TOKEN", "fixture-token-value"); // pii-test-fixture
        let server = MockServer::start();
        mock_healthz_ok(&server);
        mock_tools_list(
            &server,
            json!([
                {"name": "alpha", "description": "a", "inputSchema": {"type": "object"}},
                {"name": "beta", "description": "b", "inputSchema": {"type": "object"}}
            ]),
        );

        let req = OnboardingRequest {
            name: "candidate-a".to_string(),
            url: server.base_url(),
            transport: UpstreamTransport::Bearer,
            namespace: "cand".to_string(),
            secret_key: Some("MESH_ONBOARD_TEST_TOKEN".to_string()),
        };

        let report = onboard_upstream(&UpstreamRegistry::empty(), req)
            .await
            .expect("onboarding a healthy upstream should succeed");

        assert_eq!(report.discovered_tools.len(), 2);
        assert_eq!(
            report.catalog_delta,
            vec!["cand__alpha".to_string(), "cand__beta".to_string()]
        );
        assert!(report.health_probe_ok);
        assert!(report.warnings.is_empty());
        assert_eq!(
            report.trust,
            TrustStatus::BearerResolved { secret_key: "<REDACTED-SECRET>".to_string() }
        );
        assert_eq!(report.emitted_entry["namespace"], "cand");
        assert_eq!(report.emitted_entry["secret_key"], "MESH_ONBOARD_TEST_TOKEN");

        std::env::remove_var("MESH_ONBOARD_TEST_TOKEN");
    }

    // ── Namespace collision → rejected with next-free suggestions ──────────

    #[tokio::test]
    #[serial]
    async fn namespace_collision_is_rejected_with_suggestions() {
        std::env::set_var("MESH_ONBOARD_TEST_TOKEN", "fixture-token-value"); // pii-test-fixture
        let existing_json = r#"[{"name":"already-here","url":"https://taken.example.test","transport":"mtls","namespace":"taken"}]"#;
        let existing = UpstreamRegistry::from_json(existing_json).expect("valid fixture");

        let req = OnboardingRequest {
            name: "candidate-b".to_string(),
            url: "https://candidate-b.example.test".to_string(),
            transport: UpstreamTransport::Bearer,
            namespace: "taken".to_string(),
            secret_key: Some("MESH_ONBOARD_TEST_TOKEN".to_string()),
        };

        let err = onboard_upstream(&existing, req)
            .await
            .expect_err("a taken namespace must be rejected");
        match err {
            OnboardingError::NamespaceCollision { namespace, taken_by, suggestions } => {
                assert_eq!(namespace, "taken");
                assert_eq!(taken_by, "already-here");
                assert!(!suggestions.is_empty());
                assert!(!suggestions.contains(&"taken".to_string()));
            }
            other => panic!("expected NamespaceCollision, got {other:?}"),
        }
        std::env::remove_var("MESH_ONBOARD_TEST_TOKEN");
    }

    #[tokio::test]
    #[serial]
    async fn duplicate_name_is_rejected() {
        let existing_json = r#"[{"name":"dup","url":"https://a.example.test","transport":"mtls","namespace":"aaa"}]"#;
        let existing = UpstreamRegistry::from_json(existing_json).expect("valid fixture");

        let req = OnboardingRequest {
            name: "dup".to_string(),
            url: "https://b.example.test".to_string(),
            transport: UpstreamTransport::Mtls,
            namespace: "bbb".to_string(),
            secret_key: None,
        };

        let err = onboard_upstream(&existing, req).await.expect_err("duplicate name must be rejected");
        assert!(matches!(err, OnboardingError::DuplicateName(n) if n == "dup"));
    }

    // ── Unreachable candidate → clean failure, nothing written ─────────────

    #[tokio::test]
    #[serial]
    async fn unreachable_candidate_fails_cleanly() {
        std::env::set_var("MESH_ONBOARD_TEST_TOKEN", "fixture-token-value"); // pii-test-fixture
        let req = OnboardingRequest {
            name: "candidate-c".to_string(),
            url: "http://127.0.0.1:1".to_string(),
            transport: UpstreamTransport::Bearer,
            namespace: "canc".to_string(),
            secret_key: Some("MESH_ONBOARD_TEST_TOKEN".to_string()),
        };

        let err = onboard_upstream(&UpstreamRegistry::empty(), req)
            .await
            .expect_err("an unreachable candidate must fail cleanly, not panic");
        assert!(matches!(err, OnboardingError::Unreachable(_, _)));
        std::env::remove_var("MESH_ONBOARD_TEST_TOKEN");
    }

    // ── Zero tools → allowed, with a warning ────────────────────────────────

    #[tokio::test]
    #[serial]
    async fn zero_tools_onboards_with_a_warning() {
        std::env::set_var("MESH_ONBOARD_TEST_TOKEN", "fixture-token-value"); // pii-test-fixture
        let server = MockServer::start();
        mock_healthz_ok(&server);
        mock_tools_list(&server, json!([]));

        let req = OnboardingRequest {
            name: "candidate-d".to_string(),
            url: server.base_url(),
            transport: UpstreamTransport::Bearer,
            namespace: "cand".to_string(),
            secret_key: Some("MESH_ONBOARD_TEST_TOKEN".to_string()),
        };

        let report = onboard_upstream(&UpstreamRegistry::empty(), req)
            .await
            .expect("zero-tool upstream should still onboard");
        assert!(report.discovered_tools.is_empty());
        assert!(report.catalog_delta.is_empty());
        assert!(report.warnings.iter().any(|w| w.contains("zero tools")));
        std::env::remove_var("MESH_ONBOARD_TEST_TOKEN");
    }

    // ── Bearer secret-key missing from env → blocked, nothing printed ──────

    #[tokio::test]
    #[serial]
    async fn missing_bearer_secret_blocks_onboarding_without_printing_it() {
        std::env::remove_var("MESH_ONBOARD_TEST_TOKEN_MISSING");
        let req = OnboardingRequest {
            name: "candidate-e".to_string(),
            url: "https://candidate-e.example.test".to_string(),
            transport: UpstreamTransport::Bearer,
            namespace: "cane".to_string(),
            secret_key: Some("MESH_ONBOARD_TEST_TOKEN_MISSING".to_string()),
        };

        let err = onboard_upstream(&UpstreamRegistry::empty(), req)
            .await
            .expect_err("missing bearer secret must block onboarding");
        let message = err.to_string();
        assert!(matches!(err, OnboardingError::TrustFailed(_, _)));
        // The key NAME is fine to surface; no possible secret VALUE was ever
        // read for a missing var, so this also guards against a future
        // regression that would leak one.
        assert!(message.contains("MESH_ONBOARD_TEST_TOKEN_MISSING"));
    }

    #[tokio::test]
    async fn bearer_request_without_a_secret_key_is_rejected_up_front() {
        let req = OnboardingRequest {
            name: "candidate-f".to_string(),
            url: "https://candidate-f.example.test".to_string(),
            transport: UpstreamTransport::Bearer,
            namespace: "canf".to_string(),
            secret_key: None,
        };

        let err = onboard_upstream(&UpstreamRegistry::empty(), req)
            .await
            .expect_err("bearer transport with no secret_key must be rejected");
        assert!(matches!(err, OnboardingError::InvalidRequest(_)));
    }

    // ── Invalid namespace shape → rejected up front ─────────────────────────

    #[tokio::test]
    async fn invalid_namespace_charset_is_rejected() {
        let req = OnboardingRequest {
            name: "candidate-g".to_string(),
            url: "https://candidate-g.example.test".to_string(),
            transport: UpstreamTransport::Mtls,
            namespace: "Not_OK!".to_string(),
            secret_key: None,
        };

        let err = onboard_upstream(&UpstreamRegistry::empty(), req)
            .await
            .expect_err("a bad namespace charset must be rejected");
        assert!(matches!(err, OnboardingError::InvalidRequest(_)));
    }

    // ── No secret value ever appears in the tool's JSON output ─────────────

    #[tokio::test]
    #[serial]
    async fn tool_json_output_never_contains_the_resolved_secret_value() {
        std::env::set_var("MESH_ONBOARD_TEST_TOKEN", "fixture-token-value"); // pii-test-fixture
        let server = MockServer::start();
        mock_healthz_ok(&server);
        mock_tools_list(&server, json!([{"name": "alpha", "description": "a", "inputSchema": {}}]));

        let tool = MeshOnboardUpstream;
        let args = json!({
            "name": "candidate-h",
            "url": server.base_url(),
            "transport": "bearer",
            "namespace": "canh",
            "secret_key": "MESH_ONBOARD_TEST_TOKEN",
        });
        let output = tool.execute(args).await.expect("tool call should succeed");
        assert!(!output.contains("fixture-token-value"));
        assert!(output.contains("MESH_ONBOARD_TEST_TOKEN"));
        assert!(output.contains("\"dry_run\":true"));
        std::env::remove_var("MESH_ONBOARD_TEST_TOKEN");
    }

    #[test]
    fn tool_metadata_matches_registration_conventions() {
        let tool = MeshOnboardUpstream;
        assert_eq!(tool.name(), "mesh_onboard_upstream");
        assert!(!tool.description().is_empty());
        assert_eq!(tool.parameters()["type"], "object");
    }
}
