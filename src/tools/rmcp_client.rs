//! RMCP-08 — the connector lifecycle as Terminus tools.
//!
//! `rmcp_client_create`, `rmcp_client_list`, `rmcp_client_update`,
//! `rmcp_client_revoke`, plus the two that make gated DCR usable at all:
//! `rmcp_registration_token_mint` and `rmcp_registration_token_revoke_all`.
//!
//! ## Why the tool is the surface
//!
//! The same rule [`crate::tools::rmcp_session`] follows: the Connectors GUI
//! (RMCP-13) and an operator at a CLI must reach ONE implementation. A second
//! admin HTTP route beside these tools would be a second authorization
//! decision, a second audit path, and a second chance for "created in the UI"
//! to mean something different from "created". So the tool is the transport,
//! [`ClientService`] is the implementation, and the GUI is a caller like
//! anything else — the wire contract it already declares
//! (`constellation-web/src/types/rmcp.ts`) is what these tools return.
//!
//! ## The secret crosses this boundary exactly once
//!
//! `rmcp_client_create` returns `clientSecret` in its structured output when a
//! confidential client was asked for. That is the only time it exists outside
//! the argon2id hash in the database, and no other tool here can return one:
//! every read goes through [`crate::oauth::model::ClientAdmin`], which has no
//! field a secret could occupy. The human-readable text says so at the point
//! the operator reads it, not only in a doc they may never have opened.
//!
//! An initial access token is treated identically, and for the same reason: it
//! is a credential that authorizes creating a client.
//!
//! ## Why the owner is always named
//!
//! Every creating call takes an `owner` account name. There is deliberately no
//! inference and no default. These tools reach Terminus through the fleet's own
//! transports, which authenticate a mesh principal rather than an
//! `rmcp_account`, so this layer has no authenticated OAuth identity to read an
//! owner from — and a service that picked one anyway would be making an
//! authorization decision silently. Naming it is one word and removes the
//! guess.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::OnceCell;
use uuid::Uuid;

use crate::error::ToolError;
use crate::oauth::clients::{
    validate, ClientService, ClientView, FieldFault, SubmittedMetadata, DEFAULT_IAT_TTL_SECONDS,
};
use crate::oauth::store::OauthStore;
use crate::oauth::OauthConfig;
use crate::registry::ToolRegistry;
use crate::tool::{RustTool, ToolOutput};

/// How a tool obtains a [`ClientService`].
///
/// A seam, for the same reason [`crate::tools::rmcp_session`] has one: the
/// argument handling — which fields are required, what an absent field means,
/// what a bad one does — is testable without a database, and that is where the
/// dangerous mistakes live. "An absent `redirect_uris` silently cleared them"
/// is invisible in a code read and unrecoverable in production.
#[async_trait]
trait ClientSource: Send + Sync {
    async fn service(&self) -> Result<ClientService, ToolError>;
}

/// The production source: connect lazily, once, from the runtime environment.
struct EnvClientSource {
    cell: OnceCell<ClientService>,
}

impl EnvClientSource {
    fn new() -> Self {
        Self { cell: OnceCell::new() }
    }
}

#[async_trait]
impl ClientSource for EnvClientSource {
    async fn service(&self) -> Result<ClientService, ToolError> {
        self.cell
            .get_or_try_init(|| async {
                // The runtime secret store is materialized into the process
                // environment at startup, so this read IS the vault read — the
                // same path `rmcp_session` and `mount` take. The URL is never
                // logged or echoed.
                let config = OauthConfig::from_env()?;
                let store = OauthStore::connect(&config).await?;
                if !store.schema_ready().await {
                    return Err(ToolError::NotConfigured(
                        "the RMCP OAuth schema is not present — apply the S132 migrations \
                         (including S132-rmcp08-client-registration.sql) before managing \
                         connectors"
                            .into(),
                    ));
                }
                Ok(ClientService::new(store))
            })
            .await
            .cloned()
    }
}

// ---------------------------------------------------------------------------
// Argument helpers
// ---------------------------------------------------------------------------

/// A required, non-blank string argument.
fn required_str(args: &Value, name: &str) -> Result<String, ToolError> {
    args.get(name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .ok_or_else(|| ToolError::InvalidArgument(format!("{name} is required")))
}

/// A required UUID argument.
fn required_uuid(args: &Value, name: &str) -> Result<Uuid, ToolError> {
    let raw = required_str(args, name)?;
    Uuid::parse_str(&raw).map_err(|_| {
        // The rejected value is not echoed: these tools are reachable from a
        // GUI whose errors land in logs, and a caller that pastes a token into
        // the wrong field should not have it repeated back.
        ToolError::InvalidArgument(format!("{name} must be a UUID, as reported by rmcp_client_list"))
    })
}

/// An OPTIONAL array of strings.
///
/// Returns `Ok(None)` when the key is absent and `Ok(Some(vec![]))` when it is
/// present and empty. That distinction is the whole point on the update path:
/// absent means "leave this alone" and empty means "clear it", and a helper
/// that collapsed them would make clearing a client's namespaces impossible to
/// express — or, far worse, make omitting the field clear them.
fn optional_string_array(args: &Value, name: &str) -> Result<Option<Vec<String>>, ToolError> {
    let Some(value) = args.get(name) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let items = value
        .as_array()
        .ok_or_else(|| ToolError::InvalidArgument(format!("{name} must be an array of strings")))?;
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let text = item
            .as_str()
            .ok_or_else(|| ToolError::InvalidArgument(format!("{name} must be an array of strings")))?;
        out.push(text.to_string());
    }
    Ok(Some(out))
}

/// An OPTIONAL array of UUIDs, with the same absent-versus-empty rule.
fn optional_uuid_array(args: &Value, name: &str) -> Result<Option<Vec<Uuid>>, ToolError> {
    let Some(raw) = optional_string_array(args, name)? else {
        return Ok(None);
    };
    let mut out = Vec::with_capacity(raw.len());
    for item in &raw {
        out.push(Uuid::parse_str(item).map_err(|_| {
            ToolError::InvalidArgument(format!("{name} must contain UUIDs, as reported by rmcp_group_list"))
        })?);
    }
    Ok(Some(out))
}

/// Turn validation faults into one refusal.
///
/// Each fault renders from a static field name, an integer index and a static
/// message ([`FieldFault::render`]), so this message cannot carry a submitted
/// value however long or strange it was.
fn refuse(faults: Vec<FieldFault>) -> ToolError {
    let described: Vec<String> = faults.iter().map(FieldFault::render).collect();
    ToolError::InvalidArgument(described.join("; "))
}

/// The wire shape the GUI's `RmcpClient` declares.
///
/// camelCase because that contract is the authority on these names — this is a
/// transcription of it, not a second opinion about what a client looks like.
fn client_json(view: &ClientView) -> Value {
    let client = &view.client;
    json!({
        "id": client.id,
        "clientId": client.client_id,
        "name": client.name,
        "registrationSource": client.source().as_str(),
        // `enabled` is the negation of the stored `disabled` column, which is
        // what the GUI contract asks for. Stated once, here, so no caller has
        // to remember which polarity it is looking at.
        "enabled": !client.disabled,
        "confidential": client.confidential,
        "redirectUris": client.redirect_uris,
        "toolGroupIds": view.tool_group_ids,
        "namespaces": view.namespaces,
        "createdAt": client.created_at.to_rfc3339(),
        "version": client.version,
        // Cosmetic, per the GUI contract: the server refuses a write it should
        // not allow regardless of what this says, and the ownership check that
        // does the refusing lives in the store's own transaction. Reported as
        // `true` because RMCP-12's per-SESSION view scoping is not wired — a
        // caller reaching these tools has already passed the fleet's transport
        // authentication. It must not be read as an authorization answer.
        "editable": true,
    })
}

// ---------------------------------------------------------------------------
// rmcp_client_create
// ---------------------------------------------------------------------------

struct RmcpClientCreate {
    source: Arc<dyn ClientSource>,
}

#[async_trait]
impl RustTool for RmcpClientCreate {
    fn name(&self) -> &str {
        "rmcp_client_create"
    }

    fn description(&self) -> &str {
        "Mint a remote-MCP connector (OAuth client) owned by an account. Returns the public \
         client_id and, for a confidential client, the client secret — which is shown EXACTLY \
         ONCE and is never retrievable afterwards, because only an argon2id hash is stored. \
         Redirect URIs must be absolute https or RFC 8252 loopback URIs. A new client reaches NO \
         tools until it is scoped to tool groups and namespaces."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "owner": { "type": "string", "description": "Account name that will own this connector. Required — there is no default owner." },
                "name": { "type": "string", "description": "Display name, shown to the human on the consent page." },
                "redirect_uris": { "type": "array", "items": { "type": "string" }, "description": "Absolute https URIs, or RFC 8252 http loopback URIs. At least one." },
                "confidential": { "type": "boolean", "description": "Mint a client secret. Defaults to false — Claude and other hosted connectors are PUBLIC clients that authenticate with PKCE alone." },
                "tool_group_ids": { "type": "array", "items": { "type": "string" }, "description": "Tool groups to scope the connector to. Omit or leave empty and it reaches nothing." },
                "namespaces": { "type": "array", "items": { "type": "string" }, "description": "Mesh namespaces the connector may see. Omit or leave empty and it reaches nothing." }
            },
            "required": ["owner", "name", "redirect_uris"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.execute_structured(args).await?.text)
    }

    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let owner_name = required_str(&args, "owner")?;
        let confidential = args.get("confidential").and_then(Value::as_bool).unwrap_or(false);
        let submitted = SubmittedMetadata {
            name: args.get("name").and_then(Value::as_str).map(str::to_string),
            redirect_uris: optional_string_array(&args, "redirect_uris")?.unwrap_or_default(),
            // The tool's vocabulary is `confidential`; the RFC's is
            // `token_endpoint_auth_method`. They are translated HERE, in one
            // place, so validation and storage see one representation.
            token_endpoint_auth_method: Some(
                if confidential { "client_secret_basic" } else { "none" }.to_string(),
            ),
            ..Default::default()
        };
        let metadata = validate(&submitted).map_err(refuse)?;

        let service = self.source.service().await?;
        let owner = service.resolve_owner(&owner_name).await?;
        let minted = service.mint(owner, &metadata).await?;
        let id = minted.client.id;

        // Scoping is applied AFTER creation, through the same store methods the
        // update path uses. If it fails, the client exists and reaches nothing
        // — the safe direction, and the operator is told which half happened
        // rather than being handed a success for a half-applied request.
        let group_ids = optional_uuid_array(&args, "tool_group_ids")?;
        let namespaces = optional_string_array(&args, "namespaces")?;
        if group_ids.is_some() || namespaces.is_some() {
            if let Err(error) = service
                .update(
                    owner,
                    id,
                    minted.client.version,
                    None,
                    None,
                    group_ids.as_deref(),
                    namespaces.as_deref(),
                )
                .await
            {
                return Err(ToolError::Execution(format!(
                    "the connector was created but its scoping was NOT applied ({error}); it \
                     currently reaches no tools. Find it with rmcp_client_list and apply the \
                     scoping with rmcp_client_update"
                )));
            }
        }

        let view = service.get(id).await?;
        let mut structured = json!({ "client": client_json(&view) });
        // The one and only appearance of the plaintext.
        structured["clientSecret"] = match &minted.secret {
            Some(secret) => json!(secret),
            None => Value::Null,
        };

        let text = match &minted.secret {
            Some(secret) => format!(
                "created connector {} (client_id {})\nclient secret (SHOWN ONCE — it is stored \
                 only as an argon2id hash and cannot be retrieved again):\n{}\nscoped to {} tool \
                 group(s) and {} namespace(s)",
                view.client.name,
                view.client.client_id,
                secret,
                view.tool_group_ids.len(),
                view.namespaces.len()
            ),
            None => format!(
                "created public connector {} (client_id {}); no secret — it authenticates with \
                 PKCE alone\nscoped to {} tool group(s) and {} namespace(s)",
                view.client.name,
                view.client.client_id,
                view.tool_group_ids.len(),
                view.namespaces.len()
            ),
        };
        Ok(ToolOutput::with_structured(text, structured))
    }
}

// ---------------------------------------------------------------------------
// rmcp_client_list
// ---------------------------------------------------------------------------

struct RmcpClientList {
    source: Arc<dyn ClientSource>,
}

#[async_trait]
impl RustTool for RmcpClientList {
    fn name(&self) -> &str {
        "rmcp_client_list"
    }

    fn description(&self) -> &str {
        "List remote-MCP connectors (OAuth clients) with their scoping, registration source and \
         whether they are enabled. Never returns secret material of any kind — a client secret \
         exists only in the response to rmcp_client_create."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "owner": { "type": "string", "description": "Only this account's connectors. Omit for all of them." }
            },
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.execute_structured(args).await?.text)
    }

    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let service = self.source.service().await?;
        // Absent means "every connector", which is a safe default for a READ
        // and never for a write — the same split `rmcp_session_list` makes.
        let owner = match args.get("owner").and_then(Value::as_str).map(str::trim) {
            Some(name) if !name.is_empty() => Some(service.resolve_owner(name).await?),
            _ => None,
        };
        let views = service.list(owner).await?;

        let text = if views.is_empty() {
            "no connectors".to_string()
        } else {
            views
                .iter()
                .map(|view| {
                    format!(
                        "{} client_id={} source={} {} groups={} namespaces={} {}",
                        view.client.name,
                        view.client.client_id,
                        view.client.source().as_str(),
                        if view.client.disabled { "DISABLED" } else { "enabled" },
                        view.tool_group_ids.len(),
                        view.namespaces.len(),
                        // The state an operator most needs to notice on this
                        // listing: a client that authenticates and reaches
                        // nothing looks broken until you know it is unscoped.
                        if view.tool_group_ids.is_empty() || view.namespaces.is_empty() {
                            "(reaches NO tools — not scoped)"
                        } else {
                            ""
                        }
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        let clients: Vec<Value> = views.iter().map(client_json).collect();
        Ok(ToolOutput::with_structured(text, json!({ "clients": clients })))
    }
}

// ---------------------------------------------------------------------------
// rmcp_client_update
// ---------------------------------------------------------------------------

struct RmcpClientUpdate {
    source: Arc<dyn ClientSource>,
}

#[async_trait]
impl RustTool for RmcpClientUpdate {
    fn name(&self) -> &str {
        "rmcp_client_update"
    }

    fn description(&self) -> &str {
        "Edit a connector's scoping, redirect URIs, or enabled state. Requires the 'version' \
         reported by rmcp_client_list: if the connector has changed since it was read the edit is \
         REFUSED rather than overwriting whatever was saved in the meantime. An omitted field is \
         left alone; an empty array CLEARS that field."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "actor": { "type": "string", "description": "Account performing the edit. Must own this connector, or be an operator. Verified against the live account inside the write." },
                "id": { "type": "string", "description": "The connector's row id (UUID), as reported by rmcp_client_list." },
                "version": { "type": "integer", "description": "The version this edit was based on, from rmcp_client_list. A stale value is refused." },
                "enabled": { "type": "boolean", "description": "Enable or disable the connector. Disabling denies it at its next request." },
                "redirect_uris": { "type": "array", "items": { "type": "string" }, "description": "Replace the redirect URIs. Omit to leave them alone." },
                "tool_group_ids": { "type": "array", "items": { "type": "string" }, "description": "Replace the tool-group scoping. An empty array means the connector reaches nothing." },
                "namespaces": { "type": "array", "items": { "type": "string" }, "description": "Replace the namespace scoping. An empty array means the connector reaches nothing." }
            },
            "required": ["actor", "id", "version"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.execute_structured(args).await?.text)
    }

    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let id = required_uuid(&args, "id")?;
        let version = args
            .get("version")
            .and_then(Value::as_i64)
            .and_then(|v| i32::try_from(v).ok())
            .ok_or_else(|| {
                ToolError::InvalidArgument(
                    "version is required — pass the value rmcp_client_list reported, so a \
                     concurrent edit is refused rather than silently overwritten"
                        .into(),
                )
            })?;
        let enabled = args.get("enabled").and_then(Value::as_bool);
        let actor_name = required_str(&args, "actor")?;
        let redirect_uris = optional_string_array(&args, "redirect_uris")?;
        let group_ids = optional_uuid_array(&args, "tool_group_ids")?;
        let namespaces = optional_string_array(&args, "namespaces")?;

        if enabled.is_none()
            && redirect_uris.is_none()
            && group_ids.is_none()
            && namespaces.is_none()
        {
            // Refused rather than treated as a no-op success. An operator who
            // meant to change something and mistyped the field name would
            // otherwise be told the edit succeeded.
            return Err(ToolError::InvalidArgument(
                "name at least one thing to change: enabled, redirect_uris, tool_group_ids or \
                 namespaces"
                    .into(),
            ));
        }

        let service = self.source.service().await?;
        let actor = service.resolve_owner(&actor_name).await?;

        // Redirect URIs are validated by the SAME function registration uses.
        // A URI that could not be registered must not be reachable by editing
        // one in afterwards — that would be the second way to do one thing, and
        // the more dangerous of the two.
        if let Some(uris) = redirect_uris.as_deref() {
            let existing = service.get(id).await?;
            validate(&SubmittedMetadata {
                name: Some(existing.client.name.clone()),
                redirect_uris: uris.to_vec(),
                token_endpoint_auth_method: Some(
                    existing.client.token_endpoint_auth_method.clone(),
                ),
                ..Default::default()
            })
            .map_err(refuse)?;
        }

        // The actor is the account the CALLER named, resolved to an active
        // account, and authorized against this client inside the store's own
        // write transaction.
        //
        // Round 2 (`gpt56`): this line used to read
        // `service.get(id)?.client.owner_account_id` — the TARGET ROW's own
        // owner. That is not an authorization check, it asks the object being
        // modified who may modify it and takes its word, so it can only ever
        // answer yes.
        let view = service
            .update(
                actor,
                id,
                version,
                enabled,
                redirect_uris.as_deref(),
                group_ids.as_deref(),
                namespaces.as_deref(),
            )
            .await?;

        let text = format!(
            "updated connector {} (client_id {}): {} groups={} namespaces={} version={}",
            view.client.name,
            view.client.client_id,
            if view.client.disabled { "DISABLED" } else { "enabled" },
            view.tool_group_ids.len(),
            view.namespaces.len(),
            view.client.version
        );
        Ok(ToolOutput::with_structured(text, json!({ "client": client_json(&view) })))
    }
}

// ---------------------------------------------------------------------------
// rmcp_client_revoke
// ---------------------------------------------------------------------------

struct RmcpClientRevoke {
    source: Arc<dyn ClientSource>,
}

#[async_trait]
impl RustTool for RmcpClientRevoke {
    fn name(&self) -> &str {
        "rmcp_client_revoke"
    }

    fn description(&self) -> &str {
        "Revoke a connector: disable it and kill its live sessions. The caller is denied at its \
         NEXT request, not at the next token expiry, because the dispatch path re-reads the \
         client row on every call. Idempotent. Not approval-gated — it only ever narrows access, \
         and a confirmation step in front of the control an operator reaches for mid-incident is \
         the wrong trade."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "actor": { "type": "string", "description": "Account performing the revocation. Must own this connector, or be an operator." },
                "id": { "type": "string", "description": "The connector's row id (UUID), as reported by rmcp_client_list." }
            },
            "required": ["actor", "id"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.execute_structured(args).await?.text)
    }

    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        // No "revoke everything" shape exists here: the id is required, so the
        // empty-selector catastrophe `rmcp_session_revoke` had to refuse
        // explicitly cannot be expressed at all.
        let id = required_uuid(&args, "id")?;
        let actor_name = required_str(&args, "actor")?;
        let service = self.source.service().await?;
        let actor = service.resolve_owner(&actor_name).await?;
        let tokens = service.revoke(actor, id).await?;
        Ok(ToolOutput::with_structured(
            format!(
                "connector revoked: disabled, and {tokens} refresh token(s) killed. It is denied \
                 at its next request"
            ),
            json!({ "revoked": true, "tokensRevoked": tokens }),
        ))
    }
}

// ---------------------------------------------------------------------------
// Initial access tokens (RFC 7591)
// ---------------------------------------------------------------------------

struct RmcpRegistrationTokenMint {
    source: Arc<dyn ClientSource>,
}

#[async_trait]
impl RustTool for RmcpRegistrationTokenMint {
    fn name(&self) -> &str {
        "rmcp_registration_token_mint"
    }

    fn description(&self) -> &str {
        "Mint an initial access token for RFC 7591 dynamic client registration. Required by \
         POST /oauth/register, which refuses every unauthenticated registration — this is what \
         makes DCR an operator-authorized path rather than an anonymous write. The token is shown \
         EXACTLY ONCE (only its SHA-256 digest is stored), is single-use by default, and expires. \
         DCR is also off entirely unless RMCP_OAUTH_DCR_ENABLED is set."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "owner": { "type": "string", "description": "Account that issues the token and will own whatever registers through it." },
                "label": { "type": "string", "description": "A note for your own records, e.g. what it was minted for." },
                "uses": { "type": "integer", "description": "How many registrations it may authorize. Defaults to 1." },
                "ttl_seconds": { "type": "integer", "description": "Lifetime in seconds. Defaults to 3600." }
            },
            "required": ["owner"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.execute_structured(args).await?.text)
    }

    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let owner_name = required_str(&args, "owner")?;
        let label = args.get("label").and_then(Value::as_str).unwrap_or("").to_string();
        let uses = args
            .get("uses")
            .and_then(Value::as_i64)
            .and_then(|v| i32::try_from(v).ok())
            .unwrap_or(1);
        let ttl = args
            .get("ttl_seconds")
            .and_then(Value::as_i64)
            .unwrap_or(DEFAULT_IAT_TTL_SECONDS);

        let service = self.source.service().await?;
        let owner = service.resolve_owner(&owner_name).await?;
        let token = service.mint_registration_token(owner, &label, uses, ttl).await?;

        Ok(ToolOutput::with_structured(
            format!(
                "initial access token (SHOWN ONCE — only its digest is stored):\n{token}\n\
                 present it as `Authorization: Bearer <token>` to POST /oauth/register. \
                 {uses} use(s), expires in {ttl}s"
            ),
            json!({ "initialAccessToken": token, "uses": uses, "ttlSeconds": ttl }),
        ))
    }
}

struct RmcpRegistrationTokenRevokeAll {
    source: Arc<dyn ClientSource>,
}

#[async_trait]
impl RustTool for RmcpRegistrationTokenRevokeAll {
    fn name(&self) -> &str {
        "rmcp_registration_token_revoke_all"
    }

    fn description(&self) -> &str {
        "Revoke every outstanding initial access token, so no further dynamic client registration \
         can be authorized until a new one is minted. Blunt by design: a minted token is never \
         readable again, only its digest is stored, so there is no way to name one."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "actor": { "type": "string", "description": "Operator account performing the revocation." }
            },
            "required": ["actor"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.execute_structured(args).await?.text)
    }

    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let actor_name = required_str(&args, "actor")?;
        let service = self.source.service().await?;
        let actor = service.resolve_owner(&actor_name).await?;
        let revoked = service.revoke_registration_tokens(actor).await?;
        Ok(ToolOutput::with_structured(
            format!("{revoked} outstanding initial access token(s) revoked"),
            json!({ "revoked": revoked }),
        ))
    }
}

/// Register every RMCP-08 tool.
pub fn register(registry: &mut ToolRegistry) {
    let source: Arc<dyn ClientSource> = Arc::new(EnvClientSource::new());
    registry.register_or_replace(Box::new(RmcpClientCreate { source: source.clone() }));
    registry.register_or_replace(Box::new(RmcpClientList { source: source.clone() }));
    registry.register_or_replace(Box::new(RmcpClientUpdate { source: source.clone() }));
    registry.register_or_replace(Box::new(RmcpClientRevoke { source: source.clone() }));
    registry.register_or_replace(Box::new(RmcpRegistrationTokenMint { source: source.clone() }));
    registry.register_or_replace(Box::new(RmcpRegistrationTokenRevokeAll { source }));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A source that never yields a service. Every test below must refuse
    /// BEFORE reaching it — which is the assertion: argument handling happens
    /// first, so a bad request never touches the database.
    struct NeverConnects;

    #[async_trait]
    impl ClientSource for NeverConnects {
        async fn service(&self) -> Result<ClientService, ToolError> {
            Err(ToolError::NotConfigured("the test source must not be reached".into()))
        }
    }

    fn source() -> Arc<dyn ClientSource> {
        Arc::new(NeverConnects)
    }

    /// The tool names the GUI contract declares. A rename here breaks the
    /// Connectors page silently, so it is pinned.
    #[test]
    fn the_tool_names_match_the_declared_gui_contract() {
        let s = source();
        assert_eq!(RmcpClientCreate { source: s.clone() }.name(), "rmcp_client_create");
        assert_eq!(RmcpClientList { source: s.clone() }.name(), "rmcp_client_list");
        assert_eq!(RmcpClientUpdate { source: s.clone() }.name(), "rmcp_client_update");
        assert_eq!(RmcpClientRevoke { source: s }.name(), "rmcp_client_revoke");
    }

    /// Absent and empty are DIFFERENT instructions. This is the helper the
    /// whole update path's safety rests on: if absent collapsed to empty,
    /// editing a redirect URI would silently clear a connector's scoping.
    #[test]
    fn an_absent_array_and_an_empty_one_are_different_instructions() {
        let absent = json!({});
        let empty = json!({ "namespaces": [] });
        let full = json!({ "namespaces": ["alpha", "beta"] });
        let null = json!({ "namespaces": Value::Null });

        assert_eq!(optional_string_array(&absent, "namespaces").expect("ok"), None);
        assert_eq!(optional_string_array(&null, "namespaces").expect("ok"), None);
        assert_eq!(optional_string_array(&empty, "namespaces").expect("ok"), Some(vec![]));
        assert_eq!(
            optional_string_array(&full, "namespaces").expect("ok"),
            Some(vec!["alpha".to_string(), "beta".to_string()])
        );
        // A non-array, and an array of non-strings, are refused rather than
        // silently read as absent — which would be the fail-OPEN reading.
        assert!(optional_string_array(&json!({ "namespaces": "alpha" }), "namespaces").is_err());
        assert!(optional_string_array(&json!({ "namespaces": [1, 2] }), "namespaces").is_err());
    }

    /// An update naming nothing is REFUSED, not reported as a success.
    #[tokio::test]
    async fn an_update_that_changes_nothing_is_refused() {
        let tool = RmcpClientUpdate { source: source() };
        let error = tool
            .execute_structured(json!({
                "id": "11111111-1111-4111-8111-111111111111",
                "version": 1
            }))
            .await
            .expect_err("must refuse");
        let message = error.to_string();
        assert!(message.contains("at least one thing to change"), "{message}");
    }

    /// The version is REQUIRED, and its absence is refused before any store
    /// call. Without it two operators' edits silently overwrite each other,
    /// and the thing being overwritten is a connector's scoping.
    #[tokio::test]
    async fn an_update_without_a_version_is_refused_before_the_store_is_reached() {
        let tool = RmcpClientUpdate { source: source() };
        let error = tool
            .execute_structured(json!({
                "id": "11111111-1111-4111-8111-111111111111",
                "enabled": false
            }))
            .await
            .expect_err("must refuse");
        let message = error.to_string();
        assert!(message.contains("version is required"), "{message}");
        assert!(
            !message.contains("must not be reached"),
            "the store was consulted before the arguments were checked: {message}"
        );
    }

    /// Creating with a bad redirect URI is refused by the SAME validation the
    /// RFC 7591 endpoint uses, and the refusal never echoes the submitted URI.
    #[tokio::test]
    async fn a_bad_redirect_uri_is_refused_by_the_shared_validation() {
        let tool = RmcpClientCreate { source: source() };
        let error = tool
            .execute_structured(json!({
                "owner": "an-operator",
                "name": "A connector",
                "redirect_uris": ["http://distinctive-marker.test/cb"]
            }))
            .await
            .expect_err("must refuse");
        let message = error.to_string();
        assert!(message.contains("redirect_uris[0]"), "{message}");
        assert!(
            !message.contains("distinctive-marker"),
            "the refusal echoed the submitted URI: {message}"
        );
        assert!(
            !message.contains("must not be reached"),
            "the store was consulted before the arguments were checked: {message}"
        );
    }

    /// A revoke with no id cannot be expressed. `rmcp_session_revoke` had to
    /// refuse an empty selector explicitly because its arguments admitted one;
    /// this tool's do not, which is the stronger form of the same rule.
    #[tokio::test]
    async fn revoking_requires_naming_a_client() {
        let tool = RmcpClientRevoke { source: source() };
        for args in [json!({}), json!({ "id": "" }), json!({ "id": "not-a-uuid" })] {
            let error = tool.execute_structured(args.clone()).await.expect_err("must refuse");
            let message = error.to_string();
            assert!(
                !message.contains("must not be reached"),
                "{args} reached the store: {message}"
            );
        }
    }

    /// **Every mutating tool REFUSES a call that names no actor.**
    ///
    /// Asserted as a refusal, not as an absence of success. Round 2 (`gpt56`)
    /// found these writes unauthorized: the update tool derived its actor from
    /// the target row's own owner (which can only answer "yes"), and revoke and
    /// the token controls had no actor at all. An actor is now a required
    /// argument on all four, checked before the store is reached — and
    /// authorized against the live account inside the store's own write
    /// transaction.
    #[tokio::test]
    async fn every_mutating_tool_refuses_a_call_that_names_no_actor() {
        let s = source();
        let client_id = "11111111-1111-4111-8111-111111111111";

        let cases: Vec<(&str, Box<dyn RustTool>, Value)> = vec![
            (
                "rmcp_client_update",
                Box::new(RmcpClientUpdate { source: s.clone() }),
                json!({ "id": client_id, "version": 1, "enabled": false }),
            ),
            (
                "rmcp_client_update/redirect_uris",
                Box::new(RmcpClientUpdate { source: s.clone() }),
                json!({
                    "id": client_id,
                    "version": 1,
                    "redirect_uris": ["https://elsewhere.test/cb"]
                }),
            ),
            (
                "rmcp_client_revoke",
                Box::new(RmcpClientRevoke { source: s.clone() }),
                json!({ "id": client_id }),
            ),
            (
                "rmcp_registration_token_mint",
                Box::new(RmcpRegistrationTokenMint { source: s.clone() }),
                json!({ "label": "x" }),
            ),
            (
                "rmcp_registration_token_revoke_all",
                Box::new(RmcpRegistrationTokenRevokeAll { source: s.clone() }),
                json!({}),
            ),
        ];

        for (label, tool, args) in cases {
            let error = tool
                .execute_structured(args)
                .await
                .expect_err("a call naming no actor must be refused");
            let message = error.to_string();
            assert!(
                message.contains("is required"),
                "{label} did not refuse for a missing actor: {message}"
            );
            // And it refused BEFORE consulting the store, so an unauthorized
            // call never reaches a write at all.
            assert!(
                !message.contains("must not be reached"),
                "{label} reached the store before checking its arguments: {message}"
            );
        }
    }

    /// **The actor is never read from the object being modified.**
    ///
    /// The round-2 defect in one line: `service.get(id)?.client.owner_account_id`
    /// asked the target row who was allowed to change it. A source guard,
    /// because the property is "this expression is absent" and no runtime test
    /// can assert the absence of a line.
    ///
    /// The mutation target: restore that expression as the actor and this goes
    /// red.
    #[test]
    fn the_actor_is_never_derived_from_the_row_being_modified() {
        let file = include_str!("rmcp_client.rs");
        let production = file.split("\n#[cfg(test)]").next().expect("production half");
        let src: String = production
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            !src.contains("owner_account_id"),
            "the actor must come from the CALLER, never from the row being modified — asking \
             an object who may change it can only ever answer yes"
        );
        // Non-vacuity: the actor really is resolved from a caller-supplied
        // argument, on every mutating path.
        assert_eq!(
            src.matches("resolve_owner(&actor_name)").count(),
            3,
            "update, revoke and revoke-all must each resolve the actor the CALLER named"
        );
    }

    /// A rejected UUID must not be echoed: these tools are called from a GUI
    /// whose errors reach logs, and the field is one a caller could paste a
    /// credential into by mistake.
    #[test]
    fn a_rejected_identifier_is_not_echoed_back() {
        let error = required_uuid(&json!({ "id": "distinctive-marker-value" }), "id")
            .expect_err("must refuse");
        assert!(!error.to_string().contains("distinctive-marker-value"));
    }

    /// The wire shape must be the camelCase contract the GUI declares, and it
    /// must contain no secret-shaped field at all — asserted on the KEY SET, so
    /// adding one is what fails rather than adding one and forgetting to test.
    #[test]
    fn the_client_wire_shape_matches_the_contract_and_carries_no_secret() {
        use chrono::TimeZone as _;
        let view = ClientView {
            client: crate::oauth::model::ClientAdmin {
                id: Uuid::parse_str("11111111-1111-4111-8111-111111111111").expect("uuid"),
                client_id: "rmcp-abc".into(),
                name: "A connector".into(),
                redirect_uris: vec!["https://connector.test/cb".into()],
                grant_types: vec!["authorization_code".into()],
                token_endpoint_auth_method: "none".into(),
                owner_account_id: Uuid::parse_str("22222222-2222-4222-8222-222222222222")
                    .expect("uuid"),
                registration_source: "dcr".into(),
                disabled: true,
                confidential: false,
                created_at: chrono::Utc.timestamp_opt(0, 0).single().expect("epoch"),
                version: 3,
            },
            tool_group_ids: vec![],
            namespaces: vec![],
        };
        let rendered = client_json(&view);
        let keys: std::collections::BTreeSet<&str> =
            rendered.as_object().expect("object").keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "id",
                "clientId",
                "name",
                "registrationSource",
                "enabled",
                "confidential",
                "redirectUris",
                "toolGroupIds",
                "namespaces",
                "createdAt",
                "version",
                "editable",
            ]
            .into_iter()
            .collect::<std::collections::BTreeSet<&str>>(),
            "the wire shape no longer matches constellation-web's RmcpClient contract"
        );
        // `enabled` is the NEGATION of the stored column. Getting this backwards
        // would render a revoked connector as live on the operator's screen.
        assert_eq!(rendered["enabled"], json!(false));
        assert_eq!(rendered["registrationSource"], json!("dcr"));
        assert_eq!(rendered["version"], json!(3));
    }
}
