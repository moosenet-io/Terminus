//! RMCP-11 — per-endpoint rate limiting for the OAuth door.
//!
//! ## Why the endpoints do not share one budget
//!
//! The reachable OAuth endpoints have nothing in common operationally.
//! `/oauth/token` is called by Anthropic's infrastructure on a schedule and
//! should be generous; `/oauth/login` is a password-verification endpoint hit by
//! one human at a time and should be tight; `/oauth/register` is a write a
//! legitimate deployment performs a handful of times ever. A single shared
//! bucket has to be sized for the most generous of those, which means it never
//! constrains the tightest — a credential-stuffing run against the login form
//! would sit comfortably inside a budget sized for token refreshes. So each
//! endpoint gets its own [`EndpointBudgets`].
//!
//! ## Two dimensions, deliberately unequal, checked in one order
//!
//! Every check consults an ADDRESS bucket and, where the request names one, a
//! SUBJECT bucket (the account name at the login form, the `client_id` at the
//! token and register endpoints). Neither alone is sufficient: an address-only
//! limit lets a distributed attacker grind one account, and a subject-only limit
//! lets one address enumerate a thousand accounts one attempt each.
//!
//! Two rules make the pair work together rather than interfere:
//!
//! 1. **The subject budget is strictly larger than the address budget, and this
//!    is ENFORCED, not merely documented.** If they were equal, one address
//!    exhausting its own budget would also exhaust the victim's, and an attacker
//!    could lock a named account out from a single host for free. Sizing the
//!    subject budget as a multiple of the address budget means it takes several
//!    distinct source addresses to degrade one account at all.
//!
//!    [`EndpointBudgets::validate`] refuses any pair that breaks the ordering,
//!    in both burst and refill rate, at every construction site — an env
//!    override or a direct call to [`OauthRateLimiter::from_budgets`] alike.
//!    Review round 1 (`gpt56`) found this stated in prose and enforced nowhere,
//!    which meant a single well-meaning override could silently reopen the hole
//!    the whole two-dimension design exists to close.
//! 2. **The address bucket is checked FIRST and a denial short-circuits**, so a
//!    request already refused never spends subject budget. Without this, the
//!    ratio in (1) would be worthless: one address could keep consuming a
//!    victim's budget forever after its own ran out.
//!
//! A sufficiently distributed attacker can still degrade one account's login
//! budget. That is inherent to per-account limiting and is accepted
//! deliberately: the subject budget REFILLS, so this degrades to slow service,
//! never to a lockout. An account lockout would convert the same attack into a
//! denial of service with no recovery, which is worse than what it prevents.
//!
//! ## The subject key is a digest
//!
//! The subject arrives from the request body — an attacker chooses it, and can
//! choose a megabyte of it. It is hashed ([`crate::oauth::secret_hash`]) before
//! it becomes a key, which bounds per-key memory to a constant and, as a side
//! effect, keeps account names out of the limiter's memory.
//!
//! ## Restart re-arms; nothing fails open across the gap
//!
//! Bucket state is in-process, so a restart loses it and every key starts full.
//! Two things make that acceptable rather than a hole:
//!
//! * The limiter is always CONSTRUCTED, from defaults, with no configuration
//!   that can turn it off. There is no disable switch, and an unparseable budget
//!   falls back to the built-in default rather than to "unlimited" — a typo in a
//!   tuning knob must not silently remove the control. This is the opposite of
//!   the fallback rule [`crate::oauth::store`] applies to its pool size, and for
//!   the opposite reason: that knob grants nothing, this one denies.
//! * The cost of the gap is bounded to one burst per endpoint per restart, which
//!   is the burst a legitimate client is entitled to anyway.
//!
//! What is NOT acceptable, and is why this is written down, is the tempting
//! shape where the limiter is optional (`Option<Arc<dyn RateLimiter>>`) and
//! `None` means "skip the check". A restart, a config error, or a failed
//! construction would then admit everything. The type here has no such state.

use std::collections::HashMap;
use std::net::IpAddr;

use crate::error::ToolError;
use crate::gateway_framework::rate_limit::{
    rate_limit_key, InProcessRateLimiter, RateLimitDecision, RateLimiter,
};
use crate::oauth::audit::{AuditDetail, DenialReason, LimitDimension, OauthAuditRecord, OauthEvent};
use crate::oauth::secret_hash;

/// Ceiling on distinct keys per bucket table.
///
/// The door is internet-facing and the address dimension is attacker-chosen, so
/// this is the bound RMCP-09's runbook asked for (see
/// [`crate::gateway_framework::rate_limit::DEFAULT_MAX_KEYS`] for the eviction
/// rule that makes hitting a ceiling safe). Per table rather than global so a
/// flood against `/oauth/token` cannot evict the login endpoint's state and
/// hand a credential-stuffing run a fresh budget.
const MAX_KEYS_PER_TABLE: usize = 20_000;

/// One rate-limited endpoint on the OAuth door.
///
/// `Login` is the POST that verifies a password, kept distinct from `Authorize`
/// (the GET that renders the form) because only one of them is a credential
/// oracle and only one of them deserves a tight budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OauthEndpoint {
    /// `GET /oauth/authorize` — renders login/consent.
    Authorize,
    /// `POST /oauth/login` — verifies a password and optional second factor.
    Login,
    /// `POST /oauth/token` — code exchange and refresh.
    Token,
    /// `POST /oauth/register` — RFC 7591 dynamic client registration.
    Register,
    /// `POST /oauth/revoke` — RFC 7009 revocation.
    Revoke,
}

impl OauthEndpoint {
    /// Every endpoint, so a caller building limiters cannot miss one.
    pub const ALL: [OauthEndpoint; 5] = [
        OauthEndpoint::Authorize,
        OauthEndpoint::Login,
        OauthEndpoint::Token,
        OauthEndpoint::Register,
        OauthEndpoint::Revoke,
    ];

    /// Stable label used in keys, env var names, and audit records.
    pub fn as_str(self) -> &'static str {
        match self {
            OauthEndpoint::Authorize => "authorize",
            OauthEndpoint::Login => "login",
            OauthEndpoint::Token => "token",
            OauthEndpoint::Register => "register",
            OauthEndpoint::Revoke => "revoke",
        }
    }

    /// The env var overriding this endpoint's PER-ADDRESS budget.
    pub fn address_env_var(self) -> String {
        format!("RMCP_RATE_LIMIT_{}", self.as_str().to_uppercase())
    }

    /// The env var overriding this endpoint's PER-SUBJECT budget.
    pub fn subject_env_var(self) -> String {
        format!("RMCP_RATE_LIMIT_{}_SUBJECT", self.as_str().to_uppercase())
    }

    /// The built-in budgets. Sized from what each endpoint's legitimate traffic
    /// actually looks like, not from a uniform guess — and with the subject
    /// budget a multiple of the address budget, per rule (1) in the module docs.
    pub fn default_budgets(self) -> EndpointBudgets {
        match self {
            // A browser fetching the consent screen, plus its retries. One
            // human, a handful of loads.
            OauthEndpoint::Authorize => EndpointBudgets {
                per_address: Budget { burst: 20, refill_per_sec: 0.5 },
                per_subject: Budget { burst: 60, refill_per_sec: 1.5 },
            },
            // Password verification. Tight on purpose: a human mistypes a
            // password a few times, never sixty times a minute.
            //
            // The per-address numbers are RMCP-03's original `LOGIN_BURST` /
            // `LOGIN_REFILL_PER_SEC` verbatim (5, one refill every twenty
            // seconds), carried over unchanged when TERM #633 converged its
            // private limiter onto this table. An earlier revision of that
            // convergence used this module's own looser defaults, which would
            // have quietly RELAXED a merged item's deliberate anti-guessing
            // parameter under the banner of tidying it up. Converging two
            // definitions must not change the stricter one; the subject budget
            // is then sized above it to satisfy the invariant.
            OauthEndpoint::Login => EndpointBudgets {
                per_address: Budget { burst: 5, refill_per_sec: 0.05 },
                per_subject: Budget { burst: 15, refill_per_sec: 0.15 },
            },
            // Anthropic's infrastructure refreshes on its own schedule and a
            // connector may hold several sessions. Generous, still bounded.
            OauthEndpoint::Token => EndpointBudgets {
                per_address: Budget { burst: 60, refill_per_sec: 2.0 },
                per_subject: Budget { burst: 180, refill_per_sec: 6.0 },
            },
            // A legitimate deployment registers a client a handful of times in
            // its whole life. This is an unauthenticated WRITE when DCR is on.
            OauthEndpoint::Register => EndpointBudgets {
                per_address: Budget { burst: 5, refill_per_sec: 0.02 },
                per_subject: Budget { burst: 15, refill_per_sec: 0.06 },
            },
            // Revocation must stay available under duress — it is the "cut it
            // off now" control — so it is limited only enough to stop it being
            // used as an amplifier.
            OauthEndpoint::Revoke => EndpointBudgets {
                per_address: Budget { burst: 30, refill_per_sec: 1.0 },
                per_subject: Budget { burst: 90, refill_per_sec: 3.0 },
            },
        }
    }
}

/// A token-bucket budget: a burst size and a sustained rate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Budget {
    pub burst: u32,
    pub refill_per_sec: f64,
}

impl Budget {
    /// Parse an operator override of the form `burst:refill_per_sec`, e.g.
    /// `40:1.5`.
    ///
    /// Returns `None` for anything malformed, INCLUDING a zero or negative
    /// value. A zero burst would be an accidental total outage of the endpoint,
    /// and a caller reading `0` as "no limit" is exactly the misconfiguration
    /// this refuses to encode — so both directions of nonsense fall back to the
    /// built-in default instead of being honoured.
    pub fn parse(raw: &str) -> Option<Self> {
        let (burst, refill) = raw.trim().split_once(':')?;
        let burst: u32 = burst.trim().parse().ok().filter(|b| *b > 0)?;
        let refill_per_sec: f64 = refill
            .trim()
            .parse()
            .ok()
            .filter(|r: &f64| *r > 0.0 && r.is_finite())?;
        Some(Budget { burst, refill_per_sec })
    }
}

/// The two budgets one endpoint enforces.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EndpointBudgets {
    pub per_address: Budget,
    pub per_subject: Budget,
}

impl EndpointBudgets {
    /// Enforce rule (1) from the module docs: the subject budget must be
    /// STRICTLY larger than the address budget, in both burst and sustained
    /// rate.
    ///
    /// Not a style check. If the two are equal, a single host exhausting its own
    /// address budget has also exhausted the named account's subject budget, and
    /// that host can hold the account locked out for as long as it keeps
    /// spending — a free denial of service against any account whose name an
    /// attacker can guess. Everything the two-dimension design buys depends on
    /// this ordering, so it is checked rather than described.
    ///
    /// Both dimensions are checked: an equal REFILL rate reproduces the same
    /// hole in the steady state even when the bursts differ, because the
    /// attacker's sustained rate would exactly match the victim's recovery rate.
    ///
    /// The error names the endpoint and both values — none of which is a secret,
    /// and all of which an operator needs to fix it.
    pub fn validate(&self, endpoint: OauthEndpoint) -> Result<(), ToolError> {
        if self.per_subject.burst <= self.per_address.burst {
            return Err(ToolError::InvalidArgument(format!(
                "{}: the per-subject rate-limit burst ({}) must be strictly greater than the \
                 per-address burst ({}). An equal or smaller subject budget lets a single source \
                 address hold any named account locked out — set {} higher than {}, or leave both \
                 unset to use the built-in defaults",
                endpoint.as_str(),
                self.per_subject.burst,
                self.per_address.burst,
                endpoint.subject_env_var(),
                endpoint.address_env_var(),
            )));
        }
        if self.per_subject.refill_per_sec <= self.per_address.refill_per_sec {
            return Err(ToolError::InvalidArgument(format!(
                "{}: the per-subject refill rate ({}/s) must be strictly greater than the \
                 per-address refill rate ({}/s). An equal sustained rate reproduces the \
                 single-address lockout in the steady state — set {} higher than {}, or leave \
                 both unset to use the built-in defaults",
                endpoint.as_str(),
                self.per_subject.refill_per_sec,
                self.per_address.refill_per_sec,
                endpoint.subject_env_var(),
                endpoint.address_env_var(),
            )));
        }
        Ok(())
    }
}

/// What a check decided.
///
/// Carries a retry estimate for the caller's `Retry-After` header, and nothing
/// else. In particular it does NOT say which of the two buckets denied the
/// request: reporting "your account is throttled" as distinct from "your address
/// is throttled" would tell an unauthenticated caller that the account name it
/// guessed is one the server tracks — see [`Self::public_message`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RateLimitOutcome {
    Allowed,
    Limited { retry_after_secs: f64 },
}

impl RateLimitOutcome {
    pub fn is_limited(&self) -> bool {
        matches!(self, RateLimitOutcome::Limited { .. })
    }

    /// The HTTP status a limited request gets.
    pub fn http_status(&self) -> u16 {
        match self {
            RateLimitOutcome::Allowed => 200,
            RateLimitOutcome::Limited { .. } => 429,
        }
    }

    /// The body text handed to a limited caller.
    ///
    /// One fixed string for every limited request, whatever caused it. This is
    /// the acceptance criterion "a 429 must not leak whether an account exists",
    /// and it is enforced HERE — at the only place a message is produced —
    /// rather than left to each endpoint to remember, because a per-endpoint
    /// message is a per-endpoint opportunity to write "too many attempts for
    /// this account" and give the game away.
    pub fn public_message(&self) -> &'static str {
        "too many requests; retry later"
    }

    pub fn retry_after_secs(&self) -> Option<f64> {
        match self {
            RateLimitOutcome::Allowed => None,
            RateLimitOutcome::Limited { retry_after_secs } => Some(*retry_after_secs),
        }
    }
}

/// Proof that the per-address budget was charged for a request AND allowed it.
///
/// ## Why this is a type and not a comment
///
/// [`OauthRateLimiter::check_subject`] takes one of these, and
/// [`OauthRateLimiter::check_address`] is the only thing in the crate that can
/// produce one — the fields are private and there is no constructor, no
/// `Default`, and no `From`. So "the address dimension was charged first" is a
/// fact the compiler checks. A route that tried to charge only the subject would
/// have nothing to pass and would not build.
///
/// Before this, the ordering held because the mounted routes happened to be
/// wired that way: a layer charged the address, the handlers charged the
/// subject. Round 10 (`gpt56`) pointed out that this is a property of the
/// current wiring rather than of the code, and a future route that called the
/// subject method without sitting behind that layer would silently skip the
/// address dimension. Three times in this item the same shape has been wrong —
/// three 429 constructors, two redaction passes, five would-be charge sites —
/// and each time the fix was to remove the second path rather than document it.
///
/// ## Why this is not the token RMCP-07 correctly rejected
///
/// RMCP-07 refused a `#[must_use]` marker for its invalidation guard because
/// that marker would have been DATA-FREE: any in-crate caller could mint one and
/// assert an invalidation it never performed, which is documentation wearing a
/// type's clothes. The distinction here is that this value **carries the
/// resolved address and the endpoint** — the two things the subject charge needs
/// anyway, one for the audit record and one to select the bucket table. It
/// cannot be conjured, because producing it IS the address check, and its
/// contents are the check's own output. There is also no separate `address`
/// parameter left on `check_subject` that could disagree with the address
/// actually charged.
///
/// ## How it reaches a handler
///
/// [`crate::oauth::mount`]'s layer charges the address before any handler runs
/// and puts the witness in the request extensions; a handler receives it as an
/// ordinary extractor. A handler mounted WITHOUT that layer gets a rejection
/// rather than an unlimited request — fail-closed, and loud.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AddressCleared {
    address: IpAddr,
    endpoint: OauthEndpoint,
}

impl AddressCleared {
    /// The address that was charged. The same value the audit record names, by
    /// construction rather than by the caller passing it again.
    pub fn address(&self) -> IpAddr {
        self.address
    }

    /// The endpoint whose budget was charged, so the subject charge cannot land
    /// on a different endpoint's table than the address charge did.
    pub fn endpoint(&self) -> OauthEndpoint {
        self.endpoint
    }
}

/// Extract the witness a handler needs in order to charge a subject.
///
/// Absent means the handler is not behind
/// [`crate::oauth::mount`]'s rate-limit layer, which should be impossible for a
/// mounted route. It is answered with a `500` rather than by proceeding
/// unlimited: a route that lost its limiter is a bug to be seen, not a request
/// to be served. The body says nothing about why, for the same reason every
/// other refusal on this door says nothing.
#[axum::async_trait]
impl<S: Send + Sync> axum::extract::FromRequestParts<S> for AddressCleared {
    type Rejection = axum::response::Response;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        use axum::response::IntoResponse as _;
        parts.extensions.get::<AddressCleared>().copied().ok_or_else(|| {
            tracing::error!(
                target: "rmcp_oauth_audit",
                "a mounted OAuth route ran without the rate-limit layer — refusing the request"
            );
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                [(axum::http::header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                "internal error",
            )
                .into_response()
        })
    }
}

/// The ONE throttled response the whole OAuth door returns.
///
/// Status, body, wording and headers, all decided here and nowhere else. Every
/// limited route returns exactly this value, so the responses are byte-identical
/// by construction rather than by three authors independently choosing the same
/// words.
///
/// ## Why this is a security control and not tidying
///
/// Review round 8 (`gpt56`) found three hand-written 429 paths — `authorize`,
/// `consent` and `login` each built their own — and granted that none of them
/// leaked account existence *today*. The objection was that nothing MADE that
/// true, and the drift had in fact already begun: `authorize` said "too many
/// sign-in attempts **from this address**" while `login` said "too many sign-in
/// attempts". Both were address-dimension denials, so neither was an oracle yet.
/// But that is one edit away from a handler that knows it hit the SUBJECT bucket
/// saying so — and "too many attempts for this account" confirms the account
/// exists to anyone who can type a name.
///
/// The same argument this module has already made three times: a second path
/// that exists is a path someone takes while believing they are safe. So the
/// bespoke constructors are gone rather than merely aligned, and
/// `the_oauth_door_has_exactly_one_throttled_response` fails if one comes back.
///
/// ## What is deliberately given up
///
/// The interactive endpoints used to re-render a styled page on a throttle. They
/// now return the same plain response as the machine endpoints. That is a real
/// (small) loss of polish, taken knowingly: the rendering that could carry a
/// bespoke sentence is exactly the rendering in which an oracle would appear,
/// and a 429 with `Retry-After` is a complete, correct answer to a browser.
///
/// The login path also used to clear the session cookie as part of re-rendering
/// its form. It no longer does, which is strictly better and not merely
/// incidental: clearing it would let an attacker sign a victim out by throttling
/// them, and a throttle is not evidence about the session.
pub fn throttled_response(outcome: &RateLimitOutcome) -> axum::response::Response {
    use axum::response::IntoResponse as _;

    let mut response = (
        axum::http::StatusCode::TOO_MANY_REQUESTS,
        [(axum::http::header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        outcome.public_message(),
    )
        .into_response();

    // `Retry-After` is rounded UP, so a client that honours it is never early —
    // and a client that retries early simply meets the same response again.
    if let Some(retry) = outcome.retry_after_secs() {
        let seconds = retry.max(0.0).ceil() as u64;
        if let Ok(value) = seconds.to_string().parse() {
            response.headers_mut().insert(axum::http::header::RETRY_AFTER, value);
        }
    }
    response
}

/// One endpoint's pair of bucket tables.
struct EndpointLimiter {
    by_address: InProcessRateLimiter,
    by_subject: InProcessRateLimiter,
}

/// The OAuth door's rate limiter: two bounded bucket tables per endpoint.
///
/// Deliberately owns concrete [`InProcessRateLimiter`]s rather than
/// `Arc<dyn RateLimiter>`s. The trait exists so a Redis-backed implementation
/// can replace the in-process one wholesale (TGW-04's seam), and when that
/// happens this struct is what changes — but a `dyn` indirection today would buy
/// nothing and would let a caller construct one with a table missing for some
/// endpoint, which is the fail-open shape the module docs rule out.
pub struct OauthRateLimiter {
    limiters: HashMap<OauthEndpoint, EndpointLimiter>,
}

impl OauthRateLimiter {
    /// Build with the built-in budgets.
    ///
    /// Infallible: the compiled-in defaults satisfy
    /// [`EndpointBudgets::validate`] by construction, and
    /// `default_budgets_satisfy_the_invariant` asserts it — so the `expect` here
    /// cannot fire without that test failing first.
    pub fn with_defaults() -> Self {
        Self::from_budgets(|e| e.default_budgets())
            .expect("the compiled-in default budgets satisfy the subject>address invariant")
    }

    /// Build from the environment, falling back to the built-in budget for any
    /// value that is absent or unparseable. The two dimensions fall back
    /// independently, so a typo in one does not silently widen the other.
    ///
    /// **Fails, hard, on a well-formed but dangerous configuration.** The two
    /// cases are deliberately treated differently and the line between them is
    /// the point:
    ///
    /// * A value that is absent or does not PARSE is uninterpretable — there is
    ///   no instruction to honour — so the built-in default applies. An operator
    ///   typo must not silently remove a security control.
    /// * A value that parses but VIOLATES the subject>address invariant is a
    ///   legible instruction to weaken the control, and it is refused. Starting
    ///   anyway with a quietly-corrected budget would leave the operator
    ///   believing a configuration is in force that is not, which is how the
    ///   free-account-lockout hole in [`EndpointBudgets::validate`] would come
    ///   back unnoticed. This mirrors RMCP-09's treatment of a present-but-invalid
    ///   edge policy: a hard startup error, because a typo that silently weakens
    ///   a security control is the failure nobody notices.
    ///
    /// The env read is a plain [`std::env::var`]: a rate-limit budget is
    /// non-secret tuning configuration, the same treatment
    /// `TERMINUS_GATEWAY_RATE_LIMIT_BURST` and `RMCP_DB_MAX_CONNECTIONS` get.
    /// (The crate has no separate `SecretManager` API in any case — the runtime
    /// store is materialized into the process environment at startup, so an env
    /// read IS the vault read; see [`crate::oauth`]'s module docs.)
    pub fn from_env() -> Result<Self, ToolError> {
        Self::from_budgets(|endpoint| {
            let defaults = endpoint.default_budgets();
            let read = |var: String, fallback: Budget| {
                std::env::var(var)
                    .ok()
                    .as_deref()
                    .and_then(Budget::parse)
                    .unwrap_or(fallback)
            };
            EndpointBudgets {
                per_address: read(endpoint.address_env_var(), defaults.per_address),
                per_subject: read(endpoint.subject_env_var(), defaults.per_subject),
            }
        })
    }

    /// Build with caller-supplied budgets, validating each pair.
    ///
    /// The one place per-endpoint construction happens, which is why validation
    /// lives here rather than in the env reader: a public constructor that could
    /// produce an unbalanced pair would let the invariant be broken without
    /// touching configuration at all. Review round 1 (`gpt56`) found exactly
    /// that gap — the invariant was documented and asserted for the DEFAULTS,
    /// and enforced nowhere.
    pub fn from_budgets(
        mut budgets_for: impl FnMut(OauthEndpoint) -> EndpointBudgets,
    ) -> Result<Self, ToolError> {
        let mut limiters = HashMap::new();
        for endpoint in OauthEndpoint::ALL {
            let budgets = budgets_for(endpoint);
            budgets.validate(endpoint)?;
            let table = |b: Budget| {
                InProcessRateLimiter::with_max_keys(b.burst, b.refill_per_sec, MAX_KEYS_PER_TABLE)
            };
            limiters.insert(
                endpoint,
                EndpointLimiter {
                    by_address: table(budgets.per_address),
                    by_subject: table(budgets.per_subject),
                },
            );
        }
        Ok(Self { limiters })
    }

    /// Charge the ADDRESS dimension, and nothing else.
    ///
    /// Split out from [`Self::check`] so it can run BEFORE a request is parsed.
    /// Review round 9 (`gpt56`) found that the POST handlers validated a body
    /// first and charged afterwards, so malformed requests cost an attacker
    /// nothing — a limiter that only counts well-formed traffic does not bound
    /// the traffic worth bounding. The address is the one dimension knowable
    /// without reading a body, which is what makes charging it first possible at
    /// all; the subject follows in [`Self::check_subject`] once the body has
    /// been read.
    ///
    /// `address` is the RESOLVED client address, typed: it comes from
    /// [`crate::oauth::edge::resolve_client_ip`], which has already decided
    /// which hop of an `X-Forwarded-For` chain may be attributed. Taking an
    /// `IpAddr` rather than a `&str` means this module never parses a header and
    /// cannot be handed an arbitrary string as an "address".
    ///
    /// Returns [`AddressCleared`] on success — the witness [`Self::check_subject`]
    /// demands. That is what makes the ordering a compiler check rather than a
    /// convention; see the type's own documentation.
    pub async fn check_address(
        &self,
        endpoint: OauthEndpoint,
        address: IpAddr,
    ) -> Result<AddressCleared, RateLimitOutcome> {
        let Some(limiter) = self.limiters.get(&endpoint) else {
            // Unreachable by construction (`from_budgets` covers `ALL`), and
            // fail-CLOSED if a future refactor makes it reachable.
            return Err(RateLimitOutcome::Limited { retry_after_secs: 1.0 });
        };
        match limited(limiter.by_address.check(&address_key(address)).await) {
            // Emitted HERE, not by the caller. Round 2 established the rule and
            // an earlier pass of round 9 briefly broke it by moving the record
            // out to the layer: a record every call site must remember to write
            // is one some call site will not, and the missing one lands on
            // whichever path is under attack.
            Some(outcome) => {
                self.audit_denial(endpoint, address, LimitDimension::Address);
                Err(outcome)
            }
            // The ONLY place an `AddressCleared` is ever produced.
            None => Ok(AddressCleared { address, endpoint }),
        }
    }

    /// Charge the SUBJECT dimension, and nothing else.
    ///
    /// **Requires an [`AddressCleared`], so it cannot be reached without the
    /// address budget having been charged and allowed first.** Round 10
    /// (`gpt56`) observed that the ordering previously held only because of how
    /// the current callers happened to be wired — the layer charged the address,
    /// the handlers charged the subject — and that a future route calling this
    /// method without sitting behind that layer would bypass the address
    /// dimension entirely. That is the same shape as the three 429 constructors
    /// and the two redaction passes this item has already had to delete, one
    /// level down, and it is fixed the same way: by making the mistake
    /// unrepresentable rather than merely absent.
    ///
    /// The witness also carries the address, which this method needs anyway for
    /// the audit record it emits — so there is no separate `address` parameter
    /// that could disagree with the one that was actually charged.
    ///
    /// `subject` is the account name or `client_id` the request named, passed AS
    /// PRESENTED, not as a resolved id, so an unknown account consumes budget
    /// identically to a known one and the limiter cannot become an existence
    /// oracle.
    pub async fn check_subject(
        &self,
        cleared: &AddressCleared,
        subject: &str,
    ) -> RateLimitOutcome {
        let endpoint = cleared.endpoint();
        let Some(limiter) = self.limiters.get(&endpoint) else {
            return RateLimitOutcome::Limited { retry_after_secs: 1.0 };
        };
        match limited(limiter.by_subject.check(&subject_key(subject)).await) {
            Some(outcome) => {
                self.audit_denial(endpoint, cleared.address(), LimitDimension::Subject);
                outcome
            }
            None => RateLimitOutcome::Allowed,
        }
    }

    /// Charge both dimensions, address first, short-circuiting on a denial.
    ///
    /// The composition of the two calls above. The short-circuit is no longer
    /// something this function has to remember to do — `check_subject` cannot be
    /// called at all without the `Ok` arm's witness, so the ordering is enforced
    /// even here.
    pub async fn check(
        &self,
        endpoint: OauthEndpoint,
        address: IpAddr,
        subject: Option<&str>,
    ) -> RateLimitOutcome {
        let cleared = match self.check_address(endpoint, address).await {
            Ok(cleared) => cleared,
            Err(outcome) => return outcome,
        };
        match subject {
            Some(subject) => self.check_subject(&cleared, subject).await,
            None => RateLimitOutcome::Allowed,
        }
    }

    /// Emit the audit record for a throttled request.
    ///
    /// Emitted HERE rather than left to each endpoint, for the same reason the
    /// 429 message is produced here: a record every handler has to remember to
    /// write is a record some handler will not write, and the missing one will
    /// be on whichever path is under attack.
    ///
    /// The record carries the endpoint, the resolved address (typed) and which
    /// dimension refused. It deliberately does NOT carry the subject: that is an
    /// account name or a `client_id` the caller chose, and [`LimitDimension`]
    /// already answers the operational question without putting a login
    /// identifier in the log.
    fn audit_denial(
        &self,
        endpoint: OauthEndpoint,
        address: IpAddr,
        dimension: LimitDimension,
    ) {
        OauthAuditRecord::new(OauthEvent::RateLimited)
            .endpoint(endpoint)
            .from_address(address)
            .reason(DenialReason::RateLimited)
            .detail(AuditDetail::RateLimited { dimension })
            .emit();
    }
}

/// Collapse a [`RateLimitDecision`] into the door's own outcome.
///
/// A `Degraded` decision — the limiter backend itself being broken — is
/// deliberately treated as LIMITED rather than passed through as its own
/// variant. That distinction earns its keep on the tool-dispatch path, where an
/// operator needs to tell throttling from an outage; on an unauthenticated
/// internet-facing auth endpoint the only safe reading of "I cannot tell whether
/// you are over budget" is "you are". The distinction is not lost — the caller
/// simply is not the one who learns it; an operator reads it from the audit
/// record and from the gateway's own degraded-limiter signal.
///
/// ## Why this is an exhaustive `match` and not `is_over_budget()`
///
/// It used to ask `decision.is_over_budget()`, which today returns true for
/// `Degraded` — so the behaviour was correct. But the guarantee rested on the
/// semantics of a boolean defined in ANOTHER module, whose name reads like
/// "genuinely over budget": someone narrowing `is_over_budget` to exclude the
/// backend-fault case would be making a locally reasonable change, and this
/// door would silently start ADMITTING requests whenever its limiter was
/// degraded. Round 12 (`gpt56`) flagged it, and it is a fail-open on the
/// control's failure path — the worst place for one, because a limiter is
/// degraded exactly when something is wrong, which correlates with being under
/// attack.
///
/// Matching every variant by name makes the decision compile-checked instead:
/// a new `RateLimitDecision` variant does not build until someone chooses which
/// side of this line it falls on. That is the same shape as the `AddressCleared`
/// witness and the closed `AuditDetail` — a mistake made unrepresentable rather
/// than merely absent.
fn limited(decision: RateLimitDecision) -> Option<RateLimitOutcome> {
    match decision {
        RateLimitDecision::Allowed => None,
        // A real over-limit.
        RateLimitDecision::Limited { retry_after_secs, .. } => {
            Some(RateLimitOutcome::Limited { retry_after_secs })
        }
        // The limiter backend itself is broken. On an unauthenticated,
        // internet-facing auth endpoint the only safe reading of "I cannot tell
        // whether you are over budget" is "you are". The distinction is not
        // lost — the caller simply is not the one who learns it; an operator
        // reads it from the audit record and the gateway's own degraded signal.
        RateLimitDecision::Degraded { retry_after_secs, .. } => {
            Some(RateLimitOutcome::Limited { retry_after_secs })
        }
    }
}

/// Hex digest of an attacker-influenced string, for use as a bucket key.
///
/// Both dimensions go through this. The entry COUNT is capped by
/// [`MAX_KEYS_PER_TABLE`], but a cap on entries bounds memory only if each entry
/// is itself bounded — and both the address and the subject arrive from the
/// request, so both can be arbitrarily long. Round 3 (`gpt56`) caught the
/// address side still keying on the raw value, which let the map amplify well
/// past what the cap implied.
///
/// A fixed 64-character digest makes per-key memory constant regardless of what
/// was sent, and as a side effect keeps account names and addresses out of the
/// limiter's memory entirely.
fn digest_key(prefix: &str, value: &str) -> String {
    let hex = secret_hash(value).iter().map(|b| format!("{b:02x}")).collect::<String>();
    rate_limit_key(prefix, &hex)
}

/// Key for the address dimension. Prefixed so an address can never collide with
/// a subject digest — the two dimensions live in separate tables today, and the
/// prefix keeps that true if they are ever merged into one Redis keyspace.
///
/// Hashing the address costs nothing operationally: the address a limiter would
/// have stored in plaintext is not how an operator finds a hammering source
/// anyway. The AUDIT RECORD is, and it carries the typed, canonical `IpAddr`
/// precisely so it stays actionable while the limiter's own memory holds only
/// digests.
///
/// The digest is taken over `IpAddr`'s canonical `Display`, so two spellings of
/// the same address cannot land in two buckets and get two budgets.
fn address_key(address: IpAddr) -> String {
    digest_key("addr", &address.to_string())
}

/// Key for the subject dimension, over a digest of the presented identifier.
fn subject_key(subject: &str) -> String {
    digest_key("sub", subject)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse a documentation-range literal into the typed address the API now
    /// requires. Parsing a LITERAL in a test is a different thing from parsing
    /// caller-controlled input at an API boundary; `edge`'s own tests use the
    /// same helper shape.
    fn ip(literal: &str) -> IpAddr {
        literal.parse().expect("a documentation-range address literal")
    }

    /// Addresses in fixtures come from RFC 5737's documentation ranges, which
    /// exist precisely so an example need not name a real network.
    const ADDR_A: &str = "192.0.2.10";
    const ADDR_B: &str = "198.51.100.7";
    const ADDR_C: &str = "203.0.113.5";

    /// Negligible refill so nothing recovers mid-test, and a subject budget
    /// larger than the address budget — the production ratio in miniature.
    fn tight() -> OauthRateLimiter {
        OauthRateLimiter::from_budgets(|_| EndpointBudgets {
            per_address: Budget { burst: 2, refill_per_sec: 0.0001 },
            per_subject: Budget { burst: 5, refill_per_sec: 0.0002 },
        })
        .expect("the fixture budgets satisfy the subject>address invariant")
    }

    /// Each endpoint has its own budget: exhausting one must not throttle
    /// another. Without this, a token-refresh flood would lock the operator out
    /// of the consent screen and out of revocation.
    #[tokio::test]
    async fn endpoints_do_not_share_a_budget() {
        let limiter = tight();
        for _ in 0..2 {
            assert!(!limiter.check(OauthEndpoint::Token, ip(ADDR_A), None).await.is_limited());
        }
        assert!(limiter.check(OauthEndpoint::Token, ip(ADDR_A), None).await.is_limited());
        assert!(
            !limiter.check(OauthEndpoint::Revoke, ip(ADDR_A), None).await.is_limited(),
            "revocation must stay reachable when the token endpoint is saturated"
        );
        assert!(!limiter.check(OauthEndpoint::Authorize, ip(ADDR_A), None).await.is_limited());
    }

    /// The acceptance criterion, asserted directly: a throttled request naming
    /// an account that exists and one naming an account that does not are
    /// indistinguishable. The limiter never resolves a subject, so the only
    /// thing that could differ is the message — and there is exactly one.
    #[tokio::test]
    async fn a_429_does_not_reveal_whether_the_account_exists() {
        let limiter = tight();
        let mut outcomes = Vec::new();
        for (addr, subject) in [(ADDR_A, "an-account-that-exists"), (ADDR_B, "an-account-that-does-not")] {
            let mut last = RateLimitOutcome::Allowed;
            for _ in 0..6 {
                last = limiter.check(OauthEndpoint::Login, ip(addr), Some(subject)).await;
            }
            outcomes.push(last);
        }
        assert!(outcomes.iter().all(|o| o.is_limited()), "{outcomes:?}");
        assert_eq!(outcomes[0].public_message(), outcomes[1].public_message());
        assert_eq!(outcomes[0].http_status(), outcomes[1].http_status());
        assert_eq!(outcomes[0].http_status(), 429);
    }

    /// The subject dimension must actually bite: the same account attacked from
    /// many addresses is still throttled, which is the whole point of having a
    /// second key. Each address stays inside its own budget, so only the subject
    /// bucket can produce the denial.
    #[tokio::test]
    async fn the_subject_budget_survives_a_change_of_address() {
        let limiter = tight();
        let victim = "one-particular-account";
        let mut limited_seen = false;
        for addr in [ADDR_A, ADDR_B, ADDR_C] {
            for _ in 0..2 {
                if limiter.check(OauthEndpoint::Login, ip(addr), Some(victim)).await.is_limited() {
                    limited_seen = true;
                }
            }
        }
        assert!(limited_seen, "a distributed grind against one account went unthrottled");
    }

    /// Rule (2) from the module docs: a request already denied by the ADDRESS
    /// bucket must not spend the subject's budget. With the short-circuit, a
    /// 100-request flood from one address costs the victim only that address's
    /// own budget (2 of 5); without it, the victim would be fully drained and
    /// locked out from anywhere.
    #[tokio::test]
    async fn an_address_denial_does_not_consume_the_victims_budget() {
        let limiter = tight();
        let victim = "the-victim-account";
        for _ in 0..100 {
            let _ = limiter.check(OauthEndpoint::Login, ip(ADDR_A), Some(victim)).await;
        }
        // 2 subject tokens spent, 3 left: the victim can still authenticate
        // from clean addresses. Spread across two of them so the assertion
        // exercises the SUBJECT budget rather than tripping a clean address's
        // own (deliberately smaller) budget.
        for addr in [ADDR_B, ADDR_B, ADDR_C] {
            assert!(
                !limiter.check(OauthEndpoint::Login, ip(addr), Some(victim)).await.is_limited(),
                "the flood burned the victim's whole subject budget"
            );
        }
    }

    /// A restart re-arms rather than failing open for the gap: a freshly
    /// constructed limiter throttles from its first burst onward, and there is
    /// no constructor that yields an unarmed one.
    #[tokio::test]
    async fn a_freshly_constructed_limiter_is_already_armed() {
        let limiter = OauthRateLimiter::with_defaults();
        let budget = OauthEndpoint::Login.default_budgets().per_address;
        for _ in 0..budget.burst {
            assert!(!limiter.check(OauthEndpoint::Login, ip(ADDR_A), None).await.is_limited());
        }
        assert!(
            limiter.check(OauthEndpoint::Login, ip(ADDR_A), None).await.is_limited(),
            "a limiter rebuilt after a restart must still bound the burst"
        );
    }

    /// A malformed or nonsensical override degrades to the built-in budget, and
    /// specifically NOT to "unlimited". A typo in a tuning knob must not remove
    /// the control.
    #[test]
    fn a_bad_budget_override_falls_back_to_the_default_not_to_unlimited() {
        for bad in ["", "nonsense", "0:1", "10:0", "10:-1", "10", ":", "abc:def", "10:inf", "10:NaN"] {
            assert!(Budget::parse(bad).is_none(), "{bad} must not parse");
        }
        assert_eq!(Budget::parse("40:1.5"), Some(Budget { burst: 40, refill_per_sec: 1.5 }));
        assert_eq!(Budget::parse(" 40 : 1.5 "), Some(Budget { burst: 40, refill_per_sec: 1.5 }));

        // The fallback path really does land on the default, exercised without
        // mutating process-global environment state (which would race other
        // tests).
        let resolved = |raw: Option<&str>, fallback: Budget| {
            raw.and_then(Budget::parse).unwrap_or(fallback)
        };
        let default_login = OauthEndpoint::Login.default_budgets().per_address;
        assert_eq!(resolved(None, default_login), default_login);
        assert_eq!(resolved(Some("0:0"), default_login), default_login);
    }

    /// The invariant, enforced rather than described: inverting the ordering
    /// must be REFUSED at construction, not quietly accepted.
    ///
    /// This is the test review round 1 asked for. It fails if anyone ever makes
    /// the subject budget equal to, or smaller than, the address budget — in
    /// either dimension — because that is a free single-address lockout of any
    /// account whose name an attacker can guess.
    #[test]
    fn an_inverted_subject_budget_is_refused_at_construction() {
        let build = |per_address: Budget, per_subject: Budget| {
            OauthRateLimiter::from_budgets(move |_| EndpointBudgets { per_address, per_subject })
        };
        let base = Budget { burst: 8, refill_per_sec: 0.1 };

        // Equal burst — the exact hole: one host's own budget running out also
        // exhausts the victim's.
        assert!(build(base, Budget { burst: 8, refill_per_sec: 0.3 }).is_err());
        // Smaller burst — worse still.
        assert!(build(base, Budget { burst: 4, refill_per_sec: 0.3 }).is_err());
        // Equal refill rate reproduces the hole in the steady state even with a
        // larger burst, so it is refused too.
        assert!(build(base, Budget { burst: 24, refill_per_sec: 0.1 }).is_err());
        assert!(build(base, Budget { burst: 24, refill_per_sec: 0.05 }).is_err());
        // Strictly greater in BOTH dimensions is the only accepted shape.
        assert!(build(base, Budget { burst: 24, refill_per_sec: 0.3 }).is_ok());

        // The refusal names the endpoint and the variables to fix, because an
        // operator meets this at startup with nothing else to go on.
        //
        // Matched rather than `expect_err`-ed: the Ok side is a live limiter
        // holding bucket tables, and requiring `Debug` on it purely so a test
        // can format a value it never prints is the tail wagging the dog.
        let err = match build(base, Budget { burst: 8, refill_per_sec: 0.3 }) {
            Ok(_) => panic!("an inverted subject budget must be refused"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("authorize") || err.contains("login"), "{err}");
        assert!(err.contains("RMCP_RATE_LIMIT_"), "{err}");
    }

    /// TERM #633 regression guard: converging RMCP-03's private login limiter
    /// onto this table must not have relaxed it.
    ///
    /// The old limiter was a bare `InProcessRateLimiter` with burst 5 and a
    /// refill of one attempt every twenty seconds, chosen deliberately to bound
    /// password guessing. Convergence is supposed to remove a SECOND definition,
    /// not to renegotiate the surviving one — so the per-address numbers are
    /// pinned here. Loosening them is a decision someone must make explicitly by
    /// editing this test.
    ///
    /// It also pins the thing the old limiter could not express: a subject
    /// budget strictly above the address budget, so one host can no longer hold
    /// a named account locked out by exhausting its own budget.
    #[test]
    fn the_login_budget_is_no_looser_than_the_limiter_it_replaced() {
        let login = OauthEndpoint::Login.default_budgets();
        assert_eq!(login.per_address.burst, 5, "RMCP-03's LOGIN_BURST");
        assert!(
            (login.per_address.refill_per_sec - 0.05).abs() < f64::EPSILON,
            "RMCP-03's LOGIN_REFILL_PER_SEC (one attempt per twenty seconds)"
        );
        assert!(login.per_subject.burst > login.per_address.burst);
        assert!(login.per_subject.refill_per_sec > login.per_address.refill_per_sec);
        // And the login endpoint is the TIGHTEST of the door's endpoints, which
        // is the whole reason it gets its own budget rather than sharing one.
        for other in [OauthEndpoint::Authorize, OauthEndpoint::Token, OauthEndpoint::Revoke] {
            assert!(
                other.default_budgets().per_address.burst >= login.per_address.burst,
                "{other:?} is tighter than the credential-verifying endpoint"
            );
        }
    }

    /// The infallible constructor is only sound because the compiled-in defaults
    /// satisfy the invariant. If a future default is retuned into a violation,
    /// this fails here rather than as a panic at startup.
    #[test]
    fn default_budgets_satisfy_the_invariant() {
        for endpoint in OauthEndpoint::ALL {
            endpoint
                .default_budgets()
                .validate(endpoint)
                .unwrap_or_else(|e| panic!("default budgets for {endpoint:?} are unsafe: {e}"));
        }
        // …which is what makes `with_defaults` safe to be infallible.
        let _ = OauthRateLimiter::with_defaults();
    }

    /// Every endpoint has budgets and env vars; the labels are unique (a
    /// duplicate would silently merge two endpoints' tables); and the subject
    /// budget is strictly larger than the address budget everywhere, which is
    /// the invariant rule (1) rests on.
    #[test]
    fn every_endpoint_is_covered_distinctly_named_and_correctly_proportioned() {
        let mut labels: Vec<&str> = OauthEndpoint::ALL.iter().map(|e| e.as_str()).collect();
        let count = labels.len();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), count, "duplicate endpoint labels merge their tables");

        let limiter = OauthRateLimiter::with_defaults();
        for endpoint in OauthEndpoint::ALL {
            assert!(limiter.limiters.contains_key(&endpoint), "{endpoint:?} has no limiter");
            assert!(endpoint.address_env_var().starts_with("RMCP_RATE_LIMIT_"));
            assert_ne!(endpoint.address_env_var(), endpoint.subject_env_var());
            let budgets = endpoint.default_budgets();
            assert!(budgets.per_address.burst > 0);
            assert!(
                budgets.per_subject.burst > budgets.per_address.burst,
                "{endpoint:?}: an equal subject budget lets one address lock an account out"
            );
        }
    }

    /// A throttled request must EMIT, not merely return an outcome. Round 2
    /// (`gpt56`) noted the limiter was silent: a structurally safe record type
    /// that nothing writes is not an audit trail.
    ///
    /// Scans the shared ring for a record this test can only have produced, and
    /// checks both dimensions are distinguishable — an operator diagnosing a
    /// throttle needs to know whether one address is hammering the door or one
    /// account is being ground from everywhere.
    #[tokio::test]
    async fn a_throttled_request_emits_an_audit_record() {
        use crate::oauth::audit::{recent_records, AuditDetail, LimitDimension, OauthEvent};

        let limiter = tight();
        // A source address unique to this test, so the assertion cannot match
        // another test's record in the process-wide ring. RFC 5737 range.
        let addr = "198.51.100.231";

        // Burn the address budget: 2 allowed, the third refused.
        for _ in 0..3 {
            let _ = limiter.check(OauthEndpoint::Login, ip(addr), Some("some-account")).await;
        }

        let emitted: Vec<_> = recent_records()
            .into_iter()
            .filter(|r| r.source_address().as_deref() == Some(addr))
            .collect();
        assert!(!emitted.is_empty(), "the limiter refused a request and recorded nothing");
        let record = emitted.last().expect("at least one");
        assert_eq!(record.event_kind(), OauthEvent::RateLimited);
        assert_eq!(record.endpoint_of(), Some(OauthEndpoint::Login));
        assert_eq!(
            record.detail_kind(),
            Some(AuditDetail::RateLimited { dimension: LimitDimension::Address })
        );

        // The subject dimension is recorded distinctly. A second address stays
        // inside its own budget, so only the subject bucket can refuse.
        let subject_addr = "198.51.100.232";
        let victim = "an-account-under-distributed-attack";
        for a in [subject_addr, "198.51.100.233", "198.51.100.234"] {
            for _ in 0..2 {
                let _ = limiter.check(OauthEndpoint::Login, ip(a), Some(victim)).await;
            }
        }
        let subject_denials = recent_records().into_iter().any(|r| {
            r.detail_kind() == Some(AuditDetail::RateLimited { dimension: LimitDimension::Subject })
        });
        assert!(subject_denials, "a subject-budget denial was not recorded as one");
    }

    /// The record must not carry the throttled SUBJECT — that is an account
    /// name, the human's login identifier, and the dimension already answers
    /// the operational question.
    #[tokio::test]
    async fn a_rate_limit_record_does_not_name_the_throttled_account() {
        use crate::oauth::audit::{record_text, recent_records};

        let limiter = tight();
        let addr = "203.0.113.77";
        let account = "a-distinctive-account-name-for-this-test";
        for _ in 0..4 {
            let _ = limiter.check(OauthEndpoint::Login, ip(addr), Some(account)).await;
        }
        for record in recent_records() {
            for text in record_text(&record) {
                assert!(!text.contains(account), "the throttled account name reached a record: {text}");
            }
        }
    }

    /// A DENIED address yields no witness, so the subject budget is
    /// unreachable — the runtime half of the type-level guarantee.
    ///
    /// The compile-time half cannot be asserted from inside the crate (the code
    /// that would prove it is code that does not compile), so what is pinned
    /// here is the property that makes it work: `check_address` hands back a
    /// witness ONLY on the allowed path, and the denial path hands back an
    /// outcome instead. Nothing else in the crate constructs an
    /// `AddressCleared`, which `only_the_address_check_constructs_the_witness`
    /// checks by scanning the source.
    #[tokio::test]
    async fn a_denied_address_yields_no_witness_to_charge_a_subject_with() {
        let limiter = tight();
        let addr = ip("192.0.2.77");

        // The first calls clear, and each hands back a witness.
        let cleared = limiter
            .check_address(OauthEndpoint::Login, addr)
            .await
            .expect("an under-budget address clears");
        assert_eq!(cleared.address(), addr, "the witness carries what was charged");
        assert_eq!(cleared.endpoint(), OauthEndpoint::Login);

        // Exhaust the address budget.
        let mut denied = None;
        for _ in 0..40 {
            if let Err(outcome) = limiter.check_address(OauthEndpoint::Login, addr).await {
                denied = Some(outcome);
                break;
            }
        }
        let denied = denied.expect("the address budget is exhaustible");
        assert!(denied.is_limited());
        // There is no witness on this path at all — which is precisely why a
        // subject charge cannot follow a refused address.
    }

    /// The witness is produced in exactly ONE place.
    ///
    /// The type's privacy is what enforces this outside the module; inside it,
    /// a second construction site would quietly re-open the hole, because a
    /// caller could then obtain a witness without charging anything. Same
    /// approach as the single-429-constructor guard, and for the same reason:
    /// the property is "no second path exists", which cannot be observed by
    /// exercising the first one.
    #[test]
    fn only_the_address_check_constructs_the_witness() {
        let source = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/oauth/limits.rs"),
        )
        .expect("readable");
        let production = source.split("\n#[cfg(test)]").next().unwrap_or("");
        // Struct-literal constructions only: the declaration and the two `impl`
        // headers mention the name too, and counting those would make this test
        // pass for the wrong reason.
        let constructions: Vec<&str> = production
            .lines()
            .filter(|line| line.contains("AddressCleared {"))
            .filter(|line| {
                let t = line.trim_start();
                !t.starts_with("pub struct") && !t.starts_with("impl") && !t.starts_with("//")
            })
            .collect();
        assert_eq!(
            constructions.len(),
            1,
            "`AddressCleared` is constructed somewhere other than `check_address`, which would \
             let a caller charge a subject without charging an address first: {constructions:?}"
        );
        assert!(
            constructions[0].contains("Ok(AddressCleared"),
            "the one construction is no longer the allowed arm of the address check: {:?}",
            constructions[0]
        );
    }

    /// A DEGRADED limiter throttles, and answers identically to a real
    /// over-limit.
    ///
    /// The claim in `limited`'s doc and the code that implements it must not be
    /// able to drift: a degraded backend is the moment the door is least able to
    /// afford admitting traffic, and the response must not tell the caller which
    /// of the two it hit.
    #[tokio::test]
    async fn a_degraded_limiter_throttles_exactly_like_a_real_over_limit() {
        use axum::body::to_bytes;

        let degraded = RateLimitDecision::Degraded { retry_after_secs: 2.0, refill_per_sec: 5.0 };
        let over = RateLimitDecision::Limited { retry_after_secs: 2.0, refill_per_sec: 5.0 };

        // Both convert to a limited outcome — the degraded one is NOT admitted.
        let from_degraded = limited(degraded).expect("a degraded limiter must not admit");
        let from_over = limited(over).expect("an over-limit must not admit");
        assert!(from_degraded.is_limited());
        assert_eq!(from_degraded, from_over);

        // And the caller cannot tell them apart. Rendered inline rather than
        // through a helper closure: a closure returning an `async` block that
        // borrows its argument does not satisfy the borrow checker, and the
        // duplication is two statements.
        let mut rendered = Vec::new();
        for outcome in [from_degraded, from_over] {
            let r = throttled_response(&outcome);
            let status = r.status().as_u16();
            let retry = r
                .headers()
                .get(axum::http::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            let body = to_bytes(r.into_body(), 64 * 1024).await.expect("body");
            rendered.push((status, retry, String::from_utf8_lossy(&body).into_owned()));
        }
        assert_eq!(
            rendered[0], rendered[1],
            "a degraded limiter answers differently from a real over-limit, which tells the \
             caller which one it hit"
        );

        // Allowed is still the only admitting case.
        assert!(limited(RateLimitDecision::Allowed).is_none());
    }

    /// Every throttled response the door produces is BYTE-IDENTICAL.
    ///
    /// Not "all three currently say the same thing" — that was true before this
    /// round too, and was one edit from being false. This asserts the property
    /// over every endpoint and both dimensions at once: whatever refused the
    /// request, the caller receives the same status, the same content type and
    /// the same bytes, so a response cannot become an account-existence oracle
    /// without this failing.
    #[tokio::test]
    async fn every_throttled_response_is_byte_identical() {
        use axum::body::to_bytes;

        let limiter = tight();
        let victim = "an-account-that-may-or-may-not-exist";
        let mut rendered: Vec<(u16, String, String)> = Vec::new();

        for endpoint in OauthEndpoint::ALL {
            // Address dimension: hammer one address with no subject.
            let addr = ip("192.0.2.60");
            let mut limited = None;
            for _ in 0..40 {
                let o = limiter.check(endpoint, addr, None).await;
                if o.is_limited() {
                    limited = Some(o);
                    break;
                }
            }
            // Subject dimension: spread across addresses so only the subject
            // bucket can refuse.
            let mut subject_limited = None;
            for n in 0..40u8 {
                let a = ip(&format!("198.51.100.{n}"));
                let o = limiter.check(endpoint, a, Some(victim)).await;
                if o.is_limited() {
                    subject_limited = Some(o);
                    break;
                }
            }

            for outcome in [limited, subject_limited].into_iter().flatten() {
                let response = throttled_response(&outcome);
                let status = response.status().as_u16();
                let ctype = response
                    .headers()
                    .get(axum::http::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                let body = to_bytes(response.into_body(), 64 * 1024).await.expect("body");
                rendered.push((status, ctype, String::from_utf8_lossy(&body).into_owned()));
            }
        }

        assert!(rendered.len() >= 2 * OauthEndpoint::ALL.len(), "{rendered:?}");
        let first = rendered[0].clone();
        assert_eq!(first.0, 429);
        for r in &rendered {
            assert_eq!(
                *r, first,
                "a throttled response differed between endpoints or dimensions: {r:?} vs {first:?}"
            );
        }
        // And the body really is the one public message, not an empty string
        // that would trivially satisfy the equality above.
        assert_eq!(first.2, RateLimitOutcome::Allowed.public_message());
        assert!(!first.2.is_empty());
    }

    /// `Retry-After` is the ONLY thing allowed to vary, and it rounds up so a
    /// client that honours it is never early.
    #[test]
    fn retry_after_is_the_only_varying_part_and_rounds_up() {
        let a = throttled_response(&RateLimitOutcome::Limited { retry_after_secs: 3.2 });
        let b = throttled_response(&RateLimitOutcome::Limited { retry_after_secs: 41.0 });
        let hdr = |r: &axum::response::Response| {
            r.headers()
                .get(axum::http::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        assert_eq!(hdr(&a).as_deref(), Some("4"));
        assert_eq!(hdr(&b).as_deref(), Some("41"));
        assert_eq!(a.status(), b.status());
    }

    /// The structural half: the OAuth door must contain exactly ONE constructor
    /// of a throttled response.
    ///
    /// A source scan, in the same spirit as the crate's `no_pii_in_own_source_tree`
    /// and hermeticity guards, and for the same reason they are source scans:
    /// the property is "no second path exists", which cannot be observed by
    /// calling the first one. Adding a bespoke 429 anywhere in `crate::oauth`
    /// fails here, naming the file.
    ///
    /// `edge.rs` is exempt and stated rather than silently skipped: RMCP-09's
    /// edge has its own per-address limiter that runs BEFORE routing and never
    /// sees a subject, so it cannot express the distinction this guard exists to
    /// prevent. Coupling it here would be a false constraint on another item.
    #[test]
    fn the_oauth_door_has_exactly_one_throttled_response() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/oauth");
        let mut offenders = Vec::new();
        let mut scanned = 0;
        for entry in std::fs::read_dir(&dir).expect("src/oauth is readable") {
            let path = entry.expect("dir entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
            if name == "limits.rs" || name == "edge.rs" {
                continue;
            }
            scanned += 1;
            let text = std::fs::read_to_string(&path).expect("readable");
            // Production code only. A test may legitimately ASSERT on a 429 —
            // asserting is not constructing, and the property being guarded is
            // that no second constructor exists on a serving path.
            let text = text.split("\n#[cfg(test)]").next().unwrap_or("").to_string();
            for (n, line) in text.lines().enumerate() {
                if line.contains("TOO_MANY_REQUESTS") {
                    offenders.push(format!("{name}:{}", n + 1));
                }
            }
        }
        assert!(scanned > 0, "the scan found no files — the path is wrong, not the tree clean");
        assert!(
            offenders.is_empty(),
            "a second throttled-response constructor appeared at {offenders:?}; every limited \
             route must return `throttled_response`, which is what keeps the 429 from becoming \
             an account-existence oracle"
        );
    }

    /// BOTH keys are digests, so an enormous presented value costs a constant
    /// amount of limiter memory rather than its own length.
    ///
    /// The address half is the round-3 fix: the entry count was capped, but a
    /// cap on entries bounds memory only if each entry is bounded too, and the
    /// address is as attacker-controlled as the subject.
    #[test]
    fn both_dimensions_key_on_a_bounded_digest_however_large_the_input() {
        let huge = "a".repeat(100_000);
        for key in [subject_key(&huge), address_key(ip("192.0.2.10"))] {
            assert!(key.len() < 128, "key grew with the input: {} chars", key.len());
            assert!(!key.contains(&huge));
        }
        // Distinct values still get distinct buckets, in both dimensions.
        assert_ne!(subject_key("one"), subject_key("two"));
        assert_ne!(address_key(ip("192.0.2.1")), address_key(ip("192.0.2.2")));
        // …and the two dimensions never collide on the same value.
        assert_ne!(subject_key("192.0.2.1"), address_key(ip("192.0.2.1")));
        // The presented value itself never becomes part of the key.
        assert!(!subject_key("an-account-name").contains("an-account-name"));
        assert!(!address_key(ip("192.0.2.10")).contains("192.0.2.10"));
    }

    /// Hashing the limiter key must not cost the operator the ability to act:
    /// the audit record still names the address, parsed and canonicalized.
    #[tokio::test]
    async fn hashing_the_key_does_not_blind_the_operator_to_the_source() {
        use crate::oauth::audit::recent_records;

        let limiter = tight();
        let addr = ip("203.0.113.191");
        for _ in 0..3 {
            let _ = limiter.check(OauthEndpoint::Token, addr, None).await;
        }
        let named = recent_records().into_iter().any(|r| r.source() == Some(addr));
        assert!(named, "the throttled address is not recoverable from the audit trail");
        // …and in a form an operator can paste into a firewall rule.
        let rendered = recent_records()
            .into_iter()
            .filter_map(|r| r.source_address())
            .any(|s| s == "203.0.113.191");
        assert!(rendered, "the recorded address is not in canonical textual form");
    }
}
