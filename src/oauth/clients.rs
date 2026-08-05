//! RMCP-08 — how a `client_id` comes into existence, and the client lifecycle.
//!
//! ## Two ways in, and only two
//!
//! 1. **Operator minting.** An operator names a connector and its redirect
//!    URIs through the `rmcp_client_*` tools ([`crate::tools::rmcp_client`]),
//!    which the Connectors GUI and a CLI both call. This is the path the
//!    operator's own Claude connector uses, and it is the one that makes
//!    "only I can link my account" true: no client exists that the operator did
//!    not mint.
//! 2. **Dynamic client registration** (RFC 7591, [`crate::oauth::register`]).
//!    OFF by default, and never an unauthenticated write when on — a
//!    registration request must carry an operator-issued initial access token.
//!
//! There is deliberately no third. This module is the single implementation
//! behind both: the same validation, the same secret handling, the same store
//! writes. The registration endpoint is a transport in front of
//! [`ClientService::register_dynamic`]; the tools are a transport in front of
//! [`ClientService::mint`]. Neither carries a rule the other does not.
//!
//! ## The secret is shown exactly once
//!
//! A minted client secret is generated from operating-system entropy, hashed
//! with argon2id, and stored as a PHC string. The plaintext exists only in the
//! response to the call that created it. Nothing reads it back, and nothing
//! CAN: the administrative row type ([`crate::oauth::model::ClientAdmin`])
//! carries a `confidential` boolean computed in SQL and has no field a hash —
//! let alone a secret — could occupy. So "not retrievable" is a property of the
//! types rather than a discipline every future reader has to maintain.
//!
//! ## A DCR client reaches nothing until an operator scopes it
//!
//! A dynamically registered client lands with **no scope rows at all**, and
//! RMCP-07's resolver reads absence as the empty set. That is the whole
//! guarantee, and it is worth being precise about which control delivers it:
//!
//! - it is NOT the `disabled` column. `disabled` is the AUTHENTICATION kill
//!   switch — [`crate::oauth::store::OauthStore::find_active_client`] stops
//!   resolving a disabled client, so a disabled client cannot complete an
//!   authorization at all. Landing DCR clients disabled would mean the operator
//!   had to enable one before ever seeing it work, and would conflate "this
//!   connector is revoked" with "this connector is awaiting approval".
//! - it IS the absence of scope rows. The client can authenticate a human and
//!   obtain a token, and that token reaches **zero tools** until an operator
//!   assigns groups and namespaces. `a_dcr_client_reaches_nothing_until_scoped`
//!   asserts it against the real resolver rather than against this comment.
//!
//! Both readings satisfy "disabled for tool access until an operator scopes
//! them"; this one does it without a second meaning for a column that already
//! has one.

use uuid::Uuid;

use crate::error::ToolError;
use crate::oauth::model::{ClientAdmin, RegistrationSource};
use crate::oauth::password::hash_password;
use crate::oauth::store::OauthStore;
use crate::oauth::{random_token, SecretHash};

// ---------------------------------------------------------------------------
// Bounds and supported values
// ---------------------------------------------------------------------------

/// The grant types this authorization server issues.
///
/// The SINGLE source of truth for that list. [`crate::oauth::metadata`]
/// advertises it and this module validates against it, so a client cannot be
/// registered for a grant the metadata never offered, and the metadata cannot
/// offer a grant registration would refuse. OAuth 2.1 removed the implicit and
/// password grants; neither appears here, and neither may be registered.
pub const SUPPORTED_GRANT_TYPES: &[&str] = &["authorization_code", "refresh_token"];

/// What an ABSENT `grant_types` means: the authorization-code grant, alone.
///
/// RFC 7591 §2's default, followed exactly. Not `authorization_code` plus
/// `refresh_token` — that would hand a client a capability it did not request,
/// which is a widening on the absence path and the one direction this item
/// refuses everywhere else.
pub const DEFAULT_GRANT_TYPE: &str = "authorization_code";

/// What an ABSENT `token_endpoint_auth_method` means: a PUBLIC client.
///
/// RFC 7591's own default is `client_secret_basic`; this server departs from it
/// deliberately, because the connector this door exists for is a public client
/// and defaulting to a confidential method would mint a secret nobody asked for
/// and hand it out in a response. Choosing to hold a credential should be
/// something a caller SAID.
///
/// It applies to an ABSENT member only. A member that is present but unusable
/// — `null`, blank, wrong type — is refused, never defaulted here: that is how
/// a meaningless submission used to land on the weakest method.
pub const DEFAULT_AUTH_METHOD: &str = "none";

/// The token-endpoint authentication methods this server supports, matching
/// what the metadata advertises. `none` is a public client (PKCE only), which
/// is what Claude registers as.
pub const SUPPORTED_AUTH_METHODS: &[&str] =
    &["none", "client_secret_post", "client_secret_basic"];

/// The only response type OAuth 2.1 leaves.
pub const SUPPORTED_RESPONSE_TYPES: &[&str] = &["code"];

/// Most redirect URIs one client may register.
///
/// A connector needs one, sometimes a handful for loopback ports it cannot
/// predict. The bound exists because each registered URI is compared on every
/// authorization request, and because an unbounded array on a pre-auth endpoint
/// is a storage amplifier.
pub const MAX_REDIRECT_URIS: usize = 8;

/// Longest redirect URI accepted. Comfortably above any real callback and far
/// below anything that would make the comparison interesting.
pub const MAX_REDIRECT_URI_LEN: usize = 512;

/// Longest client display name accepted. It is rendered — escaped — on the
/// consent page, so it is bounded for the human's benefit as much as the
/// store's.
pub const MAX_CLIENT_NAME_LEN: usize = 128;

/// Entropy in a generated `client_id`. Public, not secret — but unguessable
/// ids keep a registration endpoint from being enumerable.
const CLIENT_ID_BYTES: usize = 12;

/// Entropy in a generated client secret. 32 bytes of OS entropy: this is a
/// machine-generated credential, so the argon2id hashing that follows is belt
/// and braces rather than the thing standing between an attacker and the value.
const CLIENT_SECRET_BYTES: usize = 32;

/// Default lifetime of an initial access token, in seconds (one hour).
///
/// Short because it is handed to a human to paste into one registration. A
/// token that outlives the sitting it was minted for is an invitation nobody
/// remembers issuing.
pub const DEFAULT_IAT_TTL_SECONDS: i64 = 3600;

/// Longest an initial access token may be asked to live (one day).
pub const MAX_IAT_TTL_SECONDS: i64 = 86_400;

/// Most uses an initial access token may be minted with.
pub const MAX_IAT_USES: i32 = 10;

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Why a piece of submitted client metadata was refused.
///
/// A closed enum, rendered from fixed templates, for the same reason
/// [`crate::oauth::audit::DenialReason`] is one: a rejection message on a
/// pre-auth endpoint is caller-influenced output, and a `String` reason is a
/// channel through which the rejected value itself reaches a log, an error body
/// and an operator's terminal. A variant cannot carry one.
///
/// The offending value is identified by FIELD and INDEX — `redirect_uris[2]` —
/// which is what a caller needs to fix it and is the most that can be said
/// without echoing bytes somebody else chose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataFault {
    /// No redirect URI at all. An authorization-code client without one can
    /// never complete a flow.
    NoRedirectUris,
    /// More redirect URIs than [`MAX_REDIRECT_URIS`].
    TooManyRedirectUris,
    /// Longer than [`MAX_REDIRECT_URI_LEN`], or an empty string.
    RedirectUriLength,
    /// Neither an absolute `https://` URI nor an RFC 8252 loopback URI. This is
    /// the arm that refuses `http://` to a non-loopback host.
    RedirectUriNotHttpsOrLoopback,
    /// Carries a `#` fragment. RFC 6749 forbids one, and a fragment is not sent
    /// to the server, so a client registering one has a callback that silently
    /// loses part of itself.
    RedirectUriHasFragment,
    /// Contains a `*`. There is no wildcard matching here and never will be:
    /// [`crate::oauth::authorize::redirect_uri_matches`] compares exactly, so a
    /// registered `*` is not a permissive pattern, it is a URI nothing matches
    /// — and accepting it would leave an operator believing otherwise.
    RedirectUriHasWildcard,
    /// Carries a query parameter whose name collides with an authorization
    /// response parameter (`code`, `state`, `iss`, `error`, …). Refused at
    /// registration because [`crate::oauth::authorize`] refuses it at request
    /// time — registering one mints a client that can never authorize.
    RedirectUriHasReservedParameter,
    /// Contains whitespace or control characters.
    RedirectUriMalformed,
    /// The same URI twice.
    RedirectUriDuplicated,
    /// A grant type outside [`SUPPORTED_GRANT_TYPES`].
    UnsupportedGrantType,
    /// A response type outside [`SUPPORTED_RESPONSE_TYPES`].
    UnsupportedResponseType,
    /// An authentication method outside [`SUPPORTED_AUTH_METHODS`].
    UnsupportedAuthMethod,
    /// Missing, blank, over-long, or containing control characters.
    ClientName,
    /// Security-significant metadata this server does not implement — see
    /// [`UNIMPLEMENTED_CRITICAL_METADATA`].
    UnimplementedCriticalMetadata,
    /// A member this server does not recognise at all. Refused rather than
    /// ignored, because an unrecognised name is exactly the case where we
    /// CANNOT say whether it carries a security meaning — see
    /// [`COSMETIC_METADATA`].
    UnrecognisedMember,
    /// A member that is PRESENT but of the wrong JSON type.
    ///
    /// Refused, never read as absent. This is RMCP-02's rule applied one level
    /// down: ABSENT means not configured, PRESENT means the value must be
    /// USABLE, and present-but-unusable is malformed rather than missing.
    MalformedMember,
}

impl MetadataFault {
    /// The fixed message. Every branch is a literal: nothing here interpolates
    /// a submitted value, and there is no branch that could.
    pub fn render(self) -> &'static str {
        match self {
            Self::NoRedirectUris => "at least one redirect URI is required",
            Self::TooManyRedirectUris => "too many redirect URIs",
            Self::RedirectUriLength => "must be non-empty and within the length bound",
            Self::RedirectUriNotHttpsOrLoopback => {
                "must be an absolute https URI, or an RFC 8252 http loopback URI"
            }
            Self::RedirectUriHasFragment => "must not contain a fragment",
            Self::RedirectUriHasWildcard => "must not contain a wildcard; matching is exact",
            Self::RedirectUriHasReservedParameter => {
                "must not carry a query parameter reserved for the authorization response"
            }
            Self::RedirectUriMalformed => "must not contain whitespace or control characters",
            Self::RedirectUriDuplicated => "is registered twice",
            Self::UnsupportedGrantType => "is not a grant type this server issues",
            Self::UnsupportedResponseType => "is not a response type this server supports",
            Self::UnsupportedAuthMethod => {
                "is not a token-endpoint authentication method this server supports"
            }
            Self::ClientName => "must be a non-empty name within the length bound",
            Self::UnimplementedCriticalMetadata => {
                "is security-significant metadata this server does not implement; it cannot be \
                 honoured, and honouring it partially would leave a client believing a control \
                 is active that is not"
            }
            Self::UnrecognisedMember => {
                "the request carries a member this server does not recognise. Unrecognised \
                 members are refused rather than ignored, because an unknown name is precisely \
                 the case where this server cannot tell whether it asserts a security control"
            }
            Self::MalformedMember => {
                "is present but is not of the type this member requires; a present-but-unusable \
                 value is malformed, never absent"
            }
        }
    }
}

/// One rejection, located.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FieldFault {
    /// A `&'static str` naming the metadata field. Never a caller-supplied key
    /// — see [`validate`]'s handling of unknown members.
    pub field: &'static str,
    /// Position within an array field, when there is one.
    pub index: Option<usize>,
    pub fault: MetadataFault,
}

impl FieldFault {
    /// `redirect_uris[2]: must not contain a fragment`. Built from a static
    /// field name, an integer, and a static message — three things a caller
    /// cannot influence the content of.
    pub fn render(&self) -> String {
        match self.index {
            Some(i) => format!("{}[{}]: {}", self.field, i, self.fault.render()),
            None => format!("{}: {}", self.field, self.fault.render()),
        }
    }
}

/// Members this server UNDERSTANDS and acts on.
pub const SUPPORTED_METADATA: &[&str] = &[
    "client_name",
    "redirect_uris",
    "grant_types",
    "response_types",
    "token_endpoint_auth_method",
];

/// Members deliberately IGNORED, because they are descriptive rather than
/// security-bearing.
///
/// This is the list that carries the interoperability burden, and it is the
/// only list a future author has to extend to keep an ordinary client working.
/// When somebody forgets to extend it, the failure is a LOUD refusal that names
/// the problem, not a silent acceptance of a member whose meaning nobody
/// checked — which is the whole reason the burden was moved here.
pub const COSMETIC_METADATA: &[&str] = &[
    "application_type",
    "client_description",
    "client_uri",
    "contacts",
    "logo_uri",
    "policy_uri",
    "software_id",
    "software_version",
    "tos_uri",
];

/// Security-significant members this server does not implement, named
/// individually so their refusal says WHICH one and why.
///
/// Every name here asserts something about how the client will be
/// authenticated, how a request or response will be signed or encrypted, or
/// what subject identifier it will receive. Accepting a registration carrying
/// one and quietly doing nothing about it leaves the client believing a control
/// is in force that is not — which is worse than refusing, because nobody finds
/// out.
///
/// ## Why an allowlist, after arguing for a denylist
///
/// Round 1 (`gpt56`) accepted the interoperability argument for a denylist but
/// pointed out — correctly — that the burden then falls on the denylist to be
/// COMPLETE, and the first version was not: request-object signing, response
/// signing and encryption, and TLS client authentication were all silently
/// ignored. A denylist can never carry that burden, because completeness is
/// unprovable and the failure of an incomplete one is invisible.
///
/// So the structure is inverted, and this list is now a diagnostic aid rather
/// than the control. The CONTROL is: understood ∪ cosmetic is accepted, and
/// **everything else is refused** ([`MetadataFault::UnrecognisedMember`]). This
/// list only decides whether the refusal can name the member. That keeps the
/// fail-closed property — an unanticipated security member is refused by
/// DEFAULT rather than by somebody having thought of it — while leaving
/// ordinary descriptive extensions to be handled by extending
/// [`COSMETIC_METADATA`], where a mistake is loud.
pub const UNIMPLEMENTED_CRITICAL_METADATA: &[&str] = &[
    // Pre-authorized assertions and key material.
    "software_statement",
    "jwks",
    "jwks_uri",
    "client_secret",
    // Token-endpoint authentication this server does not perform.
    "token_endpoint_auth_signing_alg",
    "tls_client_auth_subject_dn",
    "tls_client_auth_san_dns",
    "tls_client_auth_san_uri",
    "tls_client_auth_san_ip",
    // Request objects: signing, encryption, and requiring them.
    "request_object_signing_alg",
    "request_object_encryption_alg",
    "request_object_encryption_enc",
    "request_uris",
    "require_signed_request_object",
    "require_pushed_authorization_requests",
    // Authorization-response signing and encryption (JARM).
    "authorization_signed_response_alg",
    "authorization_encrypted_response_alg",
    "authorization_encrypted_response_enc",
    // ID-token and userinfo signing and encryption.
    "id_token_signed_response_alg",
    "id_token_encrypted_response_alg",
    "id_token_encrypted_response_enc",
    "userinfo_signed_response_alg",
    "userinfo_encrypted_response_alg",
    "userinfo_encrypted_response_enc",
    // Subject identifier shape — a client expecting a pairwise `sub` and
    // receiving a shared one has a disclosure it did not agree to.
    "subject_type",
    "sector_identifier_uri",
    // `scope` — moved here from the cosmetic list in review round 2, and it is
    // worth saying why, because it looks descriptive and is not.
    //
    // RFC 7591's `scope` is the set of scopes the client may request. This
    // server does not implement per-client scope restriction at all: what a
    // token carries is decided by `RMCP_OAUTH_REQUIRED_SCOPE` and the consent,
    // globally, and there is no column in which a registered scope could be
    // recorded. Accepting one would therefore mean storing nothing, enforcing
    // nothing, and returning a registration the client reasonably reads as "the
    // server agreed to this" — a client believing it registered a narrower
    // scope than it will actually be issued, or a scope this server never
    // offered. That is the exact failure this list exists for, and it is a
    // different thing entirely from ignoring a display name or a logo.
    //
    // Refused rather than silently dropped, and named, so the message says
    // which member to remove. The interoperability cost is real but bounded:
    // DCR is off by default, and operator minting — the path this item exists
    // for — never goes near it.
    "scope",
];

/// The submitted metadata, after validation. Constructed only by [`validate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedMetadata {
    pub name: String,
    pub redirect_uris: Vec<String>,
    pub grant_types: Vec<String>,
    pub token_endpoint_auth_method: String,
}

impl ValidatedMetadata {
    /// Whether this registration asks for a client secret.
    pub fn wants_secret(&self) -> bool {
        self.token_endpoint_auth_method != "none"
    }
}

/// What a caller submitted, before any of it is trusted.
///
/// Deliberately all-optional and all-owned: it is built identically from a tool
/// argument object and from an RFC 7591 JSON body, so [`validate`] is the only
/// place either shape is judged.
#[derive(Debug, Clone, Default)]
pub struct SubmittedMetadata {
    pub name: Option<String>,
    pub redirect_uris: Vec<String>,
    pub grant_types: Option<Vec<String>>,
    pub response_types: Option<Vec<String>>,
    pub token_endpoint_auth_method: Option<String>,
    /// Names of security-significant members that were present. Only names from
    /// [`UNIMPLEMENTED_CRITICAL_METADATA`] are ever recorded here, so this
    /// cannot carry caller-chosen text into a message.
    pub critical_members_present: Vec<&'static str>,
    /// Whether the request carried a member that is neither understood nor
    /// cosmetic nor a named security member. A BOOLEAN, not the names: an
    /// unrecognised key is caller-chosen text, and this type is on the path to
    /// an error body.
    pub unrecognised_member_present: bool,
    /// Members that were PRESENT but of the wrong JSON type. Static names only,
    /// from [`SUPPORTED_METADATA`].
    pub malformed_members: Vec<&'static str>,
}

/// Validate submitted client metadata.
///
/// Collects EVERY fault rather than returning the first. A registration form
/// with three bad redirect URIs should be fixable in one round trip, and a
/// caller that has to rediscover the next problem by resubmitting learns to
/// stop reading the message.
pub fn validate(submitted: &SubmittedMetadata) -> Result<ValidatedMetadata, Vec<FieldFault>> {
    let mut faults: Vec<FieldFault> = Vec::new();
    let fault = |field, index, fault| FieldFault { field, index, fault };

    // ── Metadata this server cannot honour ──────────────────────────────────
    for name in &submitted.critical_members_present {
        faults.push(fault(name, None, MetadataFault::UnimplementedCriticalMetadata));
    }
    if submitted.unrecognised_member_present {
        faults.push(fault("client_metadata", None, MetadataFault::UnrecognisedMember));
    }

    // ── Present but the wrong type ──────────────────────────────────────────
    //
    // Refused BEFORE anything is defaulted. Round 1 (`gpt56`) found the first
    // version reading a present-but-wrong-typed member as ABSENT, so
    // `grant_types: "password"` silently became the supported default and
    // `token_endpoint_auth_method: 42` silently became `none` — the weakest
    // method, chosen by a malformed request rather than by the caller. That is
    // the sprint's signature failure: absence and malformation must never be
    // the same thing.
    for name in &submitted.malformed_members {
        faults.push(fault(name, None, MetadataFault::MalformedMember));
    }

    // ── Name ────────────────────────────────────────────────────────────────
    let name = submitted.name.as_deref().unwrap_or("").trim().to_string();
    if name.is_empty()
        || name.chars().count() > MAX_CLIENT_NAME_LEN
        || name.chars().any(|c| c.is_control())
    {
        faults.push(fault("client_name", None, MetadataFault::ClientName));
    }

    // ── Redirect URIs ───────────────────────────────────────────────────────
    if submitted.redirect_uris.is_empty() {
        faults.push(fault("redirect_uris", None, MetadataFault::NoRedirectUris));
    }
    if submitted.redirect_uris.len() > MAX_REDIRECT_URIS {
        faults.push(fault("redirect_uris", None, MetadataFault::TooManyRedirectUris));
    }
    for (index, uri) in submitted.redirect_uris.iter().enumerate().take(MAX_REDIRECT_URIS) {
        for f in redirect_uri_faults(uri) {
            faults.push(fault("redirect_uris", Some(index), f));
        }
        if submitted.redirect_uris[..index].iter().any(|earlier| earlier == uri) {
            faults.push(fault("redirect_uris", Some(index), MetadataFault::RedirectUriDuplicated));
        }
    }

    // ── Grant types ─────────────────────────────────────────────────────────
    //
    // Absence means the RFC 7591 default, which is `authorization_code` ALONE.
    //
    // Round 3 (`gpt56`): this defaulted to `authorization_code` PLUS
    // `refresh_token`, which grants a capability the client never asked for —
    // a widening on the absence path, and the exact direction this item has
    // been correcting everywhere else. A client that wants to refresh has to
    // say so, in the same request, like every other capability here.
    //
    // The convenience it cost is real and is the right trade: a connector that
    // cannot refresh degrades into "reauthorize every hour", which reads to a
    // user as an unreliable server. But that is a reason to DOCUMENT the
    // requirement (the `.env.example` and README both name it), not a reason
    // for the server to decide on the client's behalf — absence must never
    // grant more than was requested.
    let grant_types = match &submitted.grant_types {
        Some(requested) => {
            for (index, grant) in requested.iter().enumerate() {
                if !SUPPORTED_GRANT_TYPES.contains(&grant.as_str()) {
                    faults.push(fault("grant_types", Some(index), MetadataFault::UnsupportedGrantType));
                }
            }
            requested.clone()
        }
        None => vec![DEFAULT_GRANT_TYPE.to_string()],
    };

    // ── Response types ──────────────────────────────────────────────────────
    if let Some(requested) = &submitted.response_types {
        for (index, response_type) in requested.iter().enumerate() {
            if !SUPPORTED_RESPONSE_TYPES.contains(&response_type.as_str()) {
                faults.push(fault(
                    "response_types",
                    Some(index),
                    MetadataFault::UnsupportedResponseType,
                ));
            }
        }
    }

    // ── Token endpoint auth method ──────────────────────────────────────────
    //
    // RFC 7591's default is `client_secret_basic`. This server defaults to
    // `none` instead, deliberately: the connector this door exists for is a
    // PUBLIC client, and defaulting to a confidential method would mint a
    // secret nobody asked for and hand it out in a response. Choosing to hold a
    // credential should be something a caller SAID.
    //
    // ABSENT takes the default; PRESENT must be usable. Round 5 (`gpt56`): the
    // `.filter(|m| !m.is_empty())` that used to sit here turned a blank value
    // into an absent one, so `token_endpoint_auth_method: ""` selected `none`
    // — registering as PUBLIC, with no client authentication, a client that
    // had said nothing meaningful. `register`'s reader now refuses blanks
    // before this point; the arm below is what makes the same true for the
    // TOOL path, which builds `SubmittedMetadata` directly and never passes
    // through that reader.
    let token_endpoint_auth_method = match submitted.token_endpoint_auth_method.as_deref() {
        None => DEFAULT_AUTH_METHOD.to_string(),
        Some(raw) if raw.trim().is_empty() => {
            faults.push(fault(
                "token_endpoint_auth_method",
                None,
                MetadataFault::MalformedMember,
            ));
            // Never used — `faults` is non-empty, so this returns `Err` below.
            // Present so the binding has a value without an `unwrap`.
            DEFAULT_AUTH_METHOD.to_string()
        }
        Some(raw) => raw.trim().to_string(),
    };
    if !SUPPORTED_AUTH_METHODS.contains(&token_endpoint_auth_method.as_str()) {
        faults.push(fault(
            "token_endpoint_auth_method",
            None,
            MetadataFault::UnsupportedAuthMethod,
        ));
    }

    if !faults.is_empty() {
        return Err(faults);
    }

    Ok(ValidatedMetadata { name, redirect_uris: submitted.redirect_uris.clone(), grant_types, token_endpoint_auth_method })
}

/// Every fault in one redirect URI.
///
/// The order of the checks is not a security property here (unlike
/// [`crate::oauth::authorize::validate`]'s), because all of them refuse — but
/// the SET is: an `https` URI and a loopback URI are the only two shapes that
/// pass, and everything else falls through to
/// [`MetadataFault::RedirectUriNotHttpsOrLoopback`]. That is an allowlist, and
/// it is written as one so that a scheme nobody thought about — `javascript:`,
/// `data:`, a custom app scheme — is refused by default rather than by a
/// denylist entry somebody has to have anticipated.
fn redirect_uri_faults(uri: &str) -> Vec<MetadataFault> {
    let mut faults = Vec::new();

    if uri.is_empty() || uri.len() > MAX_REDIRECT_URI_LEN {
        faults.push(MetadataFault::RedirectUriLength);
        // Length is the one fault worth returning alone: the checks below walk
        // the string, and there is nothing useful to say about a value that is
        // empty or absurd.
        return faults;
    }
    if uri.chars().any(|c| c.is_whitespace() || c.is_control()) {
        faults.push(MetadataFault::RedirectUriMalformed);
    }
    if uri.contains('#') {
        faults.push(MetadataFault::RedirectUriHasFragment);
    }
    if uri.contains('*') {
        faults.push(MetadataFault::RedirectUriHasWildcard);
    }
    if crate::oauth::authorize::redirect_uri_has_reserved_parameter(uri) {
        faults.push(MetadataFault::RedirectUriHasReservedParameter);
    }

    // The allowlist. `https://` with a real authority and no userinfo, or the
    // loopback form the matcher itself will later recognise — asked of
    // `authorize`, never re-derived here (see `is_loopback_redirect_uri`).
    let acceptable = is_absolute_https(uri) || crate::oauth::authorize::is_loopback_redirect_uri(uri);
    if !acceptable {
        faults.push(MetadataFault::RedirectUriNotHttpsOrLoopback);
    }
    faults
}

/// Whether this is an absolute `https://` URI with a real authority and no
/// userinfo.
///
/// The userinfo rule is the one that matters and is the same one
/// [`crate::oauth::authorize`]'s loopback parser enforces, for the same reason:
/// a URI can put a trusted-looking name in its USERINFO segment and a hostname
/// of the attacker's choosing after the `@`. It reads as trusted to anybody
/// comparing prefixes, and its real host is not the one a human sees. Refusing
/// any `@` in the authority closes it without needing to be clever.
///
/// The scheme is matched case-sensitively against lowercase `https://`. A
/// mixed-case `HTTPS://` is refused rather than normalized: the registered
/// string is compared byte-for-byte at authorization time, so storing a value
/// that differs from what the client will send would fail every request with a
/// mismatch nobody can see.
fn is_absolute_https(uri: &str) -> bool {
    let Some(after_scheme) = uri.strip_prefix("https://") else {
        return false;
    };
    let authority_end = after_scheme
        .find(|c: char| c == '/' || c == '?' || c == '#')
        .unwrap_or(after_scheme.len());
    let authority = &after_scheme[..authority_end];
    !authority.is_empty() && !authority.contains('@')
}

// ---------------------------------------------------------------------------
// The service
// ---------------------------------------------------------------------------

/// A newly created client, and its secret if one was minted.
///
/// The ONLY place a plaintext client secret exists. It is moved out by the
/// caller, rendered into exactly one response, and dropped.
pub struct MintedClient {
    pub client: ClientAdmin,
    /// `None` for a public client. Present exactly once, here, and never
    /// obtainable again from any read path.
    pub secret: Option<String>,
}

/// A client plus its scope assignments, as the administration surface reports
/// it.
pub struct ClientView {
    pub client: ClientAdmin,
    pub tool_group_ids: Vec<Uuid>,
    pub namespaces: Vec<String>,
}

/// Client lifecycle over the store. One implementation, two transports.
#[derive(Clone)]
pub struct ClientService {
    store: OauthStore,
}

impl ClientService {
    pub fn new(store: OauthStore) -> Self {
        Self { store }
    }

    /// Resolve an owning account by name.
    ///
    /// A missing account is [`ToolError::NotFound`] rather than a fallback to
    /// anything. There is no "default owner": choosing whose connector this is
    /// is an authority decision, and every path that needs one asks for it by
    /// name.
    /// A DISABLED account resolves to `NotFound`, exactly as a missing one
    /// does. An account that cannot authenticate must not become a connector's
    /// owner or an edit's actor — and collapsing the two answers keeps this
    /// from becoming an account-existence oracle, matching every other lookup
    /// in this subsystem.
    pub async fn resolve_owner(&self, name: &str) -> Result<Uuid, ToolError> {
        self.store
            .find_active_account_by_name(name)
            .await?
            .map(|account| account.id)
            .ok_or_else(|| ToolError::NotFound("no such account".into()))
    }

    /// Operator minting.
    ///
    /// `owner` is stated by the caller and is never inferred. There is no
    /// "default account" fallback: choosing whose connector this is, is an
    /// authority decision, and a service that guessed one would be making it
    /// silently — a scoping model whose owner is a guess is not a scoping
    /// model.
    pub async fn mint(
        &self,
        actor: Uuid,
        owner: Uuid,
        metadata: &ValidatedMetadata,
    ) -> Result<MintedClient, ToolError> {
        self.create(actor, owner, metadata, RegistrationSource::Operator).await
    }

    /// RFC 7591 registration, having already spent an initial access token.
    ///
    /// Takes the issuing account as the owner: whoever minted the invitation
    /// owns what walks through it. The client lands with no scope rows, which
    /// is what "reaches nothing until an operator scopes it" means here.
    /// The authorization here is the INITIAL ACCESS TOKEN, already spent by the
    /// caller — an operator-minted, single-use, expiring invitation. The issuing
    /// account is therefore both the actor and the owner: whoever minted the
    /// invitation owns what walks through it, and no third party's name can be
    /// attached to the result.
    pub async fn register_dynamic(
        &self,
        issued_by: Uuid,
        metadata: &ValidatedMetadata,
    ) -> Result<MintedClient, ToolError> {
        self.create(issued_by, issued_by, metadata, RegistrationSource::Dcr).await
    }

    /// The one creation path. Both public entry points reach it, so a rule
    /// added here cannot apply to only one way in.
    async fn create(
        &self,
        actor: Uuid,
        owner: Uuid,
        metadata: &ValidatedMetadata,
        source: RegistrationSource,
    ) -> Result<MintedClient, ToolError> {
        // A fresh `client_id` for every call, INCLUDING a resubmission of
        // byte-identical metadata. RFC 7591 has no merge semantics, and a
        // silent merge would let a second registration take over an existing
        // client's identity by describing itself the same way.
        let client_id = format!("rmcp-{}", random_token(CLIENT_ID_BYTES)?);

        let secret = if metadata.wants_secret() {
            Some(random_token(CLIENT_SECRET_BYTES)?)
        } else {
            None
        };
        // argon2id, via the same hasher account passwords use. The plaintext is
        // never handed to the store — only the hash — so there is no store
        // method that could write one by accident.
        let hash = match secret.as_deref() {
            Some(plaintext) => Some(hash_password(plaintext)?),
            None => None,
        };

        let id = self
            .store
            .insert_client(
                actor,
                &client_id,
                hash.as_ref(),
                &metadata.name,
                &metadata.redirect_uris,
                &metadata.grant_types,
                &metadata.token_endpoint_auth_method,
                owner,
                source.as_str(),
            )
            .await?;

        let client = self
            .store
            .find_client_admin(id)
            .await?
            .ok_or_else(|| ToolError::Database("the client vanished immediately after insertion".into()))?;

        Ok(MintedClient { client, secret })
    }

    /// List clients with their scope assignments.
    pub async fn list(&self, owner: Option<Uuid>) -> Result<Vec<ClientView>, ToolError> {
        let clients = self.store.list_clients_admin(owner).await?;
        let mut views = Vec::with_capacity(clients.len());
        for client in clients {
            views.push(self.view(client).await?);
        }
        Ok(views)
    }

    /// One client with its scope assignments.
    pub async fn get(&self, id: Uuid) -> Result<ClientView, ToolError> {
        let client = self
            .store
            .find_client_admin(id)
            .await?
            .ok_or_else(|| ToolError::NotFound("no such client".into()))?;
        self.view(client).await
    }

    async fn view(&self, client: ClientAdmin) -> Result<ClientView, ToolError> {
        let tool_group_ids = self
            .store
            .client_tool_groups(client.id)
            .await?
            .into_iter()
            .map(|g| g.id)
            .collect();
        let namespaces = self.store.client_namespaces(client.id).await?;
        Ok(ClientView { client, tool_group_ids, namespaces })
    }

    /// Apply an administrative edit — atomically.
    ///
    /// Round 1 (`gpt56`) found the first version writing the client's fields,
    /// then its tool groups, then its namespaces, as three separate
    /// transactions. A failure partway left the client with its new enabled
    /// state and redirect URIs and its OLD scope: a half-applied authorization
    /// change that looks, from either side, like a deliberate configuration.
    ///
    /// It is now one call and one transaction
    /// ([`OauthStore::apply_client_admin_edit`]) — the whole edit commits or
    /// none of it does. The version is checked by that same statement, so two
    /// operators editing one connector cannot both succeed against one version;
    /// the loser is told, rather than having its predecessor's scoping silently
    /// reinstated.
    ///
    /// Ownership is re-read inside that transaction under a row lock. This
    /// method deliberately does not re-derive it: one place decides, and it is
    /// the one holding the lock.
    #[allow(clippy::too_many_arguments)]
    pub async fn update(
        &self,
        actor: Uuid,
        id: Uuid,
        expected_version: i32,
        enabled: Option<bool>,
        redirect_uris: Option<&[String]>,
        tool_group_ids: Option<&[Uuid]>,
        namespaces: Option<&[String]>,
    ) -> Result<ClientView, ToolError> {
        let updated = self
            .store
            .apply_client_admin_edit(
                actor,
                id,
                expected_version,
                // The tool speaks `enabled`; the column is `disabled`. Negated
                // in exactly one place, here, so no other caller has to
                // remember which polarity it is holding.
                enabled.map(|e| !e),
                redirect_uris,
                tool_group_ids,
                namespaces,
            )
            .await?;

        let Some(updated) = updated else {
            // One message for "gone" and "moved on". The caller re-reads either
            // way, and the two are indistinguishable to the operator's next
            // action: reload and re-apply.
            return Err(ToolError::Conflict(
                "this client has changed since it was read (or no longer exists); reload and \
                 re-apply the edit"
                    .into(),
            ));
        };

        self.view(updated).await
    }

    /// Revoke a client: disable it, and kill its live sessions.
    ///
    /// `actor` is REQUIRED and is authorized against the client inside the
    /// writing transaction. Round 2 (`gpt56`) found this taking only a client
    /// id, with no actor and no authority check of any kind. Revocation only
    /// ever narrows access — which is why it is not approval-gated — but "only
    /// narrows" is not "anyone may": disabling somebody else's connector is a
    /// denial of service against their linked account.
    ///
    /// Both halves are applied in one transaction by
    /// [`OauthStore::revoke_client`]. Disabling stops
    /// [`OauthStore::find_active_client`] resolving it, which denies the caller
    /// at its NEXT request (RMCP-05 re-reads client state on the dispatch
    /// path); revoking the refresh tokens stops the session being extended, so
    /// a later re-enable cannot silently resurrect access somebody had already
    /// been cut off from.
    ///
    /// Idempotent: revoking an already-revoked client succeeds and reports what
    /// it changed.
    pub async fn revoke(&self, actor: Uuid, id: Uuid) -> Result<u64, ToolError> {
        self.store.revoke_client(actor, id).await
    }

    /// Mint an initial access token for RFC 7591 registration.
    ///
    /// Returns the plaintext, which — like a client secret — exists only in
    /// this return value. Only its SHA-256 digest is stored.
    ///
    /// `issued_by` must be an OPERATOR account, verified inside the writing
    /// transaction ([`OauthStore::insert_registration_token`]). Round 2
    /// (`gpt56`) found no authority check here at all — and this is the call
    /// that makes gated DCR reachable, so an unauthorized mint hands out the
    /// ability to create clients.
    pub async fn mint_registration_token(
        &self,
        issued_by: Uuid,
        label: &str,
        uses: i32,
        ttl_seconds: i64,
    ) -> Result<String, ToolError> {
        if !(1..=MAX_IAT_USES).contains(&uses) {
            return Err(ToolError::InvalidArgument(format!(
                "uses must be between 1 and {MAX_IAT_USES}"
            )));
        }
        if !(1..=MAX_IAT_TTL_SECONDS).contains(&ttl_seconds) {
            return Err(ToolError::InvalidArgument(format!(
                "ttl_seconds must be between 1 and {MAX_IAT_TTL_SECONDS}"
            )));
        }
        let token = random_token(CLIENT_SECRET_BYTES)?;
        self.store
            .insert_registration_token(
                &SecretHash::of(&token),
                issued_by,
                label,
                uses,
                ttl_seconds,
            )
            .await?;
        Ok(token)
    }

    /// Spend one use of a presented initial access token.
    ///
    /// Returns the issuing account, or `None` for a token that is unknown,
    /// expired, revoked or exhausted. The four are one answer by design — see
    /// [`OauthStore::claim_registration_token`].
    pub async fn claim_registration_token(&self, presented: &str) -> Result<Option<Uuid>, ToolError> {
        self.store.claim_registration_token(&SecretHash::of(presented)).await
    }

    /// Revoke every outstanding initial access token. Operator-only, verified
    /// in the writing transaction.
    pub async fn revoke_registration_tokens(&self, actor: Uuid) -> Result<u64, ToolError> {
        self.store.revoke_all_registration_tokens(actor).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn submitted(uris: &[&str]) -> SubmittedMetadata {
        SubmittedMetadata {
            name: Some("A connector".into()),
            redirect_uris: uris.iter().map(|u| u.to_string()).collect(),
            ..Default::default()
        }
    }

    fn faults_for(uris: &[&str]) -> Vec<MetadataFault> {
        validate(&submitted(uris))
            .expect_err("expected a refusal")
            .into_iter()
            .map(|f| f.fault)
            .collect()
    }

    /// The acceptance criterion, stated as the two shapes that pass.
    #[test]
    fn https_and_rfc_8252_loopback_are_the_only_accepted_redirect_shapes() {
        for good in [
            "https://connector.test/callback",
            "https://connector.test:8443/callback?tenant=one",
            "http://127.0.0.1/callback",
            "http://127.0.0.1:49152/callback",
            "http://localhost:3000/cb",
            "http://[::1]:49152/cb",
        ] {
            assert!(
                validate(&submitted(&[good])).is_ok(),
                "{good} should have been accepted"
            );
        }
    }

    /// The named rejections from the test plan, each asserted as its OWN fault
    /// rather than as "something was refused" — a check that cannot tell which
    /// rule fired would pass if all of them collapsed into one.
    #[test]
    fn non_https_non_loopback_fragments_and_wildcards_are_refused() {
        // http:// to a non-loopback host — the headline case.
        assert!(faults_for(&["http://connector.test/cb"])
            .contains(&MetadataFault::RedirectUriNotHttpsOrLoopback));
        // A fragment.
        assert!(faults_for(&["https://connector.test/cb#section"])
            .contains(&MetadataFault::RedirectUriHasFragment));
        // A wildcard, in the host and in the path.
        assert!(faults_for(&["https://*.connector.test/cb"])
            .contains(&MetadataFault::RedirectUriHasWildcard));
        assert!(faults_for(&["https://connector.test/*"])
            .contains(&MetadataFault::RedirectUriHasWildcard));
        // Schemes that are not https at all.
        for bad in [
            "javascript:alert(1)",
            "data:text/html,x",
            "myapp://callback",
            "ftp://connector.test/cb",
            "/relative/cb",
            "HTTPS://connector.test/cb",
        ] {
            assert!(
                faults_for(&[bad]).contains(&MetadataFault::RedirectUriNotHttpsOrLoopback),
                "{bad} should have been refused as neither https nor loopback"
            );
        }
    }

    /// Userinfo is the trap this validation exists for on both arms: the URI
    /// LOOKS like it names a host it does not.
    ///
    /// The fixtures are ASSEMBLED rather than written out, so no `@`-bearing
    /// host string appears as a literal anywhere in this tree. That is not
    /// squeamishness — the repository's own PII gate matches an email shape, a
    /// userinfo authority is exactly that shape, and a security test that trips
    /// the PII gate is a test somebody deletes rather than fixes. (This comment
    /// used to spell the shape out and tripped the gate itself, which is the
    /// most on-the-nose way that lesson could have been delivered.)
    #[test]
    fn a_userinfo_authority_is_refused_on_both_the_https_and_loopback_arms() {
        let at = '@';
        let https_userinfo = format!("https://connector.test{at}elsewhere.test/cb");
        let loopback_userinfo = format!("http://127.0.0.1{at}elsewhere.test/cb");

        assert!(faults_for(&[&https_userinfo])
            .contains(&MetadataFault::RedirectUriNotHttpsOrLoopback));
        // The loopback arm is `authorize`'s parser, not a second one here —
        // this asserts we are actually asking it.
        assert!(faults_for(&[&loopback_userinfo])
            .contains(&MetadataFault::RedirectUriNotHttpsOrLoopback));
        assert!(!crate::oauth::authorize::is_loopback_redirect_uri(&loopback_userinfo));
    }

    /// A URI carrying a reserved response parameter is refused at REGISTRATION,
    /// because `authorize` refuses it at request time. Registering one mints a
    /// client that can never complete a flow.
    #[test]
    fn a_reserved_response_parameter_is_refused_at_registration() {
        assert!(faults_for(&["https://connector.test/cb?code=x"])
            .contains(&MetadataFault::RedirectUriHasReservedParameter));
        assert!(faults_for(&["https://connector.test/cb?state=x"])
            .contains(&MetadataFault::RedirectUriHasReservedParameter));
    }

    /// Bounds: none, too many, too long, duplicated.
    #[test]
    fn redirect_uri_bounds_are_enforced() {
        assert!(faults_for(&[]).contains(&MetadataFault::NoRedirectUris));

        let many: Vec<String> = (0..MAX_REDIRECT_URIS + 1)
            .map(|i| format!("https://connector.test/cb{i}"))
            .collect();
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        assert!(faults_for(&refs).contains(&MetadataFault::TooManyRedirectUris));

        let long = format!("https://connector.test/{}", "a".repeat(MAX_REDIRECT_URI_LEN));
        assert!(faults_for(&[&long]).contains(&MetadataFault::RedirectUriLength));

        assert!(faults_for(&["https://connector.test/cb", "https://connector.test/cb"])
            .contains(&MetadataFault::RedirectUriDuplicated));
    }

    /// Every fault is reported, not just the first — a form with three problems
    /// is fixable in one round trip.
    #[test]
    fn validation_reports_every_fault_rather_than_the_first() {
        let faults = validate(&SubmittedMetadata {
            name: Some(String::new()),
            redirect_uris: vec!["http://connector.test/cb".into(), "https://x.test/cb#f".into()],
            grant_types: Some(vec!["password".into()]),
            ..Default::default()
        })
        .expect_err("must refuse");
        let kinds: Vec<MetadataFault> = faults.iter().map(|f| f.fault).collect();
        assert!(kinds.contains(&MetadataFault::ClientName));
        assert!(kinds.contains(&MetadataFault::RedirectUriNotHttpsOrLoopback));
        assert!(kinds.contains(&MetadataFault::RedirectUriHasFragment));
        assert!(kinds.contains(&MetadataFault::UnsupportedGrantType));
    }

    /// OAuth 2.1's removed grants must not be registrable, and the supported
    /// list must be the one the metadata advertises — a client registered for a
    /// grant the metadata never offered, or refused one it did, is a
    /// disagreement the client experiences as a broken server.
    #[test]
    fn only_the_advertised_grants_are_registrable() {
        for bad in ["password", "implicit", "client_credentials", "urn:ietf:params:oauth:grant-type:device_code"] {
            let mut s = submitted(&["https://connector.test/cb"]);
            s.grant_types = Some(vec![bad.into()]);
            let kinds: Vec<MetadataFault> =
                validate(&s).expect_err("must refuse").into_iter().map(|f| f.fault).collect();
            assert!(kinds.contains(&MetadataFault::UnsupportedGrantType), "{bad} was accepted");
        }
        assert_eq!(SUPPORTED_GRANT_TYPES, ["authorization_code", "refresh_token"]);
    }

    /// **Absence of `grant_types` grants the RFC's default and nothing more.**
    ///
    /// Round 3 (`gpt56`): this used to default to `authorization_code` PLUS
    /// `refresh_token`, handing every registration a capability it never asked
    /// for. Refresh is not cosmetic — it is what lets a client keep acting
    /// without the human present — so granting it on the absence path is a
    /// widening, and this item refuses widenings on absence paths everywhere
    /// else.
    ///
    /// The mutation target: put `refresh_token` back into the `None` arm and
    /// this goes red.
    #[test]
    fn an_absent_grant_type_list_defaults_narrow_not_wide() {
        let validated = validate(&submitted(&["https://connector.test/cb"])).expect("valid");
        assert_eq!(
            validated.grant_types,
            ["authorization_code"],
            "an absent grant_types must not grant refresh capability"
        );
        assert!(
            !validated.grant_types.iter().any(|g| g == "refresh_token"),
            "refresh must be asked for, never defaulted in"
        );
        for grant in &validated.grant_types {
            assert!(SUPPORTED_GRANT_TYPES.contains(&grant.as_str()));
        }

        // …and a client that WANTS refresh gets it by saying so, which is the
        // half that must keep working.
        let mut asks = submitted(&["https://connector.test/cb"]);
        asks.grant_types =
            Some(vec!["authorization_code".into(), "refresh_token".into()]);
        assert_eq!(
            validate(&asks).expect("valid").grant_types,
            ["authorization_code", "refresh_token"]
        );
    }

    /// The default is a PUBLIC client. Defaulting the other way would mint a
    /// secret nobody asked for.
    #[test]
    fn the_default_client_is_public_and_a_secret_must_be_asked_for() {
        let validated = validate(&submitted(&["https://connector.test/cb"])).expect("valid");
        assert_eq!(validated.token_endpoint_auth_method, "none");
        assert!(!validated.wants_secret());

        let mut s = submitted(&["https://connector.test/cb"]);
        s.token_endpoint_auth_method = Some("client_secret_basic".into());
        assert!(validate(&s).expect("valid").wants_secret());

        let mut bad = submitted(&["https://connector.test/cb"]);
        bad.token_endpoint_auth_method = Some("private_key_jwt".into());
        let kinds: Vec<MetadataFault> =
            validate(&bad).expect_err("must refuse").into_iter().map(|f| f.fault).collect();
        assert!(kinds.contains(&MetadataFault::UnsupportedAuthMethod));
    }

    /// Critical metadata this server cannot honour is REFUSED, not ignored.
    #[test]
    fn unimplemented_critical_metadata_is_refused_rather_than_dropped() {
        for member in UNIMPLEMENTED_CRITICAL_METADATA {
            let mut s = submitted(&["https://connector.test/cb"]);
            s.critical_members_present = vec![member];
            let faults = validate(&s).expect_err("must refuse");
            assert!(
                faults.iter().any(|f| f.fault == MetadataFault::UnimplementedCriticalMetadata
                    && f.field == *member),
                "{member} was ignored instead of refused"
            );
        }
    }

    /// A rejection message must carry NOTHING the caller wrote. This is the
    /// same rule the audit vocabulary enforces, applied to the error body,
    /// because the error body is the other place a submitted value could
    /// surface — in a log, in a terminal, in a GUI toast.
    #[test]
    fn a_rejection_never_echoes_the_submitted_value() {
        let secretish = "<REDACTED-SECRET>";
        let faults = validate(&SubmittedMetadata {
            name: Some("distinctive-name-value".repeat(20)),
            redirect_uris: vec![secretish.into()],
            ..Default::default()
        })
        .expect_err("must refuse");
        for fault in &faults {
            let rendered = fault.render();
            assert!(
                !rendered.contains("distinctive-marker-value")
                    && !rendered.contains("distinctive-name-value"),
                "the rejection echoed the submitted value: {rendered}"
            );
        }
    }

    /// Locating a fault by index is what makes a fixed-message refusal usable.
    #[test]
    fn a_fault_is_located_by_field_and_index() {
        let faults = validate(&submitted(&[
            "https://connector.test/cb",
            "http://connector.test/cb",
        ]))
        .expect_err("must refuse");
        let located = faults
            .iter()
            .find(|f| f.fault == MetadataFault::RedirectUriNotHttpsOrLoopback)
            .expect("the second URI must be refused");
        assert_eq!(located.field, "redirect_uris");
        assert_eq!(located.index, Some(1));
        assert_eq!(
            located.render(),
            "redirect_uris[1]: must be an absolute https URI, or an RFC 8252 http loopback URI"
        );
    }

    /// The loopback rule is ASKED of `authorize`, not re-derived. This is the
    /// mutation target for that claim: replace the call with a prefix check
    /// here and the userinfo case above goes green while the real matcher still
    /// refuses it.
    #[test]
    fn the_loopback_rule_is_the_matchers_own() {
        for uri in ["http://127.0.0.1:1/cb", "http://localhost/cb", "http://[::1]/cb"] {
            assert!(crate::oauth::authorize::is_loopback_redirect_uri(uri));
            assert!(validate(&submitted(&[uri])).is_ok());
        }
        let at = '@';
        // RFC 5737 TEST-NET-3 for the "routable, not loopback" case, and an
        // assembled userinfo host for the same reason as the test above.
        for uri in [
            format!("http://127.0.0.1{at}elsewhere.test/cb"),
            "http://203.0.113.7/cb".to_string(),
            "https://127.0.0.1/cb".to_string(),
        ] {
            assert!(
                !crate::oauth::authorize::is_loopback_redirect_uri(&uri),
                "{uri} must not be read as loopback"
            );
        }
    }

    /// A generated `client_id` is not guessable and not sequential — a
    /// registration endpoint whose ids can be enumerated hands an attacker the
    /// list of connectors to attack.
    #[test]
    fn generated_client_ids_are_unguessable_and_distinct() {
        let a = format!("rmcp-{}", random_token(CLIENT_ID_BYTES).expect("entropy"));
        let b = format!("rmcp-{}", random_token(CLIENT_ID_BYTES).expect("entropy"));
        assert_ne!(a, b);
        assert!(a.len() > 16, "a client id must carry real entropy: {a}");
    }

    /// A minted secret must be a real high-entropy value and must hash to a
    /// structurally valid argon2id PHC string that verifies — the round trip
    /// the store's type enforcement depends on.
    #[test]
    fn a_minted_secret_hashes_to_a_verifiable_argon2id_string() {
        let secret = random_token(CLIENT_SECRET_BYTES).expect("entropy");
        assert!(secret.len() >= 43, "32 bytes of entropy, base64url");
        let hash = hash_password(&secret).expect("hashing");
        assert!(hash.as_str().starts_with("$argon2id$"));
        assert!(crate::oauth::password::verify_password(&secret, hash.as_str()));
        assert!(!hash.as_str().contains(&secret), "the hash must not contain the plaintext");
    }

    /// **A DCR-registered client reaches nothing until an operator scopes it.**
    ///
    /// The acceptance criterion, asserted against RMCP-07's REAL resolver
    /// rather than against this module's prose. A dynamically registered client
    /// lands with no scope rows, and this is what those rows resolve to: the
    /// empty set, against an account grant that permits everything, so the
    /// denial can only be coming from the client's own scoping.
    ///
    /// That last part is the mutation target. Point `permits_everything` at a
    /// restrictive grant and the test would pass for the wrong reason — it
    /// would be proving the account is limited, not the client.
    #[test]
    fn a_dcr_client_reaches_nothing_until_scoped() {
        use crate::oauth::groups::CatalogTool;
        use crate::oauth::scope::{decide, ClientScope, Decision};

        let permits_everything = |_: &str| true;
        // Exactly what a freshly registered client's rows are: no groups, no
        // namespaces.
        let unscoped = ClientScope::from_rows("rmcp-freshly-registered", &[], Vec::new());

        // Both sides of the boundary: local entries, and one contributed by an
        // upstream — an unscoped client reaches neither.
        for tool in [
            CatalogTool::local("pg_query"),
            CatalogTool::local("ledger_read"),
            CatalogTool::local("vitals_summary"),
            CatalogTool::from_upstream("somenamespace", "some_tool"),
            CatalogTool::local("utc_now"),
        ] {
            assert!(
                matches!(decide(&permits_everything, &unscoped, &tool), Decision::Deny(_)),
                "an unscoped client reached {}",
                tool.name
            );
        }

        // And the account grant really was permissive, so the denials above are
        // the CLIENT's scoping and not a restrictive account.
        assert!(permits_everything("pg_query"));
    }

    /// The bounds on an initial access token are enforced before anything is
    /// written, so an unbounded-use or never-expiring invitation cannot be
    /// minted by asking for one.
    #[test]
    fn initial_access_token_bounds_are_enforced() {
        assert!(MAX_IAT_USES >= 1);
        assert!(DEFAULT_IAT_TTL_SECONDS <= MAX_IAT_TTL_SECONDS);
        for uses in [0, -1, MAX_IAT_USES + 1] {
            assert!(!(1..=MAX_IAT_USES).contains(&uses), "{uses} must be out of bounds");
        }
        for ttl in [0, -1, MAX_IAT_TTL_SECONDS + 1] {
            assert!(!(1..=MAX_IAT_TTL_SECONDS).contains(&ttl), "{ttl} must be out of bounds");
        }
    }
}
