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
//!    the namespace dimension only to tools a namespace CONTRIBUTED (TERM #643;
//!    it was "only to namespaced NAMES", which is the same sentence with a
//!    guess where the fact belongs — a local tool named `peerhub__tool` is
//!    still the operator's).
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
/// There is ONE pattern vocabulary — [`crate::oauth::groups::Pattern`] — and it
/// maps onto this. It was two until TERM #637 collapsed the enforcing copy onto
/// the authoring one; the AUTHORITY rule was never duplicated even then,
/// because both reduced to this shape and both asked [`owner_may_hold`], and
/// that is why the collapse did not have to re-decide it.
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
    authorize_operator_action(actor, DELEGATION_CHANGE_IS_OPERATOR_ONLY)
}

/// The refusal [`authorize_delegation_change`] carries.
pub const DELEGATION_CHANGE_IS_OPERATOR_ONLY: &str =
    "only an operator account may grant or revoke server ownership";

/// Authorize any OPERATOR-only administrative action.
///
/// ## Why this exists (RMCP-08, review round 2)
///
/// [`authorize_delegation_change`] was this rule, written for delegation
/// specifically. RMCP-08 needs the identical rule for its initial-access-token
/// controls — minting one is what makes gated dynamic client registration
/// reachable at all, so a delegated account able to mint one could invite
/// clients into a fleet it does not administer.
///
/// Generalised IN PLACE rather than copied. Two functions answering "is this
/// actor an operator" is exactly the duplicate-rule shape this module exists to
/// prevent, and the copy would be the one that fails to get updated. So
/// `authorize_delegation_change` is now a thin call to this, keeping its own
/// message, and the audit record is unchanged: [`ScopingRefusal::NotOperator`]
/// already covered "the action is operator-only", so no new vocabulary was
/// added.
///
/// `refusal` is a `&'static str`, never a runtime string. It names the ACTION
/// for the operator reading the error; it cannot carry anything a caller
/// submitted, and it does not reach the audit record at all — that stays the
/// closed enum, with no free text, as everywhere else in this module.
pub fn authorize_operator_action(
    actor: &ActorAuthority,
    refusal_message: &'static str,
) -> Result<(), ToolError> {
    if actor.is_operator {
        return Ok(());
    }
    refusal(actor, ScopingRefusal::NotOperator);
    Err(ToolError::InvalidArgument(refusal_message.into()))
}

/// Re-verify a delegation proof against the actor's LIVE authority, read inside
/// the writing transaction.
///
/// ## Why a proof is not enough on its own
///
/// Round 2 of review found the gap, and it is this item's own defect class
/// arriving inside the mechanism built to prevent it. A [`DelegationGrant`]
/// proves the check RAN; it does not prove the check still HOLDS. Between
/// [`ActorAuthority::resolve`] and the store's commit, the actor can be demoted
/// or disabled — and that window is not hypothetical, it is precisely the
/// moment someone is racing an operator who is cutting off a compromised
/// account.
///
/// A value that proves "authorized at some earlier point" is exactly the
/// stale-snapshot shape this module refuses everywhere else. RMCP-01 hit the
/// identical shape on namespace ownership and closed it the same way: re-read
/// under `FOR SHARE` inside the writing transaction, so the authority cannot
/// move between the check and the commit.
///
/// Two things are asserted, and both matter:
///
/// 1. **The live authority is for the SAME account the proof names.** Otherwise
///    a proof minted by an operator could be re-verified against some other
///    account that happens to be an operator, which would make the re-check a
///    formality.
/// 2. **That account is STILL an operator and still enabled** — the same
///    predicate as the mint, via [`authorize_delegation_change`], so there is
///    one rule, evaluated twice against two different reads, rather than two
///    rules that could drift.
///
/// The store passes an authority derived by its own locking helper, so "live"
/// here means locked-for-the-rest-of-the-transaction, not merely recent.
pub fn reverify_delegation_change(
    proof_actor: Uuid,
    live: &ActorAuthority,
) -> Result<(), ToolError> {
    if live.account_id() != proof_actor {
        // Not an authorization failure so much as a wiring failure, and it is
        // refused rather than tolerated: a re-check against the wrong account
        // proves nothing at all.
        return Err(ToolError::InvalidArgument(
            "the delegation authorization does not belong to the account being re-verified".into(),
        ));
    }
    authorize_delegation_change(live)
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

/// Proof that a delegation GRANT was authorized, carrying the decision's own
/// inputs.
///
/// ## Why this type exists — and why it is not a marker token
///
/// Round 1 of review found the hazard this closes: the store's raw
/// `set_server_owner`/`clear_server_owner` performed no authorization at all, so
/// [`DelegationService`] was not the only way to mutate a delegation, merely the
/// polite one. Anything holding the store — including code written later by
/// someone who has never heard of this module — could grant a namespace to
/// anybody. Every authoring rule this item added was bypassable one layer down.
///
/// The fix is structural, not documentary, and it is deliberately NOT the shape
/// RMCP-01 and RMCP-07 both threw out. A *data-free* marker (`struct Approved;`)
/// proves nothing, because any caller can mint one and claim a check it never
/// ran — that is a comment wearing a type's clothes.
///
/// This value instead CARRIES the decision's inputs, and the only constructor
/// is [`Self::authorize`], which performs the check. Producing one IS the
/// check:
///
/// - Its fields are private, so it cannot be assembled by a struct literal.
/// - Its constructor demands an [`ActorAuthority`], which has no public
///   constructor of its own — the only ways to obtain one are a live store read
///   ([`ActorAuthority::resolve`]) or the crate-visible
///   [`ActorAuthority::from_live_state`] the store calls under its own locks.
/// - The namespace and grantee are read back OUT of the proof by the store, so
///   a caller cannot authorize one namespace and then mutate a different one.
///   That last point is what makes it a proof rather than a permission slip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DelegationGrant {
    actor: Uuid,
    namespace: String,
    grantee: Uuid,
}

impl DelegationGrant {
    /// Run the check; on success, the returned value is the evidence.
    ///
    /// `actor` must be an authority derived from a LIVE read — the snapshot rule
    /// this whole module is built on. [`DelegationService::grant`] resolves one
    /// immediately before calling this.
    pub fn authorize(
        actor: &ActorAuthority,
        namespace: &str,
        grantee: Uuid,
    ) -> Result<Self, ToolError> {
        authorize_delegation_change(actor)?;
        Ok(Self {
            actor: actor.account_id(),
            namespace: namespace.to_string(),
            grantee,
        })
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    pub fn grantee(&self) -> Uuid {
        self.grantee
    }

    /// The account whose authority was checked, for the audit record.
    pub fn actor(&self) -> Uuid {
        self.actor
    }
}

/// Proof that a delegation REVOCATION was authorized. See [`DelegationGrant`];
/// same construction, same reasoning, and a separate type so a grant's evidence
/// cannot be handed to a revocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DelegationRevocation {
    actor: Uuid,
    namespace: String,
}

impl DelegationRevocation {
    pub fn authorize(actor: &ActorAuthority, namespace: &str) -> Result<Self, ToolError> {
        authorize_delegation_change(actor)?;
        Ok(Self {
            actor: actor.account_id(),
            namespace: namespace.to_string(),
        })
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    pub fn actor(&self) -> Uuid {
        self.actor
    }
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
    ///
    /// Takes the AUTHORIZATION, not the arguments: there is no way to call this
    /// without having run the check, because the only way to obtain a
    /// [`DelegationGrant`] is to pass it (see that type's docs). The namespace
    /// and grantee are read out of the proof rather than accepted alongside it,
    /// so an authorized grant cannot be redirected at a different namespace.
    async fn grant_namespace(
        &self,
        grant: &DelegationGrant,
    ) -> Result<DelegationChange, ToolError>;

    /// Remove a delegation, narrowing every client scoped to it, in one
    /// transaction. Same construction as [`Self::grant_namespace`].
    async fn revoke_namespace(
        &self,
        revocation: &DelegationRevocation,
    ) -> Result<DelegationChange, ToolError>;

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
        // Checked BEFORE the grantee is looked up, and that ordering is the
        // whole reason this call is here rather than only inside
        // `DelegationGrant::authorize` below. Resolving a name first would let
        // an unauthorized caller distinguish "no such account" from "not
        // allowed" — an account-existence oracle handed to precisely the caller
        // who should learn nothing. It is the same predicate, not a second
        // rule: the proof below is what the store will actually accept, and
        // this is only the order in which the caller learns the answer.
        authorize_delegation_change(&actor)?;
        let Some(grantee) = self.store.account_id_by_name(grantee_name).await? else {
            // Same answer as a disabled account below would give: this must not
            // become an account-existence oracle for whoever reaches the tool.
            return Err(ToolError::NotFound("no such active account".into()));
        };
        if self.store.account_authority(grantee).await?.is_none() {
            return Err(ToolError::NotFound("no such active account".into()));
        }
        let grant = DelegationGrant::authorize(&actor, namespace, grantee)?;
        let change = self.store.grant_namespace(&grant).await?;
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
        let revocation = DelegationRevocation::authorize(&actor, namespace)?;
        let change = self.store.revoke_namespace(&revocation).await?;
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

    // ── The proof values: producing one IS the check ─────────────────────────

    #[test]
    fn only_an_operator_can_produce_a_delegation_proof() {
        let operator = operator();
        let delegated = delegated(&["peerone"]);

        DelegationGrant::authorize(&operator, "peerone", Uuid::from_u128(2))
            .expect("an operator may authorize a grant");
        DelegationRevocation::authorize(&operator, "peerone")
            .expect("an operator may authorize a revocation");

        // And a delegated owner cannot — not even for the server they own, and
        // not even to give it away. Delegation does not chain.
        assert!(DelegationGrant::authorize(&delegated, "peerone", Uuid::from_u128(3)).is_err());
        assert!(DelegationRevocation::authorize(&delegated, "peerone").is_err());
    }

    /// The proof CARRIES the decision's inputs, which is what makes it a proof
    /// rather than a marker token: the store reads the namespace and grantee
    /// back out of it, so an authorized grant cannot be redirected at a
    /// different namespace after the fact.
    #[test]
    fn a_proof_carries_the_inputs_it_was_authorized_for() {
        let operator = operator();
        let grant = DelegationGrant::authorize(&operator, "peerone", Uuid::from_u128(2)).unwrap();
        assert_eq!(grant.namespace(), "peerone");
        assert_eq!(grant.grantee(), Uuid::from_u128(2));
        assert_eq!(grant.actor(), operator.account_id());

        let revocation = DelegationRevocation::authorize(&operator, "peertwo").unwrap();
        assert_eq!(revocation.namespace(), "peertwo");
        assert_eq!(revocation.actor(), operator.account_id());
    }

    /// Mutation-verify: delete the `authorize_delegation_change(actor)?` line
    /// from either constructor and this goes red. It asserts the REFUSAL, which
    /// is the half a happy-path test cannot see.
    #[test]
    fn deleting_the_check_from_a_proof_constructor_would_be_caught() {
        let delegated = delegated(&["peerone"]);
        let err = DelegationGrant::authorize(&delegated, "peerone", Uuid::from_u128(3))
            .expect_err("a delegated owner must not be able to mint a grant proof");
        assert!(matches!(err, ToolError::InvalidArgument(_)));
    }

    // ── The proof is re-verified at write time, not trusted ──────────────────

    /// **The round-2 assertion.** A proof minted by an operator who is DEMOTED
    /// before the write lands must not complete the mutation.
    ///
    /// This is the one a happy-path test cannot make, and the one that fails if
    /// someone later removes the "redundant-looking" second read inside the
    /// store's transaction.
    #[test]
    fn a_proof_minted_before_a_demotion_does_not_survive_the_re_check() {
        let operator = operator();
        let grant = DelegationGrant::authorize(&operator, "peerone", Uuid::from_u128(9))
            .expect("minted while still an operator");

        // Same account, live state re-read inside the writing transaction —
        // no longer an operator.
        let demoted = ActorAuthority::from_live_state(
            operator.account_id(),
            false,
            Vec::<String>::new(),
        );
        assert!(
            reverify_delegation_change(grant.actor(), &demoted).is_err(),
            "a stale proof must not authorize a write after the actor was demoted"
        );

        // And the same proof against the still-operator state is fine, so the
        // re-check refuses the CHANGE rather than refusing everything.
        assert!(reverify_delegation_change(grant.actor(), &operator).is_ok());
    }

    /// A disabled account is caught too — though by a different mechanism, and
    /// this asserts the mechanism rather than assuming it.
    ///
    /// The store's `locked_active_account` filters `NOT disabled` in SQL, so a
    /// disabled actor never yields an `ActorAuthority` at all and this function
    /// is never reached. That is the fail-closed direction, but it means the
    /// coverage lives in the query, not here — stated so nobody reads this
    /// module's tests as proving something they do not.
    #[test]
    fn the_re_check_is_bound_to_the_account_the_proof_names() {
        let operator = operator();
        let grant = DelegationGrant::authorize(&operator, "peerone", Uuid::from_u128(9)).unwrap();

        // A DIFFERENT account that happens to be an operator must not satisfy
        // the re-check: otherwise the second read would be a formality that any
        // live operator anywhere could pass.
        let other_operator =
            ActorAuthority::from_live_state(Uuid::from_u128(42), true, Vec::<String>::new());
        assert!(
            reverify_delegation_change(grant.actor(), &other_operator).is_err(),
            "the live authority must be for the SAME account the proof names"
        );
    }

    /// The revocation half, so neither path can be hardened alone.
    #[test]
    fn a_revocation_proof_is_re_checked_the_same_way() {
        let operator = operator();
        let revocation = DelegationRevocation::authorize(&operator, "peerone").unwrap();
        let demoted = ActorAuthority::from_live_state(
            operator.account_id(),
            false,
            Vec::<String>::new(),
        );
        assert!(reverify_delegation_change(revocation.actor(), &demoted).is_err());
        assert!(reverify_delegation_change(revocation.actor(), &operator).is_ok());
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
            let rows_narrowed = if reassigned { *self.narrowed.lock().unwrap() } else { 0 };
            Ok(DelegationChange { reassigned, rows_narrowed })
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
