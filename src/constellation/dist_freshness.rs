//! TERM #654: the built web bundle must actually contain the routes the source registers.
//!
//! ## The trap this closes
//!
//! `constellation-web/dist` is a COMMITTED build artifact, embedded into the binary by
//! rust-embed. Nothing in either test suite connected the two: the TypeScript tests exercise
//! `src/`, the Rust tests exercise Rust, and `dist` sat between them, checked by nobody. So a
//! change could add a page — component, panel registration, client, tests, all green — and merge
//! with a STALE bundle, and the page would simply not exist in the deployed GUI. Every gate
//! passes. The operator sees a nav entry that was never built.
//!
//! That is this sprint's signature defect once more: a component that cannot produce a result
//! reporting one anyway. It was found by review, not by a test, which is exactly why this file
//! exists.
//!
//! ## Why it scans the ARTIFACT and not the source
//!
//! A test that read `registerPanels.ts` and checked for the route would pass on a stale bundle —
//! it would be green precisely when the defect is present, which is the fake-guard shape this
//! item has spent four review rounds removing. The only evidence that survives the question "did
//! the dist get rebuilt?" is the dist itself.
//!
//! ## What it does NOT claim
//!
//! It is a FRESHNESS check on a small set of load-bearing routes, not a build-equivalence proof.
//! It cannot tell you the bundle was built from THIS commit — only that every route named below
//! is present in it. That is enough to catch the failure it exists to catch (a route added to the
//! source and never built) and it is honest about the rest: a bundle stale in some other way
//! passes here, and only a rebuild-and-diff would catch that.

/// Routes that must be present in the shipped bundle.
///
/// Deliberately short and load-bearing rather than exhaustive. Each entry is a route whose
/// absence makes a feature unreachable in the deployed GUI, and each is a plain string literal
/// that Vite carries into the bundle verbatim (they are `path:` values in `registerPanels.ts`).
///
/// ADD A ROUTE HERE when you add a panel whose absence would be invisible. The cost of a stale
/// entry is a red test that tells you to rebuild; the cost of a missing one is a feature that
/// silently does not ship.
const REQUIRED_ROUTES: &[&str] = &[
    // TERM #654 — the account surface. First because it is the one page a fresh deployment
    // cannot come up without: nothing else can create the account every other surface needs.
    "/terminus/accounts",
    // Its immediate neighbour, and the reason a stale bundle is plausible here at all: these two
    // are edited together and only one of them existed before.
    "/terminus/connectors",
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Every JS asset in the committed bundle, concatenated.
    ///
    /// Read from disk rather than through `WEB_ASSETS` so a failure names a real file the
    /// developer can look at, and so this does not depend on the embed macro having picked the
    /// files up (which is a different property, covered by `index_html_is_embedded`).
    fn bundle_text() -> (usize, String) {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/constellation-web/dist/assets");
        let mut text = String::new();
        let mut count = 0usize;
        let entries = std::fs::read_dir(dir)
            .unwrap_or_else(|e| panic!("cannot read the built bundle at {dir}: {e}"));
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("js") {
                count += 1;
                text.push_str(&std::fs::read_to_string(&path).unwrap_or_default());
            }
        }
        (count, text)
    }

    /// **Every required route is in the BUILT bundle.**
    ///
    /// Mutation-verify: revert `constellation-web/dist/` to its previous contents (or add a route
    /// to `registerPanels.ts` without running `npm run build`) and this goes red naming the route.
    /// That is precisely the state this change was in when review caught it.
    #[test]
    fn every_required_route_is_present_in_the_built_bundle() {
        let (assets, text) = bundle_text();
        // Non-vacuity: an empty or unreadable bundle must FAIL rather than pass a scan over
        // nothing. Without this the whole test is satisfied by a missing directory.
        assert!(assets > 0, "no JS assets found in the built bundle — the scan is looking at nothing");
        assert!(
            text.len() > 100_000,
            "the built bundle is {} bytes, far too small to be the real app — the scan would pass \
             over an empty or truncated artifact",
            text.len()
        );

        let missing: Vec<&str> = REQUIRED_ROUTES
            .iter()
            .copied()
            .filter(|route| !text.contains(route))
            .collect();
        assert!(
            missing.is_empty(),
            "these routes are registered in the web source but are ABSENT from the committed \
             build artifact, so they do not exist in the deployed GUI: {missing:?}. Run \
             `npm run build` in constellation-web/ and commit the regenerated dist/."
        );
    }
}
