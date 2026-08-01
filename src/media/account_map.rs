//! TERM #576 — which household media account a gateway principal IS.
//!
//! ## The problem this exists to solve
//!
//! `media_recommend` builds a taste profile from Plex watch history and
//! narrates it back ("because you watched X"). Watch history is per household
//! MEMBER, so the tool has to answer "whose?" before it answers anything else.
//! It used to answer it two ways, both wrong for a caller who is not the
//! operator:
//!
//! 1. With no `account_id` argument it took the account of the MOST RECENT
//!    history entry — i.e. whoever watched last, normally the operator. The
//!    answer depended on who watched, not on who asked.
//! 2. `account_id` was a free caller-supplied argument, so a guest could name
//!    another household member and receive that person's profile, rationales
//!    and (via the taste-memory decorator) curation notes. IDOR-shaped.
//!
//! Both are the same defect: the identity used to fetch household data came
//! from somewhere other than the authenticated caller.
//!
//! ## The mechanism (reused, not reinvented)
//!
//! The fix rides the TRTR-05 [`crate::tool::CallerContext`] channel that the
//! weather location fix established: a value minted ONLY by the gateway from a
//! server-verified `Principal`, threaded to the tool through
//! `RustTool::execute_with_caller`, unforgeable because the entitled
//! constructors are `pub(super)` to `crate::gateway_framework`. This module is
//! just the lookup that gateway calls; it holds no authority of its own, and
//! nothing here is reachable from tool arguments.
//!
//! ## Configuration
//!
//! `TERMINUS_MEDIA_ACCOUNT_MAP` — a JSON object mapping gateway principal name
//! to that person's media (Plex) account id:
//!
//! ```text
//! TERMINUS_MEDIA_ACCOUNT_MAP={"<operator-principal>":"<account-id>","<family-principal>":"<account-id>"}
//! ```
//!
//! **Unset, malformed, or missing an entry all resolve to `None`**, and `None`
//! is the unentitled path — no taste profile, no curation notes, no titles
//! drawn from anyone's history. That is deliberate: the failure mode of a
//! typo'd map must be "nobody gets personalisation", never "everybody gets the
//! operator's". An operator who wants personalisation opts in per principal, by
//! name.
//!
//! ## Scope limit (inherited, and load-bearing — TERM #577)
//!
//! A principal names a SERVICE, not a person, and every human who talks to
//! Lumina currently arrives as one shared assistant identity. So mapping that
//! shared identity to an account gives THAT account's personalisation to
//! whoever is in the room — the same gap
//! [`crate::gateway_framework::GUEST_BASELINE_ALLOW`] documents for the weather
//! probes. This module closes the separately-authenticated-principal case (a
//! guest with its own cert/PAT, and any future per-human identity) and does not
//! pretend to close the shared-identity case. Do not read a populated map as
//! household-level privacy until TERM #577 propagates human identity.

use std::sync::Arc;

use tracing::warn;

/// Env var holding the principal → media-account map. Read at lookup time
/// rather than cached so an operator can correct a mapping by restarting the
/// service without a rebuild, and so tests can vary it; the map is a handful of
/// entries and this runs once per authorized tool dispatch.
pub const ACCOUNT_MAP_ENV: &str = "TERMINUS_MEDIA_ACCOUNT_MAP";

/// Env var naming the media account that `PLEX_TOKEN` itself speaks for — the
/// account whose "continue watching" row `GET /library/onDeck` returns.
///
/// `media_on_deck` has no per-account query surface (see
/// [`crate::media::clients::plex::PlexClient::on_deck`]: one endpoint, one
/// admin token, no account parameter), so the only honest scoping available is
/// "disclose it to the account it actually belongs to". Unset ⇒ nobody, which
/// is fail-closed and means an operator must name their own account id to keep
/// their on-deck surface working.
pub const TOKEN_ACCOUNT_ENV: &str = "PLEX_ACCOUNT_ID";

/// Pure lookup, split out from the env read so it is unit-testable without
/// touching process state. `raw` is the JSON map document.
///
/// Fail-closed on every unhappy shape: absent, blank, not an object, a
/// non-string or blank value. A malformed map yields `None` for EVERY
/// principal rather than a partial map, because a partially-parsed
/// authorization input is how a typo turns into a silent grant.
fn lookup(raw: Option<&str>, principal: &str) -> Option<Arc<str>> {
    let raw = raw.map(str::trim).filter(|s| !s.is_empty())?;
    let parsed: serde_json::Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(e) => {
            warn!("{ACCOUNT_MAP_ENV} is not valid JSON, treating every caller as unmapped: {e}");
            return None;
        }
    };
    let Some(obj) = parsed.as_object() else {
        warn!("{ACCOUNT_MAP_ENV} must be a JSON object of principal -> account id; treating every caller as unmapped");
        return None;
    };
    obj.get(principal)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(Arc::from)
}

/// The household media account `principal` IS, or `None` when the operator has
/// not bound it to one.
///
/// Called by [`crate::gateway_framework::GatewayFramework::caller_context`] and
/// nowhere else in production — this is a lookup, not a decision point.
pub fn account_for_principal(principal: &str) -> Option<Arc<str>> {
    lookup(std::env::var(ACCOUNT_MAP_ENV).ok().as_deref(), principal)
}

/// The account `PLEX_TOKEN` speaks for — see [`TOKEN_ACCOUNT_ENV`]. `None` when
/// unset/blank, which withholds the on-deck surface from everyone.
pub fn plex_token_account() -> Option<String> {
    std::env::var(TOKEN_ACCOUNT_ENV).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_a_named_principal_to_its_account() {
        let raw = r#"{"principal-a":"11","principal-b":"22"}"#;
        assert_eq!(lookup(Some(raw), "principal-a").as_deref(), Some("11"));
        assert_eq!(lookup(Some(raw), "principal-b").as_deref(), Some("22"));
    }

    #[test]
    fn an_unnamed_principal_is_unmapped_not_defaulted() {
        // The load-bearing one: a guest that nobody bound to an account must
        // come back `None`, never "the first entry" or "the busiest account".
        let raw = r#"{"principal-a":"11"}"#;
        assert_eq!(lookup(Some(raw), "guest-principal"), None);
    }

    #[test]
    fn unset_or_blank_map_is_unmapped() {
        assert_eq!(lookup(None, "principal-a"), None);
        assert_eq!(lookup(Some("   "), "principal-a"), None);
    }

    #[test]
    fn malformed_map_fails_closed_for_everyone() {
        // Not JSON at all.
        assert_eq!(lookup(Some("principal-a=11"), "principal-a"), None);
        // Valid JSON, wrong shape.
        assert_eq!(lookup(Some(r#"["principal-a"]"#), "principal-a"), None);
        assert_eq!(lookup(Some(r#""principal-a""#), "principal-a"), None);
        // Right shape, unusable value.
        assert_eq!(lookup(Some(r#"{"principal-a":11}"#), "principal-a"), None);
        assert_eq!(lookup(Some(r#"{"principal-a":""}"#), "principal-a"), None);
        assert_eq!(lookup(Some(r#"{"principal-a":"  "}"#), "principal-a"), None);
    }

    /// END-TO-END through the real gateway, nothing stubbed between the config
    /// string and the minted context: env JSON → [`account_for_principal`] →
    /// `GatewayFramework::caller_context(Principal)` → `CallerContext`.
    ///
    /// This is the wire the media tools' own tests take as given, so it is
    /// asserted here rather than assumed.
    #[test]
    #[serial_test::serial]
    fn the_gateway_mints_the_mapped_account_and_only_for_a_mapped_principal() {
        use crate::gateway_framework::rate_limit::InProcessRateLimiter;
        use crate::gateway_framework::{AllowlistPolicy, GatewayFramework};
        use crate::mesh::{Principal, PrincipalSource};

        std::env::set_var(ACCOUNT_MAP_ENV, r#"{"principal-operator":"acct-operator"}"#);

        let fw = GatewayFramework::new(
            AllowlistPolicy::from_config_for_test(r#"{"guest-principal": ["media_recommend"]}"#, vec!["guest-principal".to_string()]),
            std::sync::Arc::new(InProcessRateLimiter::new(10, 1000.0)),
        );

        let mapped = fw.caller_context(Some(&Principal::new("principal-operator", PrincipalSource::MtlsCert)));
        assert_eq!(mapped.media_account(), Some("acct-operator"));

        // A guest holding the media_recommend GRANT still has no account --
        // the grant lets them call the tool, it does not make them somebody.
        let guest = fw.caller_context(Some(&Principal::new("guest-principal", PrincipalSource::MtlsCert)));
        assert_eq!(guest.media_account(), None);

        // No verified principal at all: the same fail-closed answer.
        assert_eq!(fw.caller_context(None).media_account(), None);

        std::env::remove_var(ACCOUNT_MAP_ENV);
    }
}
