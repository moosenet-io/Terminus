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
        Self { may_infer_from_calendar: false, may_infer_from_routine: false, media_account: None }
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
        Self { may_infer_from_calendar, may_infer_from_routine, media_account: None }
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
        self.media_account = account.map(account_intern::intern);
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
        Self { may_infer_from_calendar, may_infer_from_routine, media_account: None }
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

    /// Return the one `'static` copy of `s`, allocating it on first sight.
    ///
    /// A poisoned lock is recovered rather than propagated: this table holds no
    /// invariant that a panic elsewhere could have broken (it is a set of
    /// immutable strings), and panicking here would turn an unrelated failure
    /// into a fail-OPEN gateway outage.
    pub(super) fn intern(s: &str) -> &'static str {
        let mut table = table().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(existing) = table.get(s) {
            return existing;
        }
        let leaked: &'static str = Box::leak(s.to_owned().into_boxed_str());
        table.insert(leaked);
        leaked
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

    #[test]
    fn from_allowlist_decision_carries_each_source_independently() {
        let cal = CallerContext::from_allowlist_decision(true, false);
        assert!(cal.may_infer_from_calendar() && !cal.may_infer_from_routine());

        let routine = CallerContext::from_allowlist_decision(false, true);
        assert!(!routine.may_infer_from_calendar() && routine.may_infer_from_routine());

        assert_eq!(CallerContext::from_allowlist_decision(false, false), CallerContext::untrusted());
    }
}
