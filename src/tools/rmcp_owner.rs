//! RMCP-12 — `rmcp_server_owner_set` and `rmcp_server_owner_list`.
//!
//! The operator's control over WHO administers which federated server: hand a
//! namespace to a friend's account, take it back, and see the current state.
//!
//! ## Where the acting identity comes from — and where it does NOT
//!
//! Not from the arguments. A tool argument is caller-controlled text, and an
//! identity a caller can type is not an identity (the same doctrine as
//! [`crate::tool::CallerContext`], whose entitled constructor is module-private
//! for exactly this reason). The actor is resolved from the deployment: the sole
//! active operator account, or the one named by
//! [`crate::oauth::OPERATOR_ACCOUNT_ENV`] when a fleet has several. If neither
//! resolves, the tool refuses rather than picking one — an administrative action
//! attributed to the wrong human is worse than an action that did not happen.
//!
//! The GRANTEE is an argument, because naming who receives a delegation is the
//! whole request. That is a lookup, not an authentication.
//!
//! ## Why granting is not approval-gated
//!
//! RMCP-11 argued the reverse case (revocation must not wait behind a
//! confirmation, because it is reached for mid-incident) and the asymmetry holds
//! here too, but for a different reason: this tool's caller has ALREADY been
//! authenticated as the operator by the surface it arrived on, and the action it
//! performs is bounded — a delegation can only ever hand out a namespace the
//! operator already controls, it is audited on both the grant and the revoke,
//! and it is revocable in one call whose effect lands on the very next request.
//! Gating it would add a second authorization path
//! ([`crate::approval`]'s own database) for a decision this module already
//! makes, which is precisely the "two ways to do one thing" this item refuses.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::OnceCell;
use uuid::Uuid;

use crate::error::ToolError;
use crate::oauth::delegation::{DelegationService, DelegationStore};
use crate::oauth::store::OauthStore;
use crate::oauth::{OauthConfig, OPERATOR_ACCOUNT_ENV};
use crate::registry::ToolRegistry;
use crate::tool::{RustTool, ToolOutput};

/// A resolved delegation surface: the service, plus the account acting on it.
///
/// The two are resolved together because a service with no established actor
/// cannot authorize anything — and an actor resolved separately, earlier, would
/// be the stale-snapshot mistake this item exists to avoid. Every call
/// re-resolves the actor's AUTHORITY inside [`DelegationService`]; what is
/// cached here is only which account that is.
struct Delegation {
    service: DelegationService,
    store: Arc<dyn DelegationStore>,
    actor: Uuid,
}

/// How a tool obtains one. A seam, so argument handling is testable without a
/// database — the same reason `rmcp_session` has one.
#[async_trait]
trait DelegationSource: Send + Sync {
    async fn delegation(&self) -> Result<Delegation, ToolError>;
}

/// The production source: connect lazily, once, from the runtime environment.
///
/// Lazy because the OAuth door is optional; a deployment with no connector
/// configured must not open a Postgres pool merely because these tools are
/// registered.
struct EnvDelegationSource {
    cell: OnceCell<(Arc<OauthStore>, Uuid)>,
}

impl EnvDelegationSource {
    fn new() -> Self {
        Self { cell: OnceCell::new() }
    }
}

#[async_trait]
impl DelegationSource for EnvDelegationSource {
    async fn delegation(&self) -> Result<Delegation, ToolError> {
        let (store, actor) = self
            .cell
            .get_or_try_init(|| async {
                // As in `rmcp_session`: the runtime secret store is materialized
                // into the process environment at startup, so this read IS the
                // vault read. The URL is never logged or echoed.
                let config = OauthConfig::from_env()?;
                let store = Arc::new(OauthStore::connect(&config).await?);
                if !store.schema_ready().await {
                    return Err(ToolError::NotConfigured(
                        "the RMCP OAuth schema is not present — apply the S132 migration before \
                         administering server ownership"
                            .into(),
                    ));
                }
                let actor = resolve_actor(store.as_ref()).await?;
                Ok((store, actor))
            })
            .await?
            .clone();
        let store: Arc<dyn DelegationStore> = store;
        Ok(Delegation {
            service: DelegationService::new(store.clone()),
            store,
            actor,
        })
    }
}

/// Establish which operator account this surface acts as.
///
/// Refuses rather than guesses in both ambiguous directions: no active operator
/// at all, or several with none named.
async fn resolve_actor(store: &OauthStore) -> Result<Uuid, ToolError> {
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
        // Verified to be an ACTIVE OPERATOR here, not assumed from the fact that
        // an operator set the variable: the flag is revocable, and a stale
        // configuration must not carry authority the account no longer has.
        if store.account_authority(id).await? != Some(true) {
            return Err(ToolError::NotConfigured(format!(
                "{OPERATOR_ACCOUNT_ENV} names an account that is not an active operator"
            )));
        }
        return Ok(id);
    }
    store.find_sole_operator_account().await?.ok_or_else(|| {
        ToolError::NotConfigured(
            "this deployment has no active operator account to administer server ownership as"
                .into(),
        )
    })
}

/// Read a trimmed, non-empty string argument.
fn field(args: &Value, name: &str) -> Option<String> {
    args.get(name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// `rmcp_server_owner_set`.
struct RmcpServerOwnerSet {
    source: Arc<dyn DelegationSource>,
}

#[async_trait]
impl RustTool for RmcpServerOwnerSet {
    fn name(&self) -> &str {
        "rmcp_server_owner_set"
    }

    fn description(&self) -> &str {
        "Grant or revoke ownership of a federated mesh server (namespace). The owning account may \
         then scope its own connectors to that server and author tool groups over it, and nothing \
         else. Operator-only; revoking narrows every connector that drew on the delegation."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "namespace": { "type": "string", "description": "The mesh namespace (federated server) to delegate." },
                "account": { "type": "string", "description": "Account name to grant it to. Mutually exclusive with revoke." },
                "revoke": { "type": "boolean", "description": "Remove the delegation instead of granting one." }
            },
            "required": ["namespace"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.execute_structured(args).await?.text)
    }

    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let Some(namespace) = field(&args, "namespace") else {
            return Err(ToolError::InvalidArgument("namespace is required".into()));
        };
        let account = field(&args, "account");
        let revoke = args.get("revoke").and_then(Value::as_bool).unwrap_or(false);

        // Refused rather than resolved by precedence, exactly as
        // `rmcp_session`'s selector is: a request that says both things means
        // something this API cannot express, and guessing which half to honour
        // is how an operator revokes a delegation they meant to move.
        if revoke && account.is_some() {
            return Err(ToolError::InvalidArgument(
                "give either account or revoke, not both".into(),
            ));
        }

        let delegation = self.source.delegation().await?;
        let (change, text) = if revoke {
            let change = delegation.service.revoke(delegation.actor, &namespace).await?;
            (
                change,
                format!(
                    "server ownership revoked; {} client scoping row(s) narrowed",
                    change.rows_narrowed
                ),
            )
        } else {
            let Some(account) = account else {
                return Err(ToolError::InvalidArgument(
                    "give account to grant ownership, or revoke: true to remove it".into(),
                ));
            };
            let change = delegation
                .service
                .grant(delegation.actor, &namespace, &account)
                .await?;
            (
                change,
                format!(
                    "server ownership granted (reassigned={}); {} client scoping row(s) narrowed",
                    change.reassigned, change.rows_narrowed
                ),
            )
        };

        Ok(ToolOutput {
            text,
            structured: Some(json!({
                "namespace": namespace,
                "revoked": revoke,
                "reassigned": change.reassigned,
                "rows_narrowed": change.rows_narrowed,
            })),
        })
    }
}

/// `rmcp_server_owner_list`.
struct RmcpServerOwnerList {
    source: Arc<dyn DelegationSource>,
}

#[async_trait]
impl RustTool for RmcpServerOwnerList {
    fn name(&self) -> &str {
        "rmcp_server_owner_list"
    }

    fn description(&self) -> &str {
        "List federated servers (mesh namespaces) that have been delegated to an account, with who \
         owns each and when it was granted. Namespaces with no row are the operator's by default."
    }

    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {}, "additionalProperties": false })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.execute_structured(args).await?.text)
    }

    async fn execute_structured(&self, _args: Value) -> Result<ToolOutput, ToolError> {
        let delegation = self.source.delegation().await?;
        let owners = delegation.service.list(delegation.actor).await?;

        let configured: Option<std::collections::BTreeSet<String>> =
            crate::mesh::registry::UpstreamRegistry::from_env()
                .ok()
                .map(|registry| registry.namespaces().map(str::to_string).collect());

        let mut rows = Vec::with_capacity(owners.len());
        for owner in &owners {
            // Resolved one at a time on purpose: the list is bounded by the
            // number of DELEGATED namespaces, which is a handful, and a join
            // would put an account name into a query whose result is filtered
            // per caller — two rules to keep in step instead of one.
            let account = delegation
                .store
                .account_name(owner.owner_account_id)
                .await?
                .unwrap_or_else(|| "(account removed)".to_string());
            rows.push(json!({
                "namespace": owner.namespace,
                "owner": account,
                "granted_at": owner.granted_at.to_rfc3339(),
                // Display metadata, never an authorization input: a delegation
                // for a namespace this fleet no longer federates with already
                // resolves to nothing (no catalog tool carries the prefix), so
                // this only tells the operator WHY a friend's connector went
                // quiet. Read fail-soft — an unreadable mesh configuration
                // reports "unknown", it does not fail the listing.
                "configured": configured.as_ref().map(|known| known.contains(&owner.namespace)),
            }));
        }

        let text = if rows.is_empty() {
            // Says "no delegations", never "you may not see them": the answer
            // for a delegated caller with none of their own and for a fleet with
            // none at all is deliberately the same.
            "no server ownership delegations".to_string()
        } else {
            rows.iter()
                .map(|row| {
                    format!(
                        "{} -> {} (granted {})",
                        row["namespace"].as_str().unwrap_or_default(),
                        row["owner"].as_str().unwrap_or_default(),
                        row["granted_at"].as_str().unwrap_or_default()
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        };

        Ok(ToolOutput {
            text,
            structured: Some(json!({ "delegations": rows })),
        })
    }
}

/// Register both tools.
pub fn register(registry: &mut ToolRegistry) {
    let source: Arc<dyn DelegationSource> = Arc::new(EnvDelegationSource::new());
    registry.register_or_replace(Box::new(RmcpServerOwnerSet { source: source.clone() }));
    registry.register_or_replace(Box::new(RmcpServerOwnerList { source }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth::delegation::{DelegationChange, DelegationGrant, DelegationRevocation};
    use crate::oauth::model::ServerOwner;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeStore {
        owners: Mutex<Vec<ServerOwner>>,
    }

    fn operator_id() -> Uuid {
        Uuid::from_u128(1)
    }

    fn friend_id() -> Uuid {
        Uuid::from_u128(2)
    }

    #[async_trait]
    impl DelegationStore for FakeStore {
        async fn account_authority(&self, account_id: Uuid) -> Result<Option<bool>, ToolError> {
            Ok(if account_id == operator_id() {
                Some(true)
            } else if account_id == friend_id() {
                Some(false)
            } else {
                None
            })
        }

        async fn namespaces_owned_by(&self, account_id: Uuid) -> Result<Vec<String>, ToolError> {
            Ok(self
                .owners
                .lock()
                .unwrap()
                .iter()
                .filter(|o| o.owner_account_id == account_id)
                .map(|o| o.namespace.clone())
                .collect())
        }

        async fn grant_namespace(
            &self,
            grant: &DelegationGrant,
        ) -> Result<DelegationChange, ToolError> {
            let (namespace, grantee) = (grant.namespace(), grant.grantee());
            let mut owners = self.owners.lock().unwrap();
            let reassigned = owners.iter().any(|o| o.namespace == namespace);
            owners.retain(|o| o.namespace != namespace);
            owners.push(ServerOwner {
                namespace: namespace.to_string(),
                owner_account_id: grantee,
                granted_at: chrono::Utc::now(),
            });
            Ok(DelegationChange { reassigned, rows_narrowed: 0 })
        }

        async fn revoke_namespace(
            &self,
            revocation: &DelegationRevocation,
        ) -> Result<DelegationChange, ToolError> {
            let namespace = revocation.namespace();
            let mut owners = self.owners.lock().unwrap();
            let existed = owners.iter().any(|o| o.namespace == namespace);
            owners.retain(|o| o.namespace != namespace);
            Ok(DelegationChange {
                reassigned: false,
                rows_narrowed: if existed { 2 } else { 0 },
            })
        }

        async fn list_server_owners(&self) -> Result<Vec<ServerOwner>, ToolError> {
            Ok(self.owners.lock().unwrap().clone())
        }

        async fn account_id_by_name(&self, name: &str) -> Result<Option<Uuid>, ToolError> {
            Ok(match name {
                "operator" => Some(operator_id()),
                "friend" => Some(friend_id()),
                _ => None,
            })
        }

        async fn account_name(&self, account_id: Uuid) -> Result<Option<String>, ToolError> {
            Ok(if account_id == operator_id() {
                Some("operator".to_string())
            } else if account_id == friend_id() {
                Some("friend".to_string())
            } else {
                None
            })
        }
    }

    struct FixedSource {
        store: Arc<FakeStore>,
        actor: Uuid,
    }

    #[async_trait]
    impl DelegationSource for FixedSource {
        async fn delegation(&self) -> Result<Delegation, ToolError> {
            let store: Arc<dyn DelegationStore> = self.store.clone();
            Ok(Delegation {
                service: DelegationService::new(store.clone()),
                store,
                actor: self.actor,
            })
        }
    }

    /// The OAuth door is not configured on this deployment.
    struct UnconfiguredSource;

    #[async_trait]
    impl DelegationSource for UnconfiguredSource {
        async fn delegation(&self) -> Result<Delegation, ToolError> {
            Err(ToolError::NotConfigured("no RMCP database configured".into()))
        }
    }

    fn tools(actor: Uuid) -> (RmcpServerOwnerSet, RmcpServerOwnerList, Arc<FakeStore>) {
        let store = Arc::new(FakeStore::default());
        let source: Arc<dyn DelegationSource> = Arc::new(FixedSource {
            store: store.clone(),
            actor,
        });
        (
            RmcpServerOwnerSet { source: source.clone() },
            RmcpServerOwnerList { source },
            store,
        )
    }

    #[tokio::test]
    async fn granting_and_revoking_round_trips_and_reports_the_narrowing() {
        let (set, list, _store) = tools(operator_id());
        set.execute_structured(json!({ "namespace": "peerone", "account": "friend" }))
            .await
            .expect("the operator may grant");

        let listed = list.execute_structured(json!({})).await.unwrap();
        assert!(listed.text.contains("peerone -> friend"), "{}", listed.text);

        let revoked = set
            .execute_structured(json!({ "namespace": "peerone", "revoke": true }))
            .await
            .unwrap();
        assert!(revoked.text.contains("2 client scoping row(s) narrowed"), "{}", revoked.text);

        let listed = list.execute_structured(json!({})).await.unwrap();
        assert_eq!(listed.text, "no server ownership delegations");
    }

    #[tokio::test]
    async fn a_delegated_actor_may_not_grant_and_sees_only_its_own_row() {
        let (operator_set, _, store) = tools(operator_id());
        operator_set
            .execute_structured(json!({ "namespace": "peerone", "account": "friend" }))
            .await
            .unwrap();
        operator_set
            .execute_structured(json!({ "namespace": "peertwo", "account": "operator" }))
            .await
            .unwrap();
        assert_eq!(store.owners.lock().unwrap().len(), 2);

        let source: Arc<dyn DelegationSource> = Arc::new(FixedSource {
            store: store.clone(),
            actor: friend_id(),
        });
        let set = RmcpServerOwnerSet { source: source.clone() };
        let list = RmcpServerOwnerList { source };

        set.execute_structured(json!({ "namespace": "peertwo", "account": "friend" }))
            .await
            .expect_err("a delegated account may not grant ownership");
        // Not even of the server it already owns — delegation does not chain.
        set.execute_structured(json!({ "namespace": "peerone", "revoke": true }))
            .await
            .expect_err("a delegated account may not revoke its own delegation");

        let listed = list.execute_structured(json!({})).await.unwrap();
        assert!(listed.text.contains("peerone"), "{}", listed.text);
        assert!(!listed.text.contains("peertwo"), "{}", listed.text);
    }

    #[tokio::test]
    async fn an_ambiguous_or_empty_request_is_refused_rather_than_guessed() {
        let (set, _, _store) = tools(operator_id());
        set.execute_structured(json!({ "namespace": "peerone", "account": "friend", "revoke": true }))
            .await
            .expect_err("both account and revoke must be refused");
        set.execute_structured(json!({ "namespace": "peerone" }))
            .await
            .expect_err("neither account nor revoke must be refused");
        set.execute_structured(json!({ "namespace": "   " }))
            .await
            .expect_err("a blank namespace must be refused");
    }

    #[tokio::test]
    async fn granting_to_an_unknown_account_does_not_confirm_which_accounts_exist() {
        let (set, _, _store) = tools(operator_id());
        let unknown = set
            .execute_structured(json!({ "namespace": "peerone", "account": "nobody" }))
            .await
            .expect_err("an unknown grantee must be refused");
        assert!(!format!("{unknown}").contains("nobody"), "{unknown}");
    }

    #[tokio::test]
    async fn an_unconfigured_deployment_reports_it_rather_than_failing_obscurely() {
        let source: Arc<dyn DelegationSource> = Arc::new(UnconfiguredSource);
        let set = RmcpServerOwnerSet { source: source.clone() };
        let err = set
            .execute_structured(json!({ "namespace": "peerone", "account": "friend" }))
            .await
            .expect_err("an unconfigured door must refuse");
        assert!(matches!(err, ToolError::NotConfigured(_)));
    }
}
