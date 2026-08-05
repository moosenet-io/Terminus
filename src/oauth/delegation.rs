//! RMCP-12 — namespace ownership, and the ONE place delegation is decided.
//!
//! ## The model, in one line
//!
//! `allowed_servers ⊆ owned_namespaces(actor)`. A delegated owner administers
//! connectors for the federated server they own and for nothing else; the
//! operator owns every namespace by default and may hand one to another
//! account.
//!
//! ## Why this module exists at all, given the store already checks things
//!
//! RMCP-06 and RMCP-07 each landed a piece of this rule where it happened to be
//! needed: the store refuses a cross-account client write, `groups.rs` refuses a
//! bare `*` from a non-operator, and `scope.rs` intersects a client's namespaces
//! at dispatch. Three correct checks in three files is how a fourth write path
//! gets added with only two of them. So the RULE lives here, as pure functions
//! over an authority value, and every write path — tool, GUI, or store method —
//! calls the same functions. Nothing here opens a database or reads an
//! environment variable; that is what makes it callable from inside a
//! transaction, which is where the authority has to be derived.
//!
//! ## The rule that shapes every signature here
//!
//! **A write-time authorization check is point-in-time; any authority that can
//! be REVOKED must be re-derived on the READ path, and absence must always mean
//! the empty set.** Delegation is where that bites hardest, because a delegation
//! is exactly the kind of authority an operator revokes in a hurry:
//!
//! - The WRITE path derives its authority inside the writing transaction, under
//!   `FOR SHARE` locks (see [`crate::oauth::store::OauthStore`]'s
//!   `actor_authority`), so nothing can change between the check and the write.
//! - The READ path never trusts the write. A namespace grant resolves only
//!   while `rmcp_server_owner` still says so
//!   ([`crate::oauth::store::OauthStore::client_namespaces`]), and a delegated
//!   owner's patterns are re-filtered through [`owner_may_hold`] on every
//!   resolution ([`crate::oauth::groups::resolve_groups`],
//!   [`crate::oauth::scope::ClientScope::from_rows`]) against the account's
//!   CURRENT `is_operator` flag.
//! - Clearing a delegation therefore stops authorizing on the very next call,
//!   not at the next cache TTL. The tidy-up that removes the now-unjustified
//!   `rmcp_client_server` rows is bookkeeping on top of that, not the mechanism.
//!
//! ## The decision this item owed, and made: delegated `<ns>::*`
//!
//! RMCP-06 left one question open deliberately — a bare `*` is operator-only,
//! but a delegated author could write `peerhub::*`, bounded only later, at
//! `decide()`, by the client's own namespace rows. This item decides it, in
//! both directions:
//!
//! 1. **`<ns>::*` stays available to a delegated owner** — for a namespace they
//!    OWN. That is not a loophole, it is the product: "a friend administers
//!    their own server's access" means granting their whole server is the
//!    ordinary case, and forcing them to enumerate their own tools would only
//!    push them to ask the operator for a wildcard instead.
//! 2. **Ownership is now checked at authoring time, not only at dispatch.** A
//!    delegated owner writing `otherpeer::*` — or `otherpeer::alerts_list` — is
//!    refused at the write, because a pattern naming someone else's server has
//!    no legitimate reading.
//! 3. **A delegated owner may hold NO unqualified pattern at all.** `weather_*`
//!    and `pg_stat` address the LOCAL namespace, which is the operator's, and
//!    RMCP-06's grammar makes unqualified patterns local-only by construction.
//!    Permitting them would let a delegated account grant its connectors the
//!    fleet's own tools — the widening this item exists to prevent — and no
//!    client-side namespace row would ever bound it, because `decide()` applies
//!    the namespace dimension only to namespaced NAMES.
//!
//! Point 3 is the one that needed a shared rule rather than a check: it must
//! hold at authoring time AND be re-derived at resolution time, so that an
//! operator who authored `pg_*` and was later DEMOTED does not leave a delegated
//! account holding local tools. [`owner_may_hold`] is that rule, and it is
//! called from both places.

use std::collections::BTreeSet;

use async_trait::async_trait;
use uuid::Uuid;

use crate::error::ToolError;
use crate::oauth::audit::{AuditDetail, OauthAuditRecord, OauthEvent, ScopingRefusal};
use crate::oauth::groups::GroupOwner;
use crate::oauth::model::ServerOwner;

/// A grant pattern reduced to the only property delegation cares about.
///
/// Both pattern vocabularies map onto this — [`crate::oauth::groups::Pattern`]
/// (authoring) and [`crate::oauth::scope::ScopePattern`] (enforcing). Those two
/// types are a known, documented duplication with a migration plan; the
/// AUTHORITY rule over them is not duplicated, because both reduce to this
/// shape and both ask [`owner_may_hold`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PatternShape<'a> {
    /// The bare `*`.
    Everything,
    /// An unqualified pattern — exact or prefix. Addresses LOCAL tools only,
    /// per RMCP-06's grammar.
    Local,
    /// Any pattern naming a mesh namespace: `<ns>::*`, `<ns>::<prefix>*`, or
    /// `<ns>::<bare>`.
    Namespaced(&'a str),
}

/// **The** rule about which pattern shapes an owner may hold.
///
/// Called at authoring time (with the ownership check below it) and again at
/// every resolution (without it — the namespace dimension is bounded there by
/// the client's own `rmcp_client_server` rows, re-derived per call). One
/// function, two call sites, no second copy to drift.
///
/// Fail-closed by shape: an operator may hold anything; a delegated owner may
/// hold ONLY namespaced patterns. Note what that means at read time — a group
/// authored by an operator who was later demoted collapses to just its
/// namespaced patterns, and a group whose only pattern was `pg_*` collapses to
/// the empty set. That is the intended direction, and it is why this is a
/// filter over PATTERNS rather than over results.
pub fn owner_may_hold(owner: GroupOwner, shape: PatternShape<'_>) -> bool {
    match owner {
        GroupOwner::Operator => true,
        GroupOwner::Delegated => matches!(shape, PatternShape::Namespaced(_)),
    }
}

/// The live authority of the account performing a write.
///
/// **Fields are private and there is no public constructor.** The only ways to
/// obtain one are [`ActorAuthority::resolve`] (a fresh read, for read paths and
/// for a tool's pre-flight) and the crate-visible
/// [`ActorAuthority::from_live_state`] that the store calls from INSIDE its
/// writing transaction. That is the same reason
/// [`crate::tool::CallerContext`]'s entitled constructor is module-private: an
/// authority a caller can assemble field-by-field is an authority a caller can
/// grant itself.
///
/// The value is a SNAPSHOT and is documented as one everywhere it is used. It is
/// authoritative only for the transaction it was derived in; a copy carried
/// across an `await` into a later request is not evidence of anything.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActorAuthority {
    account_id: Uuid,
    is_operator: bool,
    owned: BTreeSet<String>,
}

impl ActorAuthority {
    /// Build from state just read, under lock, by the store.
    ///
    /// `pub(crate)` so only this crate's store can mint one from a live read.
    /// Callers must pass what they READ, never what they intend.
    pub(crate) fn from_live_state(
        account_id: Uuid,
        is_operator: bool,
        owned: impl IntoIterator<Item = String>,
    ) -> Self {
        Self {
            account_id,
            is_operator,
            owned: owned.into_iter().collect(),
        }
    }

    /// Read an account's current authority.
    ///
    /// For read paths, listings, and a tool's pre-flight refusal — NOT for
    /// authorizing a write. A write authorizes against an authority derived in
    /// its own transaction, because the two reads here can be overtaken between
    /// them; the store does exactly that and this method is deliberately not
    /// what it calls.
    ///
    /// A missing or DISABLED account is an error, never a delegated actor with
    /// an empty owned set: "we could not establish who this is" and "this is
    /// somebody who owns nothing" must not collapse, and the first must not be
    /// able to proceed.
    pub async fn resolve(
        store: &dyn DelegationStore,
        account_id: Uuid,
    ) -> Result<Self, ToolError> {
        let Some(is_operator) = store.account_authority(account_id).await? else {
            return Err(ToolError::NotFound("no such active account".into()));
        };
        let owned = store.namespaces_owned_by(account_id).await?;
        Ok(Self::from_live_state(account_id, is_operator, owned))
    }

    pub fn account_id(&self) -> Uuid {
        self.account_id
    }

    pub fn is_operator(&self) -> bool {
        self.is_operator
    }

    /// The namespaces this actor owns outright. Empty for an actor who owns
    /// none — which means "may scope a client to nothing", never "to anything".
    pub fn owned(&self) -> &BTreeSet<String> {
        &self.owned
    }

    /// This actor as an authoring authority.
    pub fn authoring(&self) -> Authoring<'_> {
        if self.is_operator {
            Authoring::Operator
        } else {
            Authoring::Delegated { owned: &self.owned }
        }
    }

    /// Whether this actor may attach `namespace` to a client.
    ///
    /// The operator branch is the deliberate widening RMCP-07 left a note for
    /// on [`crate::oauth::store::OauthStore::client_namespaces`]: the operator
    /// owns namespaces by DEFAULT, so an unclaimed namespace is the operator's
    /// to attach. It is not implicit reach — an explicit `rmcp_client_server`
    /// row still has to exist — and it stays the operator's alone: for anyone
    /// else, an unowned namespace is refused, exactly as before.
    pub fn owns(&self, namespace: &str) -> bool {
        self.is_operator || self.owned.contains(namespace)
    }

    /// Build one for a test in another module. Never compiled into a release.
    #[cfg(test)]
    pub fn for_test(
        account_id: Uuid,
        is_operator: bool,
        owned: impl IntoIterator<Item = String>,
    ) -> Self {
        Self::from_live_state(account_id, is_operator, owned)
    }
}

/// The authoring half of an [`ActorAuthority`], as `groups.rs` needs it.
///
/// A borrowed view rather than a copy of the set: authoring happens inside the
/// transaction that derived it, so there is nothing to own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Authoring<'a> {
    Operator,
    Delegated { owned: &'a BTreeSet<String> },
}

impl Authoring<'_> {
    /// The read-path authority class this authoring authority corresponds to.
    /// One mapping, so the two paths cannot disagree about who is an operator.
    pub fn owner(&self) -> GroupOwner {
        match self {
            Authoring::Operator => GroupOwner::Operator,
            Authoring::Delegated { .. } => GroupOwner::Delegated,
        }
    }

    /// Whether this author owns `namespace`.
    pub fn owns(&self, namespace: &str) -> bool {
        match self {
            Authoring::Operator => true,
            Authoring::Delegated { owned } => owned.contains(namespace),
        }
    }

    /// The WRITE-ONLY half of pattern authorization: does this author own the
    /// server their pattern names?
    ///
    /// Deliberately NOT combined with [`owner_may_hold`], which is the shape
    /// rule and is enforced by [`crate::oauth::groups::Pattern::parse`] on the
    /// write path and by the resolvers on the read path. Two questions, two
    /// functions, each asked exactly once per path — rather than one function
    /// that would have to be given an owned set at dispatch time, where the
    /// only available one is a stale snapshot.
    ///
    /// The error does not name the offending namespace: naming it would confirm
    /// which servers exist, which is the enumeration disclosure this item
    /// treats as a leak in its own right.
    pub fn authorize_ownership(&self, shape: PatternShape<'_>) -> Result<(), ToolError> {
        if let PatternShape::Namespaced(namespace) = shape {
            if !self.owns(namespace) {
                return Err(ToolError::InvalidArgument(
                    "one or more patterns name a server this account does not own".into(),
                ));
            }
        }
        Ok(())
    }
}

/// Authorize a set of namespaces for attachment to a client — the headline
/// check, and the one every scoping write path calls.
///
/// Empty request: always allowed, for anyone. "Scope this client to no servers"
/// is a narrowing, and an actor who owns zero namespaces must still be able to
/// create a client and scope it to nothing (the spec's own edge case).
pub fn authorize_namespace_scoping(
    actor: &ActorAuthority,
    requested: &[String],
) -> Result<(), ToolError> {
    if requested.iter().all(|namespace| actor.owns(namespace)) {
        return Ok(());
    }
    refusal(actor, ScopingRefusal::NamespaceNotOwned);
    Err(ToolError::InvalidArgument(
        "one or more servers are not owned by this account".into(),
    ))
}

/// Authorize a write against a client owned by `client_owner`.
///
/// The operator may administer any client; anyone else, only their own. The
/// error is the same for "no such client" and "not yours" — distinguishing them
/// would confirm another account's client exists.
pub fn authorize_client_write(
    actor: &ActorAuthority,
    client_owner: Uuid,
) -> Result<(), ToolError> {
    if actor.is_operator || actor.account_id == client_owner {
        return Ok(());
    }
    refusal(actor, ScopingRefusal::NotClientOwner);
    Err(ToolError::NotFound("no such client for this account".into()))
}

/// Authorize an OPERATOR-only administrative action: granting or revoking a
/// delegation. Delegation is the operator's to hand out; a delegated owner
/// administering their namespace cannot sub-delegate it, because a chain of
/// delegations is a chain nobody can audit.
pub fn authorize_delegation_change(actor: &ActorAuthority) -> Result<(), ToolError> {
    if actor.is_operator {
        return Ok(());
    }
    refusal(actor, ScopingRefusal::NotOperator);
    Err(ToolError::InvalidArgument(
        "only an operator account may grant or revoke server ownership".into(),
    ))
}

/// Emit the one audit shape a refusal produces. Closed vocabulary, no free
/// text, no namespace, no client id — the reason and the actor, nothing else.
fn refusal(actor: &ActorAuthority, reason: ScopingRefusal) {
    OauthAuditRecord::new(OauthEvent::ScopingDenied)
        .account(actor.account_id)
        .detail(AuditDetail::ScopingRefused { reason })
        .emit();
}

/// What a delegation write changed. Counts only — never the affected client
/// ids, which would make an audit record an enumeration of another account's
/// objects.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DelegationChange {
    /// Whether an existing delegation was replaced (a grant that reassigns).
    pub reassigned: bool,
    /// `rmcp_client_server` rows removed because the ownership that justified
    /// them is gone.
    pub rows_narrowed: u64,
}

/// The store operations delegation needs, as a seam.
///
/// A trait for the same reason [`crate::oauth::revoke::SessionStore`] is one:
/// this crate stands up no Postgres in tests, and "revoking a delegation
/// narrows the affected clients and audits it" must be assertable without one.
#[async_trait]
pub trait DelegationStore: Send + Sync {
    /// `Some(is_operator)` for an ACTIVE account; `None` for one that is
    /// missing or disabled. Collapsed on purpose — neither may act.
    async fn account_authority(&self, account_id: Uuid) -> Result<Option<bool>, ToolError>;

    /// The namespaces an account owns.
    async fn namespaces_owned_by(&self, account_id: Uuid) -> Result<Vec<String>, ToolError>;

    /// Assign ownership, narrowing any clients the PREVIOUS owner had scoped to
    /// it, in one transaction.
    async fn grant_namespace(
        &self,
        namespace: &str,
        grantee: Uuid,
    ) -> Result<DelegationChange, ToolError>;

    /// Remove a delegation, narrowing every client scoped to it, in one
    /// transaction.
    async fn revoke_namespace(&self, namespace: &str) -> Result<DelegationChange, ToolError>;

    /// Every delegation, for the operator's view.
    async fn list_server_owners(&self) -> Result<Vec<ServerOwner>, ToolError>;

    /// Resolve an account name to its id, or `None`.
    async fn account_id_by_name(&self, name: &str) -> Result<Option<Uuid>, ToolError>;

    /// The name of an account, for display in a listing the caller is already
    /// entitled to see. `None` for an account that no longer exists.
    async fn account_name(&self, account_id: Uuid) -> Result<Option<String>, ToolError>;
}

/// Delegation administration: grant, revoke, list.
///
/// Every method re-derives the actor's authority from the store rather than
/// taking one as an argument, so a caller cannot present an authority it
/// obtained before being demoted.
#[derive(Clone)]
pub struct DelegationService {
    store: std::sync::Arc<dyn DelegationStore>,
}

impl DelegationService {
    pub fn new(store: std::sync::Arc<dyn DelegationStore>) -> Self {
        Self { store }
    }

    /// Grant `namespace` to the account named `grantee_name`.
    ///
    /// Reassignment is allowed and is not silent: the previous owner's clients
    /// lose the namespace in the same transaction, and the count is audited.
    /// Leaving those rows in place would let a former owner's connector keep
    /// reaching a server that now belongs to someone else — the read path
    /// already refuses them, and this makes the state match the decision.
    pub async fn grant(
        &self,
        actor_account_id: Uuid,
        namespace: &str,
        grantee_name: &str,
    ) -> Result<DelegationChange, ToolError> {
        let actor = ActorAuthority::resolve(self.store.as_ref(), actor_account_id).await?;
        authorize_delegation_change(&actor)?;
        let Some(grantee) = self.store.account_id_by_name(grantee_name).await? else {
            // Same answer as a disabled account below would give: this must not
            // become an account-existence oracle for whoever reaches the tool.
            return Err(ToolError::NotFound("no such active account".into()));
        };
        if self.store.account_authority(grantee).await?.is_none() {
            return Err(ToolError::NotFound("no such active account".into()));
        }
        let change = self.store.grant_namespace(namespace, grantee).await?;
        OauthAuditRecord::new(OauthEvent::DelegationChanged)
            .account(actor.account_id())
            .detail(AuditDetail::DelegationGranted {
                reassigned: change.reassigned,
                rows_narrowed: change.rows_narrowed,
            })
            .emit();
        Ok(change)
    }

    /// Remove the delegation on `namespace`.
    ///
    /// The narrowing is immediate on the READ path whether or not this
    /// transaction's row cleanup runs — `client_namespaces` re-joins
    /// `rmcp_server_owner` on every resolution — so a partial failure here can
    /// only ever leave rows that already authorize nothing.
    pub async fn revoke(
        &self,
        actor_account_id: Uuid,
        namespace: &str,
    ) -> Result<DelegationChange, ToolError> {
        let actor = ActorAuthority::resolve(self.store.as_ref(), actor_account_id).await?;
        authorize_delegation_change(&actor)?;
        let change = self.store.revoke_namespace(namespace).await?;
        OauthAuditRecord::new(OauthEvent::DelegationChanged)
            .account(actor.account_id())
            .detail(AuditDetail::DelegationCleared {
                rows_narrowed: change.rows_narrowed,
            })
            .emit();
        Ok(change)
    }

    /// List delegations: everything for an operator, only your own otherwise.
    ///
    /// The delegated view is filtered in this process rather than by a query
    /// per caller because the filter IS the rule and belongs next to it — and
    /// because a delegated owner's own row is the only one they may see, so the
    /// filtered result is at most one element regardless of fleet size.
    pub async fn list(&self, actor_account_id: Uuid) -> Result<Vec<ServerOwner>, ToolError> {
        let actor = ActorAuthority::resolve(self.store.as_ref(), actor_account_id).await?;
        let all = self.store.list_server_owners().await?;
        Ok(if actor.is_operator() {
            all
        } else {
            all.into_iter()
                .filter(|owner| owner.owner_account_id == actor.account_id())
                .collect()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn delegated(owned: &[&str]) -> ActorAuthority {
        ActorAuthority::from_live_state(
            Uuid::from_u128(2),
            false,
            owned.iter().map(|v| (*v).to_string()),
        )
    }

    fn operator() -> ActorAuthority {
        ActorAuthority::from_live_state(Uuid::from_u128(1), true, Vec::<String>::new())
    }

    // ── The headline test ────────────────────────────────────────────────────

    #[test]
    fn a_delegated_owner_cannot_scope_a_client_to_a_namespace_it_does_not_own() {
        let actor = delegated(&["peerone"]);
        assert!(authorize_namespace_scoping(&actor, &["peerone".to_string()]).is_ok());
        assert!(authorize_namespace_scoping(&actor, &["peertwo".to_string()]).is_err());
        // And not by mixing one it owns with one it does not.
        assert!(authorize_namespace_scoping(
            &actor,
            &["peerone".to_string(), "peertwo".to_string()]
        )
        .is_err());
    }

    #[test]
    fn an_actor_owning_nothing_may_scope_a_client_to_nothing_but_not_to_something() {
        let actor = delegated(&[]);
        assert!(authorize_namespace_scoping(&actor, &[]).is_ok());
        assert!(authorize_namespace_scoping(&actor, &["peerone".to_string()]).is_err());
    }

    #[test]
    fn an_unowned_namespace_is_the_operators_and_nobody_elses() {
        // The deliberate widening, and its bound. "Nobody has claimed this
        // server" reads as "the operator's by default" for the operator, and as
        // "not yours" for everyone else — never as "free for anyone".
        assert!(authorize_namespace_scoping(&operator(), &["unclaimed".to_string()]).is_ok());
        assert!(authorize_namespace_scoping(&delegated(&["peerone"]), &["unclaimed".to_string()])
            .is_err());
    }

    #[test]
    fn the_operator_retains_full_access() {
        let operator = operator();
        assert!(authorize_namespace_scoping(&operator, &["anything".to_string()]).is_ok());
        assert!(authorize_client_write(&operator, Uuid::from_u128(99)).is_ok());
        assert!(authorize_delegation_change(&operator).is_ok());
        for shape in [
            PatternShape::Everything,
            PatternShape::Local,
            PatternShape::Namespaced("anything"),
        ] {
            assert!(owner_may_hold(GroupOwner::Operator, shape));
            assert!(operator.authoring().authorize_ownership(shape).is_ok());
        }
    }

    #[test]
    fn a_delegated_owner_cannot_touch_another_owners_client() {
        let actor = delegated(&["peerone"]);
        assert!(authorize_client_write(&actor, actor.account_id()).is_ok());
        assert!(authorize_client_write(&actor, Uuid::from_u128(77)).is_err());
    }

    #[test]
    fn a_delegated_owner_cannot_grant_or_revoke_delegation() {
        assert!(authorize_delegation_change(&delegated(&["peerone"])).is_err());
    }

    // ── The `<ns>::*` decision, pinned in both directions ────────────────────

    #[test]
    fn a_delegated_owner_may_author_a_wildcard_over_a_namespace_it_owns() {
        let actor = delegated(&["peerone"]);
        assert!(owner_may_hold(GroupOwner::Delegated, PatternShape::Namespaced("peerone")));
        assert!(actor
            .authoring()
            .authorize_ownership(PatternShape::Namespaced("peerone"))
            .is_ok());
    }

    #[test]
    fn a_delegated_owner_may_not_author_a_pattern_naming_another_server() {
        let actor = delegated(&["peerone"]);
        assert!(actor
            .authoring()
            .authorize_ownership(PatternShape::Namespaced("peertwo"))
            .is_err());
    }

    #[test]
    fn a_delegated_owner_may_hold_no_unqualified_or_bare_pattern() {
        // The shape rule — the function `Pattern::parse`, `resolve_groups` and
        // `ClientScope::from_rows` all call.
        assert!(!owner_may_hold(GroupOwner::Delegated, PatternShape::Local));
        assert!(!owner_may_hold(GroupOwner::Delegated, PatternShape::Everything));
        assert!(owner_may_hold(GroupOwner::Delegated, PatternShape::Namespaced("peerone")));
    }

    #[test]
    fn owner_may_hold_is_the_read_path_rule_and_ignores_ownership() {
        // Deliberate: at read time the namespace dimension is bounded by the
        // client's own rows, re-derived per call. If this function started
        // consulting an owned set it would need one at dispatch, which is the
        // stale-snapshot mistake this module exists to avoid.
        assert!(owner_may_hold(GroupOwner::Delegated, PatternShape::Namespaced("anything")));
    }

    // ── Mutation-verification of the shape guard ─────────────────────────────

    #[test]
    fn deleting_the_ownership_check_would_be_caught() {
        // Mutation-verified: delete the `if !self.owns(namespace)` branch in
        // `Authoring::authorize_ownership` and this goes red, because it asserts
        // the REFUSAL rather than only the permitted case.
        let actor = delegated(&["peerone"]);
        let err = actor
            .authoring()
            .authorize_ownership(PatternShape::Namespaced("peertwo"))
            .expect_err("a delegated owner must not name another account's server");
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    #[test]
    fn deleting_the_operator_branch_of_owner_may_hold_would_be_caught() {
        // The other half, same discipline: the rule must SEPARATE the two
        // owners, so both directions are asserted.
        assert!(owner_may_hold(GroupOwner::Operator, PatternShape::Local));
        assert!(!owner_may_hold(GroupOwner::Delegated, PatternShape::Local));
    }

    // ── The service, against a fake store ────────────────────────────────────

    #[derive(Default)]
    struct FakeStore {
        /// (account_id, is_operator, active)
        accounts: Vec<(Uuid, bool, bool, String)>,
        owners: Mutex<Vec<ServerOwner>>,
        narrowed: Mutex<u64>,
    }

    #[async_trait]
    impl DelegationStore for FakeStore {
        async fn account_authority(&self, account_id: Uuid) -> Result<Option<bool>, ToolError> {
            Ok(self
                .accounts
                .iter()
                .find(|(id, _, active, _)| *id == account_id && *active)
                .map(|(_, is_operator, _, _)| *is_operator))
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
            namespace: &str,
            grantee: Uuid,
        ) -> Result<DelegationChange, ToolError> {
            let mut owners = self.owners.lock().unwrap();
            let reassigned = owners.iter().any(|o| o.namespace == namespace);
            owners.retain(|o| o.namespace != namespace);
            owners.push(ServerOwner {
                namespace: namespace.to_string(),
                owner_account_id: grantee,
                granted_at: chrono::Utc::now(),
            });
            let rows_narrowed = if reassigned { *self.narrowed.lock().unwrap() } else { 0 };
            Ok(DelegationChange { reassigned, rows_narrowed })
        }

        async fn revoke_namespace(&self, namespace: &str) -> Result<DelegationChange, ToolError> {
            let mut owners = self.owners.lock().unwrap();
            let existed = owners.iter().any(|o| o.namespace == namespace);
            owners.retain(|o| o.namespace != namespace);
            Ok(DelegationChange {
                reassigned: false,
                rows_narrowed: if existed { *self.narrowed.lock().unwrap() } else { 0 },
            })
        }

        async fn list_server_owners(&self) -> Result<Vec<ServerOwner>, ToolError> {
            Ok(self.owners.lock().unwrap().clone())
        }

        async fn account_id_by_name(&self, name: &str) -> Result<Option<Uuid>, ToolError> {
            Ok(self
                .accounts
                .iter()
                .find(|(_, _, _, account)| account == name)
                .map(|(id, _, _, _)| *id))
        }

        async fn account_name(&self, account_id: Uuid) -> Result<Option<String>, ToolError> {
            Ok(self
                .accounts
                .iter()
                .find(|(id, _, _, _)| *id == account_id)
                .map(|(_, _, _, name)| name.clone()))
        }
    }

    fn fake() -> std::sync::Arc<FakeStore> {
        std::sync::Arc::new(FakeStore {
            accounts: vec![
                (Uuid::from_u128(1), true, true, "operator".to_string()),
                (Uuid::from_u128(2), false, true, "friend".to_string()),
                (Uuid::from_u128(3), false, false, "former".to_string()),
            ],
            owners: Mutex::new(Vec::new()),
            narrowed: Mutex::new(3),
        })
    }

    #[tokio::test]
    async fn granting_requires_an_operator_and_records_the_narrowing() {
        let store = fake();
        let service = DelegationService::new(store.clone());

        // A delegated account cannot hand out ownership, even of its own server.
        service
            .grant(Uuid::from_u128(1), "peerone", "friend")
            .await
            .expect("the operator may grant");
        service
            .grant(Uuid::from_u128(2), "peerone", "friend")
            .await
            .expect_err("a delegated owner may not grant");

        // Reassigning narrows the previous owner's clients and says so.
        let change = service
            .grant(Uuid::from_u128(1), "peerone", "operator")
            .await
            .expect("reassignment is allowed for an operator");
        assert!(change.reassigned);
        assert_eq!(change.rows_narrowed, 3);
    }

    #[tokio::test]
    async fn granting_to_a_disabled_account_is_refused_without_confirming_it_exists() {
        let service = DelegationService::new(fake());
        let disabled = service.grant(Uuid::from_u128(1), "peerone", "former").await;
        let absent = service.grant(Uuid::from_u128(1), "peerone", "nobody").await;
        // Identical answers: a disabled account and a nonexistent one are the
        // same non-answer, so this cannot be used to enumerate accounts.
        assert_eq!(
            format!("{}", disabled.expect_err("disabled must be refused")),
            format!("{}", absent.expect_err("absent must be refused"))
        );
    }

    #[tokio::test]
    async fn revoking_narrows_and_is_operator_only() {
        let store = fake();
        let service = DelegationService::new(store.clone());
        service.grant(Uuid::from_u128(1), "peerone", "friend").await.unwrap();

        service
            .revoke(Uuid::from_u128(2), "peerone")
            .await
            .expect_err("a delegated owner may not revoke its own delegation");
        let change = service
            .revoke(Uuid::from_u128(1), "peerone")
            .await
            .expect("the operator may revoke");
        assert_eq!(change.rows_narrowed, 3);
        assert!(store.owners.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_delegated_owner_lists_only_its_own_delegation() {
        let store = fake();
        let service = DelegationService::new(store.clone());
        service.grant(Uuid::from_u128(1), "peerone", "friend").await.unwrap();
        service.grant(Uuid::from_u128(1), "peertwo", "operator").await.unwrap();

        let operator_view = service.list(Uuid::from_u128(1)).await.unwrap();
        assert_eq!(operator_view.len(), 2);

        let delegated_view = service.list(Uuid::from_u128(2)).await.unwrap();
        assert_eq!(delegated_view.len(), 1);
        assert_eq!(delegated_view[0].namespace, "peerone");
    }

    #[tokio::test]
    async fn a_disabled_actor_cannot_act_and_is_not_treated_as_owning_nothing() {
        let service = DelegationService::new(fake());
        // Uuid 3 is disabled. The refusal is an error, not a silent downgrade
        // to a delegated actor with an empty owned set.
        service
            .list(Uuid::from_u128(3))
            .await
            .expect_err("a disabled account must not resolve to an authority");
    }
}
