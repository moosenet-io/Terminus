//! Atlas KG canonical project keys (TERM #652 / TERM #653).
//!
//! # Why this module exists
//!
//! The Atlas knowledge graph is stored as one file per project under
//! `SCRIBE_KG_STORE_DIR`, named after the caller-supplied `project_id`. Because
//! every caller supplied its own spelling, the live store accumulated TWO
//! entries per project with DIVERGENT freshness — measured 2026-08-05 in the
//! deployed store:
//!
//! ```text
//! chord.*    2026-07-17 23:40   STALE
//! chrd.*     2026-08-05 13:44   FRESH
//! ```
//!
//! …and likewise `harmony`/`harm`, `terminus`/`term`, `lumina`/`lum`. A query
//! against the stale spelling returned stale answers SILENTLY, and the wrong
//! answer looked perfectly healthy. Separately, a project **UUID** (the key
//! space `kg_findings` uses) matched no file at all, and that miss was reported
//! as *"no knowledge graph for this project"* — a statement about the system
//! that was FALSE, and which caused a real false bug report.
//!
//! # The fix shape: make the bad state unrepresentable
//!
//! Both defects are the same defect: **more than one key could address one
//! project's graph.** So this module removes that possibility rather than
//! patching its symptoms.
//!
//! [`ProjectKey`] is the ONLY type that can name a graph in the store. Its sole
//! constructor, [`ProjectKey::resolve`], normalizes and then collapses every
//! known alias onto ONE canonical key. `GraphStore` builds its paths from a
//! `ProjectKey` and nothing else, so a raw, un-canonicalized string cannot
//! reach the filesystem — there is no code path left that writes `chord.json`
//! while reading `chrd.json`. The stale side does not "refuse to answer"; it
//! becomes *unaddressable*, which is strictly stronger.
//!
//! A source-level ratchet test (`no_raw_slug_paths_in_kg_module`) fails the
//! build if any KG module reconstructs a store path from a raw slug again.
//!
//! # Dependencies: deliberately none
//!
//! This module uses `std` only. That is a requirement, not an accident: it lets
//! the mutation-testing harness `#[path]`-include this exact file and compile it
//! standalone with `rustc --test` in ~2s, so every defended behaviour here can
//! be broken and re-verified without spending a cargo build. Do not add a
//! `crate::` import here.

use std::collections::hash_map::DefaultHasher;
use std::collections::BTreeSet;
use std::hash::{Hash, Hasher};

/// Env var carrying DEPLOYMENT-SPECIFIC aliases as `alias=canonical` pairs
/// separated by `,` or whitespace, e.g.
/// `SCRIBE_KG_PROJECT_ALIASES="<project-uuid>=chrd,<project-uuid>=term"`.
///
/// Plane project UUIDs live HERE and never in tracked source: they are
/// fleet-identifying values, and S1 forbids literal infrastructure identifiers
/// in the repo. The built-in table below therefore carries only the four
/// plain-word duplicates that were actually measured in the store.
///
/// Parsing is fail-soft by design — a malformed entry is skipped, never fatal.
/// A key resolver that can panic or hard-error would take down every `kg_*`
/// read for a typo in an ops config.
pub const ALIASES_ENV: &str = "SCRIBE_KG_PROJECT_ALIASES";

/// Built-in alias table: the long-form project names that were found duplicating
/// their canonical Plane-prefix key in the live store (measured 2026-08-05).
///
/// Entries are `(alias, canonical)` and BOTH sides must already be normalized —
/// [`self_check_builtin_aliases`] proves it, and a canonical target must never
/// itself be an alias key (no chains).
const BUILTIN_ALIASES: &[(&str, &str)] = &[
    ("chord", "chrd"),
    ("harmony", "harm"),
    ("lumina", "lum"),
    ("terminus", "term"),
];

/// The canonical, filesystem-safe key naming one project's Atlas graph.
///
/// Constructible ONLY through [`ProjectKey::resolve`]. The inner `String` is
/// private specifically so no caller can mint a key that skipped alias
/// collapsing — that skip is exactly the bug this type exists to remove.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProjectKey(String);

impl ProjectKey {
    /// Resolve any caller-supplied project identifier — a Plane prefix
    /// (`TERM`), a long name (`Chord`), or a project UUID — to the one
    /// canonical key that addresses its graph.
    ///
    /// Two steps, in order:
    /// 1. **Normalize** ([`normalize`]) — lowercase ASCII alphanumerics,
    ///    everything else collapsed to a single `-`. Byte-for-byte compatible
    ///    with the `slugify` that named the graph files already on disk, so
    ///    existing stores keep resolving (pinned by
    ///    `normalize_matches_slugify_for_existing_store_keys`).
    /// 2. **Collapse aliases** — built-in table first, then the deployment
    ///    table from [`ALIASES_ENV`], which may override a built-in.
    ///
    /// Alias resolution is single-hop-with-a-guard: it iterates to a fixed
    /// point but tracks visited keys, so a mis-configured cycle
    /// (`a=b,b=a`) terminates and yields a stable key instead of hanging.
    pub fn resolve(raw: &str) -> ProjectKey {
        let start = normalize(raw);
        ProjectKey(collapse(start, &alias_pairs()))
    }

    /// The canonical key as a string — a safe single path component.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Every raw key that resolves to this canonical key, INCLUDING the
    /// canonical key itself, sorted and de-duplicated.
    ///
    /// This is the read-side counterpart of collapsing: a store that was
    /// written under an alias before this change (or a row in an external
    /// table keyed by UUID) is still readable by asking for all of a project's
    /// spellings at once, with no data migration and no rewrite of history.
    pub fn aliases(&self) -> Vec<String> {
        let pairs = alias_pairs();
        let mut out: BTreeSet<String> = BTreeSet::new();
        out.insert(self.0.clone());
        // Compare each alias's FULLY RESOLVED key, not its direct target.
        // Review finding (codex, verified): matching only direct targets missed
        // chained aliases — with `xx=chord` configured and the built-in
        // `chord -> chrd`, `xx` resolves to `chrd` but was absent from
        // `aliases("chrd")`, so kg_findings' ANY-binding silently skipped every
        // row recorded under `xx`. Built-ins forbid chains; env entries do not.
        for (alias, _) in &pairs {
            if collapse(alias.clone(), &pairs) == self.0 {
                out.insert(alias.clone());
            }
        }
        out.into_iter().collect()
    }
}

impl std::fmt::Display for ProjectKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A conservative filesystem-safe normalization: lowercase ASCII alphanumerics
/// and single hyphens only. Path separators, `..`, and every other byte are
/// dropped or collapsed, so a caller-supplied project id can never escape the
/// store directory.
///
/// Deliberately byte-for-byte identical to `crate::scribe::vault::slugify`,
/// including its `untitled-<hash>` fallback for input with no ASCII
/// alphanumerics at all — the graph files on disk were named by that function,
/// so any divergence would silently orphan a live store. The equivalence is
/// asserted by a test in the crate build; this copy exists so the module stays
/// dependency-free for the standalone mutation harness.
pub fn normalize(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut last_was_sep = false;
    for c in input.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_was_sep = false;
        } else if !last_was_sep && !out.is_empty() {
            out.push('-');
            last_was_sep = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        let mut hasher = DefaultHasher::new();
        input.hash(&mut hasher);
        return format!("untitled-{:08x}", hasher.finish() as u32);
    }
    out
}

/// Follow the alias table to a fixed point.
///
/// A cyclic table must not merely TERMINATE — every member of a cycle has to
/// land on the SAME key. Review finding (codex, verified): stopping at
/// "the key I re-entered on" made `a=b,b=c,c=a` resolve a->a, b->b, c->c —
/// three keys for one alias group, i.e. silently re-creating the exact
/// duplicate-key state (TERM #653) this type exists to remove, from a
/// misconfigured env var. So on a cycle we pick the lexicographically smallest
/// member, which is deterministic and identical whichever member you enter from.
///
/// No special case for a self-mapping: `parse_alias_env` skips those and
/// `check_alias_table` forbids them, and a 1-cycle would be handled by the same
/// branch anyway.
fn collapse(start: String, pairs: &[(String, String)]) -> String {
    let mut seen: Vec<String> = Vec::new();
    let mut cur = start;
    loop {
        if let Some(i) = seen.iter().position(|s| *s == cur) {
            let mut cycle: Vec<&String> = seen[i..].iter().collect();
            cycle.sort();
            return cycle[0].clone();
        }
        seen.push(cur.clone());
        match pairs.iter().find(|(a, _)| *a == cur) {
            Some((_, canonical)) => cur = canonical.clone(),
            None => return cur,
        }
    }
}

/// The effective alias table: built-ins, then [`ALIASES_ENV`] overrides.
///
/// Read fresh on each call rather than cached in a `OnceLock`: a cached table
/// would make a deployment's alias config unfixable without a service restart,
/// and this is a handful of string compares on a path that already touches the
/// filesystem.
fn alias_pairs() -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = BUILTIN_ALIASES
        .iter()
        .map(|(a, c)| ((*a).to_string(), (*c).to_string()))
        .collect();
    if let Ok(raw) = std::env::var(ALIASES_ENV) {
        for (alias, canonical) in parse_alias_env(&raw) {
            match pairs.iter_mut().find(|(a, _)| *a == alias) {
                Some(slot) => slot.1 = canonical,
                None => pairs.push((alias, canonical)),
            }
        }
    }
    pairs
}

/// Parse `alias=canonical` pairs separated by `,` or whitespace.
///
/// Fail-soft: an entry with no `=`, an empty side, or a side that normalizes
/// away entirely is SKIPPED. A self-mapping (`x=x`) is skipped too — it is
/// inert, and keeping it would only add a no-op hop. Both sides are normalized
/// so an operator can write `Chord=CHRD` and get the same table as `chord=chrd`.
pub fn parse_alias_env(raw: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for entry in raw.split(|c: char| c == ',' || c.is_whitespace()) {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let Some((alias, canonical)) = entry.split_once('=') else {
            continue;
        };
        if alias.trim().is_empty() || canonical.trim().is_empty() {
            continue;
        }
        let alias = normalize(alias);
        let canonical = normalize(canonical);
        if alias.starts_with("untitled-") || canonical.starts_with("untitled-") {
            // Nothing alphanumeric on one side — not a usable key.
            continue;
        }
        if alias == canonical {
            continue;
        }
        out.push((alias, canonical));
    }
    out
}

/// Suggest the closest known key to `input`, if one is close enough to be worth
/// printing. Used to turn a key miss into an actionable message.
///
/// Returns `None` when nothing is a plausible match, because a confidently
/// wrong suggestion is worse than none — that is the same failure class this
/// whole change is about.
pub fn nearest(input: &str, candidates: &[String]) -> Option<String> {
    let target = normalize(input);
    let mut best: Option<(usize, &String)> = None;
    for c in candidates {
        let d = edit_distance(&target, c);
        // Accept only a genuinely close neighbour. Project keys are SHORT
        // (`lum`, `term`, `chrd`), so a fraction-of-length bound alone rounds
        // down to 1 and rejects an ordinary transposition (`trem` -> `term`,
        // Levenshtein 2). Floor it at 1 and cap it at 4 so a 36-char UUID —
        // which resembles nothing — still gets no suggestion at all.
        let bound = std::cmp::min(4, std::cmp::max(1, std::cmp::max(target.len(), c.len()) / 2));
        if d > bound {
            continue;
        }
        if best.map_or(true, |(bd, _)| d < bd) {
            best = Some((d, c));
        }
    }
    best.map(|(_, c)| c.clone())
}

/// Levenshtein distance (two-row DP) over bytes — the keys are ASCII by
/// construction after [`normalize`].
fn edit_distance(a: &str, b: &str) -> usize {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, &ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = std::cmp::min(
                std::cmp::min(cur[j] + 1, prev[j + 1] + 1),
                prev[j] + cost,
            );
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Prove an alias table is well-formed: both sides already normalized, no alias
/// that is also a canonical target (no chains), no self-mapping.
///
/// Takes the table as a PARAMETER rather than reading the built-in const
/// directly, so each rejection can actually be exercised by a test. A
/// self-check whose failure branches cannot be reached is indistinguishable
/// from a self-check that does nothing — the same "looks like a result" trap
/// this module exists to remove.
pub fn check_alias_table(pairs: &[(&str, &str)]) -> Result<(), String> {
    for (alias, canonical) in pairs {
        if normalize(alias) != *alias {
            return Err(format!("alias {alias:?} is not normalized"));
        }
        if normalize(canonical) != *canonical {
            return Err(format!("canonical {canonical:?} is not normalized"));
        }
        if alias == canonical {
            return Err(format!("alias {alias:?} maps to itself"));
        }
        if pairs.iter().any(|(a, _)| a == canonical) {
            return Err(format!("canonical {canonical:?} is itself an alias (chain)"));
        }
    }
    Ok(())
}

/// Apply [`check_alias_table`] to the shipped built-in table.
pub fn self_check_builtin_aliases() -> Result<(), String> {
    check_alias_table(BUILTIN_ALIASES)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes the tests that mutate `SCRIBE_KG_PROJECT_ALIASES`. The
    /// standalone harness has no `serial_test` dependency, so this is a plain
    /// mutex — and it must survive a panicking test, hence the poison recovery.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn builtin_table_is_well_formed() {
        self_check_builtin_aliases().expect("built-in alias table");
    }

    #[test]
    fn a_malformed_alias_table_is_rejected_for_each_reason() {
        // A chain: resolving `a` would depend on iteration order.
        assert!(
            check_alias_table(&[("a", "b"), ("b", "c")])
                .unwrap_err()
                .contains("chain"),
            "a chained table must be rejected"
        );
        // A self-mapping: inert at best, confusing at worst.
        assert!(check_alias_table(&[("a", "a")]).unwrap_err().contains("itself"));
        // An un-normalized side would never match a resolved key, so the entry
        // would silently do nothing — the worst possible failure mode here.
        assert!(check_alias_table(&[("Chord", "chrd")]).unwrap_err().contains("not normalized"));
        assert!(check_alias_table(&[("chord", "CHRD")]).unwrap_err().contains("not normalized"));
        // A well-formed table passes.
        assert!(check_alias_table(&[("chord", "chrd"), ("harmony", "harm")]).is_ok());
        assert!(check_alias_table(&[]).is_ok());
    }

    #[test]
    fn canonical_key_is_idempotent() {
        for k in ["term", "chrd", "harm", "lum", "muse", "rail", "aptr"] {
            let once = ProjectKey::resolve(k);
            let twice = ProjectKey::resolve(once.as_str());
            assert_eq!(once, twice, "resolving a canonical key must be a no-op: {k}");
        }
    }

    #[test]
    fn measured_duplicate_spellings_collapse_onto_the_fresh_key() {
        // The exact four duplicate pairs measured in the deployed store on
        // 2026-08-05. The long name must land on the SHORT (fresh) side.
        for (stale, fresh) in [
            ("chord", "chrd"),
            ("harmony", "harm"),
            ("lumina", "lum"),
            ("terminus", "term"),
        ] {
            assert_eq!(ProjectKey::resolve(stale).as_str(), fresh, "{stale} must collapse");
            assert_eq!(ProjectKey::resolve(fresh).as_str(), fresh, "{fresh} is canonical");
        }
    }

    #[test]
    fn case_and_punctuation_do_not_create_a_second_key() {
        for spelling in ["Chord", "CHORD", "  chord  ", "Chord!", "chord\n"] {
            assert_eq!(
                ProjectKey::resolve(spelling).as_str(),
                "chrd",
                "{spelling:?} must not mint a distinct key"
            );
        }
    }

    #[test]
    fn a_project_without_an_alias_resolves_to_itself() {
        // muse/rail/aptr have graphs but no long-form duplicate — they must be
        // untouched by alias collapsing.
        for k in ["muse", "rail", "aptr"] {
            assert_eq!(ProjectKey::resolve(k).as_str(), k);
        }
    }

    #[test]
    fn normalization_can_never_escape_the_store_directory() {
        for evil in ["../../etc/passwd", "/abs/path", "a/b", "..", "."] {
            let k = ProjectKey::resolve(evil);
            let s = k.as_str();
            assert!(!s.contains('/'), "{evil:?} -> {s:?}");
            assert!(!s.contains('\\'), "{evil:?} -> {s:?}");
            assert!(!s.contains(".."), "{evil:?} -> {s:?}");
            assert!(!s.is_empty(), "{evil:?} -> empty");
        }
    }

    #[test]
    fn env_table_resolves_a_uuid_and_can_override_a_builtin() {
        let _g = env_lock();
        std::env::set_var( // hermeticity-allow: serialized on env_lock(); serial_test is unavailable to the standalone harness
            ALIASES_ENV,
            "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee=chrd, harmony=harmony-x", // pii-test-fixture (fabricated uuid shape)
        );
        assert_eq!(
            ProjectKey::resolve("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee").as_str(), // pii-test-fixture (fabricated uuid shape)
            "chrd",
            "a UUID must resolve through the deployment table"
        );
        assert_eq!(
            ProjectKey::resolve("HARMONY").as_str(),
            "harmony-x",
            "the deployment table overrides a built-in"
        );
        std::env::remove_var(ALIASES_ENV); // hermeticity-allow: serialized on env_lock(); serial_test is unavailable to the standalone harness
    }

    #[test]
    fn malformed_env_entries_are_skipped_not_fatal() {
        let _g = env_lock();
        std::env::set_var(ALIASES_ENV, "no-equals,=nothing,alsonothing=,,  ,x=x,ok=term"); // hermeticity-allow: serialized on env_lock(); serial_test is unavailable to the standalone harness
        // The good entry still lands...
        assert_eq!(ProjectKey::resolve("ok").as_str(), "term");
        // ...and nothing else was corrupted.
        assert_eq!(ProjectKey::resolve("chord").as_str(), "chrd");
        assert_eq!(ProjectKey::resolve("x").as_str(), "x", "self-mapping is inert");
        std::env::remove_var(ALIASES_ENV); // hermeticity-allow: serialized on env_lock(); serial_test is unavailable to the standalone harness
    }

    #[test]
    fn a_chained_alias_is_reported_by_aliases() {
        let _g = env_lock();
        // `xx -> chord -> chrd`. `xx` resolves to `chrd`, so anything keyed by
        // `xx` belongs to that project and MUST appear in its alias set, or
        // kg_findings' ANY-binding silently drops those rows.
        std::env::set_var(ALIASES_ENV, "xx=chord"); // hermeticity-allow: serialized on env_lock(); serial_test is unavailable to the standalone harness
        assert_eq!(ProjectKey::resolve("xx").as_str(), "chrd");
        let a = ProjectKey::resolve("chrd").aliases();
        assert!(a.contains(&"xx".to_string()), "chained alias must be listed: {a:?}");
        assert!(a.contains(&"chord".to_string()), "direct alias still listed: {a:?}");
        assert!(a.contains(&"chrd".to_string()), "canonical still listed: {a:?}");
        std::env::remove_var(ALIASES_ENV); // hermeticity-allow: serialized on env_lock(); serial_test is unavailable to the standalone harness
    }

    #[test]
    fn a_self_mapping_cannot_re_split_a_collapsed_project() {
        let _g = env_lock();
        // `chord=chord` is the obvious way an operator would try to "turn off"
        // a built-in alias. It must be INERT, not an un-alias switch: allowing
        // it would let one line of ops config re-create the exact duplicate-key
        // state (TERM #653) this type exists to make unrepresentable —
        // `chord.json` drifting stale beside a fresh `chrd.json` all over again.
        std::env::set_var(ALIASES_ENV, "chord=chord"); // hermeticity-allow: serialized on env_lock(); serial_test is unavailable to the standalone harness
        assert_eq!(
            ProjectKey::resolve("chord").as_str(),
            "chrd",
            "a self-mapping must not un-alias a project"
        );
        std::env::remove_var(ALIASES_ENV); // hermeticity-allow: serialized on env_lock(); serial_test is unavailable to the standalone harness
    }

    #[test]
    fn a_cyclic_alias_config_terminates() {
        let _g = env_lock();
        std::env::set_var(ALIASES_ENV, "a=b,b=c,c=a"); // hermeticity-allow: serialized on env_lock(); serial_test is unavailable to the standalone harness
        let k = ProjectKey::resolve("a");
        assert!(["a", "b", "c"].contains(&k.as_str()), "got {k}");
        // Deterministic across calls, so it cannot flap between two stores.
        assert_eq!(ProjectKey::resolve("a"), k);
        // ...and — the part that matters — EVERY member of the cycle must land
        // on that SAME key. Terminating is not enough: a cycle that resolved
        // each member to itself would hand one project three store keys, which
        // is precisely the duplicate-key defect, re-introduced by ops config.
        assert_eq!(ProjectKey::resolve("b"), k, "cycle member b must converge");
        assert_eq!(ProjectKey::resolve("c"), k, "cycle member c must converge");
        // Entering from any member yields one canonical alias group.
        assert_eq!(ProjectKey::resolve("a").aliases(), ProjectKey::resolve("c").aliases());
        std::env::remove_var(ALIASES_ENV); // hermeticity-allow: serialized on env_lock(); serial_test is unavailable to the standalone harness
    }

    #[test]
    fn aliases_lists_every_spelling_of_a_project_including_itself() {
        let _g = env_lock();
        std::env::remove_var(ALIASES_ENV); // hermeticity-allow: serialized on env_lock(); serial_test is unavailable to the standalone harness
        let a = ProjectKey::resolve("chrd").aliases();
        assert!(a.contains(&"chrd".to_string()), "canonical included: {a:?}");
        assert!(a.contains(&"chord".to_string()), "stale spelling included: {a:?}");
        // A project with no alias still reports itself — never an empty set,
        // which would make a caller read "no keys" as "no data".
        assert_eq!(ProjectKey::resolve("muse").aliases(), vec!["muse".to_string()]);
    }

    #[test]
    fn aliases_includes_a_configured_uuid() {
        let _g = env_lock();
        std::env::set_var(ALIASES_ENV, "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee=chrd"); // hermeticity-allow: serialized on env_lock(); serial_test is unavailable to the standalone harness
        let a = ProjectKey::resolve("chrd").aliases();
        assert!(
            a.contains(&"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".to_string()), // pii-test-fixture (fabricated uuid shape)
            "a UUID-keyed store must be reachable from the slug: {a:?}"
        );
        std::env::remove_var(ALIASES_ENV); // hermeticity-allow: serialized on env_lock(); serial_test is unavailable to the standalone harness
    }

    #[test]
    fn nearest_suggests_a_close_key_and_declines_a_far_one() {
        let known: Vec<String> = ["chrd", "term", "harm", "lum", "muse"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(nearest("chrdd", &known).as_deref(), Some("chrd"));
        assert_eq!(nearest("trem", &known).as_deref(), Some("term"));
        // A UUID resembles nothing — suggesting one would be a confident lie.
        assert_eq!(
            nearest("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee", &known), // pii-test-fixture (fabricated uuid shape)
            None
        );
        assert_eq!(nearest("completely-unrelated", &known), None);
    }

    #[test]
    fn nearest_over_an_empty_candidate_set_is_none() {
        assert_eq!(nearest("chrd", &[]), None);
    }

    #[test]
    fn normalize_matches_the_documented_slugify_contract() {
        // Pinned samples copied from vault::slugify's own tests — the crate
        // build additionally asserts full equivalence against the real
        // function; these keep the harness honest on its own.
        assert_eq!(normalize("Hello, World!"), "hello-world");
        assert_eq!(normalize("  Foo   Bar  "), "foo-bar");
        assert_eq!(
            normalize("S91-scribe-knowledge-infrastructure"),
            "s91-scribe-knowledge-infrastructure"
        );
        // Non-ASCII-only input gets the stable, non-empty fallback.
        let c = normalize("Модуль");
        assert!(c.starts_with("untitled-"));
        assert_eq!(normalize("Модуль"), c, "fallback is deterministic");
        assert_ne!(normalize("🎉🎊"), c, "distinct inputs stay distinct");
    }
}
