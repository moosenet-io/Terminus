//! RMCP-11 — the OAuth door's audit trail.
//!
//! ## Why this is not just `gateway_framework::audit`
//!
//! [`crate::gateway_framework::audit`] records the shape every DISPATCHED
//! request shares: who called, which tool, allowed or denied. That is the right
//! record for a request that reached the gate. It is the wrong record for the
//! events this module exists for, because the interesting OAuth events happen
//! BEFORE there is a principal to attribute them to — an authorization denied
//! for an unknown client, a login refused, a refresh token replayed by a thief.
//! Those have no tool, no `ActionKind`, and often no identity at all.
//!
//! So this is a sibling record with its own vocabulary — and, unlike the
//! gateway's, one that needs no redaction helper at all, because it accepts no
//! free text to redact. See below for how it got there.
//!
//! ## No raw credential can reach a record, because there is nowhere to put one
//!
//! The item's acceptance criterion is that no audit record contains a raw
//! token, code, or secret, asserted by SCANNING emitted records rather than by
//! reading the code. **This record type accepts no free-form text at all**, and
//! that is what makes the assertion hold for a reason rather than by luck.
//!
//! Two review rounds got this module here, and the history is worth keeping
//! because both failures pointed the same way — see "Why there is no redaction
//! pass any more" below. The failure mode they share is silent and permanent, in
//! the one artifact nobody re-reads until an incident.
//!
//! Every field is constrained at the type level:
//!
//! 1. **Structured facts are typed.** UUIDs (account, client, token family), an
//!    [`OauthEndpoint`], an [`OauthEvent`], and a closed [`DenialReason`]. A
//!    caller cannot pass a token where a `Uuid` is expected. The family id is
//!    what identifies a session in the log — never the token hash, following
//!    [`crate::oauth::model`]'s rule that a digest is still live authentication
//!    material.
//! 2. **The narrative is a closed enum too.** [`AuditDetail`] replaces the prose
//!    field: each variant is a fixed sentence template plus integers and other
//!    enums, rendered by [`AuditDetail::render`]. There is no variant that
//!    carries a `String`, so no call site — including one added later by someone
//!    who never read this module — can put caller data into it.
//! 3. **The two genuinely variable values are CHARACTERISED, not filtered.**
//!    The presented `client_id` and the resolved source address are the only
//!    real runtime strings this module sees, and they get opposite treatment
//!    according to whether their content can be PROVEN safe:
//!    - A source address is never parsed here at all. It arrives as a typed,
//!      already-normalized [`IpAddr`] from
//!      [`crate::oauth::edge::resolve_client_ip`], the trusted-proxy logic that
//!      legitimately has one, and is recorded in full. Round 4 removed the
//!      parse-a-string-and-trust-it version: parseability is not proof about
//!      caller-controlled input.
//!    - A `client_id` is opaque by definition and no parser can prove anything
//!      about it, so the VALUE is never recorded. A client that resolved is
//!      identified by `client_uuid`; one that did not contributes only a
//!      [`ValueShape`].
//!
//! ### Why there is no redaction pass any more
//!
//! There were two earlier attempts at this field, and both failed in the same
//! direction. Round 1 rejected a prose `detail` string defended by redaction:
//! private fields with sanitizing setters constrain the ROUTE IN, not the
//! CONTENT. Round 3 then found the seam in what replaced it — a charset
//! allowlist that accepted short alphanumeric strings, paired with an opaque-run
//! redactor that only fired at 24+ characters, so an 8- or 12-character
//! authorization code or OTP passed BOTH layers and was logged verbatim.
//!
//! The tempting fix — lower the run threshold — only moves the seam down and
//! starts eating legitimate short identifiers. The durable fix is the one
//! applied to `detail` in round 1 and to these two fields now: stop letting a
//! free-form runtime string reach the record, and there is no threshold left to
//! sit on the wrong side of.
//!
//! So the S6 `sanitize` helper and the opaque-run pass are **gone from this
//! module**, not kept as a backstop. A filter left lying around is a filter
//! someone routes a string through while believing they are safe, which is
//! precisely the belief that produced both of the above. The gateway's own
//! [`crate::gateway_framework::audit::sanitize`] is still the right tool for
//! the gateway's free-text detail strings; this module simply has none.
//!
//! ## The ring buffer
//!
//! Every record is emitted twice: as a structured `tracing` event (the durable
//! trail an operator greps) and into a small in-process ring. The ring is what
//! makes "scan the emitted records" a test that can actually run, and it is
//! also genuinely useful in production — the last few hundred auth events are
//! what an operator wants when a connector stops working. It holds only
//! already-sanitized records, so it is not a new place secrets accumulate.

use std::collections::VecDeque;
use std::net::IpAddr;
use std::sync::{Mutex, OnceLock};

use chrono::{DateTime, Utc};
use serde::Serialize;
use uuid::Uuid;

use crate::oauth::limits::OauthEndpoint;

/// How many recent records the in-process ring keeps. Small on purpose: this is
/// a diagnostic tail, not a log store — the durable trail is the `tracing`
/// event, which goes wherever the process's subscriber sends it.
const RECENT_CAPACITY: usize = 256;

/// What happened. One variant per event the item requires auditing, plus the
/// two the OAuth door cannot be operated without (login and revocation).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OauthEvent {
    /// A human authenticated at the login form.
    LoginSucceeded,
    /// A login attempt was refused. Deliberately does NOT distinguish "no such
    /// account" from "wrong password" in its [`DenialReason`] — see
    /// [`DenialReason::BadCredentials`].
    LoginDenied,
    /// An authorization request was approved and a code issued.
    AuthorizationGranted,
    /// An authorization request was refused.
    AuthorizationDenied,
    /// An authorization code was exchanged for a token pair.
    TokenIssued,
    /// A refresh token was accepted.
    TokenRefreshed,
    /// A refresh token was rotated into its successor.
    TokenRotated,
    /// An already-rotated refresh token was presented — a theft signal. The
    /// family is revoked in response, which produces a `Revoked` record too.
    RefreshReuseDetected,
    /// A token request was refused.
    TokenDenied,
    /// A client registration attempt was refused.
    RegistrationDenied,
    /// A client was registered.
    RegistrationAccepted,
    /// Something was revoked: a family, a client's tokens, or an account's
    /// consents and tokens.
    Revoked,
    /// A request was refused for exceeding a per-endpoint budget.
    RateLimited,
    /// RMCP-12: server ownership was granted, reassigned, or revoked.
    DelegationChanged,
    /// RMCP-12: a scoping write was refused because the actor's live authority
    /// did not cover it.
    ScopingDenied,
}

/// Why something was refused.
///
/// A closed enum rather than a string, and that is a security property, not a
/// tidiness preference: a `&str` reason is a channel through which a caller can
/// put an arbitrary value — including the credential it just rejected — into the
/// audit trail. A variant cannot carry one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DenialReason {
    /// No client with that `client_id`, or the client is disabled. One reason
    /// for both, mirroring [`crate::oauth::store::OauthStore::find_active_client`]:
    /// distinguishing them in the LOG is harmless, but a separate variant
    /// invites a caller to branch on it and hand the difference to the client.
    UnknownOrDisabledClient,
    /// No such account, or the account is disabled, or the password/second
    /// factor did not verify. Collapsed for the same reason the store collapses
    /// them: any response that separates "no such account" from "wrong
    /// password" is an account-existence oracle, and an audit vocabulary that
    /// separates them is one refactor away from becoming one.
    BadCredentials,
    /// The human declined at the consent screen.
    ConsentDeclined,
    /// No live consent exists for this account/client pair.
    NoConsent,
    /// The redirect URI did not exactly match a registered one.
    RedirectUriMismatch,
    /// The PKCE verifier did not match the challenge, or the challenge method
    /// was not S256.
    PkceMismatch,
    /// The authorization code was unknown, expired, or already consumed.
    CodeNotUsable,
    /// The presented refresh token was unknown, expired, or revoked.
    RefreshNotUsable,
    /// The presented refresh token had already been rotated — the reuse case.
    RefreshReused,
    /// The token's audience is not this resource server.
    AudienceMismatch,
    /// The request was malformed or missing a required parameter.
    MalformedRequest,
    /// The grant type is not one this server issues.
    UnsupportedGrant,
    /// Dynamic client registration is disabled, or the initial access token was
    /// missing or wrong.
    RegistrationNotPermitted,
    /// A per-endpoint budget was exhausted.
    RateLimited,
    /// The caller's own state (client disabled, consent revoked, family
    /// revoked) denies it at dispatch even though its token verifies.
    Revoked,
}

/// How a runtime string is characterised when its content cannot be proven
/// credential-free.
///
/// Round 3 (`gpt56`) found the seam between this module's two previous defences:
/// the charset allowlist accepted short alphanumeric strings, and the opaque-run
/// redactor only fired at 24+ characters, so an 8- or 12-character authorization
/// code, OTP, or secret arriving through `client_id` was logged verbatim. The
/// finding also named the wrong fix — lowering the run threshold only moves the
/// seam downward and starts eating legitimate short identifiers.
///
/// So there is no filter here at all any more. A value whose content cannot be
/// characterised is not scrubbed, redacted, or truncated; it is simply **not
/// recorded**, and this descriptor is recorded instead. A length and a charset
/// class are enough to tell "someone typo'd a client name" from "someone is
/// posting a 43-byte blob", which is the actual diagnostic question, and neither
/// is content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ValueShape {
    /// Character count. A credential's LENGTH is not a credential, and it is the
    /// single most useful thing about an unidentifiable value.
    pub len: usize,
    pub charset: CharsetClass,
}

impl ValueShape {
    pub fn of(value: &str) -> Self {
        let trimmed = value.trim();
        Self { len: trimmed.chars().count(), charset: CharsetClass::of(trimmed) }
    }

    pub fn render(&self) -> String {
        format!("len={} charset={}", self.len, self.charset.as_str())
    }
}

/// A coarse classification of a value's characters. Coarse on purpose: a finer
/// class would start to describe the value rather than its shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CharsetClass {
    Empty,
    Digits,
    Alphabetic,
    Alphanumeric,
    /// Alphanumerics plus the punctuation an identifier legitimately carries.
    IdentifierPunctuation,
    /// Anything else — whitespace, control characters, non-ASCII, markup.
    Other,
}

impl CharsetClass {
    pub fn of(value: &str) -> Self {
        if value.is_empty() {
            return CharsetClass::Empty;
        }
        if value.chars().all(|c| c.is_ascii_digit()) {
            return CharsetClass::Digits;
        }
        if value.chars().all(|c| c.is_ascii_alphabetic()) {
            return CharsetClass::Alphabetic;
        }
        if value.chars().all(|c| c.is_ascii_alphanumeric()) {
            return CharsetClass::Alphanumeric;
        }
        if value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':' | '[' | ']' | '/'))
        {
            return CharsetClass::IdentifierPunctuation;
        }
        CharsetClass::Other
    }

    pub fn as_str(self) -> &'static str {
        match self {
            CharsetClass::Empty => "empty",
            CharsetClass::Digits => "digits",
            CharsetClass::Alphabetic => "alphabetic",
            CharsetClass::Alphanumeric => "alphanumeric",
            CharsetClass::IdentifierPunctuation => "identifier_punctuation",
            CharsetClass::Other => "other",
        }
    }
}

// The source address is NOT defined here, and that is the point.
//
// An earlier revision had a `SourceAddress` enum that PARSED a `&str` at this
// boundary and recorded the value whenever it parsed as an `IpAddr` or
// `SocketAddr`. Round 4 (`gpt56`) rejected that, correctly: parseability was
// doing the work of proof, and "it parsed, therefore it is not a secret" is not
// something that can be asserted about caller-controlled input. It was the last
// place in this module where a runtime string became a recorded value.
//
// So the record carries a plain [`IpAddr`], and there is no constructor that
// takes a string. The address arrives already resolved and already normalized
// from [`crate::oauth::edge::resolve_client_ip`] — the trusted-proxy logic that
// legitimately HAS one, having decided which hop in an `X-Forwarded-For` chain
// may be attributed. Making the untyped entry point impossible is the same move
// applied to `detail` and `client_id`, and it is the only one of the three that
// costs nothing at all: the caller that would have passed a string is holding an
// `IpAddr` already.

/// Which of the two rate-limit dimensions refused a request.
///
/// Recorded instead of the subject VALUE. The subject is an account name or a
/// `client_id` the caller chose, and an account name is the human's login
/// identifier — the dimension is what an operator needs to diagnose a throttle
/// ("one address is hammering us" versus "one account is being ground from
/// everywhere"), and the name adds nothing to that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitDimension {
    /// The per-address budget for the endpoint.
    Address,
    /// The per-account / per-client budget.
    Subject,
}

impl LimitDimension {
    pub fn as_str(self) -> &'static str {
        match self {
            LimitDimension::Address => "address",
            LimitDimension::Subject => "subject",
        }
    }
}

/// Which selector a revocation used. The audit vocabulary's own term, so
/// [`AuditDetail`] carries no borrowed string even for this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectorKind {
    Account,
    Client,
    AccountAndClient,
    Family,
}

impl SelectorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SelectorKind::Account => "account",
            SelectorKind::Client => "client",
            SelectorKind::AccountAndClient => "account_and_client",
            SelectorKind::Family => "family",
        }
    }
}

/// What happened, in a closed vocabulary.
///
/// **No variant carries a `String`, and that is the invariant.** Every field is
/// an integer, a `bool`, or another closed enum, so the rendered text is a
/// template this module wrote plus values that cannot encode caller data. A
/// variant added later with a `String` field would silently reopen the free-text
/// channel this type exists to close — the test
/// `no_audit_detail_variant_carries_free_text` is what makes that visible in
/// review rather than in an incident.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum AuditDetail {
    /// A revocation completed. Counts, not names.
    Revocation {
        selector: SelectorKind,
        matched: usize,
        newly_revoked: usize,
        tokens_revoked: u64,
        consents_revoked: u64,
    },
    /// The verify-after-write step found sessions still live. The loudest
    /// record this module emits.
    RevocationNotEffective { still_live: usize, matched: usize },
    /// A selector that resolved to nothing; nothing was revoked.
    RevocationMatchedNothing { selector: SelectorKind },
    /// RFC 7009: the presented token is not one this server knows.
    TokenNotRecognised,
    /// RFC 7009: the request carried no token at all.
    RevocationRequestHadNoToken,
    /// RFC 7009: the caller authenticated as a client that does not own the
    /// presented token's session. Answered 200, revoked nothing.
    ForeignSessionRefused,
    /// RFC 7009: the session holder revoked its own session.
    SessionRevokedByHolder { newly_revoked: usize },
    /// An already-rotated refresh token was presented — the theft signal.
    RefreshReuse,
    /// A request was denied at dispatch by live store state.
    DispatchDenied { state: crate::oauth::revoke::DispatchState },
    /// A request exceeded a per-endpoint budget. Carries which dimension
    /// refused it, never the subject that was throttled.
    RateLimited { dimension: LimitDimension },

    // ── The endpoint decisions (mounted in RMCP-11's round-5 scope) ────────
    //
    // These exist so the authorize/login/token paths emit through THIS record
    // rather than only through the gateway's general `AuditEntry`. The
    // difference is not cosmetic: an OAuth question ("which client was issued
    // tokens for this account, and when was that session revoked?") cannot be
    // answered from a record keyed on a tool name and an mTLS identity, because
    // at these points there is no tool and no mTLS identity.
    /// An authorization request was approved and a code issued.
    AuthorizationCodeIssued,
    /// A human authenticated at the login form.
    LoginAccepted,
    /// An access/refresh token pair was issued against an authorization code.
    TokensIssuedForCode,
    /// A refresh succeeded. `scope_narrowed` records whether the caller asked
    /// for less than it held — a legitimate RFC 6749 §6 narrowing, and worth
    /// seeing in the trail because the opposite (a widening attempt) is
    /// refused and audited as a denial.
    TokensRefreshed { scope_narrowed: bool },
    /// A refresh token was rotated into its successor.
    RefreshRotated,
    /// A client registration was accepted (RMCP-08 will emit this; the variant
    /// exists so that item adds a call site rather than a vocabulary).
    ClientRegistered,
    /// A request was refused before it could be parsed or attributed — a wrong
    /// content type, an undecodable body, a missing required parameter.
    ///
    /// Carries nothing about the offending input, deliberately. The content-type
    /// header and the body are caller-controlled text, and this module deleted
    /// its last redaction pass precisely so that nothing has to be trusted to
    /// sanitize them. The operational question a burst of these answers is
    /// "requests are being refused at the door before they are even parsed",
    /// which the endpoint and the count already answer; what the header said
    /// is not worth a channel for arbitrary bytes.
    RefusedBeforeParsing,

    // ── RMCP-12: delegation ───────────────────────────────────────────────
    /// Server ownership was assigned. `reassigned` distinguishes a fresh grant
    /// from one that took a namespace off its previous owner, and
    /// `rows_narrowed` counts the client-scoping rows removed as a result.
    ///
    /// Counts, never namespaces or client ids: an audit record naming another
    /// account's objects is the same enumeration disclosure this item refuses
    /// on its read paths.
    DelegationGranted { reassigned: bool, rows_narrowed: u64 },
    /// A delegation was removed, narrowing `rows_narrowed` client-scoping rows.
    DelegationCleared { rows_narrowed: u64 },
    /// A scoping write was refused. The reason is a closed code; nothing about
    /// WHICH namespace or client was refused is recorded, for the reason above.
    ScopingRefused { reason: ScopingRefusal },
}

/// Why a scoping write was refused. Closed, like every other reason vocabulary
/// here, so no caller-controlled text can reach the trail through it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopingRefusal {
    /// The client belongs to another account and the actor is not an operator.
    NotClientOwner,
    /// One or more requested namespaces are not owned by the actor.
    NamespaceNotOwned,
    /// The action is operator-only (granting or revoking a delegation).
    NotOperator,
}

impl ScopingRefusal {
    /// The stable audit code. Treated as a wire contract, like
    /// [`crate::oauth::scope::DenyReason::code`].
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotClientOwner => "not_client_owner",
            Self::NamespaceNotOwned => "namespace_not_owned",
            Self::NotOperator => "not_operator",
        }
    }
}

impl AuditDetail {
    /// Render to the log line's text. Every branch is a fixed template; the only
    /// interpolated values are integers and `&'static str`s from closed enums.
    pub fn render(&self) -> String {
        match self {
            AuditDetail::Revocation {
                selector,
                matched,
                newly_revoked,
                tokens_revoked,
                consents_revoked,
            } => format!(
                "revocation selector={} matched={matched} newly_revoked={newly_revoked} \
                 tokens={tokens_revoked} consents={consents_revoked}",
                selector.as_str()
            ),
            AuditDetail::RevocationNotEffective { still_live, matched } => format!(
                "revocation did not take effect: {still_live} of {matched} sessions still live"
            ),
            AuditDetail::RevocationMatchedNothing { selector } => {
                format!("revocation selector={} matched nothing", selector.as_str())
            }
            AuditDetail::TokenNotRecognised => "presented token not recognised".to_string(),
            AuditDetail::RevocationRequestHadNoToken => "revocation request carried no token".to_string(),
            AuditDetail::ForeignSessionRefused => {
                "presented client does not own this session; nothing revoked".to_string()
            }
            AuditDetail::SessionRevokedByHolder { newly_revoked } => {
                format!("session revoked by its holder (newly_revoked={newly_revoked})")
            }
            AuditDetail::RefreshReuse => {
                "already-rotated refresh token presented; revoking the family".to_string()
            }
            AuditDetail::DispatchDenied { state } => {
                format!("dispatch denied: {}", state.as_str())
            }
            AuditDetail::RateLimited { dimension } => {
                format!("rate limited on the {} budget", dimension.as_str())
            }
            AuditDetail::AuthorizationCodeIssued => "authorization code issued".to_string(),
            AuditDetail::LoginAccepted => "login accepted".to_string(),
            AuditDetail::TokensIssuedForCode => {
                "access and refresh tokens issued for an authorization code".to_string()
            }
            AuditDetail::TokensRefreshed { scope_narrowed } => format!(
                "tokens refreshed (scope_narrowed={scope_narrowed})"
            ),
            AuditDetail::RefreshRotated => "refresh token rotated into its successor".to_string(),
            AuditDetail::ClientRegistered => "client registered".to_string(),
            AuditDetail::RefusedBeforeParsing => {
                "request refused before it could be parsed".to_string()
            }
            AuditDetail::DelegationGranted { reassigned, rows_narrowed } => format!(
                "server ownership granted (reassigned={reassigned}, \
                 client_scoping_rows_narrowed={rows_narrowed})"
            ),
            AuditDetail::DelegationCleared { rows_narrowed } => format!(
                "server ownership revoked (client_scoping_rows_narrowed={rows_narrowed})"
            ),
            AuditDetail::ScopingRefused { reason } => {
                format!("scoping write refused: {}", reason.as_str())
            }
        }
    }
}

/// One OAuth audit record.
///
/// **Every field is private and the only way to populate one is through a
/// setter that sanitizes.** That is deliberate and is the difference between a
/// rule and a compiler check: with `pub` fields, any call site in this crate —
/// including one added later by someone who never read this module — could
/// assemble a record with a struct literal and put a raw token straight into
/// `detail`, and the scanning test would keep passing because it only sees what
/// the sanitizing path emitted. Making the sanitizing path the ONLY path is the
/// same reasoning [`crate::oauth::SecretHash`] applies to storage.
///
/// Reading is unrestricted, through the accessors below.
#[derive(Debug, Clone, Serialize)]
pub struct OauthAuditRecord {
    event: OauthEvent,
    /// Which endpoint the event happened at, when it happened at one. `None`
    /// for an event raised by a tool or a background revocation.
    endpoint: Option<OauthEndpoint>,
    /// The SHAPE of the presented `client_id` — never the value.
    ///
    /// A `client_id` is an opaque string chosen by whoever sent it, and there is
    /// no parser that can prove an arbitrary one is not a credential. So for a
    /// client that RESOLVED, the record identifies it by `client_uuid` (which
    /// correlates with the store and is what an operator actually joins on), and
    /// for one that did not, only this descriptor is kept. Round 3 established
    /// that recording the presented value and filtering it is not a defence that
    /// holds at short lengths.
    client_shape: Option<ValueShape>,
    /// The client's internal id, when it resolved.
    client_uuid: Option<Uuid>,
    /// The account's internal id, when one was resolved. The account NAME is
    /// never recorded: it is the human's login identifier, and the id
    /// correlates just as well.
    account_id: Option<Uuid>,
    /// The refresh-token family, which is how a SESSION is named throughout
    /// this item. Never a token hash.
    family_id: Option<Uuid>,
    /// The resolved client address, already typed and already normalized by
    /// [`crate::oauth::edge::resolve_client_ip`]. Never parsed here — see the
    /// note above the [`OauthEvent`] definitions for why that boundary was
    /// removed.
    source: Option<IpAddr>,
    reason: Option<DenialReason>,
    /// The narrative, as a closed structured value rather than prose. See
    /// [`AuditDetail`].
    detail: Option<AuditDetail>,
    at: DateTime<Utc>,
}

impl OauthAuditRecord {
    pub fn event_kind(&self) -> OauthEvent {
        self.event
    }
    pub fn at(&self) -> DateTime<Utc> {
        self.at
    }
    pub fn endpoint_of(&self) -> Option<OauthEndpoint> {
        self.endpoint
    }
    /// The shape of the presented `client_id`, if one was recorded. There is
    /// deliberately no accessor returning the presented VALUE, because none is
    /// stored.
    pub fn client_shape(&self) -> Option<ValueShape> {
        self.client_shape
    }
    pub fn client_id(&self) -> Option<Uuid> {
        self.client_uuid
    }
    pub fn account_id(&self) -> Option<Uuid> {
        self.account_id
    }
    pub fn family_id(&self) -> Option<Uuid> {
        self.family_id
    }
    pub fn source(&self) -> Option<IpAddr> {
        self.source
    }

    /// The rendered source, in `IpAddr`'s canonical form — which is what an
    /// operator pastes into a firewall rule.
    pub fn source_address(&self) -> Option<String> {
        self.source.map(|ip| ip.to_string())
    }
    pub fn denial_reason(&self) -> Option<DenialReason> {
        self.reason
    }
    /// The structured narrative. Named `detail_kind` rather than `detail`
    /// because the builder already owns that name — and the builder is the more
    /// important of the two to keep obvious at a call site.
    pub fn detail_kind(&self) -> Option<AuditDetail> {
        self.detail
    }
    /// The rendered narrative — what reaches a log line. Built on demand from
    /// the closed [`AuditDetail`], so there is no stored string a caller could
    /// have chosen.
    pub fn detail_text(&self) -> Option<String> {
        self.detail.map(|d| d.render())
    }
}

impl OauthAuditRecord {
    pub fn new(event: OauthEvent) -> Self {
        Self {
            event,
            endpoint: None,
            client_shape: None,
            client_uuid: None,
            account_id: None,
            family_id: None,
            source: None,
            reason: None,
            detail: None,
            at: Utc::now(),
        }
    }

    pub fn endpoint(mut self, endpoint: OauthEndpoint) -> Self {
        self.endpoint = Some(endpoint);
        self
    }

    /// Record the SHAPE of a presented `client_id`. The value itself never
    /// enters the record — a caller that passes a token here, by mistake or
    /// because an attacker put one in the field, contributes a length and a
    /// charset class and nothing more.
    pub fn client(mut self, client_id: &str) -> Self {
        self.client_shape = Some(ValueShape::of(client_id));
        self
    }

    pub fn client_uuid(mut self, id: Uuid) -> Self {
        self.client_uuid = Some(id);
        self
    }

    pub fn account(mut self, id: Uuid) -> Self {
        self.account_id = Some(id);
        self
    }

    pub fn family(mut self, id: Uuid) -> Self {
        self.family_id = Some(id);
        self
    }

    /// Record the resolved source address.
    ///
    /// Takes an [`IpAddr`], never a string. There is deliberately no overload
    /// that parses: the only caller that legitimately has a source address got
    /// it from [`crate::oauth::edge::resolve_client_ip`] and is holding a typed,
    /// normalized value already.
    pub fn from_address(mut self, address: IpAddr) -> Self {
        self.source = Some(address);
        self
    }

    pub fn reason(mut self, reason: DenialReason) -> Self {
        self.reason = Some(reason);
        self
    }

    /// Attach the structured narrative. Takes an [`AuditDetail`], never a
    /// string — see the module docs for why that distinction is the point.
    pub fn detail(mut self, detail: AuditDetail) -> Self {
        self.detail = Some(detail);
        self
    }

    /// Emit: one structured `tracing` event, and one push into the ring.
    ///
    /// Cannot fail and cannot block the request, for the same reason
    /// [`crate::gateway_framework::audit::AuditEntry::log`] cannot: an audit
    /// write failing must never be able to fail the authentication it is
    /// describing. A poisoned ring mutex is recovered rather than propagated.
    pub fn emit(self) -> Self {
        tracing::info!(
            target: "rmcp_oauth_audit",
            event = ?self.event,
            endpoint = self.endpoint.map(|e| e.as_str()).unwrap_or(""),
            client_shape = self.client_shape.map(|s| s.render()).unwrap_or_default(),
            client_uuid = self.client_uuid.map(|u| u.to_string()).unwrap_or_default(),
            account_id = self.account_id.map(|u| u.to_string()).unwrap_or_default(),
            family_id = self.family_id.map(|u| u.to_string()).unwrap_or_default(),
            source = self.source_address().unwrap_or_default(),
            reason = ?self.reason,
            detail = self.detail_text().unwrap_or_default(),
            "rmcp_oauth_audit"
        );
        let ring = recent_ring();
        let mut guard = ring.lock().unwrap_or_else(|e| e.into_inner());
        if guard.len() == RECENT_CAPACITY {
            guard.pop_front();
        }
        guard.push_back(self.clone());
        self
    }
}

fn recent_ring() -> &'static Mutex<VecDeque<OauthAuditRecord>> {
    static RING: OnceLock<Mutex<VecDeque<OauthAuditRecord>>> = OnceLock::new();
    RING.get_or_init(|| Mutex::new(VecDeque::with_capacity(RECENT_CAPACITY)))
}

/// A snapshot of the recent-record ring, oldest first.
///
/// This is what makes the "no record contains a raw credential" acceptance
/// criterion checkable by SCANNING rather than by reading the code, and it is
/// what an operator-facing recent-auth-events view reads.
pub fn recent_records() -> Vec<OauthAuditRecord> {
    recent_ring()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .cloned()
        .collect()
}

/// Every textual value a record can produce, for a scanner that wants to assert
/// something is absent from ALL of them without knowing the field layout.
pub fn record_text(record: &OauthAuditRecord) -> Vec<String> {
    [
        record.client_shape().map(|s| s.render()),
        record.source_address(),
        record.detail_text(),
    ]
    .into_iter()
    .flatten()
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 256-bit value rendered base64url — the shape RMCP-03/RMCP-04 generate
    /// for an authorization code and a refresh token. Synthetic and
    /// self-evidently a fixture: a fixed literal in a unit test, unique to this
    /// file so a scan of the process-wide ring cannot match it by accident.
    const LONG_FIXTURE_VALUE: &str = "fixture-value-Zm9vYmFyYmF6cXV4Y29ycmdlZ3JhdWx0bHk";

    /// The cases round 3 found: SHORT values that the old charset allowlist
    /// accepted and the old 24-character opaque-run redactor never fired on, so
    /// they were logged verbatim. Both are plain alphanumerics, which is exactly
    /// what made them invisible to both previous layers.
    const SHORT_FIXTURE_8: &str = "Kp7Wq2Zt";
    const SHORT_FIXTURE_12: &str = "Rm4Xb9Ld2Vn6";

    /// The headline requirement, asserted the way the item demands: push
    /// credentials through EVERY entry point that accepts a runtime string, then
    /// scan what was actually emitted.
    ///
    /// The SHORT values are the point of this test. A 43-character blob was
    /// already caught by the old opaque-run pass; an 8-character one was not,
    /// and it is the case that proves the guarantee no longer rests on a length
    /// threshold at all. There is likewise no way to pass a credential to
    /// `detail`, because it takes an [`AuditDetail`] and no variant carries a
    /// `String`.
    #[test]
    fn no_emitted_record_carries_a_raw_credential_at_any_length() {
        // A documentation-range address, so the records under test carry a
        // realistic source alongside the credential-shaped client_id. Note there
        // is no longer any way to put a credential in the SOURCE field at all —
        // it takes an `IpAddr` — so `client` is the only remaining entry point a
        // credential could arrive through, which is exactly the point.
        let source: IpAddr = "192.0.2.99".parse().expect("documentation-range literal");
        let mut emitted = Vec::new();
        for value in [LONG_FIXTURE_VALUE, SHORT_FIXTURE_8, SHORT_FIXTURE_12] {
            emitted.push(
                OauthAuditRecord::new(OauthEvent::AuthorizationDenied).client(value).emit(),
            );
            emitted.push(
                OauthAuditRecord::new(OauthEvent::TokenDenied)
                    .client(value)
                    .from_address(source)
                    .detail(AuditDetail::TokenNotRecognised)
                    .emit(),
            );
        }

        let secrets = [LONG_FIXTURE_VALUE, SHORT_FIXTURE_8, SHORT_FIXTURE_12];
        for record in &emitted {
            for text in record_text(record) {
                for secret in secrets {
                    assert!(
                        !text.contains(secret),
                        "a raw credential ({} chars) reached an audit record: {text}",
                        secret.chars().count()
                    );
                }
            }
        }

        // And the same holds for what actually landed in the ring, which is
        // the thing an operator (or a log shipper) reads.
        for record in recent_records() {
            for text in record_text(&record) {
                for secret in secrets {
                    assert!(
                        !text.contains(secret),
                        "a raw credential ({} chars) reached the ring: {text}",
                        secret.chars().count()
                    );
                }
            }
        }
    }

    /// A short credential is not merely absent — the record says something
    /// USEFUL in its place, which is what makes discarding the value affordable.
    #[test]
    fn a_discarded_value_is_replaced_by_a_shape_not_by_nothing() {
        let record = OauthAuditRecord::new(OauthEvent::AuthorizationDenied).client(SHORT_FIXTURE_8);
        let shape = record.client_shape().expect("a shape is recorded");
        assert_eq!(shape.len, 8);
        assert_eq!(shape.charset, CharsetClass::Alphanumeric);
        assert!(!shape.render().contains(SHORT_FIXTURE_8));
        // The distinction an operator actually needs: a typo'd client name and a
        // pasted blob are visibly different without either being reproduced.
        let typo = ValueShape::of("cluade-connector");
        assert_eq!(typo.charset, CharsetClass::IdentifierPunctuation);
        assert_ne!(typo.len, ValueShape::of(LONG_FIXTURE_VALUE).len);
    }

    /// The operational property protected in round 3 and preserved in round 4:
    /// a recorded address is nameable well enough to write a firewall rule.
    ///
    /// The limiter hashes its bucket key, so the audit trail is the ONLY place a
    /// hammering address appears. Making the setter take an `IpAddr` did not
    /// weaken that — it strengthened it, because the value is now canonical by
    /// construction rather than by a parse this module performed on a string it
    /// was handed.
    #[test]
    fn a_recorded_address_is_actionable_and_canonical() {
        // RFC 5737 / RFC 3849 documentation ranges. Parsed from literals HERE,
        // in a test, which is a different thing from parsing caller-controlled
        // input at the API boundary — the same helper `edge`'s own tests use.
        for (literal, expected) in [
            ("192.0.2.10", "192.0.2.10"),
            ("2001:db8::1", "2001:db8::1"),
            // `IpAddr`'s Display is canonical, so a long-form v6 literal renders
            // collapsed without this module doing anything.
            ("2001:0db8:0000:0000:0000:0000:0000:0001", "2001:db8::1"),
        ] {
            let ip: IpAddr = literal.parse().expect("a documentation-range literal");
            let record = OauthAuditRecord::new(OauthEvent::RateLimited).from_address(ip);
            assert_eq!(record.source_address().as_deref(), Some(expected));
            assert_eq!(record.source(), Some(ip));
        }
    }

    /// There is no longer any way to put a non-address into the source field, so
    /// there is no "not an address" case left to characterise. This test records
    /// that as a deliberate absence rather than an omission: the setter's
    /// signature is the guarantee, and a `&str` overload reintroduced later
    /// would make this comment false.
    #[test]
    fn the_source_field_admits_nothing_but_an_address() {
        fn accepts_only_ipaddr(f: fn(OauthAuditRecord, IpAddr) -> OauthAuditRecord) -> bool {
            let _ = f;
            true
        }
        assert!(accepts_only_ipaddr(OauthAuditRecord::from_address));
    }

    /// The structural guarantee behind the test above, asserted directly: no
    /// [`AuditDetail`] variant can carry caller data.
    ///
    /// `AuditDetail` is `Copy`, and a `String` field would make it not `Copy` —
    /// so this binding is a compile-time proof, not a runtime check. Someone
    /// adding a free-text variant has to delete this test to do it, which is
    /// exactly the visibility the review round asked for.
    #[test]
    fn no_audit_detail_variant_carries_free_text() {
        fn assert_copy<T: Copy>() {}
        assert_copy::<AuditDetail>();

        // And every variant renders from its own template — spot-checked on the
        // one variant with the most interpolated values.
        let rendered = AuditDetail::Revocation {
            selector: SelectorKind::AccountAndClient,
            matched: 3,
            newly_revoked: 2,
            tokens_revoked: 7,
            consents_revoked: 1,
        }
        .render();
        assert!(rendered.contains("selector=account_and_client"), "{rendered}");
        assert!(rendered.contains("newly_revoked=2"), "{rendered}");
    }

    /// A `ValueShape` describes without reproducing. Asserted directly, because
    /// this type is now the ONLY thing standing where a filter used to be.
    #[test]
    fn a_value_shape_describes_without_reproducing() {
        for value in [
            SHORT_FIXTURE_8,
            SHORT_FIXTURE_12,
            LONG_FIXTURE_VALUE,
            "a client id with spaces",
            "<script>alert(1)</script>",
            "claude-connector.v1",
        ] {
            let rendered = ValueShape::of(value).render();
            assert!(!rendered.contains(value), "{value} was reproduced as {rendered}");
            assert!(rendered.starts_with("len="), "{rendered}");
        }
        // Charset classification is coarse but discriminating enough to be worth
        // recording in place of the value.
        assert_eq!(ValueShape::of("").charset, CharsetClass::Empty);
        assert_eq!(ValueShape::of("482913").charset, CharsetClass::Digits);
        assert_eq!(ValueShape::of("connector").charset, CharsetClass::Alphabetic);
        assert_eq!(ValueShape::of(SHORT_FIXTURE_8).charset, CharsetClass::Alphanumeric);
        assert_eq!(
            ValueShape::of("claude-connector.v1").charset,
            CharsetClass::IdentifierPunctuation
        );
        assert_eq!(ValueShape::of("a client id").charset, CharsetClass::Other);
        assert_eq!(ValueShape::of("<script>").charset, CharsetClass::Other);
    }

    /// The record type must not regain a free-text channel by the back door:
    /// there is no accessor returning the presented `client_id`, because none is
    /// stored.
    ///
    /// `ValueShape`, `IpAddr` and `AuditDetail` are all `Copy`, which makes
    /// "carries no `String`" a compile-time property rather than a convention. A
    /// field added later that could hold caller text breaks this binding.
    #[test]
    fn no_recorded_field_can_hold_a_caller_supplied_string() {
        fn assert_copy<T: Copy>() {}
        assert_copy::<ValueShape>();
        assert_copy::<IpAddr>();
        assert_copy::<AuditDetail>();
        assert_copy::<CharsetClass>();
    }

    /// A denial reason is a variant, so there is no way to smuggle text through
    /// it. Asserted on the serialized form because that is what reaches a log.
    #[test]
    fn denial_reasons_serialize_as_closed_vocabulary() {
        let json = serde_json::to_string(&DenialReason::BadCredentials).expect("serializable");
        assert_eq!(json, "\"bad_credentials\"");
    }

    /// The ring must not grow without bound — it is a diagnostic tail, and an
    /// auth flood must not turn it into a memory leak.
    #[test]
    fn the_ring_is_bounded() {
        for _ in 0..(RECENT_CAPACITY + 50) {
            OauthAuditRecord::new(OauthEvent::RateLimited).emit();
        }
        assert!(recent_records().len() <= RECENT_CAPACITY);
    }

    /// Emitting must never panic, whatever the subscriber situation — an audit
    /// write failing cannot be allowed to fail the authentication it describes.
    #[test]
    fn emit_never_panics_without_a_subscriber() {
        OauthAuditRecord::new(OauthEvent::LoginSucceeded)
            .account(Uuid::nil())
            .endpoint(OauthEndpoint::Login)
            .emit();
    }
}
