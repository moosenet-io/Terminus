//! TRTR-05 (privacy): the caller-entitlement value a tool sees, and the
//! module boundary that makes an ENTITLED one unforgeable.
//!
//! This type lives HERE — inside `gateway_framework` — rather than next to
//! [`crate::tool::RustTool`] for one reason: Rust privacy is module-scoped, so
//! putting the struct in the same module tree as the authorization decision is
//! the only way to make "only the gateway may mint an entitled context" a
//! COMPILER-CHECKED fact instead of a comment. `crate::tool` re-exports it, so
//! every existing `crate::tool::CallerContext` path is unchanged.

/// What the DISPATCH LAYER knows about the caller of ONE tool invocation.
///
/// TRTR-05 (privacy). Some tools can answer a question by reaching for context
/// the OPERATOR owns — the operator's calendar, the operator's configured home
/// and work addresses — even though the tool itself looks stateless. That is
/// safe for the operator and a disclosure for anyone else, so a tool that does
/// it must know who is asking. This struct is the ONLY channel by which it may
/// find out: it is constructed from the same server-verified `Principal` that
/// `GatewayFramework::guard` authorizes, and it is never derived from tool
/// arguments, a header, or an env var.
///
/// **Unforgeable by construction, not by convention.** The fields are private
/// to this module, and the only constructor that can set one to `true` —
/// [`CallerContext::from_allowlist_decision`] — is `pub(super)`, i.e. callable
/// ONLY from within `crate::gateway_framework`, which is the module that owns
/// the [`AllowlistPolicy`](super::AllowlistPolicy) decision. No other module in
/// this crate, no tool implementation, and no downstream crate can name it, so
/// there is no `CallerContext::new(true, true)` shortcut around the grant map.
/// The single in-crate production call site is
/// [`GatewayFramework::caller_context`](super::GatewayFramework::caller_context).
/// (An earlier revision exposed a `pub const fn new(bool, bool)`; that made the
/// entitlement gate convention-based — any in-process code could mint operator
/// privilege without holding the underlying grants. This is that hole closed.)
///
/// **Fail closed by construction.** [`Default`] and [`CallerContext::untrusted`]
/// are "we know nothing about this caller", with every capability `false`, and
/// they are the ONLY constructors reachable from outside this module. Every
/// existing dispatch path that does not thread a caller therefore gets the safe
/// value, and a NEW dispatch path that forgets to thread one is safe by default
/// rather than accidentally operator-privileged. A spurious "which location did
/// you mean?" costs one conversational turn; a leaked home or appointment
/// address cannot be taken back.
///
/// Each flag is a permission to USE ONE SOURCE of operator context, and the
/// gateway grants it only when the caller is already authorized for the tool
/// that exposes that source directly (`google_calendar_today`,
/// `commute_estimate`) — so an inference can never disclose anything the caller
/// could not have fetched for itself.
///
/// # TERM #576 — [`CallerContext::media_account`]
///
/// The media tools need a slightly different shape of the same idea. `weather`
/// asks "may I read the operator's calendar for you?", a yes/no question about
/// ONE shared source. `media_recommend` asks "WHOSE watch history am I building
/// a taste profile from?", which needs an identity, not a permission — the
/// answer must be the caller's own household account and nothing else, because
/// the leak it prevents is one household member's titles being narrated to
/// another ("because you watched X").
///
/// So this type also carries the caller's OWN media account, resolved by the
/// gateway from the same server-verified principal
/// ([`crate::media::account_map`]). It is minted in exactly the same place, is
/// unforgeable by the same module-privacy boundary, and is `None` — the
/// fail-closed value — for every caller the operator has not explicitly bound
/// to an account. `None` is NOT "unknown, proceed": it is the unentitled path,
/// and a tool that finds it there must fall back to a response with no
/// household-derived content at all.
///
/// # The type stays `Copy` — deliberately (TERM #576, review round 2)
///
/// An earlier revision of TERM #576 carried the account as an
/// `Option<Arc<str>>`, which silently downgraded this PUBLIC type from `Copy`
/// to `Clone`-only. Nothing in-crate broke, so the break was invisible here and
/// would only have surfaced in a downstream consumer — an API break as an
/// incidental side effect of a media change, which is not a decision anyone
/// made. It is now `Copy` again, and that is asserted (see
/// [`Self::untrusted`]'s doctest and `caller_context_is_copy`).
///
/// `Copy` is preserved by carrying the account as an INTERNED `&'static str`
/// rather than a refcounted handle. Interning is sound here precisely because
/// the value is NOT caller-controlled: an account id can only enter through
/// [`Self::with_media_account`], which only `crate::gateway_framework` can
/// call, and it only ever passes a value read out of the operator's
/// [`ACCOUNT_MAP_ENV`](crate::media::account_map::ACCOUNT_MAP_ENV)
/// configuration. The intern table is therefore bounded by the number of
/// household accounts the operator configured — a handful — and never by
/// request volume or by anything a caller can type. (A tool argument never
/// reaches this constructor; see `crate::media::recommend::resolve_history_scope`,
/// which COMPARES an argument against this value and refuses on mismatch.)
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CallerContext {
    may_infer_from_calendar: bool,
    may_infer_from_routine: bool,
    /// TERM #576: the household media account this caller IS, or `None` when
    /// the gateway could not determine one. Never read from tool arguments.
    media_account: Option<&'static str>,
    /// TERM #595: which HUMAN this turn is for. Minted only from a verified,
    /// principal-bound assertion; see [`PersonScope`].
    person: PersonScope,
}

/// Compile-time assertion that [`CallerContext`] keeps its `Copy` contract.
///
/// This is not decoration: dropping `Copy` from a public shared type is a
/// semver break, and the previous round dropped it by accident. If someone adds
/// a non-`Copy` field, THIS line fails to compile — before any test runs, and
/// with a message that points at the contract rather than at some distant
/// consumer's move error.
const _: fn() = || {
    fn assert_copy<T: Copy>() {}
    assert_copy::<CallerContext>();
};

/// TERM #595: WHICH HUMAN, if any, this turn is being run for.
///
/// Three states, and the third is the whole reason this is not an
/// `Option<&str>`. "Nobody asserted a person" and "somebody asserted a person
/// and we could not trust it" must lead to different outcomes: the first is a
/// legacy service-scoped caller and behaves exactly as it did before TERM #595;
/// the second must get LESS than that caller, because the alternative is that a
/// malformed or forged identity silently inherits the service's — which is to
/// say the operator's — records and entitlements.
///
/// Collapsing the two is the specific bug this shape prevents. It is the same
/// mistake as treating a failed lookup as an empty result: both read as
/// "proceed", and only one of them should.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PersonScope {
    /// No human identity was asserted on this request. The caller is the
    /// SERVICE principal and nothing narrower — the pre-TERM #595 world, which
    /// is still the correct answer for a background refresh, an internal
    /// dispatch, or a client that predates the mechanism.
    #[default]
    Service,
    /// A verified, roster-known human, cryptographically bound to this hop's
    /// principal (see [`crate::mesh::person`]). Interned so this type stays
    /// `Copy`; interning is bounded because the value can only come from the
    /// operator's configured roster.
    Person(&'static str),
    /// A human identity was ASSERTED and REFUSED — absent signing key, bad
    /// signature, expired, wrong issuer, blank/oversized/unknown person,
    /// principal mismatch, or an asserting principal without the grant.
    ///
    /// This is strictly LESS privilege than [`Self::Service`]: no operator
    /// context, no media account, and (once the per-caller record key is wired)
    /// no records at all. A tool that finds this must decline and ask, never
    /// answer as the operator.
    Unidentified,
}

impl PersonScope {
    /// The human identifier, when there is a trustworthy one.
    ///
    /// `None` for BOTH [`Self::Service`] and [`Self::Unidentified`] — a caller
    /// that only needs "who is this" is served correctly, and a caller that
    /// needs to distinguish "no person" from "a person we rejected" must match
    /// on the variant, which makes the distinction impossible to overlook by
    /// accident.
    pub const fn person(&self) -> Option<&'static str> {
        match self {
            Self::Person(p) => Some(p),
            _ => None,
        }
    }

    /// Whether this scope names a specific human.
    pub const fn is_person(&self) -> bool {
        matches!(self, Self::Person(_))
    }

    /// Whether an assertion was attempted and refused.
    pub const fn is_unidentified(&self) -> bool {
        matches!(self, Self::Unidentified)
    }
}

impl CallerContext {
    /// The fail-closed value: an unknown, unauthenticated or unrecognised
    /// caller. No operator context may be used on its behalf.
    ///
    /// This (and the identical [`Default`]) is the only constructor that is
    /// public — deliberately, since the only context anyone outside the
    /// gateway has any business building is a harmless one:
    ///
    /// ```
    /// use terminus_rs::tool::CallerContext;
    /// let c = CallerContext::untrusted();
    /// assert!(!c.may_infer_from_calendar() && !c.may_infer_from_routine());
    /// // TERM #576: and it is nobody in particular.
    /// assert!(c.media_account().is_none());
    /// ```
    ///
    /// **The TRTR-05 boundary check.** The entitled constructor is NOT
    /// reachable from outside `crate::gateway_framework`, and this is the
    /// executable proof of it — a real compile-fail test (`cargo test --doc`),
    /// which needs no `trybuild`/compile-fail harness (this repo has none, and
    /// TRTR-05 does not add the dependency):
    ///
    /// ```compile_fail
    /// use terminus_rs::tool::CallerContext;
    /// // error[E0624]: `from_allowlist_decision` is private
    /// let _forged = CallerContext::from_allowlist_decision(true, true);
    /// ```
    ///
    /// If someone widens `from_allowlist_decision` to `pub`, that block starts
    /// compiling and the doctest FAILS — which is exactly the alarm we want.
    ///
    /// TERM #595 rides it too: no code outside `crate::gateway_framework` can
    /// declare which HUMAN a caller is, so a tool holding a
    /// [`PersonScope::Person`] knows the grant, the signature, the principal
    /// binding and the roster were all checked.
    ///
    /// ```compile_fail
    /// use terminus_rs::tool::{CallerContext, PersonScope};
    /// // error[E0624]: `with_person_scope` is private
    /// let _forged = CallerContext::untrusted().with_person_scope(PersonScope::Person("someone"));
    /// ```
    ///
    /// ```compile_fail
    /// use terminus_rs::tool::CallerContext;
    /// // error[E0624]: `person_scope_for` is private
    /// let _forged = CallerContext::person_scope_for("someone");
    /// ```
    ///
    /// TERM #576 rides the same boundary, and gets the same executable proof:
    /// no code outside `crate::gateway_framework` can claim to BE a household
    /// media account, so a tool holding one knows the gateway put it there.
    ///
    /// ```compile_fail
    /// use terminus_rs::tool::CallerContext;
    /// // error[E0624]: `with_media_account` is private
    /// let _forged = CallerContext::untrusted().with_media_account(Some("acct-operator"));
    /// ```
    ///
    /// # The public `Copy` contract (TERM #576, review round 2)
    ///
    /// Asserted from OUTSIDE the crate, which is the only place the break would
    /// ever have been felt: this doctest passes the value by value twice, which
    /// compiles only while `CallerContext: Copy`. If the type is downgraded to
    /// `Clone`-only again, this FAILS with "use of moved value" under
    /// `cargo test --doc` — the alarm that was missing last round.
    ///
    /// ```
    /// use terminus_rs::tool::CallerContext;
    /// fn takes_by_value(_c: CallerContext) {}
    /// let c = CallerContext::untrusted();
    /// takes_by_value(c);
    /// takes_by_value(c); // still usable: Copy, not moved
    /// assert!(c.media_account().is_none());
    /// ```
    pub const fn untrusted() -> Self {
        Self {
            may_infer_from_calendar: false,
            may_infer_from_routine: false,
            media_account: None,
            person: PersonScope::Service,
        }
    }

    /// TERM #595: the fail-closed value for "a human identity was ASSERTED and
    /// we could not trust it".
    ///
    /// Public, unlike the entitled constructors, and safely so: it can only
    /// ever REDUCE privilege. It is strictly below [`Self::untrusted`] — same
    /// zero entitlements, plus a scope that tells a per-person consumer to
    /// decline rather than reach for a shared record. A dispatch path that
    /// cannot evaluate an assertion (no gateway configured, so no grant map to
    /// check) needs to be able to say this, and the alternative — falling back
    /// to `untrusted()`/`default()` — would quietly reinstate the service-scoped
    /// path for exactly the requests that failed verification.
    ///
    /// ```
    /// use terminus_rs::tool::{CallerContext, PersonScope};
    /// let c = CallerContext::unidentified();
    /// assert!(!c.may_infer_from_calendar() && !c.may_infer_from_routine());
    /// assert!(c.media_account().is_none());
    /// assert_eq!(c.person_scope(), PersonScope::Unidentified);
    /// assert_ne!(c, CallerContext::untrusted());
    /// ```
    pub const fn unidentified() -> Self {
        Self {
            may_infer_from_calendar: false,
            may_infer_from_routine: false,
            media_account: None,
            person: PersonScope::Unidentified,
        }
    }

    /// Build a context from a real, per-source `AllowlistPolicy` decision.
    ///
    /// `pub(super)` is the enforcement mechanism, not a style choice: it makes
    /// `crate::gateway_framework` the only module in the crate that can produce
    /// an entitled `CallerContext`, so holding one is proof that the allowlist
    /// was actually consulted. Do NOT widen this — see the type doc.
    pub(super) const fn from_allowlist_decision(
        may_infer_from_calendar: bool,
        may_infer_from_routine: bool,
    ) -> Self {
        Self {
            may_infer_from_calendar,
            may_infer_from_routine,
            media_account: None,
            person: PersonScope::Service,
        }
    }

    /// TERM #595: attach the human-identity scope the gateway resolved for this
    /// request.
    ///
    /// Same `pub(super)` boundary and same reason as
    /// [`Self::from_allowlist_decision`]: only `crate::gateway_framework` may
    /// say which HUMAN a caller is, because saying so requires having checked
    /// the on-behalf-of grant, the signature, the principal binding and the
    /// roster. A tool holding a [`PersonScope::Person`] therefore has proof all
    /// four happened.
    ///
    /// [`PersonScope::Unidentified`] additionally CLEARS every entitlement and
    /// the media account, in this one place, so that a refused assertion cannot
    /// be less privileged in name only: there is no path where a caller carries
    /// `Unidentified` and still holds operator context.
    pub(super) fn with_person_scope(mut self, scope: PersonScope) -> Self {
        if scope.is_unidentified() {
            self.may_infer_from_calendar = false;
            self.may_infer_from_routine = false;
            self.media_account = None;
        }
        self.person = scope;
        self
    }

    /// TERM #595: intern a roster-drawn person identifier into a
    /// [`PersonScope::Person`].
    ///
    /// `pub(super)` for the same reason as the constructors above. The value
    /// MUST already have been checked against the operator's roster by
    /// [`crate::mesh::person::verify`]: that is what keeps the intern table
    /// driven by OPERATOR configuration rather than by callers, and passing an
    /// unchecked, caller-varying string here would hand an attacker the ability
    /// to fill it.
    ///
    /// Note what that check does and does not buy. The roster is re-read per
    /// request, not frozen at startup, so the table is bounded by every
    /// identifier ever accepted in this process's lifetime — NOT by the roster
    /// as currently configured. Rotating the roster repeatedly would grow it.
    /// The hard bound is `account_intern::MAX_INTERNED`; past it, a person who
    /// cannot be recorded is treated as unidentified rather than as the service.
    pub(super) fn person_scope_for(person: &str) -> PersonScope {
        match account_intern::intern(person) {
            Some(interned) => PersonScope::Person(interned),
            // The table is full, so this identity cannot be recorded. Answering
            // as the shared service would be the one direction this item exists
            // to rule out, so an unrecordable person is an UNidentified one.
            None => PersonScope::Unidentified,
        }
    }

    /// TERM #576: attach the household media account the gateway resolved for
    /// this principal. Same `pub(super)` boundary and same reason as
    /// [`Self::from_allowlist_decision`] — only `crate::gateway_framework` can
    /// say which account a caller IS, so a tool holding a populated value has
    /// proof that the mapping was actually consulted rather than read off an
    /// argument.
    ///
    /// Deliberately additive rather than a wider `from_allowlist_decision`
    /// signature: the two entitlements are independent, and keeping the
    /// existing constructor untouched means every caller that does not care
    /// about media keeps the fail-closed `None`.
    /// Takes a plain `&str` and interns it, so the field can stay `Copy` (see
    /// the type doc) without the caller having to know that.
    pub(super) fn with_media_account(mut self, account: Option<&str>) -> Self {
        // `and_then`, not `map`: an account we cannot intern resolves to NO
        // account rather than to someone else's.
        self.media_account = account.and_then(account_intern::intern);
        self
    }

    /// TEST-ONLY escape hatch for tests that need an already-entitled caller
    /// (the positive controls in `crate::weather`) without standing up a whole
    /// gateway + allowlist. Compiled ONLY under `cfg(test)`, so it does not
    /// exist in any shipped binary and cannot widen the production API.
    ///
    /// The deliberately awkward name is the point: a reviewer seeing it in
    /// non-test code knows immediately that something is wrong.
    #[cfg(test)]
    pub(crate) const fn entitled_for_test_only(
        may_infer_from_calendar: bool,
        may_infer_from_routine: bool,
    ) -> Self {
        Self {
            may_infer_from_calendar,
            may_infer_from_routine,
            media_account: None,
            person: PersonScope::Service,
        }
    }

    /// TERM #595 TEST-ONLY: build a person-scoped context without standing up a
    /// gateway, an allowlist and a signing key. `cfg(test)` only.
    #[cfg(test)]
    pub(crate) fn with_person_for_test_only(person: &str) -> Self {
        Self::untrusted().with_person_scope(Self::person_scope_for(person))
    }

    /// TERM #576 TEST-ONLY counterpart of [`Self::entitled_for_test_only`] for
    /// the media account, so `crate::media`'s tests can build an
    /// account-scoped caller without standing up a gateway. `cfg(test)` only —
    /// it does not exist in any shipped binary.
    #[cfg(test)]
    pub(crate) fn with_media_account_for_test_only(account: &str) -> Self {
        Self::untrusted().with_media_account(Some(account))
    }

    /// May a tool consult the OPERATOR's calendar on this caller's behalf?
    pub const fn may_infer_from_calendar(&self) -> bool {
        self.may_infer_from_calendar
    }

    /// May a tool consult the OPERATOR's configured home/work routine on this
    /// caller's behalf?
    pub const fn may_infer_from_routine(&self) -> bool {
        self.may_infer_from_routine
    }

    /// TERM #576: the household media account this caller IS, if the gateway
    /// could determine one.
    ///
    /// `None` means the caller's account is UNKNOWN, and a tool must read that
    /// as "not entitled to any household-derived personalisation" — never as
    /// "pick a reasonable default". Defaulting is exactly the defect this
    /// closes: `media_recommend` used to fall back to whichever household
    /// member watched most recently, so the answer depended on who watched
    /// last rather than on who was asking.
    pub fn media_account(&self) -> Option<&str> {
        self.media_account
    }

    /// TERM #595: which human this turn is being run for.
    ///
    /// A consumer that scopes per-person data MUST match on the variant rather
    /// than calling `.person()` and treating `None` as "use the shared record":
    /// [`PersonScope::Unidentified`] also yields `None`, and it means the
    /// opposite of "no person was involved".
    pub const fn person_scope(&self) -> PersonScope {
        self.person
    }

    /// Shorthand for [`PersonScope::person`] — the human identifier, when
    /// there is a trustworthy one.
    pub const fn person(&self) -> Option<&'static str> {
        self.person.person()
    }
}

/// TERM #576 (review round 2): the interning that lets [`CallerContext`] stay
/// `Copy` while still carrying a variable-length account id.
///
/// Deliberately tiny and deliberately private. The safety argument is entirely
/// about WHAT gets interned, not about the table: only
/// [`CallerContext::with_media_account`] calls this, only
/// `crate::gateway_framework` can call that, and the value it passes always
/// comes from the operator's configured account map — so the set of interned
/// strings is bounded by the operator's configuration (a handful of household
/// accounts), not by traffic. A caller-supplied `account_id` never reaches
/// here; it is only ever COMPARED against an already-interned value.
mod account_intern {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};

    fn table() -> &'static Mutex<HashSet<&'static str>> {
        static TABLE: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
        TABLE.get_or_init(|| Mutex::new(HashSet::new()))
    }

    /// The most distinct identifiers this process will ever intern.
    ///
    /// Interning leaks by construction (`Box::leak` is what buys the `'static`
    /// that keeps [`CallerContext`] `Copy`), so the table can only grow. Every
    /// value that reaches here has already been checked against operator
    /// configuration — the roster for a person, the account map for a media
    /// account — so growth is driven by an operator editing config, never by a
    /// caller. But config is re-read per request rather than frozen at startup,
    /// so repeated roster rotation over a long-lived process WOULD otherwise
    /// grow this without bound. The cap turns "unbounded" into "bounded", which
    /// is the honest guarantee.
    ///
    /// Sized for a household with room for years of churn: the real roster is a
    /// handful of names, so anything approaching this means something is wrong.
    pub(super) const MAX_INTERNED: usize = 1024;

    /// Return the one `'static` copy of `s`, allocating it on first sight, or
    /// `None` when the table is full.
    ///
    /// `None` is deliberately NOT "carry on without the identifier": every
    /// caller maps it to LESS privilege (an unidentified person, no media
    /// account), because failing to record who someone is must never resolve to
    /// treating them as the shared service. An already-interned value is always
    /// returned even at the cap, so a full table degrades new identities rather
    /// than breaking every existing one.
    ///
    /// A poisoned lock is recovered rather than propagated: this table holds no
    /// invariant that a panic elsewhere could have broken (it is a set of
    /// immutable strings), and panicking here would turn an unrelated failure
    /// into a gateway outage.
    pub(super) fn intern(s: &str) -> Option<&'static str> {
        let mut table = table().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        intern_into(&mut table, s, MAX_INTERNED)
    }

    /// The cap logic itself, against a CALLER-SUPPLIED table.
    ///
    /// Split out purely so a test can prove the bound binds without filling the
    /// process-wide table: interning leaks by construction, so a test that
    /// exhausted the real one would permanently degrade every later test in the
    /// same binary to `Unidentified` — a test that breaks its neighbours is
    /// worse than the leak it was checking for.
    fn intern_into(
        table: &mut HashSet<&'static str>,
        s: &str,
        max: usize,
    ) -> Option<&'static str> {
        if let Some(existing) = table.get(s) {
            return Some(existing);
        }
        if table.len() >= max {
            return None;
        }
        let leaked: &'static str = Box::leak(s.to_owned().into_boxed_str());
        table.insert(leaked);
        Some(leaked)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// The cap binds, and it fails in the SAFE direction: a value that
        /// cannot be recorded yields `None`, which every caller maps to LESS
        /// privilege (unidentified person, no media account) rather than to the
        /// shared service identity.
        #[test]
        fn the_cap_binds_and_already_interned_values_still_resolve() {
            let mut table = HashSet::new();
            for i in 0..4 {
                assert!(intern_into(&mut table, &format!("person-{i}"), 4).is_some());
            }
            assert_eq!(table.len(), 4);

            // Full: a NEW identifier can no longer be recorded.
            assert!(
                intern_into(&mut table, "one-too-many", 4).is_none(),
                "the cap must actually bind, or the leak is still unbounded"
            );
            assert_eq!(table.len(), 4, "a refused intern must not grow the table");

            // POSITIVE CONTROL: an ALREADY-interned value still resolves even at
            // the cap, so a full table degrades NEW identities rather than
            // breaking every existing one. Without this, an implementation that
            // simply returned `None` always would pass the assertion above.
            assert!(
                intern_into(&mut table, "person-0", 4).is_some(),
                "an already-interned value must keep working at the cap"
            );
        }

        /// Interning is idempotent: the same string always yields the same
        /// pointer, which is what makes `PersonScope` comparable by identity.
        #[test]
        fn interning_the_same_value_twice_returns_the_same_pointer() {
            let mut table = HashSet::new();
            let a = intern_into(&mut table, "alice", 8).unwrap(); // pii-test-fixture
            let b = intern_into(&mut table, "alice", 8).unwrap(); // pii-test-fixture
            assert!(std::ptr::eq(a, b));
            assert_eq!(table.len(), 1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// TRTR-05 boundary: the ONLY constructors this type exposes beyond
    /// `crate::gateway_framework` are the two unprivileged ones, and both are
    /// fully unentitled.
    ///
    /// ## What a runtime test can and cannot prove here
    ///
    /// The real boundary — "no module outside `crate::gateway_framework` can
    /// mint an entitled context" — is enforced by the COMPILER via the
    /// `pub(super)` on [`CallerContext::from_allowlist_decision`], so it cannot
    /// be observed at runtime: code that violates it does not build, and a test
    /// that tried to violate it would stop the test binary from compiling at
    /// all. This crate has no `trybuild`/compile-fail harness and TRTR-05 does
    /// not add one; the compile-time half is checked instead by the
    /// `compile_fail` doctest on [`CallerContext::untrusted`], which runs under
    /// `cargo test --doc` (NOT under `cargo test --lib`).
    ///
    /// What IS asserted here at runtime is the observable consequence: the
    /// public surface is unprivileged.
    #[test]
    fn public_constructors_are_never_entitled() {
        for ctx in [CallerContext::untrusted(), CallerContext::default()] {
            assert!(!ctx.may_infer_from_calendar());
            assert!(!ctx.may_infer_from_routine());
            // TERM #576: the media account is part of the same fail-closed
            // surface — a caller nobody vouched for IS nobody.
            assert_eq!(ctx.media_account(), None);
            // TERM #595: and it is not person-scoped either. Note this is
            // `Service`, NOT `Unidentified` — nobody attempted an assertion, so
            // the pre-#595 behaviour is the honest answer.
            assert_eq!(ctx.person_scope(), PersonScope::Service);
            assert_eq!(ctx.person(), None);
        }
        assert_eq!(CallerContext::default(), CallerContext::untrusted());
    }

    /// TERM #576: the media account rides the SAME unforgeable boundary as the
    /// weather entitlements — `from_allowlist_decision` alone never populates
    /// it, and the constructor that does is `pub(super)`.
    #[test]
    fn media_account_is_absent_unless_the_gateway_attaches_one() {
        assert_eq!(CallerContext::from_allowlist_decision(true, true).media_account(), None);

        let scoped = CallerContext::from_allowlist_decision(false, false).with_media_account(Some("acct-1"));
        assert_eq!(scoped.media_account(), Some("acct-1"));
        // Orthogonal to the weather entitlements in both directions.
        assert!(!scoped.may_infer_from_calendar() && !scoped.may_infer_from_routine());

        assert_eq!(
            CallerContext::from_allowlist_decision(false, false).with_media_account(None),
            CallerContext::untrusted()
        );
    }

    /// TERM #576 (review round 2): the public `Copy` contract, asserted in a
    /// form that FAILS TO COMPILE if it is ever dropped again.
    ///
    /// `ctx` is used after being passed by value twice; that is only legal
    /// while the type is `Copy`. Note what the failure looks like: `cargo test
    /// --lib` goes RED at COMPILE time, not with an assertion message. That is
    /// the honest shape of this property — it is a type-system fact, so a
    /// runtime `assert!` could never observe it. (The `const _` assertion at
    /// the top of this file catches the same regression even earlier, and the
    /// doctest on `untrusted()` catches it from OUTSIDE the crate, which is
    /// where a consumer would feel it.)
    #[test]
    fn caller_context_is_copy() {
        fn by_value(c: CallerContext) -> Option<&'static str> {
            c.media_account
        }

        let ctx = CallerContext::from_allowlist_decision(true, false).with_media_account(Some("acct-copy")); // pii-test-fixture: invented account id
        assert_eq!(by_value(ctx), Some("acct-copy"));
        assert_eq!(by_value(ctx), Some("acct-copy"));
        assert!(ctx.may_infer_from_calendar());

        // Interning is transparent: equal ids compare equal (and, because they
        // are interned, are literally the same pointer).
        let again = CallerContext::untrusted().with_media_account(Some("acct-copy")); // pii-test-fixture: invented account id
        assert_eq!(again.media_account(), Some("acct-copy"));
        assert!(std::ptr::eq(again.media_account().unwrap(), ctx.media_account().unwrap()));
    }

    /// TERM #595 POSITIVE CONTROL: a person-scoped context really does carry
    /// its own person, and two people are distinguishable. A build that
    /// refused every assertion would fail here.
    #[test]
    fn a_person_scoped_context_carries_that_person() {
        let alice = CallerContext::with_person_for_test_only("alice"); // pii-test-fixture: invented household name
        let bob = CallerContext::with_person_for_test_only("bob"); // pii-test-fixture
        assert_eq!(alice.person(), Some("alice"));
        assert_eq!(bob.person(), Some("bob"));
        assert_ne!(alice.person(), bob.person());
        assert!(alice.person_scope().is_person());
        assert!(!alice.person_scope().is_unidentified());
    }

    /// TERM #595: a REFUSED assertion is strictly less than the service
    /// identity — the clearing happens in `with_person_scope`, in one place, so
    /// there is no path that carries `Unidentified` alongside entitlements.
    #[test]
    fn an_unidentified_scope_clears_every_entitlement() {
        let entitled = CallerContext::from_allowlist_decision(true, true)
            .with_media_account(Some("acct-operator")); // pii-test-fixture: invented account id
        assert!(entitled.may_infer_from_calendar() && entitled.may_infer_from_routine());
        assert_eq!(entitled.media_account(), Some("acct-operator"));

        let refused = entitled.with_person_scope(PersonScope::Unidentified);
        assert!(!refused.may_infer_from_calendar(), "a refused identity keeps no calendar entitlement");
        assert!(!refused.may_infer_from_routine(), "a refused identity keeps no routine entitlement");
        assert_eq!(refused.media_account(), None, "a refused identity is nobody's account");
        assert_eq!(refused.person(), None);
        assert!(refused.person_scope().is_unidentified());
    }

    /// TERM #595: `Unidentified` and `Service` must never compare or read as
    /// the same thing — collapsing them is the fail-open bug the tri-state
    /// exists to prevent.
    #[test]
    fn unidentified_is_not_the_service_scope() {
        let service = CallerContext::from_allowlist_decision(true, true);
        let refused = service.with_person_scope(PersonScope::Unidentified);
        assert_ne!(service.person_scope(), refused.person_scope());
        assert_ne!(service, refused);
        // Both yield `None` from `person()` — which is exactly why a consumer
        // that scopes data must match on the variant, and why this test exists.
        assert_eq!(service.person(), None);
        assert_eq!(refused.person(), None);
    }

    /// TERM #595: the person rides the same unforgeable boundary as the weather
    /// entitlements and the media account — `from_allowlist_decision` alone
    /// never sets it, and the constructor that does is `pub(super)`.
    #[test]
    fn a_person_is_absent_unless_the_gateway_attaches_one() {
        assert_eq!(
            CallerContext::from_allowlist_decision(true, true).person_scope(),
            PersonScope::Service
        );
    }

    /// TERM #595: interning keeps the type `Copy` and is pointer-stable, so a
    /// person identifier costs nothing to carry through dispatch.
    #[test]
    fn person_scope_is_copy_and_interned() {
        fn by_value(c: CallerContext) -> Option<&'static str> {
            c.person()
        }
        let ctx = CallerContext::with_person_for_test_only("copy-person"); // pii-test-fixture
        assert_eq!(by_value(ctx), Some("copy-person"));
        assert_eq!(by_value(ctx), Some("copy-person"));
        let again = CallerContext::with_person_for_test_only("copy-person"); // pii-test-fixture
        assert!(std::ptr::eq(again.person().unwrap(), ctx.person().unwrap()));
    }

    #[test]
    fn from_allowlist_decision_carries_each_source_independently() {
        let cal = CallerContext::from_allowlist_decision(true, false);
        assert!(cal.may_infer_from_calendar() && !cal.may_infer_from_routine());

        let routine = CallerContext::from_allowlist_decision(false, true);
        assert!(!routine.may_infer_from_calendar() && routine.may_infer_from_routine());

        assert_eq!(CallerContext::from_allowlist_decision(false, false), CallerContext::untrusted());
    }
}
