//! TERM #654 — `rmcp_account_create`, `rmcp_account_promote`, `rmcp_account_list`.
//!
//! ## The gap these close
//!
//! S132 built the whole RMCP OAuth door — login, consent, codes, tokens,
//! scoping, delegation, revocation — and shipped it with no way to create an
//! account. `OauthStore`'s insert had zero production callers, there was no
//! account-creation tool and no account-creation route, so `rmcp_account` was
//! empty and unpopulatable: nobody could log in at `/oauth/login`, no consent
//! could ever be granted, and `rmcp_client_create` could not name a valid
//! owner. Every layer was correct in isolation and the assembled door reached
//! nothing.
//!
//! ## The first account is the hard case, and here is the rule
//!
//! An "operator-only" guard cannot authenticate against an empty table. Three
//! options were considered:
//!
//! 1. **A bootstrap secret in the environment.** Rejected: it is a credential
//!    that must be provisioned, rotated and then remembered to be removed, and
//!    a bootstrap secret left set is a permanent second way in — the very thing
//!    this must not leave behind.
//! 2. **A CLI subcommand writing the row directly.** Rejected: a second writer
//!    of `rmcp_account` with its own copy of the rules. One door.
//! 3. **The emptiness of the operator set IS the authorization.** Chosen.
//!
//! So: **while this deployment has never had an account, whoever reaches this
//! tool may create the first one, and it is created as an operator. The instant
//! ANY account exists, that path is closed and every subsequent creation
//! requires an operator.** Trust on first use, closed forever after.
//!
//! (The first cut gated on "no ACTIVE OPERATOR", which is revocable and
//! therefore fail-open — review round 1 rejected it; see `create_account`.)
//!
//! What makes that acceptable rather than a hole is WHERE the tool is reachable
//! from. It is a local Terminus core tool, reached over the mTLS-verified
//! sanctioned door — not an HTTP route on the public edge, and deliberately not
//! added to the OAuth router, whose whole surface is unauthenticated by design.
//! Reaching this tool at all already required a verified fleet identity. The
//! bootstrap converts that into the door's first account exactly once.
//!
//! **The transition is not a mode this module carries.** The condition is
//! re-derived on the WRITE path, inside `OauthStore::create_account`'s
//! `BEGIN IMMEDIATE` transaction. What this module resolves is a HINT used to
//! choose which request to make; if it is stale — an operator was created in
//! between, or two bootstraps race — the store refuses. A check that an account
//! "was" the only one is point-in-time, and this module never relies on one.
//!
//! ## Who is acting — and why `actor` is an argument
//!
//! These tools reach Terminus over the fleet's own transports, which authenticate
//! a MESH PRINCIPAL, not an `rmcp_account`. There is therefore no authenticated
//! OAuth identity here to read an actor from — the same fact `rmcp_client_create`
//! documents when it refuses to default its `owner`.
//!
//! Two conventions already exist in this tree for that problem, and they pull in
//! opposite directions. `rmcp_owner` takes NO identity from arguments ("an
//! identity a caller can type is not an identity") and resolves the sole active
//! operator, or the one named by `RMCP_OPERATOR_ACCOUNT`. `rmcp_client_create`
//! (RMCP-08, TERM #647) takes an explicit `actor` and infers nothing.
//!
//! This module takes both, in that order of preference, and the distinction that
//! makes it safe is **attribution versus authority**: naming an `actor` confers
//! NOTHING. The store re-derives that account's operator status inside the
//! transaction, and a caller who can reach this tool can already act as any
//! operator by setting the environment variable — so the argument changes who
//! the action is recorded against, not what it may do. What it buys is the case
//! `rmcp_owner` can only refuse: a fleet with several operators, where the
//! environment can name exactly one and a GUI serving several humans would
//! otherwise attribute every administrative action to whichever one is
//! configured. With several operators and no `actor`, a WRITE refuses rather
//! than guessing — the property `rmcp_owner` was protecting all along. A READ
//! (`rmcp_account_list`) resolves any operator instead, because its answer does
//! not depend on which one asks and refusing would make a multi-operator door
//! unlistable; see `resolve_reader`.
//!
//! **What this does NOT yet do: emit an audit record.** `actor` decides whose
//! authority is used and is echoed in the result, but nothing here writes to
//! `crate::oauth::audit` the way RMCP-12's delegation does. Review round 2
//! (codex) was right to call that out: until it lands, this surface must not be
//! described as audited, and the docs no longer do. Tracked as follow-up.
//!
//! **`actor` is never inferred from anything else**, and there is nothing to
//! infer it from: unlike `rmcp_client_create` there is no `owner` here, so the
//! auto-copy defect TERM #647 fixed has no shape to take. A caller that omits it
//! on a multi-operator fleet gets a refusal naming the requirement.
//!
//! ## The password
//!
//! Arrives as plaintext, is hashed by RMCP-03's verifier
//! ([`crate::oauth::password::hash_password`]) before anything else touches it,
//! and reaches the store only as an [`crate::oauth::Argon2idHash`] — the store's
//! parameter type is what makes a caller that forgot to hash unable to reach the
//! column at all.
//!
//! It is never echoed: not in a tool result, not in a structured payload, not in
//! an error. The error paths are the part that needs saying, because that is
//! where an argument usually leaks — every refusal below is a fixed string or is
//! built from the account NAME, never from the submitted secret, and
//! `hash_password`'s own error text is deliberately generic for the same reason.
//!
//! These tools are also NOT in [`crate::approval`]'s guarded list, and that is
//! load-bearing rather than incidental: the approval gate PERSISTS a guarded
//! call's full `args_json` to its database, so guarding `rmcp_account_create`
//! would write the plaintext password to disk. `account_creation_is_not_approval_gated`
//! pins it.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::OnceCell;
use uuid::Uuid;

use crate::error::ToolError;
use crate::oauth::password::hash_password;
use crate::oauth::store::{AccountCreation, ActorSelection, OauthStore};
use crate::oauth::{OauthConfig, OPERATOR_ACCOUNT_ENV};
use crate::registry::ToolRegistry;
use crate::tool::{RustTool, ToolOutput};

/// The shortest password this surface will hash.
///
/// A floor, not a policy engine. The first account it protects can administer
/// the entire door, and a four-character operator password would make every
/// other control in S132 decorative. Length is the only property worth
/// enforcing here — composition rules push people toward predictable
/// substitutions and argon2id already answers the offline-guessing threat that
/// motivates them.
const MIN_PASSWORD_LEN: usize = 12;

/// Who this surface is acting as.
///
/// Deliberately NOT a decision. Both variants are requests the store
/// re-authorizes; see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Acting {
    /// This door has never had an account, so the one-shot first-account path
    /// is open.
    Bootstrap,
    /// Accounts exist but none is an active operator. Nothing can administer
    /// the door and the bootstrap will NOT reopen (it is gated on account
    /// existence, not on operator existence), so this needs an operator to fix
    /// out of band.
    ///
    /// A VARIANT, not an error, because of what review round 2 found when it
    /// was an error: the listing tool caught `ToolError::Conflict` to detect
    /// it, and `find_sole_operator_account` raises the SAME variant for a
    /// completely different fact — "several operators, name one". A healthy
    /// multi-operator deployment therefore reported itself as stranded and told
    /// the operator to go perform database surgery. Two distinct facts sharing
    /// one error variant, distinguished by catching it broadly: the collapse is
    /// the defect, so the fix is to stop collapsing them rather than to inspect
    /// the message.
    Stranded,
    /// This account is an active operator, and this is how the caller arrived
    /// at it — carried so the store can re-verify the sole-ness that an
    /// INFERRED actor depends on (review round 3).
    Operator(ActorSelection),
}

/// How a tool obtains a store and an acting identity.
///
/// A seam, exactly as `rmcp_session` and `rmcp_owner` have one, so argument
/// handling and the bootstrap/operator branching are testable without a
/// database.
#[async_trait]
trait AccountSource: Send + Sync {
    async fn store(&self) -> Result<Arc<OauthStore>, ToolError>;
    /// `requested` is the caller's explicit `actor` argument, if any.
    async fn acting(&self, requested: Option<&str>) -> Result<Acting, ToolError>;
    /// As [`Self::acting`], but for a READ — see [`resolve_reader`].
    async fn reading(&self, requested: Option<&str>) -> Result<Acting, ToolError>;
}

/// The production source: connect lazily, once, from the runtime environment.
///
/// The STORE is cached; the ACTING IDENTITY is not. Caching the identity would
/// be the stale-authority mistake this whole item is about — an operator
/// demoted or disabled after the first call would keep administering accounts
/// for the life of the process. It is re-resolved per call, and re-derived
/// again inside the store's transaction after that.
struct EnvAccountSource {
    cell: OnceCell<Arc<OauthStore>>,
}

impl EnvAccountSource {
    fn new() -> Self {
        Self { cell: OnceCell::new() }
    }
}

#[async_trait]
impl AccountSource for EnvAccountSource {
    async fn store(&self) -> Result<Arc<OauthStore>, ToolError> {
        self.cell
            .get_or_try_init(|| async {
                // As in `rmcp_session` and `rmcp_owner`: the runtime secret
                // store is materialized into the process environment at
                // startup, so this read IS the vault read. Nothing here is
                // logged or echoed.
                let config = OauthConfig::from_env()?;
                let store = OauthStore::connect(&config).await?;
                if !store.schema_ready().await {
                    return Err(ToolError::NotConfigured(
                        "the RMCP OAuth schema is not present — apply the S132 migration before \
                         creating accounts"
                            .into(),
                    ));
                }
                Ok(Arc::new(store))
            })
            .await
            .cloned()
    }

    async fn acting(&self, requested: Option<&str>) -> Result<Acting, ToolError> {
        resolve_acting(self.store().await?.as_ref(), requested).await
    }

    async fn reading(&self, requested: Option<&str>) -> Result<Acting, ToolError> {
        resolve_reader(self.store().await?.as_ref(), requested).await
    }
}

/// Establish which account this surface acts as, or that there is none yet.
///
/// Mirrors `rmcp_owner::resolve_actor` — same environment variable, same
/// refusal to guess between several operators — with ONE difference: "no active
/// operator" is [`Acting::Bootstrap`] here rather than an error, because it is
/// the state this surface exists to end.
///
/// `RMCP_OPERATOR_ACCOUNT` naming an account that is not an active operator
/// stays an ERROR rather than degrading to bootstrap. A configured deployment
/// whose named operator was demoted is a misconfiguration to report, and
/// silently falling through to the first-account path there would be the
/// unauthenticated creation route this item promised not to leave behind.
async fn resolve_acting(store: &OauthStore, requested: Option<&str>) -> Result<Acting, ToolError> {
    if let Some(name) = requested {
        return Ok(Acting::Operator(ActorSelection::Named(
            resolve_named_operator(store, name).await?,
        )));
    }
    if let Some(name) = std::env::var(OPERATOR_ACCOUNT_ENV)
        .ok()
        .map(|raw| raw.trim().to_string())
        .filter(|raw| !raw.is_empty())
    {
        let Some(id) = store.resolve_account_id(&name).await? else {
            return Err(ToolError::NotConfigured(format!(
                "{OPERATOR_ACCOUNT_ENV} names an account this deployment does not have"
            )));
        };
        if store.account_authority(id).await? != Some(true) {
            return Err(ToolError::NotConfigured(format!(
                "{OPERATOR_ACCOUNT_ENV} names an account that is not an active operator"
            )));
        }
        // NAMED, deliberately — and this is checked BEFORE the several-operator
        // ambiguity on purpose, which review round 4 read as a hole and is not.
        // `RMCP_OPERATOR_ACCOUNT` is the sanctioned way to say WHICH operator a
        // multi-operator fleet acts as (the same mechanism `rmcp_owner` uses);
        // an operator wrote that name into the deployment's configuration, so it
        // is a choice, not an inference, and it carries no sole-ness condition
        // to re-verify. Ambiguity is only a refusal when NOBODY has said which.
        // Pinned by `a_configured_environment_actor_resolves_a_multi_operator_fleet`.
        return Ok(Acting::Operator(ActorSelection::Named(id)));
    }

    // ONE snapshot answers all three questions (review round 3): asking them
    // separately let a bootstrap commit in between and turn a healthy door into
    // a reported-stranded one.
    let state = store.read_door_state().await?;
    if state.several_operators {
        return Err(ToolError::Conflict(
            "this fleet has more than one operator account; name the acting one so the action is \
             attributed to a person"
                .into(),
        ));
    }
    match state.any_operator {
        // INFERRED: sound only while this stays the only operator, which the
        // store re-verifies inside the write transaction.
        Some(id) => Ok(Acting::Operator(ActorSelection::InferredSole(id))),
        None if state.any_account => Ok(Acting::Stranded),
        None => Ok(Acting::Bootstrap),
    }
}

/// Resolve a NAMED account to an active operator, or refuse.
async fn resolve_named_operator(store: &OauthStore, name: &str) -> Result<Uuid, ToolError> {
    let Some(id) = store.resolve_account_id(name).await? else {
        return Err(ToolError::NotFound(format!("no account named {name}")));
    };
    if store.account_authority(id).await? != Some(true) {
        // Not distinguished further: which of "disabled" and "delegated" it is
        // would describe somebody else's row.
        return Err(ToolError::InvalidArgument(format!("{name} is not an active operator")));
    }
    Ok(id)
}

/// The refusal a write carries on a stranded door.
const STRANDED: &str =
    "this deployment has accounts but no ACTIVE operator, so nothing can administer it and the \
     first-account path will not reopen — re-enable or re-promote an operator account directly";

/// Resolve an operator identity for a READ.
///
/// Differs from [`resolve_acting`] in exactly one way: with several active
/// operators and no explicit `actor` it picks one instead of refusing. See
/// [`OauthStore::find_any_operator_account`] for why that is sound here and
/// unsound for a write — a listing is not scoped per operator, so there is no
/// attribution to get wrong, and refusing would make a multi-operator door
/// unlistable (you would have to already know an operator's name to discover
/// who the operators are).
///
/// It therefore only ever yields [`ActorSelection::Named`]: there is no
/// sole-ness claim to re-verify, because it never made one.
async fn resolve_reader(store: &OauthStore, requested: Option<&str>) -> Result<Acting, ToolError> {
    if let Some(name) = requested {
        return Ok(Acting::Operator(ActorSelection::Named(
            resolve_named_operator(store, name).await?,
        )));
    }
    let state = store.read_door_state().await?;
    match state.any_operator {
        Some(id) => Ok(Acting::Operator(ActorSelection::Named(id))),
        None if state.any_account => Ok(Acting::Stranded),
        None => Ok(Acting::Bootstrap),
    }
}

/// Read a trimmed, non-empty string argument, STRICTLY.
///
/// Only a MISSING property is absence. A present `null`, number, boolean, array
/// or whitespace-only string is a refusal — review round 3 (codex) found the
/// same malformed-as-default defect `strict_bool` fixed, hiding in the actor:
/// `{"actor": null}` was read as "no actor given" and the write then proceeded
/// as the INFERRED sole operator, so a malformed request was answered as a
/// different, well-formed one. On an identity argument that is the worst place
/// for it.
fn field(args: &Value, name: &str) -> Result<Option<String>, ToolError> {
    match args.get(name) {
        None => Ok(None),
        Some(Value::String(raw)) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                // A blank string is a filled-in-then-cleared field, not an
                // omitted one. Refusing says so instead of silently inferring.
                return Err(ToolError::InvalidArgument(format!("{name} must not be blank")));
            }
            Ok(Some(trimmed.to_string()))
        }
        Some(_) => Err(ToolError::InvalidArgument(format!("{name} must be a string"))),
    }
}

/// Read the password argument — TRIMMED ONLY OF NOTHING.
///
/// Separate from [`field`] on purpose. Trimming a password silently changes the
/// credential: an account created with a trailing space would then be
/// unloginnable with the exact string its owner was given, because
/// `/oauth/login` compares what was submitted. Leading and trailing whitespace
/// are legitimate password characters and this surface must not eat them.
///
/// Absent or non-string is `None`; an EMPTY string is `Some("")`, which the
/// length floor then refuses with the same message as any other short password.
fn password_field(args: &Value) -> Option<String> {
    args.get("password").and_then(Value::as_str).map(str::to_string)
}

/// Read an optional boolean argument STRICTLY: absent is `false`, a real
/// boolean is itself, and anything else is a REFUSAL.
///
/// Review round 1 (codex) found the fail-open this replaces. The idiomatic
/// `get(name).and_then(as_bool).unwrap_or(false)` treats a present-but-
/// wrong-typed value as ABSENT, and the registry does not validate arguments
/// against the declared JSON Schema — so `{"revoke": "true"}`, which is what a
/// hand-written client or a form that forgot to parse its checkbox sends, read
/// as `revoke: false` and PROMOTED the account instead of demoting it. The
/// caller asked to remove authority and was told, truthfully, that the
/// operation succeeded.
///
/// The defaulting direction cannot save this: `operator` defaults fail-closed
/// and `revoke`/`enable` do not, but all three are the same defect — a
/// malformed request answered as if it were a different, well-formed one. So
/// none of them guess. A refusal names the FIELD, never the submitted value.
fn strict_bool(args: &Value, name: &str) -> Result<bool, ToolError> {
    match args.get(name) {
        None => Ok(false),
        Some(Value::Bool(value)) => Ok(*value),
        // Round 2 (codex): an explicit `null` was folded into "absent", so
        // `{"revoke": null}` promoted and `{"enable": null}` disabled. It is the
        // same defect the string case was — a malformed request answered as if
        // it were a different, well-formed one — and it is what a client whose
        // variable failed to populate actually sends. The declared schema says
        // `"type": "boolean"`, which does not admit null, so refusing it is
        // also the schema-conformant reading.
        Some(_) => Err(ToolError::InvalidArgument(format!(
            "{name} must be a boolean (true or false), not null, a string or a number"
        ))),
    }
}

/// `rmcp_account_create`.
struct RmcpAccountCreate {
    source: Arc<dyn AccountSource>,
}

#[async_trait]
impl RustTool for RmcpAccountCreate {
    fn name(&self) -> &str {
        "rmcp_account_create"
    }

    fn description(&self) -> &str {
        "Create an account for the remote-MCP OAuth door — the human identity that logs in at \
         /oauth/login, grants consent, and owns connectors. If this deployment has no active \
         accounts yet, this creates the FIRST one as an operator; once ANY account exists, only \
         an operator may create further accounts and that first-account path is permanently \
         closed. The password is argon2id-hashed before storage and is never echoed back."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "actor": {
                    "type": "string",
                    "description": "The operator account performing this action. Optional when \
                                    this deployment has exactly one operator (or one is named in \
                                    the environment); REQUIRED when several exist, so an \
                                    administrative action is never performed as a guessed \
                                    identity. Never inferred from any other field."
                },
                "name": {
                    "type": "string",
                    "description": "Account name. This is what the person types at /oauth/login \
                                    and what maps to a fleet principal (RMCP-05)."
                },
                "password": {
                    "type": "string",
                    "description": "The account's password. Minimum 12 characters. Hashed with \
                                    argon2id before storage; never logged, echoed, or returned."
                },
                "operator": {
                    "type": "boolean",
                    "description": "Whether the new account holds fleet-operator authority. \
                                    Ignored when bootstrapping the first account, which is \
                                    always an operator. Defaults to false."
                }
            },
            "required": ["name", "password"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.execute_structured(args).await?.text)
    }

    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let Some(name) = field(&args, "name")? else {
            return Err(ToolError::InvalidArgument("name is required".into()));
        };
        let Some(password) = password_field(&args) else {
            return Err(ToolError::InvalidArgument("password is required".into()));
        };
        // Counted in CHARACTERS, not bytes: a byte floor would accept a shorter
        // non-ASCII passphrase than an ASCII one for no reason.
        if password.chars().count() < MIN_PASSWORD_LEN {
            // The refusal names the requirement, never the submission.
            return Err(ToolError::InvalidArgument(format!(
                "password must be at least {MIN_PASSWORD_LEN} characters"
            )));
        }
        let requested_operator = strict_bool(&args, "operator")?;

        let source = &self.source;
        let creation = match source.acting(field(&args, "actor")?.as_deref()).await? {
            Acting::Bootstrap => AccountCreation::Bootstrap,
            Acting::Stranded => return Err(ToolError::Conflict(STRANDED.into())),
            Acting::Operator(actor) => {
                AccountCreation::ByOperator { actor, operator: requested_operator }
            }
        };
        let bootstrap = creation == AccountCreation::Bootstrap;

        // Hashed HERE, by RMCP-03's verifier, before the store is touched. The
        // plaintext lives only in this function and is dropped at its end.
        let hashed = hash_password(&password)?;
        let id = source.store().await?.create_account(&name, &hashed, creation).await?;
        // Echoed so the caller can see WHICH operator the action was performed
        // as — the point of `actor` when it was inferred rather than named.
        let acted_as = match creation {
            AccountCreation::ByOperator { actor, .. } => Some(actor.account_id().to_string()),
            AccountCreation::Bootstrap => None,
        };

        // A bootstrap account is ALWAYS an operator, whatever `operator` said —
        // reported from the rule rather than from the argument, so the answer
        // cannot disagree with the row.
        let is_operator = bootstrap || requested_operator;
        let text = if bootstrap {
            format!(
                "created {name} as this deployment's first operator account. The first-account \
                 path is now closed; further accounts require an operator."
            )
        } else if is_operator {
            format!("created operator account {name}")
        } else {
            format!("created account {name}")
        };
        Ok(ToolOutput {
            text,
            structured: Some(json!({
                "account": name,
                "id": id.to_string(),
                "operator": is_operator,
                "bootstrap": bootstrap,
                "acted_as": acted_as,
            })),
        })
    }
}

/// `rmcp_account_promote`.
struct RmcpAccountPromote {
    source: Arc<dyn AccountSource>,
}

#[async_trait]
impl RustTool for RmcpAccountPromote {
    fn name(&self) -> &str {
        "rmcp_account_promote"
    }

    fn description(&self) -> &str {
        "Grant or withdraw fleet-operator authority on an RMCP OAuth account. Operator-only. \
         Withdrawing it from the last active operator is refused, because a deployment with no \
         operator can neither administer itself nor be bootstrapped again."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "actor": {
                    "type": "string",
                    "description": "The operator account performing this action. Optional when \
                                    this deployment has exactly one operator (or one is named in \
                                    the environment); REQUIRED when several exist, so an \
                                    administrative action is never performed as a guessed \
                                    identity. Never inferred from any other field."
                },
                "account": { "type": "string", "description": "The account name to change." },
                "revoke": {
                    "type": "boolean",
                    "description": "Withdraw operator authority instead of granting it."
                }
            },
            "required": ["account"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.execute_structured(args).await?.text)
    }

    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let Some(account) = field(&args, "account")? else {
            return Err(ToolError::InvalidArgument("account is required".into()));
        };
        let revoke = strict_bool(&args, "revoke")?;

        // Bootstrap is not a promotion path: an account created by the
        // first-account route is already an operator, so there is no state in
        // which this tool has work to do and no operator exists. Refusing here
        // says so, instead of reaching the store to be refused less clearly.
        let actor = match self.source.acting(field(&args, "actor")?.as_deref()).await? {
            Acting::Operator(actor) => actor,
            Acting::Stranded => return Err(ToolError::Conflict(STRANDED.into())),
            Acting::Bootstrap => {
                return Err(ToolError::InvalidArgument(
                    "this deployment has no accounts; create the first operator with \
                     rmcp_account_create"
                        .into(),
                ))
            }
        };

        let store = self.source.store().await?;
        let Some(target) = store.resolve_account_id(&account).await? else {
            return Err(ToolError::NotFound(format!("no account named {account}")));
        };
        let changed = store.set_account_operator(actor, target, !revoke).await?;
        let acted_as = actor.account_id().to_string();

        let text = match (revoke, changed) {
            (false, true) => format!("{account} is now an operator"),
            (false, false) => format!("{account} was already an operator"),
            (true, true) => format!("{account} is no longer an operator"),
            (true, false) => format!("{account} was not an operator"),
        };
        Ok(ToolOutput {
            text,
            structured: Some(json!({
                "account": account,
                "operator": !revoke,
                "changed": changed,
                "acted_as": acted_as,
            })),
        })
    }
}

/// `rmcp_account_disable`.
struct RmcpAccountDisable {
    source: Arc<dyn AccountSource>,
}

#[async_trait]
impl RustTool for RmcpAccountDisable {
    fn name(&self) -> &str {
        "rmcp_account_disable"
    }

    fn description(&self) -> &str {
        "Disable or re-enable an RMCP OAuth account. A disabled account cannot log in, cannot \
         consent, and stops satisfying every authorization its owner held — existing sessions \
         and tokens are refused on their next use, because the account is re-checked on the read \
         path. Operator-only. Disabling the last active operator is refused."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "actor": {
                    "type": "string",
                    "description": "The operator account performing this action. Optional when \
                                    this deployment has exactly one operator (or one is named in \
                                    the environment); REQUIRED when several exist, so an \
                                    administrative action is never performed as a guessed \
                                    identity. Never inferred from any other field."
                },
                "account": { "type": "string", "description": "The account name to change." },
                "enable": {
                    "type": "boolean",
                    "description": "Re-enable the account instead of disabling it."
                }
            },
            "required": ["account"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.execute_structured(args).await?.text)
    }

    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let Some(account) = field(&args, "account")? else {
            return Err(ToolError::InvalidArgument("account is required".into()));
        };
        let enable = strict_bool(&args, "enable")?;

        let actor = match self.source.acting(field(&args, "actor")?.as_deref()).await? {
            Acting::Operator(actor) => actor,
            Acting::Stranded => return Err(ToolError::Conflict(STRANDED.into())),
            Acting::Bootstrap => {
                return Err(ToolError::InvalidArgument(
                    "this deployment has no accounts; create the first operator with \
                     rmcp_account_create"
                        .into(),
                ))
            }
        };

        let store = self.source.store().await?;
        // NOT `resolve_account_id`-then-filter-active: re-enabling requires
        // naming a DISABLED account, so this lookup must see one. The store's
        // own read does the same, for the same reason.
        let Some(target) = store.resolve_account_id(&account).await? else {
            return Err(ToolError::NotFound(format!("no account named {account}")));
        };
        let changed = store.set_account_disabled(actor, target, !enable).await?;
        let acted_as = actor.account_id().to_string();

        let text = match (enable, changed) {
            (false, true) => format!("{account} is disabled"),
            (false, false) => format!("{account} was already disabled"),
            (true, true) => format!("{account} is enabled"),
            (true, false) => format!("{account} was already enabled"),
        };
        Ok(ToolOutput {
            text,
            structured: Some(json!({
                "account": account,
                "disabled": !enable,
                "changed": changed,
                "acted_as": acted_as,
            })),
        })
    }
}

/// `rmcp_account_list`.
struct RmcpAccountList {
    source: Arc<dyn AccountSource>,
}

#[async_trait]
impl RustTool for RmcpAccountList {
    fn name(&self) -> &str {
        "rmcp_account_list"
    }

    fn description(&self) -> &str {
        "List the RMCP OAuth door's accounts, with who holds operator authority and which are \
         disabled. Operator-only, except that a deployment with no operator reports that it \
         needs bootstrapping."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "actor": {
                    "type": "string",
                    "description": "Read as this operator account. ALWAYS optional, unlike the \
                                    writing tools: every active operator gets the identical \
                                    listing, so there is no attribution to guess and requiring \
                                    one would make a multi-operator door unlistable. A named \
                                    account must still be an active operator."
                }
            },
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.execute_structured(args).await?.text)
    }

    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let store = self.source.store().await?;
        // `reading`, not `acting`: a listing must not require the caller to
        // already know an operator's name in order to discover who the
        // operators are (round 2). And it MATCHES ON THE VARIANT rather than
        // catching an error — the previous cut caught `ToolError::Conflict` to
        // detect "stranded", which `find_sole_operator_account` also raises for
        // "several operators, name one", so a healthy multi-operator door
        // reported itself stranded and sent the operator to do database
        // surgery. `Acting::Stranded` exists so those two facts cannot be
        // confused again.
        let acting = self.source.reading(field(&args, "actor")?.as_deref()).await?;
        let Acting::Operator(actor) = acting else {
            // Derived from the SAME resolution, not re-read (review round 3):
            // a third query is a third chance for the answer to have moved, and
            // this is precisely the pair of facts whose disagreement produced a
            // false "stranded" report.
            let populated = matches!(acting, Acting::Stranded);
            let text = if populated {
                "this deployment has accounts but no ACTIVE operator, so nothing can administer \
                 it and the first-account path will not reopen — re-enable or re-promote an \
                 operator account directly"
                    .to_string()
            } else {
                "this deployment has no accounts; create the first operator with \
                 rmcp_account_create"
                    .to_string()
            };
            return Ok(ToolOutput {
                text,
                structured: Some(json!({
                    "accounts": [],
                    "bootstrap_available": !populated,
                    "stranded": populated,
                })),
            });
        };

        // `list_accounts` takes a bare id: a read has no sole-ness claim to
        // re-verify, so there is nothing for `ActorSelection` to carry here.
        let accounts = store.list_accounts(actor.account_id()).await?;
        let rows: Vec<Value> = accounts
            .iter()
            .map(|a| {
                json!({
                    "account": a.name,
                    "id": a.id.to_string(),
                    "operator": a.is_operator,
                    "disabled": a.disabled,
                    "created_at": a.created_at.to_rfc3339(),
                })
            })
            .collect();
        // No hash, no TOTP ciphertext, no `password_hash` — a listing is a
        // display surface and the row carries credential material.
        let text = if rows.is_empty() {
            "no accounts".to_string()
        } else {
            accounts
                .iter()
                .map(|a| {
                    let mut flags = Vec::new();
                    if a.is_operator {
                        flags.push("operator");
                    }
                    if a.disabled {
                        flags.push("disabled");
                    }
                    if flags.is_empty() {
                        a.name.clone()
                    } else {
                        format!("{} ({})", a.name, flags.join(", "))
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        Ok(ToolOutput {
            text,
            structured: Some(json!({
                "accounts": rows,
                "bootstrap_available": false,
                "stranded": false,
            })),
        })
    }
}

/// Register all four tools.
pub fn register(registry: &mut ToolRegistry) {
    let source: Arc<dyn AccountSource> = Arc::new(EnvAccountSource::new());
    registry.register_or_replace(Box::new(RmcpAccountCreate { source: source.clone() }));
    registry.register_or_replace(Box::new(RmcpAccountPromote { source: source.clone() }));
    registry.register_or_replace(Box::new(RmcpAccountDisable { source: source.clone() }));
    registry.register_or_replace(Box::new(RmcpAccountList { source }));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every tool this module registers. One list, so a tool added without
    /// being covered by the contract tests below is a compile-visible omission
    /// rather than a silently narrower assertion.
    const ACCOUNT_TOOLS: &[&str] = &[
        "rmcp_account_create",
        "rmcp_account_promote",
        "rmcp_account_disable",
        "rmcp_account_list",
    ];

    /// A source over a real, migrated SQLite database, resolving the acting
    /// identity exactly as production does.
    struct TestSource {
        store: Arc<OauthStore>,
    }

    #[async_trait]
    impl AccountSource for TestSource {
        async fn store(&self) -> Result<Arc<OauthStore>, ToolError> {
            Ok(self.store.clone())
        }
        async fn acting(&self, requested: Option<&str>) -> Result<Acting, ToolError> {
            resolve_acting(self.store.as_ref(), requested).await
        }
        async fn reading(&self, requested: Option<&str>) -> Result<Acting, ToolError> {
            resolve_reader(self.store.as_ref(), requested).await
        }
    }

    /// The database file's path is returned alongside the source so a test can
    /// open its OWN connection to it — see
    /// [`strand_the_deployment`]. The `TempDir` must be held by the caller:
    /// dropping it deletes the file.
    async fn fixture() -> (tempfile::TempDir, std::path::PathBuf, Arc<dyn AccountSource>) {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("rmcp-tools.db");
        let store = OauthStore::open_for_test(&path).await.expect("open and migrate");
        let source: Arc<dyn AccountSource> = Arc::new(TestSource { store: Arc::new(store) });
        (dir, path, source)
    }

    /// Demote every operator, OUT OF BAND.
    ///
    /// Deliberately a second connection to the same file rather than a
    /// test-only method on the store: the state under test is one the store's
    /// last-operator guard exists to make unreachable through its own API, so
    /// reaching it through that API would require adding a production
    /// back door to test that the front door is shut.
    async fn strand_the_deployment(path: &std::path::Path) {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                sqlx::sqlite::SqliteConnectOptions::new()
                    .filename(path)
                    .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal),
            )
            .await
            .expect("open the same database file");
        sqlx::query("UPDATE rmcp_account SET is_operator = 0")
            .execute(&pool)
            .await
            .expect("demote every operator");
    }

    fn create(source: &Arc<dyn AccountSource>) -> RmcpAccountCreate {
        RmcpAccountCreate { source: source.clone() }
    }

    /// **The end-to-end property this item exists for.**
    ///
    /// On an empty deployment, one call produces a usable first operator — and
    /// the account it produces is one `/oauth/login` can verify, which is the
    /// part that was actually missing. Verifying the stored hash with RMCP-03's
    /// own verifier is what proves the password went through the verifier layer
    /// rather than into the column raw.
    #[tokio::test]
    async fn the_first_call_on_an_empty_deployment_creates_a_usable_operator() {
        let (_dir, _path, source) = fixture().await;
        let out = create(&source)
            .execute_structured(json!({ "name": "moose", "password": "a-long-enough-passphrase" }))
            .await
            .expect("bootstrap must succeed");
        let s = out.structured.expect("structured");
        assert_eq!(s["operator"], json!(true), "the first account must be an operator");
        assert_eq!(s["bootstrap"], json!(true));

        let store = source.store().await.expect("store");
        let account = store
            .find_active_account_by_name("moose")
            .await
            .expect("no error")
            .expect("the account must exist");
        assert!(account.is_operator);
        assert!(
            crate::oauth::password::verify_password(
                "a-long-enough-passphrase",
                &account.password_hash
            ),
            "the stored value must verify with RMCP-03's verifier — otherwise nobody can log in"
        );
        assert_ne!(
            account.password_hash, "a-long-enough-passphrase",
            "the plaintext reached the column"
        );
    }

    /// **A second create RESOLVES to the operator path, not the bootstrap.**
    ///
    /// This is a TOOL-LAYER test and its claim is scoped to that, twice
    /// corrected. It does NOT mutation-verify the store's bootstrap gate:
    /// review rounds 1 and 3 (codex) both pointed out that its second call goes
    /// through `AccountCreation::ByOperator`, so deleting that gate would leave
    /// this green. What it pins is that `resolve_acting` stops reporting
    /// `Bootstrap` once an account exists, and that a delegated account is
    /// created delegated. The gate itself is pinned by
    /// `a_bootstrap_request_against_a_populated_deployment_is_refused_at_the_store`
    /// and by the store's concurrency tests.
    #[tokio::test]
    async fn a_second_create_resolves_to_the_operator_path_not_the_bootstrap() {
        let (_dir, _path, source) = fixture().await;
        create(&source)
            .execute_structured(json!({ "name": "first", "password": "a-long-enough-passphrase" }))
            .await
            .expect("bootstrap");

        // Nothing about the environment changed; the TABLE changed. The second
        // call resolves to the operator path and, having no operator identity
        // of its own, is authorized as that operator — which is correct, and is
        // what `resolve_acting` is for. The property under test is the one
        // below it: it is no longer a BOOTSTRAP.
        let out = create(&source)
            .execute_structured(json!({ "name": "second", "password": "another-long-passphrase" }))
            .await
            .expect("an operator may create a further account");
        let s = out.structured.expect("structured");
        assert_eq!(s["bootstrap"], json!(false), "the first-account path must not reopen");
        assert_eq!(
            s["operator"],
            json!(false),
            "an account created without operator: true must be delegated"
        );
    }

    /// **A stale `Bootstrap` request is refused by the store, not honoured.**
    ///
    /// This is the constraint stated as a test: the tool's resolution is a
    /// hint, and the authority is re-derived on the write path. Handing the
    /// store a `Bootstrap` it did not itself resolve — exactly what a race or a
    /// stale read would produce — must fail.
    ///
    /// Mutation-verify: same deleted check as above; this goes red too, and it
    /// is the one that catches it even if the tool layer were rewritten.
    #[tokio::test]
    async fn a_bootstrap_request_against_a_populated_deployment_is_refused_at_the_store() {
        let (_dir, _path, source) = fixture().await;
        create(&source)
            .execute_structured(json!({ "name": "first", "password": "a-long-enough-passphrase" }))
            .await
            .expect("bootstrap");

        let store = source.store().await.expect("store");
        let hashed = hash_password("a-long-enough-passphrase").expect("hash");
        let err = store
            .create_account("sneaky", &hashed, AccountCreation::Bootstrap)
            .await
            .expect_err("a bootstrap must not succeed once an operator exists");
        assert!(
            err.to_string().contains("already has accounts"),
            "unexpected refusal: {err}"
        );
    }

    /// **The password never appears in any output the tool produces.**
    ///
    /// Success and every refusal, checked together — the refusal paths are
    /// where an argument normally leaks, because an error message is the one
    /// place a developer reaches for the input to be helpful.
    #[tokio::test]
    async fn the_password_is_never_echoed_in_a_result_or_an_error() {
        let (_dir, _path, source) = fixture().await;
        const SECRET: &str = "correct-horse-battery-staple"; // pii-test-fixture

        let out = create(&source)
            .execute_structured(json!({ "name": "moose", "password": SECRET }))
            .await
            .expect("bootstrap");
        assert!(!out.text.contains(SECRET), "the result text carried the password: {}", out.text);
        let rendered = serde_json::to_string(&out.structured).expect("serialize");
        assert!(!rendered.contains(SECRET), "the structured result carried the password");

        // Duplicate name: the refusal that has the most reason to quote the
        // submission back.
        let err = create(&source)
            .execute_structured(json!({ "name": "moose", "password": SECRET }))
            .await
            .expect_err("a duplicate name must be refused");
        assert!(!err.to_string().contains(SECRET), "the error carried the password: {err}");

        // And the length refusal, whose message is ABOUT the password.
        let err = create(&source)
            .execute_structured(json!({ "name": "another", "password": "short" }))
            .await
            .expect_err("a short password must be refused");
        assert!(!err.to_string().contains("short"), "the error carried the password: {err}");
    }

    /// **The length floor is enforced, and it is a real refusal.**
    ///
    /// Mutation-verify: delete the `MIN_PASSWORD_LEN` check and this goes red.
    #[tokio::test]
    async fn a_password_below_the_floor_is_refused_and_no_account_is_created() {
        let (_dir, _path, source) = fixture().await;
        let short = "x".repeat(MIN_PASSWORD_LEN - 1);
        create(&source)
            .execute_structured(json!({ "name": "moose", "password": short }))
            .await
            .expect_err("below the floor must be refused");
        assert!(
            !source.store().await.expect("store").any_account_exists().await.expect("no error"),
            "a refused creation must not have written a row"
        );
        // Exactly at the floor is accepted — otherwise the boundary is off by
        // one and this test would pass over a floor of any size.
        create(&source)
            .execute_structured(json!({ "name": "moose", "password": "y".repeat(MIN_PASSWORD_LEN) }))
            .await
            .expect("exactly at the floor must be accepted");
    }

    /// **A password is not trimmed.** An account created with surrounding
    /// whitespace must be loginnable with exactly the string its owner holds.
    #[tokio::test]
    async fn surrounding_whitespace_in_a_password_is_preserved() {
        let (_dir, _path, source) = fixture().await;
        const SECRET: &str = "  a-long-enough-passphrase  ";
        create(&source)
            .execute_structured(json!({ "name": "moose", "password": SECRET }))
            .await
            .expect("create");
        let account = source
            .store()
            .await
            .expect("store")
            .find_active_account_by_name("moose")
            .await
            .expect("no error")
            .expect("exists");
        assert!(
            crate::oauth::password::verify_password(SECRET, &account.password_hash),
            "the exact submitted string must verify"
        );
        assert!(
            !crate::oauth::password::verify_password(SECRET.trim(), &account.password_hash),
            "the password was trimmed, so its owner cannot log in with what they were given"
        );
    }

    /// **Promotion and demotion both work, and the last operator is protected.**
    ///
    /// Mutation-verify: delete the post-update `an_operator_exists` check in
    /// `set_account_operator` and the final assertion goes red.
    #[tokio::test]
    async fn promotion_works_and_the_last_operator_cannot_be_demoted() {
        let (_dir, _path, source) = fixture().await;
        create(&source)
            .execute_structured(json!({ "name": "first", "password": "a-long-enough-passphrase" }))
            .await
            .expect("bootstrap");
        create(&source)
            .execute_structured(json!({ "name": "second", "password": "another-long-passphrase" }))
            .await
            .expect("create delegated");

        let promote = RmcpAccountPromote { source: source.clone() };
        let out = promote
            .execute_structured(json!({ "account": "second" }))
            .await
            .expect("promotion must succeed");
        assert_eq!(out.structured.expect("structured")["changed"], json!(true));

        // From here the fleet has TWO operators, so every further call must name
        // its actor — the ambiguity refusal is working, and a test that did not
        // name one would be asserting that it is not.
        let out = promote
            .execute_structured(json!({ "actor": "first", "account": "second" }))
            .await
            .expect("no-op");
        assert_eq!(
            out.structured.expect("structured")["changed"],
            json!(false),
            "re-promoting must report changed=false, not a phantom success"
        );

        // With two operators, a demotion is fine.
        promote
            .execute_structured(json!({ "actor": "first", "account": "second", "revoke": true }))
            .await
            .expect("demoting one of two operators must succeed");

        // With one left, it is refused — and the account is still an operator,
        // which is what proves the transaction rolled back rather than half
        // applying.
        let err = promote
            .execute_structured(json!({ "account": "first", "revoke": true }))
            .await
            .expect_err("demoting the last operator must be refused");
        assert!(err.to_string().contains("last active operator"), "unexpected refusal: {err}");
        let store = source.store().await.expect("store");
        let still = store
            .find_active_account_by_name("first")
            .await
            .expect("no error")
            .expect("exists");
        assert!(still.is_operator, "the refused demotion was partially applied");
    }

    /// **An empty deployment and a stranded one are reported differently.**
    ///
    /// The distinction matters operationally: one is bootstrappable and one is
    /// not, and a listing that collapsed them would send an operator to run a
    /// command that cannot work.
    #[tokio::test]
    async fn the_listing_distinguishes_an_empty_deployment_from_a_stranded_one() {
        let (_dir, path, source) = fixture().await;
        let list = RmcpAccountList { source: source.clone() };

        let out = list.execute_structured(json!({})).await.expect("empty listing");
        let s = out.structured.expect("structured");
        assert_eq!(s["bootstrap_available"], json!(true));
        assert_eq!(s["stranded"], json!(false));

        create(&source)
            .execute_structured(json!({ "name": "only", "password": "a-long-enough-passphrase" }))
            .await
            .expect("bootstrap");
        let out = list.execute_structured(json!({})).await.expect("listing");
        let s = out.structured.expect("structured");
        assert_eq!(s["accounts"].as_array().expect("array").len(), 1);
        assert_eq!(s["bootstrap_available"], json!(false));

        // Strand it by demoting out of band — the state the last-operator guard
        // refuses to create, reached the only way it can be.
        strand_the_deployment(&path).await;
        let out = list.execute_structured(json!({})).await.expect("stranded listing");
        let s = out.structured.expect("structured");
        assert_eq!(s["stranded"], json!(true), "a stranded door must say so");
        assert_eq!(
            s["bootstrap_available"],
            json!(false),
            "a stranded door is NOT bootstrappable, and saying it is would be a lie an operator \
             acts on"
        );
    }

    /// **A listing never carries credential material.**
    #[tokio::test]
    async fn a_listing_carries_no_password_hash() {
        let (_dir, _path, source) = fixture().await;
        create(&source)
            .execute_structured(json!({ "name": "moose", "password": "a-long-enough-passphrase" }))
            .await
            .expect("bootstrap");
        let out = RmcpAccountList { source: source.clone() }
            .execute_structured(json!({}))
            .await
            .expect("listing");
        let rendered = serde_json::to_string(&out.structured).expect("serialize");
        assert!(!rendered.contains("argon2"), "the listing carried a password hash: {rendered}");
        assert!(!rendered.contains("password"), "the listing carried a password field");
    }


    /// **Disable/enable is reachable, and the last-operator guard is the
    /// SERVER's, surfaced here rather than enforced here.**
    ///
    /// The refusal must come back from the store as an error the page renders —
    /// not from a check in this layer. Mutation-verify: delete
    /// `ensure_an_operator_remains` from `set_account_disabled` and the second
    /// assertion goes red, which is the proof that nothing in the tool layer is
    /// what is holding the line.
    #[tokio::test]
    async fn disabling_is_reachable_and_the_last_operator_is_refused_by_the_server() {
        let (_dir, _path, source) = fixture().await;
        create(&source)
            .execute_structured(json!({ "name": "first", "password": "a-long-enough-passphrase" }))
            .await
            .expect("bootstrap");
        create(&source)
            .execute_structured(json!({ "name": "second", "password": "another-long-passphrase" }))
            .await
            .expect("create delegated");

        let disable = RmcpAccountDisable { source: source.clone() };
        let out = disable
            .execute_structured(json!({ "account": "second" }))
            .await
            .expect("disabling a delegated account must succeed");
        assert_eq!(out.structured.expect("structured")["disabled"], json!(true));

        let err = disable
            .execute_structured(json!({ "account": "first" }))
            .await
            .expect_err("disabling the last active operator must be refused");
        assert!(err.to_string().contains("last active operator"), "unexpected refusal: {err}");

        // Re-enabling must be able to NAME a disabled account — the lookup on
        // this path must not filter on active, or there is no way back.
        disable
            .execute_structured(json!({ "account": "second", "enable": true }))
            .await
            .expect("a disabled account must be re-enableable by name");
    }

    /// **An explicit `actor` is honoured, and a non-operator named as one is
    /// refused.**
    ///
    /// Mutation-verify: drop the `account_authority(id) != Some(true)` check in
    /// `resolve_acting`'s explicit-actor branch and the second half goes red —
    /// any account name would then act with operator authority.
    #[tokio::test]
    async fn an_explicit_actor_is_honoured_and_must_be_an_active_operator() {
        let (_dir, _path, source) = fixture().await;
        create(&source)
            .execute_structured(json!({ "name": "boss", "password": "a-long-enough-passphrase" }))
            .await
            .expect("bootstrap");
        create(&source)
            .execute_structured(json!({ "name": "friend", "password": "another-long-passphrase" }))
            .await
            .expect("create delegated");

        create(&source)
            .execute_structured(json!({
                "actor": "boss", "name": "third", "password": "a-third-long-passphrase"
            }))
            .await
            .expect("an explicit operator actor must be honoured");

        let err = create(&source)
            .execute_structured(json!({
                "actor": "friend", "name": "fourth", "password": "a-fourth-long-passphrase"
            }))
            .await
            .expect_err("a delegated account named as actor must be refused");
        assert!(err.to_string().contains("not an active operator"), "unexpected: {err}");

        let err = create(&source)
            .execute_structured(json!({
                "actor": "nobody", "name": "fifth", "password": "a-fifth-long-passphrase"
            }))
            .await
            .expect_err("an unknown actor must be refused");
        assert!(err.to_string().contains("no account named"), "unexpected: {err}");
    }

    /// **With several operators and no `actor`, the tools REFUSE rather than
    /// guessing which human to attribute the action to.**
    ///
    /// The property `rmcp_owner`'s doctrine protects, preserved here even though
    /// this module accepts an explicit actor. Mutation-verify: make
    /// `find_sole_operator_account`'s several-operators case pick the first and
    /// this goes red.
    #[tokio::test]
    async fn several_operators_and_no_actor_is_refused_not_guessed() {
        let (_dir, _path, source) = fixture().await;
        create(&source)
            .execute_structured(json!({ "name": "boss", "password": "a-long-enough-passphrase" }))
            .await
            .expect("bootstrap");
        create(&source)
            .execute_structured(json!({
                "name": "co-boss", "password": "another-long-passphrase", "operator": true
            }))
            .await
            .expect("a second operator");

        create(&source)
            .execute_structured(json!({ "name": "third", "password": "a-third-long-passphrase" }))
            .await
            .expect_err("with two operators and no actor, this must refuse rather than pick one");

        // …and naming one resolves it.
        create(&source)
            .execute_structured(json!({
                "actor": "boss", "name": "third", "password": "a-third-long-passphrase"
            }))
            .await
            .expect("naming the actor must resolve the ambiguity");
    }


    /// **A malformed boolean is REFUSED, never read as its default.**
    ///
    /// Review round 1 (codex). `{"revoke": "true"}` used to read as
    /// `revoke: false` and PROMOTE the account — the caller asked to remove
    /// authority and was told it succeeded. Both directions are covered because
    /// the defaulting direction is not what makes it a bug.
    ///
    /// Mutation-verify: restore `get(name).and_then(as_bool).unwrap_or(false)`
    /// in `strict_bool` and every assertion here goes red.
    #[tokio::test]
    async fn a_malformed_boolean_argument_is_refused() {
        let (_dir, _path, source) = fixture().await;
        create(&source)
            .execute_structured(json!({ "name": "boss", "password": "a-long-enough-passphrase" }))
            .await
            .expect("bootstrap");
        create(&source)
            .execute_structured(json!({ "name": "friend", "password": "another-long-passphrase" }))
            .await
            .expect("create delegated");

        // `null` included deliberately — round 2 (codex) found it was folded
        // into "absent", which is what a client whose variable failed to
        // populate actually sends.
        for bad in [json!("true"), json!(1), json!("false"), json!([]), json!(null)] {
            create(&source)
                .execute_structured(json!({
                    "name": "x", "password": "a-long-enough-passphrase", "operator": bad
                }))
                .await
                .expect_err("a non-boolean `operator` must be refused");
            RmcpAccountPromote { source: source.clone() }
                .execute_structured(json!({ "account": "friend", "revoke": bad }))
                .await
                .expect_err("a non-boolean `revoke` must be refused");
            RmcpAccountDisable { source: source.clone() }
                .execute_structured(json!({ "account": "friend", "enable": bad }))
                .await
                .expect_err("a non-boolean `enable` must be refused");
        }

        // The refused calls changed nothing — a refusal that had already written
        // would be worse than the bug it replaced.
        let store = source.store().await.expect("store");
        let friend = store
            .find_active_account_by_name("friend")
            .await
            .expect("no error")
            .expect("exists");
        assert!(!friend.is_operator, "a refused promote wrote anyway");
        assert!(store.resolve_account_id("x").await.expect("no error").is_none());
    }

    /// **A stranded deployment is not offered the bootstrap.**
    ///
    /// The tool-layer half of review round 1's finding: `resolve_acting` must
    /// not report `Bootstrap` merely because no operator is active. It refuses,
    /// naming the state, so an operator is told what is actually wrong instead
    /// of being handed a fresh operator account.
    ///
    /// Mutation-verify: delete the `any_account_exists` branch in
    /// `resolve_acting` and the first assertion goes red — the create succeeds
    /// and mints an unauthenticated operator.
    #[tokio::test]
    async fn a_stranded_deployment_is_refused_rather_than_re_bootstrapped() {
        let (_dir, path, source) = fixture().await;
        create(&source)
            .execute_structured(json!({ "name": "only", "password": "a-long-enough-passphrase" }))
            .await
            .expect("bootstrap");
        strand_the_deployment(&path).await;

        let err = create(&source)
            .execute_structured(json!({ "name": "usurper", "password": "another-long-passphrase" }))
            .await
            .expect_err("a stranded door must not be re-bootstrapped");
        assert!(err.to_string().contains("no ACTIVE operator"), "unexpected refusal: {err}");
        assert!(
            source
                .store()
                .await
                .expect("store")
                .resolve_account_id("usurper")
                .await
                .expect("no error")
                .is_none(),
            "the refused create wrote a row anyway"
        );
    }


    /// **A healthy multi-operator door LISTS, and does not call itself
    /// stranded.**
    ///
    /// Review round 2 (codex and opus, independently). The listing detected
    /// "stranded" by catching `ToolError::Conflict`, which
    /// `find_sole_operator_account` also raises for "several operators, name
    /// one" — so a perfectly healthy deployment reported `stranded: true`,
    /// `accounts: []`, and told the operator to go re-promote an account
    /// directly against the database. By this module's own standard that is a
    /// lie an operator acts on.
    ///
    /// Two properties, and BOTH are needed: the listing must succeed without an
    /// actor (round 2's other finding — otherwise the page's first load can
    /// never render the operator picker it needs to name one), and it must
    /// report the door truthfully.
    ///
    /// Mutation-verify: make `RmcpAccountList` call `acting` instead of
    /// `reading` and the first assertion goes red; make `resolve_reader` return
    /// `Stranded` whenever no sole operator resolves and the `stranded`
    /// assertion goes red.
    #[tokio::test]
    async fn a_multi_operator_deployment_lists_without_an_actor_and_is_not_stranded() {
        let (_dir, _path, source) = fixture().await;
        create(&source)
            .execute_structured(json!({ "name": "boss", "password": "a-long-enough-passphrase" }))
            .await
            .expect("bootstrap");
        create(&source)
            .execute_structured(json!({
                "name": "co-boss", "password": "another-long-passphrase", "operator": true
            }))
            .await
            .expect("a second operator");

        let out = RmcpAccountList { source: source.clone() }
            .execute_structured(json!({}))
            .await
            .expect("a multi-operator door must be listable without naming an actor");
        let s = out.structured.expect("structured");
        assert_eq!(
            s["accounts"].as_array().expect("array").len(),
            2,
            "the listing returned no accounts on a door that has two"
        );
        assert_eq!(s["stranded"], json!(false), "a healthy door reported itself stranded");
        assert_eq!(s["bootstrap_available"], json!(false));

        // The WRITE path keeps refusing — the read's relaxation must not have
        // leaked into it, or attribution is being guessed after all.
        create(&source)
            .execute_structured(json!({ "name": "third", "password": "a-third-long-passphrase" }))
            .await
            .expect_err("a write on a multi-operator door must still require an actor");
    }

    /// **A named actor is validated on the READ path too.**
    ///
    /// `resolve_reader`'s relaxation is "pick any operator when none was
    /// named", not "accept whoever was named". A delegated account naming
    /// itself must not be able to read the account list.
    ///
    /// Mutation-verify: have `resolve_reader` skip its `requested.is_some()`
    /// delegation to `resolve_acting` and this goes red.
    #[tokio::test]
    async fn a_named_non_operator_cannot_list_accounts() {
        let (_dir, _path, source) = fixture().await;
        create(&source)
            .execute_structured(json!({ "name": "boss", "password": "a-long-enough-passphrase" }))
            .await
            .expect("bootstrap");
        create(&source)
            .execute_structured(json!({ "name": "friend", "password": "another-long-passphrase" }))
            .await
            .expect("create delegated");

        RmcpAccountList { source: source.clone() }
            .execute_structured(json!({ "actor": "friend" }))
            .await
            .expect_err("a delegated account named as actor must not list accounts");
    }

    /// **A stranded door is reported as stranded by the listing, and refuses
    /// every write.**
    ///
    /// The other half of the variant split: `Stranded` must still reach both
    /// surfaces, now as a value rather than as a caught error.
    #[tokio::test]
    async fn a_stranded_door_lists_as_stranded_and_refuses_writes() {
        let (_dir, path, source) = fixture().await;
        create(&source)
            .execute_structured(json!({ "name": "only", "password": "a-long-enough-passphrase" }))
            .await
            .expect("bootstrap");
        strand_the_deployment(&path).await;

        let out = RmcpAccountList { source: source.clone() }
            .execute_structured(json!({}))
            .await
            .expect("a stranded door must still report");
        let s = out.structured.expect("structured");
        assert_eq!(s["stranded"], json!(true));
        assert_eq!(
            s["bootstrap_available"],
            json!(false),
            "a stranded door is NOT bootstrappable, and saying it is would be a lie an operator \
             acts on"
        );

        RmcpAccountPromote { source: source.clone() }
            .execute_structured(json!({ "account": "only" }))
            .await
            .expect_err("a stranded door must refuse a write");
    }


    /// **A configured `RMCP_OPERATOR_ACCOUNT` resolves a multi-operator fleet.**
    ///
    /// Review round 4 (codex) read the environment branch running BEFORE the
    /// several-operator check as a hole — "a write succeeds without the caller
    /// naming an actor". It is not: the environment variable IS the naming, it
    /// is the mechanism the tool schema and the README both document for
    /// exactly this case, and it is how `rmcp_owner` has always resolved the
    /// same ambiguity. The finding is recorded as disproven, and pinned here so
    /// the next reader gets the answer from a test rather than from the diff.
    ///
    /// The complementary half — several operators and NOTHING named — is
    /// `several_operators_and_no_actor_is_refused_not_guessed`. Together they
    /// say the rule precisely: ambiguity is refused only when nobody has said
    /// which operator is acting.
    #[tokio::test]
    async fn a_configured_environment_actor_resolves_a_multi_operator_fleet() {
        let (_dir, _path, source) = fixture().await;
        create(&source)
            .execute_structured(json!({ "name": "boss", "password": "a-long-enough-passphrase" }))
            .await
            .expect("bootstrap");
        create(&source)
            .execute_structured(json!({
                "name": "co-boss", "password": "another-long-passphrase", "operator": true
            }))
            .await
            .expect("a second operator");

        let store = source.store().await.expect("store");
        // Resolved directly rather than through the process environment: these
        // tests run concurrently in one process, so mutating a global would make
        // this test's outcome depend on its neighbours. What is under test is
        // the RESOLUTION rule, and `resolve_named_operator` is the step the
        // environment branch performs once it has read the name.
        let id = resolve_named_operator(store.as_ref(), "boss")
            .await
            .expect("a named operator must resolve on a multi-operator fleet");
        assert_eq!(
            store.account_authority(id).await.expect("no error"),
            Some(true),
            "the resolved account must be an active operator"
        );
        // And it is NAMED, so no sole-ness condition rides along — the store
        // would refuse an InferredSole here, which is the whole distinction.
        let hashed = hash_password("a-third-long-passphrase").expect("hash");
        store
            .create_account(
                "made-by-a-named-actor",
                &hashed,
                AccountCreation::ByOperator { actor: ActorSelection::Named(id), operator: false },
            )
            .await
            .expect("a named actor must be able to write on a multi-operator fleet");
        store
            .create_account(
                "made-by-an-inferred-actor",
                &hashed,
                AccountCreation::ByOperator {
                    actor: ActorSelection::InferredSole(id),
                    operator: false,
                },
            )
            .await
            .expect_err("an INFERRED actor must still be refused on a multi-operator fleet");
    }

    /// **These tools must never be approval-gated.**
    ///
    /// Not a style preference: [`crate::approval`] PERSISTS a guarded call's
    /// full `args_json`, so guarding `rmcp_account_create` would write the
    /// plaintext password to the approvals database. If a future item decides
    /// these should be gated, the password must stop being an argument first.
    #[test]
    fn account_creation_is_not_approval_gated() {
        // NARROWED in round 3 (codex): only `rmcp_account_create` carries a
        // password, so only it must never be guarded. Pinning the other three
        // as permanently unguarded bought no password safety and would have
        // blocked a future decision to put promotion or disablement behind
        // operator approval — a guard that forbids the wrong thing.
        assert!(
            !crate::approval::is_guarded("rmcp_account_create"),
            "rmcp_account_create is approval-gated, which persists its raw arguments — including \
             the plaintext password — to the approvals database"
        );
        // Non-vacuity: the predicate must really be able to say yes, or this
        // passes against a function that always returns false.
        assert!(crate::approval::is_guarded("pg_ddl"), "is_guarded matches nothing");
    }

    /// The three tools register under the names an operator is told to call.
    #[test]
    fn the_tools_register_under_their_documented_names() {
        let mut registry = ToolRegistry::new();
        register(&mut registry);
        for tool in ACCOUNT_TOOLS {
            assert!(registry.contains(tool), "{tool} is not registered");
        }
    }
}
