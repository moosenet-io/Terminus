//! RMCP-11 — revocation: RFC 7009, session listing, and the "cut it off now"
//! control.
//!
//! ## One implementation, three surfaces
//!
//! Access can be cut off from three places: the RFC 7009 endpoint a client calls
//! ([`RevocationService::revoke_presented_token`]), the `rmcp_session_revoke`
//! tool an operator calls, and the GUI (RMCP-13), which calls the tool. All three
//! land on [`RevocationService::revoke`]. That is not merely tidy — the sanctioned
//! surface rule is that the GUI and the CLI share ONE implementation, and a
//! second path is how "revoked in the UI" and "still working in practice" happen
//! to different people at the same time.
//!
//! ## What "revoked" means here, and why it is family-wide
//!
//! A session is a refresh-token FAMILY (see [`TokenFamily`]). Revoking anything
//! revokes whole families, never individual rows, because
//! [`crate::oauth::store::OauthStore::refresh_token_is_live`] already decides liveness family-wide:
//! any revoked row in a family kills every member, including rows inserted after
//! the revocation. RMCP-01 chose that specifically to close a rotation/revocation
//! race, and this module inherits it rather than reasoning about rows.
//!
//! Revoking a CONSENT is likewise inseparable from revoking its tokens —
//! [`crate::oauth::store::OauthStore::revoke_consent`] does both in one transaction, because a revoked
//! consent whose refresh tokens still work is not a revocation. This module never
//! offers a way to do half of it.
//!
//! ## Effective at the next dispatch, and verified
//!
//! The acceptance criterion is that revocation bites at the next dispatch rather
//! than at the next token expiry, checked against the STORE rather than against a
//! signature. Two things implement that:
//!
//! * [`SessionStore::dispatch_state`] is the single predicate the resource
//!   server (RMCP-05) consults per request. It re-reads client, account, consent
//!   and family state every time, so anything this module writes is visible to
//!   the very next call. An access token's signature is never evidence of
//!   anything but its own integrity.
//! * [`RevocationService::revoke`] does not report success on the strength of an
//!   `UPDATE` returning. It RE-READS the affected families afterwards and fails
//!   loudly if any is still live. A revocation that silently did not land is the
//!   worst outcome available here — the operator believes the door is shut and
//!   stops looking — so it is the one case that is never reported as success.
//!
//! ## Idempotence
//!
//! Revoking an already-revoked session is success, not an error. An operator
//! hitting the button twice, a GUI retrying a request, and a reuse-detection path
//! racing an operator all converge on "it is off", which is the state the caller
//! asked for. The report says whether this call was the one that changed
//! anything, so the outcome is still legible.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Serialize;
use uuid::Uuid;

use crate::error::ToolError;
use crate::oauth::audit::{AuditDetail, DenialReason, OauthAuditRecord, OauthEvent, SelectorKind};
use crate::oauth::limits::OauthEndpoint;
use crate::oauth::model::TokenFamily;
use crate::oauth::SecretHash;

/// Whether a request may be dispatched RIGHT NOW, judged from live store state.
///
/// Fail-closed by shape: there is no `Unknown` and no default. Every variant
/// other than [`Self::Allowed`] denies, so a future variant added for a new
/// revocation lever cannot accidentally be permissive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchState {
    Allowed,
    /// The client record is disabled — [`crate::oauth::store::OauthStore::find_active_client`]
    /// already treats a disabled client as nonexistent, which makes disabling
    /// one a revocation lever in its own right.
    ClientDisabled,
    /// The account is disabled.
    AccountDisabled,
    /// No live consent remains for this account/client pair.
    ConsentRevoked,
    /// The token's session family carries a revocation, or is not bound to this
    /// account and client at all.
    SessionRevoked,
    /// The request named no session at all.
    ///
    /// **This is a DENIAL, and it is the whole point of the variant.** An
    /// earlier revision let a `None` family id mean "no session constraint to
    /// check" and fell through to allowed — so revoking one family did not stop
    /// an access token whose dispatch path happened not to carry a family id.
    /// The authorization was revoked and the read path kept honouring it, which
    /// is precisely the defect class this item exists to close: an absent value
    /// reading as permission.
    ///
    /// A token this server issues always carries its session, so in practice
    /// this fires only for a token minted before sessions were bound, a
    /// malformed one, or a call site that forgot to thread the value through.
    /// All three must be refused rather than trusted.
    SessionUnidentified,
}

impl DispatchState {
    pub fn is_allowed(self) -> bool {
        matches!(self, DispatchState::Allowed)
    }

    /// The audit reason a denial is recorded under. Every denial collapses to
    /// [`DenialReason::Revoked`] in the log's vocabulary; the precise variant is
    /// carried in the record's detail, which is a closed value here rather than
    /// caller text.
    pub fn denial_reason(self) -> Option<DenialReason> {
        match self {
            DispatchState::Allowed => None,
            _ => Some(DenialReason::Revoked),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            DispatchState::Allowed => "allowed",
            DispatchState::ClientDisabled => "client_disabled",
            DispatchState::AccountDisabled => "account_disabled",
            DispatchState::ConsentRevoked => "consent_revoked",
            DispatchState::SessionRevoked => "session_revoked",
            DispatchState::SessionUnidentified => "session_unidentified",
        }
    }
}

/// What to revoke, or what to list.
///
/// Deliberately does NOT include "by raw token". A token belongs in exactly one
/// place — the RFC 7009 request body, where the client that holds it presents it
/// over TLS — and putting one in a tool argument would mean a credential
/// travelling through tool dispatch, argument summaries, and every audit and
/// trace layer in between. [`RevocationService::revoke_presented_token`] is the
/// only entry point that takes a token, and it converts it to a family id
/// immediately.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionSelector {
    /// Every session belonging to an account, across all its clients. The
    /// account is named, not identified by UUID, because that is what an
    /// operator has to hand.
    Account(String),
    /// Every session issued to a client, across all accounts.
    Client(String),
    /// One account's sessions with one client — the ordinary "disconnect this
    /// connector" case, and the only selector that also revokes CONSENT.
    AccountAndClient { account: String, client: String },
    /// One session, by family id (what [`SessionSummary`] reports).
    Family(Uuid),
}

impl SessionSelector {
    /// Which KIND of selector this is, as a closed audit-vocabulary term.
    ///
    /// Returns [`SelectorKind`] rather than a string so the value can be put
    /// straight into an [`AuditDetail`] without reopening a text channel — the
    /// selector's *contents* (an account name, a client id) are caller data and
    /// deliberately never reach a record; only which of the four shapes it was.
    pub fn kind(&self) -> SelectorKind {
        match self {
            SessionSelector::Account(_) => SelectorKind::Account,
            SessionSelector::Client(_) => SelectorKind::Client,
            SessionSelector::AccountAndClient { .. } => SelectorKind::AccountAndClient,
            SessionSelector::Family(_) => SelectorKind::Family,
        }
    }
}

/// One session as reported to an operator.
///
/// Carries no token material of any kind — not a token, not a hash, not a
/// prefix. `family_id` is the handle for every subsequent operation, which is
/// what makes a listing safe to render in a GUI and to paste into a chat.
#[derive(Debug, Clone, Serialize)]
pub struct SessionSummary {
    pub family_id: Uuid,
    pub account_id: Uuid,
    pub client_id: Uuid,
    pub resource: String,
    pub scope: String,
    pub issued_at: String,
    pub last_used_at: String,
    pub expires_at: String,
    /// Rotations + 1 — a rough measure of how active the session has been.
    pub token_count: i64,
    pub live: bool,
    pub revoked_at: Option<String>,
}

impl From<&TokenFamily> for SessionSummary {
    fn from(f: &TokenFamily) -> Self {
        Self {
            family_id: f.family_id,
            account_id: f.account_id,
            client_id: f.client_id,
            resource: f.resource.clone(),
            scope: f.scope.clone(),
            issued_at: f.issued_at.to_rfc3339(),
            last_used_at: f.last_issued_at.to_rfc3339(),
            expires_at: f.expires_at.to_rfc3339(),
            token_count: f.token_count,
            live: f.live,
            revoked_at: f.revoked_at.map(|t| t.to_rfc3339()),
        }
    }
}

/// The outcome of a revocation.
#[derive(Debug, Clone, Serialize)]
pub struct RevocationReport {
    pub selector: SelectorKind,
    /// Families the selector matched. Zero is not an error — see
    /// [`RevocationService::revoke`].
    pub families_matched: usize,
    /// Families that were live before this call and are dead after it. Zero with
    /// a non-zero `families_matched` is the idempotent re-revocation case.
    pub families_newly_revoked: usize,
    /// Individual token rows the store transitioned to revoked.
    pub tokens_revoked: u64,
    /// Consent rows revoked. Only the account+client selector revokes consent.
    pub consents_revoked: u64,
    /// Whether every matched family was RE-READ after the write and confirmed
    /// dead. Always `true` in a successful return — a `false` here is returned
    /// as an error instead, because an unverified revocation reported as success
    /// is the failure this field exists to make impossible.
    pub verified: bool,
}

/// The persistence operations revocation and session listing need.
///
/// A trait rather than a direct dependency on [`OauthStore`] for the reason
/// [`crate::locations::store::LocationStore`] is one: the interesting behaviour
/// here — idempotence, the verify-after-write step, the RFC 7009 client-mismatch
/// rule — is logic, not SQL, and it should be testable without a database.
/// RMCP-01 explicitly deferred DB-backed integration testing, and this seam is
/// what keeps THIS item's contracts from being deferred with it.
#[async_trait]
pub trait SessionStore: Send + Sync {
    /// Resolve an account name to its id, INCLUDING a disabled account.
    ///
    /// Deliberately different from
    /// [`crate::oauth::store::OauthStore::find_active_account_by_name`], which hides disabled
    /// accounts so a caller cannot use it as an existence oracle. That rule
    /// protects the AUTHENTICATION path; applying it here would mean an operator
    /// could not revoke the sessions of an account they had just disabled, which
    /// is precisely when they most want to. Revocation only ever narrows, and
    /// this resolution is reachable only from an operator tool, so the oracle
    /// concern does not apply.
    async fn resolve_account(&self, name: &str) -> Result<Option<Uuid>, ToolError>;

    /// Resolve a public `client_id` to its internal id, INCLUDING a disabled
    /// client — same reasoning as [`Self::resolve_account`].
    async fn resolve_client(&self, client_id: &str) -> Result<Option<Uuid>, ToolError>;

    /// Sessions matching any combination of filters. All-`None` lists every
    /// session, which is what an unfiltered operator listing wants.
    async fn list_families(
        &self,
        account_id: Option<Uuid>,
        client_id: Option<Uuid>,
        family_id: Option<Uuid>,
    ) -> Result<Vec<TokenFamily>, ToolError>;

    /// The family a presented refresh token belongs to, whatever its state.
    async fn family_of_refresh_token(
        &self,
        token_hash: &SecretHash,
    ) -> Result<Option<TokenFamily>, ToolError>;

    /// Revoke every live token in a family. Returns rows affected — zero for an
    /// already-revoked family, which the caller reads as idempotent success.
    async fn revoke_family(&self, family_id: Uuid) -> Result<u64, ToolError>;

    /// Revoke every live token issued to a client.
    async fn revoke_client_tokens(&self, client_id: Uuid) -> Result<u64, ToolError>;

    /// Revoke an account's consent to a client AND that pair's tokens, in one
    /// transaction. Returns `(consents, tokens)`.
    async fn revoke_consent_and_tokens(
        &self,
        account_id: Uuid,
        client_id: Uuid,
    ) -> Result<(u64, u64), ToolError>;

    /// Revoke every consent and every token an account holds, in one
    /// transaction. Returns `(consents, tokens)`.
    async fn revoke_account_everything(&self, account_id: Uuid) -> Result<(u64, u64), ToolError>;

    /// Whether a request bearing this account, client and session may dispatch
    /// right now.
    ///
    /// `family_id` is MANDATORY here, deliberately. The service layer
    /// ([`RevocationService::dispatch_state`]) is what turns an absent session
    /// into [`DispatchState::SessionUnidentified`]; by the time a query is
    /// built there is no "no session" case left to express, so the SQL has no
    /// null-tolerant arm that a later edit could make permissive again.
    async fn dispatch_state(
        &self,
        account_id: Uuid,
        client_id: Uuid,
        family_id: Uuid,
    ) -> Result<DispatchState, ToolError>;
}

/// An RFC 7009 revocation request, already parsed out of the form body.
///
/// `token` is the only field that is a credential, and it does not survive past
/// [`RevocationService::revoke_presented_token`]'s first statement.
pub struct RevocationRequest {
    pub token: String,
    /// RFC 7009's `token_type_hint`. Accepted and ignored: the hint is advisory,
    /// this server keeps only refresh tokens in the store (access tokens are
    /// stateless JWTs), and honouring a wrong hint by skipping the lookup would
    /// turn a client's mistake into a failed revocation.
    pub token_type_hint: Option<String>,
    /// The `client_id` the caller authenticated as, when it did.
    pub client_id: Option<String>,
    /// Resolved source address, for the audit record. Typed, and resolved by
    /// [`crate::oauth::edge::resolve_client_ip`] before it gets here — this
    /// module never parses one, for the reason
    /// [`crate::oauth::audit::OauthAuditRecord::from_address`] documents.
    pub source: Option<std::net::IpAddr>,
}

/// An RFC 7009 response. The body is a fixed string, never a rendered error
/// carrying anything from the request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevocationResponse {
    pub status: u16,
    pub body: &'static str,
}

impl RevocationResponse {
    /// RFC 7009 §2.2: the server responds 200 for a successful revocation AND
    /// for a token it does not recognise. Returning 404 for an unknown token
    /// would make the endpoint a token-validity oracle for anyone who has
    /// harvested candidate values.
    fn ok() -> Self {
        Self { status: 200, body: "" }
    }

    fn invalid_request() -> Self {
        Self { status: 400, body: r#"{"error":"invalid_request"}"# }
    }
}

/// Revocation and session listing over a [`SessionStore`].
#[derive(Clone)]
pub struct RevocationService {
    store: Arc<dyn SessionStore>,
}

impl RevocationService {
    pub fn new(store: Arc<dyn SessionStore>) -> Self {
        Self { store }
    }

    /// List sessions matching a selector, or every session when given `None`.
    ///
    /// A selector naming an account or client that does not exist yields an
    /// EMPTY list rather than an error. Distinguishing the two would make the
    /// listing tool an existence oracle for account names, and "no sessions" is
    /// the operationally correct answer either way.
    pub async fn list(
        &self,
        selector: Option<&SessionSelector>,
        include_dead: bool,
    ) -> Result<Vec<SessionSummary>, ToolError> {
        let filters = match selector {
            Some(s) => self.resolve(s).await?,
            // No selector genuinely means "everything", which is a different
            // thing from a selector that failed to resolve.
            None => ResolvedSelector { resolved: true, ..Default::default() },
        };
        if !filters.resolved {
            // The selector named something that does not exist. Returning here
            // rather than querying with all-`None` filters is the guard that
            // keeps an unresolvable name from silently meaning "every session".
            return Ok(Vec::new());
        }
        let families = self
            .store
            .list_families(filters.account_id, filters.client_id, filters.family_id)
            .await?;
        Ok(families
            .iter()
            .filter(|f| include_dead || f.live)
            .map(SessionSummary::from)
            .collect())
    }

    /// Cut off everything the selector names, then PROVE it.
    ///
    /// A selector that matches nothing is success with zero counts, not an
    /// error: the caller asked for a state ("this has no live sessions") and
    /// that state holds. Erroring would also, again, be an existence oracle.
    pub async fn revoke(&self, selector: &SessionSelector) -> Result<RevocationReport, ToolError> {
        let filters = self.resolve(selector).await?;
        if !filters.resolved {
            // The selector named an account or client that does not exist.
            // Bailing out HERE, before any query, is what stops an unresolvable
            // name from degrading into all-`None` filters — which
            // `list_families` would answer with every session in the database,
            // and the revocation arms would then destroy. This is the single
            // most dangerous failure available in this module, so it is refused
            // at the top rather than relied upon to fall through the match.
            //
            // Audited even though nothing changed: a run of revocations against
            // names that do not exist is somebody probing for account names, and
            // a silent no-op is exactly what makes that invisible.
            OauthAuditRecord::new(OauthEvent::Revoked)
                .detail(AuditDetail::RevocationMatchedNothing { selector: selector.kind() })
                .emit();
            return Ok(RevocationReport {
                selector: selector.kind(),
                families_matched: 0,
                families_newly_revoked: 0,
                tokens_revoked: 0,
                consents_revoked: 0,
                verified: true,
            });
        }

        // Snapshot BEFORE the write, so "newly revoked" counts what this call
        // actually changed rather than what was already off.
        let before = self
            .store
            .list_families(filters.account_id, filters.client_id, filters.family_id)
            .await?;
        let live_before: Vec<Uuid> = before.iter().filter(|f| f.live).map(|f| f.family_id).collect();

        let (consents_revoked, tokens_revoked) = match (selector, &filters) {
            // The account+client case revokes CONSENT too, inseparably. An
            // operator disconnecting a connector means "this client no longer
            // has my permission", and leaving the consent standing would let the
            // next authorization sail through without a consent screen.
            (
                SessionSelector::AccountAndClient { .. },
                ResolvedSelector { account_id: Some(a), client_id: Some(c), .. },
            ) => self.store.revoke_consent_and_tokens(*a, *c).await?,
            (SessionSelector::Account(_), ResolvedSelector { account_id: Some(a), .. }) => {
                self.store.revoke_account_everything(*a).await?
            }
            (SessionSelector::Client(_), ResolvedSelector { client_id: Some(c), .. }) => {
                (0, self.store.revoke_client_tokens(*c).await?)
            }
            (SessionSelector::Family(id), _) => (0, self.store.revoke_family(*id).await?),
            // The selector named something that does not exist: nothing to
            // revoke, and nothing to report as an error.
            _ => (0, 0),
        };

        // Verify against the store rather than trusting the write. This is the
        // acceptance criterion "revocation is effective at the next dispatch,
        // verified against the store" — and it is checked on the same predicate
        // (`live`, computed by the database clock) that gates dispatch, not on a
        // separate notion of doneness that could drift from it.
        let after = self
            .store
            .list_families(filters.account_id, filters.client_id, filters.family_id)
            .await?;
        let still_live: Vec<Uuid> = after.iter().filter(|f| f.live).map(|f| f.family_id).collect();
        if !still_live.is_empty() {
            OauthAuditRecord::new(OauthEvent::Revoked)
                .reason(DenialReason::Revoked)
                .detail(AuditDetail::RevocationNotEffective {
                    still_live: still_live.len(),
                    matched: after.len(),
                })
                .emit();
            return Err(ToolError::Execution(format!(
                "revocation did not take effect: {} session(s) matching this selector are still \
                 live after the write. Nothing has been reported as revoked; investigate before \
                 relying on this control",
                still_live.len()
            )));
        }

        let newly = live_before.len();
        let record = OauthAuditRecord::new(OauthEvent::Revoked).detail(AuditDetail::Revocation {
            selector: selector.kind(),
            matched: before.len(),
            newly_revoked: newly,
            tokens_revoked,
            consents_revoked,
        });
        let record = match filters.account_id {
            Some(a) => record.account(a),
            None => record,
        };
        let record = match filters.client_id {
            Some(c) => record.client_uuid(c),
            None => record,
        };
        let record = match filters.family_id {
            Some(f) => record.family(f),
            None => record,
        };
        record.emit();

        Ok(RevocationReport {
            selector: selector.kind(),
            families_matched: before.len(),
            families_newly_revoked: newly,
            tokens_revoked,
            consents_revoked,
            verified: true,
        })
    }

    /// RFC 7009 `POST /oauth/revoke`.
    ///
    /// Transport-agnostic on purpose: RMCP-09 owns the edge router and mounts
    /// this, so the handler here takes an already-parsed request and returns a
    /// status and body. Keeping it out of an HTTP framework is also what lets
    /// its semantics — the 200-for-unknown-token rule, the client-mismatch rule —
    /// be tested directly.
    pub async fn revoke_presented_token(
        &self,
        request: RevocationRequest,
    ) -> Result<RevocationResponse, ToolError> {
        // First statement: the raw token becomes a digest and is never seen
        // again. Nothing below this line can log, return, or store it.
        let token = request.token.trim();
        if token.is_empty() {
            self.audit_revoke_endpoint(
                &request,
                None,
                Some(DenialReason::MalformedRequest),
                AuditDetail::RevocationRequestHadNoToken,
            );
            return Ok(RevocationResponse::invalid_request());
        }
        let hash = SecretHash::of(token);

        let family = self.store.family_of_refresh_token(&hash).await?;
        let family = match family {
            Some(f) => f,
            None => {
                // Unknown token: 200, nothing revoked. Audited so a burst of
                // them is visible as the probing it probably is.
                self.audit_revoke_endpoint(&request, None, None, AuditDetail::TokenNotRecognised);
                return Ok(RevocationResponse::ok());
            }
        };

        // A client may only revoke its OWN tokens. When the caller identified
        // itself and the token belongs elsewhere, the answer is still 200 — a
        // 403 would confirm that the token is valid for somebody — but nothing
        // is revoked, and the mismatch is audited because it is either a
        // misconfiguration or someone testing a harvested token.
        if let Some(presented) = request.client_id.as_deref() {
            let resolved = self.store.resolve_client(presented).await?;
            if resolved != Some(family.client_id) {
                self.audit_revoke_endpoint(
                    &request,
                    Some(family.family_id),
                    Some(DenialReason::UnknownOrDisabledClient),
                    AuditDetail::ForeignSessionRefused,
                );
                return Ok(RevocationResponse::ok());
            }
        }

        // Revoke through the same path as every other surface, verification and
        // all, so the endpoint cannot drift from the tool.
        let report = self.revoke(&SessionSelector::Family(family.family_id)).await?;
        self.audit_revoke_endpoint(
            &request,
            Some(family.family_id),
            None,
            AuditDetail::SessionRevokedByHolder { newly_revoked: report.families_newly_revoked },
        );
        Ok(RevocationResponse::ok())
    }

    /// The reuse-detection response RMCP-04 calls: a presented refresh token
    /// that had already been rotated means the legitimate holder and a thief
    /// cannot be told apart, so the whole family goes.
    ///
    /// Lives here rather than in RMCP-04 so that reuse revocation, operator
    /// revocation and RFC 7009 revocation are one code path with one audit
    /// vocabulary — three implementations of "kill this family" is three places
    /// for the verify-after-write step to be forgotten.
    pub async fn revoke_on_reuse(&self, family_id: Uuid) -> Result<RevocationReport, ToolError> {
        OauthAuditRecord::new(OauthEvent::RefreshReuseDetected)
            .family(family_id)
            .reason(DenialReason::RefreshReused)
            .detail(AuditDetail::RefreshReuse)
            .emit();
        self.revoke_family_verified(family_id).await
    }

    /// Revoke one family, VERIFIED, with no opinion about why.
    ///
    /// The primitive behind [`Self::revoke_on_reuse`], split out for the token
    /// endpoint's other family revocations — a disabled account, a token
    /// presented by the wrong client, a rotation that lost its race. Those are
    /// not all theft signals, so they must not all emit
    /// [`OauthEvent::RefreshReuseDetected`]; but they must all be verified, so
    /// they share this.
    ///
    /// Round 13 (`gpt56`) found `crate::oauth::token` calling
    /// [`SessionStore::revoke_family`] on the store directly, which meant the
    /// crate had TWO ways to revoke a family and the unverified one was on the
    /// reuse path — the moment a credential has probably been stolen, and
    /// therefore the moment a revocation that reports success without
    /// confirming the store agrees is least affordable. That is the same
    /// argument round 2 settled when reuse detection was made to emit its audit
    /// record BEFORE the write: the failure case is the one an operator most
    /// needs to be true.
    pub async fn revoke_family_verified(
        &self,
        family_id: Uuid,
    ) -> Result<RevocationReport, ToolError> {
        self.revoke(&SessionSelector::Family(family_id)).await
    }

    /// The per-FAMILY dispatch check — **not currently on the dispatch path**.
    ///
    /// RMCP-05 wired enforcement through its own `TokenState` seam instead, and
    /// had to: this function needs a `family_id`, and an access token carries no
    /// session claim for a resource server to supply one from. So the live check
    /// asks the coarser question it can answer — whether ANY session is live for
    /// an `(account, client)` pair — and rejects with `AllSessionsRevoked`.
    ///
    /// This is the implementation that replaces it once **TERM #635** puts a
    /// session claim in the token, at which point revoking one session among
    /// several starts denying that session specifically. Until then it is
    /// deliberately not called from dispatch: two live checkers for one decision
    /// is the dual-writer hazard this subsystem has already been bitten by, and
    /// the coarser check is the one that is wired.
    ///
    /// It remains exercised by this module's tests, which is what keeps it
    /// honest in the meantime, and it is what the revocation tools verify
    /// against.
    pub async fn dispatch_state(
        &self,
        account_id: Uuid,
        client_id: Uuid,
        family_id: Option<Uuid>,
    ) -> Result<DispatchState, ToolError> {
        // Absence is DENIED, and it is decided here — before the store is
        // consulted at all — so there is no query whose null handling could
        // later drift back toward permissive. Round 2 (`gpt56`) found the
        // previous shape treating `None` as "no session constraint" and falling
        // through to allowed, which meant revoking a family did not stop an
        // access token that arrived without one: a revoked authority surviving
        // on the read path.
        let Some(family_id) = family_id else {
            let state = DispatchState::SessionUnidentified;
            self.audit_dispatch_denial(account_id, client_id, None, state);
            return Ok(state);
        };

        let state = self.store.dispatch_state(account_id, client_id, family_id).await?;
        if !state.is_allowed() {
            self.audit_dispatch_denial(account_id, client_id, Some(family_id), state);
        }
        Ok(state)
    }

    fn audit_dispatch_denial(
        &self,
        account_id: Uuid,
        client_id: Uuid,
        family_id: Option<Uuid>,
        state: DispatchState,
    ) {
        let record = OauthAuditRecord::new(OauthEvent::TokenDenied)
            .account(account_id)
            .client_uuid(client_id)
            .reason(DenialReason::Revoked)
            .detail(AuditDetail::DispatchDenied { state });
        match family_id {
            Some(f) => record.family(f).emit(),
            None => record.emit(),
        };
    }

    fn audit_revoke_endpoint(
        &self,
        request: &RevocationRequest,
        family_id: Option<Uuid>,
        reason: Option<DenialReason>,
        detail: AuditDetail,
    ) {
        let mut record = OauthAuditRecord::new(OauthEvent::Revoked)
            .endpoint(OauthEndpoint::Revoke)
            .detail(detail);
        if let Some(client) = request.client_id.as_deref() {
            record = record.client(client);
        }
        if let Some(source) = request.source {
            record = record.from_address(source);
        }
        if let Some(family) = family_id {
            record = record.family(family);
        }
        if let Some(reason) = reason {
            record = record.reason(reason);
        }
        record.emit();
    }

    /// Turn a selector's human-facing names into ids. An unresolvable name
    /// leaves its filter `None` and sets [`ResolvedSelector::resolved`] false, so
    /// callers can distinguish "matched nothing" from "everything" — which
    /// matters enormously, because a selector that silently degraded to
    /// "no filters" would revoke the entire fleet's sessions.
    async fn resolve(&self, selector: &SessionSelector) -> Result<ResolvedSelector, ToolError> {
        Ok(match selector {
            SessionSelector::Account(name) => {
                let account_id = self.store.resolve_account(name).await?;
                ResolvedSelector { account_id, resolved: account_id.is_some(), ..Default::default() }
            }
            SessionSelector::Client(client) => {
                let client_id = self.store.resolve_client(client).await?;
                ResolvedSelector { client_id, resolved: client_id.is_some(), ..Default::default() }
            }
            SessionSelector::AccountAndClient { account, client } => {
                let account_id = self.store.resolve_account(account).await?;
                let client_id = self.store.resolve_client(client).await?;
                ResolvedSelector {
                    account_id,
                    client_id,
                    resolved: account_id.is_some() && client_id.is_some(),
                    ..Default::default()
                }
            }
            SessionSelector::Family(id) => ResolvedSelector {
                family_id: Some(*id),
                resolved: true,
                ..Default::default()
            },
        })
    }
}

/// A selector with its names resolved to ids.
///
/// `resolved == false` with all-`None` filters is the dangerous state this type
/// exists to name: it must NEVER be handed to `list_families` as "no filters",
/// which would match every session in the database. Both call sites in
/// [`RevocationService`] therefore branch on the concrete id being `Some`, and
/// the unresolved case falls into an arm that touches nothing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ResolvedSelector {
    account_id: Option<Uuid>,
    client_id: Option<Uuid>,
    family_id: Option<Uuid>,
    resolved: bool,
}

#[cfg(test)]
pub mod fake {
    //! An in-memory [`SessionStore`] with the same fail-closed semantics as the
    //! Postgres one, so this item's contracts — idempotence, verify-after-write,
    //! the RFC 7009 rules — are covered without a database. The SQL itself is
    //! covered by RMCP-14's end-to-end run.

    use super::*;
    use chrono::{Duration, Utc};
    use std::collections::HashMap;
    use std::sync::Mutex;

    pub struct FakeSessionStore {
        pub accounts: HashMap<String, Uuid>,
        pub clients: HashMap<String, Uuid>,
        pub families: Mutex<Vec<TokenFamily>>,
        pub consents: Mutex<Vec<(Uuid, Uuid)>>,
        pub disabled_clients: Vec<Uuid>,
        pub disabled_accounts: Vec<Uuid>,
        /// When set, `revoke_family` reports success and changes nothing.
        pub revocation_silently_fails: bool,
        /// Refresh-token digest -> family id.
        pub tokens: HashMap<Vec<u8>, Uuid>,
    }

    impl FakeSessionStore {
        pub fn new() -> Self {
            Self {
                accounts: HashMap::new(),
                clients: HashMap::new(),
                families: Mutex::new(Vec::new()),
                consents: Mutex::new(Vec::new()),
                disabled_clients: Vec::new(),
                disabled_accounts: Vec::new(),
                revocation_silently_fails: false,
                tokens: HashMap::new(),
            }
        }

        pub fn with_account(mut self, name: &str, id: Uuid) -> Self {
            self.accounts.insert(name.to_string(), id);
            self
        }

        pub fn with_client(mut self, client_id: &str, id: Uuid) -> Self {
            self.clients.insert(client_id.to_string(), id);
            self
        }

        pub fn with_session(self, family_id: Uuid, account_id: Uuid, client_id: Uuid) -> Self {
            let now = Utc::now();
            self.families.lock().unwrap().push(TokenFamily {
                family_id,
                client_id,
                account_id,
                resource: "https://connector.example.test/mcp".into(),
                scope: "mcp".into(),
                issued_at: now,
                last_issued_at: now,
                expires_at: now + Duration::days(30),
                token_count: 1,
                revoked_at: None,
                live: true,
            });
            self.consents.lock().unwrap().push((account_id, client_id));
            self
        }

        pub fn with_refresh_token(mut self, plaintext: &str, family_id: Uuid) -> Self {
            self.tokens.insert(SecretHash::of(plaintext).as_bytes().to_vec(), family_id);
            self
        }

        /// Make revocation report success while changing nothing.
        pub fn with_revocation_that_does_not_take_effect(mut self) -> Self {
            self.revocation_silently_fails = true;
            self
        }

        pub fn with_disabled_client(mut self, id: Uuid) -> Self {
            self.disabled_clients.push(id);
            self
        }

        pub fn with_disabled_account(mut self, id: Uuid) -> Self {
            self.disabled_accounts.push(id);
            self
        }

        /// Mark a family dead exactly as the store's family-wide rule would.
        fn kill(f: &mut TokenFamily) -> u64 {
            if f.revoked_at.is_some() {
                return 0;
            }
            f.revoked_at = Some(Utc::now());
            f.live = false;
            f.token_count as u64
        }

        pub fn is_live(&self, family_id: Uuid) -> bool {
            self.families
                .lock()
                .unwrap()
                .iter()
                .any(|f| f.family_id == family_id && f.live)
        }
    }

    #[async_trait]
    impl SessionStore for FakeSessionStore {
        async fn resolve_account(&self, name: &str) -> Result<Option<Uuid>, ToolError> {
            Ok(self.accounts.get(name).copied())
        }

        async fn resolve_client(&self, client_id: &str) -> Result<Option<Uuid>, ToolError> {
            Ok(self.clients.get(client_id).copied())
        }

        async fn list_families(
            &self,
            account_id: Option<Uuid>,
            client_id: Option<Uuid>,
            family_id: Option<Uuid>,
        ) -> Result<Vec<TokenFamily>, ToolError> {
            Ok(self
                .families
                .lock()
                .unwrap()
                .iter()
                .filter(|f| account_id.is_none_or(|a| f.account_id == a))
                .filter(|f| client_id.is_none_or(|c| f.client_id == c))
                .filter(|f| family_id.is_none_or(|id| f.family_id == id))
                .cloned()
                .collect())
        }

        async fn family_of_refresh_token(
            &self,
            token_hash: &SecretHash,
        ) -> Result<Option<TokenFamily>, ToolError> {
            let Some(family_id) = self.tokens.get(token_hash.as_bytes()).copied() else {
                return Ok(None);
            };
            Ok(self
                .families
                .lock()
                .unwrap()
                .iter()
                .find(|f| f.family_id == family_id)
                .cloned())
        }

        async fn revoke_family(&self, family_id: Uuid) -> Result<u64, ToolError> {
            if self.revocation_silently_fails {
                // Reports rows affected while changing nothing — the exact
                // shape the verify-after-write step exists to catch, and the
                // shape a direct store call would have reported as success.
                return Ok(1);
            }
            let mut families = self.families.lock().unwrap();
            Ok(families
                .iter_mut()
                .filter(|f| f.family_id == family_id)
                .map(Self::kill)
                .sum())
        }

        async fn revoke_client_tokens(&self, client_id: Uuid) -> Result<u64, ToolError> {
            let mut families = self.families.lock().unwrap();
            Ok(families
                .iter_mut()
                .filter(|f| f.client_id == client_id)
                .map(Self::kill)
                .sum())
        }

        async fn revoke_consent_and_tokens(
            &self,
            account_id: Uuid,
            client_id: Uuid,
        ) -> Result<(u64, u64), ToolError> {
            let mut consents = self.consents.lock().unwrap();
            let before = consents.len();
            consents.retain(|(a, c)| !(*a == account_id && *c == client_id));
            let removed = (before - consents.len()) as u64;
            let mut families = self.families.lock().unwrap();
            let tokens = families
                .iter_mut()
                .filter(|f| f.account_id == account_id && f.client_id == client_id)
                .map(Self::kill)
                .sum();
            Ok((removed, tokens))
        }

        async fn revoke_account_everything(
            &self,
            account_id: Uuid,
        ) -> Result<(u64, u64), ToolError> {
            let mut consents = self.consents.lock().unwrap();
            let before = consents.len();
            consents.retain(|(a, _)| *a != account_id);
            let removed = (before - consents.len()) as u64;
            let mut families = self.families.lock().unwrap();
            let tokens = families
                .iter_mut()
                .filter(|f| f.account_id == account_id)
                .map(Self::kill)
                .sum();
            Ok((removed, tokens))
        }

        async fn dispatch_state(
            &self,
            account_id: Uuid,
            client_id: Uuid,
            family_id: Uuid,
        ) -> Result<DispatchState, ToolError> {
            if self.disabled_clients.contains(&client_id) {
                return Ok(DispatchState::ClientDisabled);
            }
            if self.disabled_accounts.contains(&account_id) {
                return Ok(DispatchState::AccountDisabled);
            }
            if !self
                .consents
                .lock()
                .unwrap()
                .iter()
                .any(|(a, c)| *a == account_id && *c == client_id)
            {
                return Ok(DispatchState::ConsentRevoked);
            }
            // No `if let Some(..)` to fall past: the session is mandatory here,
            // so the only way to reach `Allowed` is a family that exists, is
            // bound to this account and client, and carries no revocation.
            let families = self.families.lock().unwrap();
            let bound = families.iter().find(|f| {
                f.family_id == family_id && f.account_id == account_id && f.client_id == client_id
            });
            match bound {
                Some(f) if f.revoked_at.is_none() => {}
                _ => return Ok(DispatchState::SessionRevoked),
            }
            Ok(DispatchState::Allowed)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeSessionStore;
    use super::*;

    fn ids() -> (Uuid, Uuid, Uuid) {
        (
            Uuid::parse_str("11111111-1111-4111-8111-111111111111").unwrap(),
            Uuid::parse_str("22222222-2222-4222-8222-222222222222").unwrap(),
            Uuid::parse_str("33333333-3333-4333-8333-333333333333").unwrap(),
        )
    }

    /// A high-entropy value standing in for a refresh token. Named so it is
    /// self-evidently a test fixture rather than anything presentable, and it is
    /// never a real credential: it is a fixed literal in a unit test.
    const NOT_A_REAL_REFRESH_TOKEN: &str = "fixture-refresh-value-for-unit-tests-only";

    fn service(store: FakeSessionStore) -> (RevocationService, Arc<FakeSessionStore>) {
        let store = Arc::new(store);
        (RevocationService::new(store.clone()), store)
    }

    fn populated() -> (RevocationService, Arc<FakeSessionStore>, (Uuid, Uuid, Uuid)) {
        let (account, client, family) = ids();
        let (svc, store) = service(
            FakeSessionStore::new()
                .with_account("operator", account)
                .with_client("a-connector", client)
                .with_session(family, account, client)
                .with_refresh_token(NOT_A_REAL_REFRESH_TOKEN, family),
        );
        (svc, store, (account, client, family))
    }

    /// The headline behaviour: revoking a consent kills the session, and the
    /// NEXT dispatch is denied — not the next expiry. Checked through
    /// `dispatch_state`, which is what the resource server calls, so this
    /// asserts the thing that actually gates a request.
    #[tokio::test]
    async fn revoking_a_consent_denies_the_very_next_dispatch() {
        let (svc, store, (account, client, family)) = populated();
        assert_eq!(
            svc.dispatch_state(account, client, Some(family)).await.unwrap(),
            DispatchState::Allowed
        );

        let report = svc
            .revoke(&SessionSelector::AccountAndClient {
                account: "operator".into(),
                client: "a-connector".into(),
            })
            .await
            .expect("revocation succeeds");
        assert_eq!(report.families_newly_revoked, 1);
        assert_eq!(report.consents_revoked, 1, "consent and tokens go together or not at all");
        assert!(report.verified);

        assert!(!store.is_live(family));
        // The refresh token is dead AND the access-token path is denied, which
        // is the distinction the acceptance criterion is about.
        assert_eq!(
            svc.dispatch_state(account, client, Some(family)).await.unwrap(),
            DispatchState::ConsentRevoked
        );
        assert_eq!(
            svc.dispatch_state(account, client, None).await.unwrap(),
            DispatchState::SessionUnidentified,
            "a token that names no session must be denied too"
        );
    }

    /// The hole review round 2 found, pinned so it cannot come back: revoke ONE
    /// FAMILY (leaving consent, client and account entirely healthy), then
    /// present a token that carries no session id. It must be REFUSED.
    ///
    /// The previous shape read a `None` family id as "no session constraint to
    /// check" and fell through to `Allowed`, so a family revocation simply did
    /// not reach a token whose dispatch path did not thread the id — the
    /// revoked authority survived on the read path. This test fails if that arm
    /// is ever restored, because the surrounding state is deliberately all
    /// green: consent is live, the client is enabled, the account is enabled,
    /// and the ONLY thing denying is the absent session.
    #[tokio::test]
    async fn a_family_less_token_is_refused_after_its_family_is_revoked() {
        let (svc, store, (account, client, family)) = populated();

        // Baseline: with its session named, the token dispatches.
        assert_eq!(
            svc.dispatch_state(account, client, Some(family)).await.unwrap(),
            DispatchState::Allowed
        );

        svc.revoke(&SessionSelector::Family(family)).await.expect("family revoked");
        assert!(!store.is_live(family));

        // Naming the session: denied, as it always was.
        assert_eq!(
            svc.dispatch_state(account, client, Some(family)).await.unwrap(),
            DispatchState::SessionRevoked
        );

        // Not naming it: ALSO denied. Consent is untouched and live — revoking
        // a family does not revoke consent — so if absence were still read as
        // family-valid, every check above this line would pass and this one
        // would return `Allowed`.
        let state = svc.dispatch_state(account, client, None).await.unwrap();
        assert!(
            !state.is_allowed(),
            "a family-less token dispatched after its family was revoked: {state:?}"
        );
        assert_eq!(state, DispatchState::SessionUnidentified);
    }

    /// Absence denies even when nothing at all is wrong. The complementary half
    /// of the test above: it isolates the `None` arm from any other denial, so
    /// a regression cannot hide behind a revoked consent or a disabled client.
    #[tokio::test]
    async fn an_unidentified_session_is_denied_even_when_everything_else_is_healthy() {
        let (svc, store, (account, client, family)) = populated();
        assert!(store.is_live(family), "the session is live and consent is intact");
        assert_eq!(
            svc.dispatch_state(account, client, Some(family)).await.unwrap(),
            DispatchState::Allowed,
            "precondition: this binding is otherwise fully permitted"
        );
        assert_eq!(
            svc.dispatch_state(account, client, None).await.unwrap(),
            DispatchState::SessionUnidentified,
            "an absent session id must never read as permission"
        );
    }

    /// A reuse-triggered revocation is VERIFIED: if the store reports success
    /// while the family stays live, the call fails rather than reporting a
    /// clean cut-off.
    ///
    /// This is the property round 13 found missing on the path that needs it
    /// most. `crate::oauth::token` used to call the store's
    /// `revoke_refresh_family` directly, which reports rows affected and
    /// nothing else — so a revocation that did not take effect looked exactly
    /// like one that did, at the moment a credential has probably been stolen.
    #[tokio::test]
    async fn a_reuse_revocation_is_verified_against_the_store() {
        let (account, client, family) = ids();
        let store = Arc::new(
            FakeSessionStore::new()
                .with_account("operator", account)
                .with_client("a-connector", client)
                .with_session(family, account, client)
                .with_revocation_that_does_not_take_effect(),
        );
        let svc = RevocationService::new(store.clone());

        let err = svc
            .revoke_on_reuse(family)
            .await
            .expect_err("a revocation that did not take effect must not report success");
        assert!(
            err.to_string().contains("did not take effect"),
            "the error must say what actually happened: {err}"
        );
        assert!(store.is_live(family), "precondition: the fake really did not revoke");

        // And the same holds for the non-theft entry point, so the two cannot
        // diverge on whether they check their work.
        assert!(svc.revoke_family_verified(family).await.is_err());
    }

    /// A working reuse revocation still succeeds and reports what it did — the
    /// verification must not make the ordinary path fail.
    #[tokio::test]
    async fn a_reuse_revocation_that_lands_reports_it() {
        let (svc, store, (_a, _c, family)) = populated();
        let report = svc.revoke_on_reuse(family).await.expect("revocation lands");
        assert_eq!(report.families_newly_revoked, 1);
        assert!(report.verified);
        assert!(!store.is_live(family));
    }

    /// The store's raw family revocation has exactly ONE caller: the trait impl
    /// that backs the verified service.
    ///
    /// The half that cannot be observed by exercising the good path. Round 13's
    /// defect was a second call site, not a broken first one, and the same shape
    /// has now been caught three times in this item — the 429 constructors, the
    /// `AddressCleared` construction sites, and this. A source scan is what
    /// makes "no second path exists" checkable.
    #[test]
    fn the_raw_family_revocation_has_exactly_one_caller() {
        use std::path::Path;

        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut callers = Vec::new();
        let mut stack = vec![src.clone()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("readable dir") {
                let path = entry.expect("entry").path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                    continue;
                }
                let rel = path.strip_prefix(&src).unwrap_or(&path).display().to_string();
                let text = std::fs::read_to_string(&path).expect("readable");
                // Production code only: a test may name the method freely.
                let production = text.split("\n#[cfg(test)]").next().unwrap_or("");
                for (n, line) in production.lines().enumerate() {
                    if !line.contains("revoke_refresh_family") {
                        continue;
                    }
                    let t = line.trim_start();
                    // The definition itself, and doc comments referring to it.
                    if t.starts_with("pub async fn") || t.starts_with("//") {
                        continue;
                    }
                    callers.push(format!("{rel}:{}", n + 1));
                }
            }
        }
        assert_eq!(
            callers.len(),
            1,
            "`revoke_refresh_family` must be reached only through the verified service; extra \
             call sites bypass the verify-after-write: {callers:?}"
        );
        assert!(
            callers[0].starts_with("oauth/store.rs"),
            "the one caller should be the `SessionStore` impl: {:?}",
            callers[0]
        );
    }

    /// Idempotence: the second revocation is success, and says plainly that it
    /// changed nothing.
    #[tokio::test]
    async fn revoking_an_already_revoked_family_is_idempotent_success() {
        let (svc, _store, (_a, _c, family)) = populated();
        let first = svc.revoke(&SessionSelector::Family(family)).await.expect("first");
        assert_eq!(first.families_newly_revoked, 1);

        let second = svc.revoke(&SessionSelector::Family(family)).await.expect("second must succeed");
        assert_eq!(second.families_matched, 1);
        assert_eq!(second.families_newly_revoked, 0, "nothing left to change");
        assert_eq!(second.tokens_revoked, 0);
        assert!(second.verified);
    }

    /// A selector naming something that does not exist must not degrade into
    /// "no filters" and revoke everything. This is the single most dangerous
    /// failure available in this module.
    #[tokio::test]
    async fn an_unresolvable_selector_revokes_nothing_rather_than_everything() {
        let (svc, store, (_a, _c, family)) = populated();
        let report = svc
            .revoke(&SessionSelector::Account("no-such-account".into()))
            .await
            .expect("an unknown account is not an error");
        assert_eq!(report.families_matched, 0);
        assert_eq!(report.families_newly_revoked, 0);
        assert!(store.is_live(family), "an unresolvable selector revoked a live session");

        // Listing behaves the same way, and does not disclose non-existence.
        let listed = svc
            .list(Some(&SessionSelector::Account("no-such-account".into())), true)
            .await
            .expect("listing an unknown account is not an error");
        assert!(listed.is_empty());
    }

    /// RFC 7009 §2.2: an unrecognised token gets 200, not 404 — otherwise the
    /// endpoint tells anyone with a list of candidate values which ones are real.
    #[tokio::test]
    async fn an_unknown_token_is_answered_200_and_revokes_nothing() {
        let (svc, store, (_a, _c, family)) = populated();
        let response = svc
            .revoke_presented_token(RevocationRequest {
                token: "<REDACTED-SECRET>".into(),
                token_type_hint: Some("refresh_token".into()),
                client_id: None,
                source: Some("192.0.2.20".parse().expect("documentation-range literal")),
            })
            .await
            .expect("no error");
        assert_eq!(response, RevocationResponse::ok());
        assert!(store.is_live(family));
    }

    /// A presented token DOES revoke its own family, and the response is
    /// indistinguishable from the unknown-token case.
    #[tokio::test]
    async fn a_presented_token_revokes_its_own_family() {
        let (svc, store, (_a, _c, family)) = populated();
        let response = svc
            .revoke_presented_token(RevocationRequest {
                token: NOT_A_REAL_REFRESH_TOKEN.into(),
                token_type_hint: None,
                client_id: Some("a-connector".into()),
                source: None,
            })
            .await
            .expect("no error");
        assert_eq!(response, RevocationResponse::ok());
        assert!(!store.is_live(family));
    }

    /// One client must not be able to revoke another's session — and learns
    /// nothing from trying, because the response is the same 200 either way.
    #[tokio::test]
    async fn a_client_cannot_revoke_a_session_it_does_not_own() {
        let (account, client, family) = ids();
        let other_client = Uuid::parse_str("44444444-4444-4444-8444-444444444444").unwrap();
        let (svc, store) = service(
            FakeSessionStore::new()
                .with_account("operator", account)
                .with_client("a-connector", client)
                .with_client("another-connector", other_client)
                .with_session(family, account, client)
                .with_refresh_token(NOT_A_REAL_REFRESH_TOKEN, family),
        );

        let response = svc
            .revoke_presented_token(RevocationRequest {
                token: NOT_A_REAL_REFRESH_TOKEN.into(),
                token_type_hint: None,
                client_id: Some("another-connector".into()),
                source: None,
            })
            .await
            .expect("no error");
        assert_eq!(response, RevocationResponse::ok(), "the answer must not differ");
        assert!(store.is_live(family), "a foreign client revoked somebody else's session");
    }

    /// A missing token is a malformed request, which RFC 7009 does allow an
    /// error for — and it is the one case that is NOT a 200, because it reveals
    /// nothing about any token.
    #[tokio::test]
    async fn a_missing_token_is_a_400() {
        let (svc, _store, _) = populated();
        let response = svc
            .revoke_presented_token(RevocationRequest {
                token: "   ".into(),
                token_type_hint: None,
                client_id: None,
                source: None,
            })
            .await
            .expect("no error");
        assert_eq!(response.status, 400);
    }

    /// Disabling a client is a revocation lever in its own right, and it must
    /// bite at dispatch even though the session itself was never revoked.
    #[tokio::test]
    async fn a_disabled_client_is_denied_at_dispatch_with_its_session_intact() {
        let (account, client, family) = ids();
        let (svc, _store) = service(
            FakeSessionStore::new()
                .with_account("operator", account)
                .with_client("a-connector", client)
                .with_session(family, account, client)
                .with_disabled_client(client),
        );
        assert_eq!(
            svc.dispatch_state(account, client, Some(family)).await.unwrap(),
            DispatchState::ClientDisabled
        );
    }

    /// A token quoting a session that is not bound to its own account and client
    /// fails closed, rather than being waved through because the family exists.
    #[tokio::test]
    async fn a_session_id_from_another_binding_is_refused() {
        let (account, client, family) = ids();
        let stranger = Uuid::parse_str("55555555-5555-4555-8555-555555555555").unwrap();
        let (svc, _store) = service(
            FakeSessionStore::new()
                .with_account("operator", account)
                .with_client("a-connector", client)
                .with_session(family, account, client),
        );
        assert_eq!(
            svc.dispatch_state(stranger, client, Some(family)).await.unwrap(),
            DispatchState::ConsentRevoked,
            "an account with no consent to this client is denied before the session is consulted"
        );
    }

    /// Revoking by account cuts every client off at once — the "somebody has my
    /// laptop" control.
    #[tokio::test]
    async fn revoking_an_account_cuts_off_every_client() {
        let (account, client_a, family_a) = ids();
        let client_b = Uuid::parse_str("66666666-6666-4666-8666-666666666666").unwrap();
        let family_b = Uuid::parse_str("77777777-7777-4777-8777-777777777777").unwrap();
        let (svc, store) = service(
            FakeSessionStore::new()
                .with_account("operator", account)
                .with_client("connector-one", client_a)
                .with_client("connector-two", client_b)
                .with_session(family_a, account, client_a)
                .with_session(family_b, account, client_b),
        );
        let report = svc
            .revoke(&SessionSelector::Account("operator".into()))
            .await
            .expect("revocation succeeds");
        assert_eq!(report.families_matched, 2);
        assert_eq!(report.families_newly_revoked, 2);
        assert!(!store.is_live(family_a));
        assert!(!store.is_live(family_b));
    }

    /// A session listing must never carry token material — it is rendered in a
    /// GUI and pasted into chats. Asserted on the serialized form, which is what
    /// actually leaves the process.
    #[tokio::test]
    async fn a_session_listing_carries_no_token_material() {
        let (svc, _store, (_a, _c, family)) = populated();
        let listed = svc.list(None, true).await.expect("listing");
        let json = serde_json::to_string(&listed).expect("serializable");
        assert!(json.contains(&family.to_string()), "the family id is the handle: {json}");
        assert!(!json.contains(NOT_A_REAL_REFRESH_TOKEN), "token material in a listing: {json}");
        let digest_hex: String = SecretHash::of(NOT_A_REAL_REFRESH_TOKEN)
            .as_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert!(!json.contains(&digest_hex), "even a digest must not appear: {json}");
    }

    /// `include_dead=false` is the default operator view: a revoked session is
    /// noise once it is off.
    #[tokio::test]
    async fn a_listing_can_exclude_dead_sessions() {
        let (svc, _store, (_a, _c, family)) = populated();
        svc.revoke(&SessionSelector::Family(family)).await.expect("revoked");
        assert!(svc.list(None, false).await.unwrap().is_empty());
        assert_eq!(svc.list(None, true).await.unwrap().len(), 1);
    }
}
