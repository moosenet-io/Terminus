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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CallerContext {
    may_infer_from_calendar: bool,
    may_infer_from_routine: bool,
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
    pub const fn untrusted() -> Self {
        Self { may_infer_from_calendar: false, may_infer_from_routine: false }
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
        Self { may_infer_from_calendar, may_infer_from_routine }
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
        Self { may_infer_from_calendar, may_infer_from_routine }
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
        }
        assert_eq!(CallerContext::default(), CallerContext::untrusted());
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
