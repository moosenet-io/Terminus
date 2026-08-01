//! LOCREG-01 — the shared, per-caller location registry.
//!
//! Operator framing (2026-08-01): *"set COMMUTE_HOME and COMMUTE_WORK as
//! variables that lumina can chat and set (asks each user)"*, then clarified:
//! *"these should just be generic work, home and other locations the user wants
//! to track, referenced by weather, commute, news and other modules"*.
//!
//! So this is **not a weather feature**. It is a registry that weather, commute,
//! news and future modules all consume, and none of them own. `weather` is
//! merely the first consumer wired to it (see the consumer contract below).
//!
//! # What it stores
//!
//! Named places, per caller: the well-known [`HOME`], [`WORK`] and [`CURRENT`],
//! plus any name the user chooses ("mum's house", "the jobsite", "denver").
//! Each entry is either permanent or **temporary with an absolute expiry**, so
//! "I'm in Denver this week" cannot quietly become "I live in Denver".
//!
//! # The rule that shapes everything else
//!
//! A home address is among the most sensitive data in this fleet. It must never
//! appear in a response to an unentitled caller, in an error message, in a log
//! line, or in a test fixture that reaches the PII-scrubbed public mirror.
//! Concretely, in this module:
//!
//! * The entitlement gate runs **before the store is touched at all**, so an
//!   unentitled call causes zero reads rather than a read whose result is
//!   discarded. There is nothing in memory to leak through a later refactor.
//! * [`StoreError`](store::StoreError) is categorical and content-free — no
//!   values, no keys, no paths.
//! * Every fixture in this module's tests is an obvious placeholder carrying a
//!   `// pii-test-fixture` marker and a reason.
//!
//! # Whose locations? The identity gap (TERM #577)
//!
//! **Every human talking to Lumina currently arrives at this gateway as
//! `identity=lumina`.** The mTLS principal names the SERVICE, not the person;
//! `X-Lumina-User` exists at the web edge but is never forwarded through Chord
//! and never reaches authorization. That is tracked as TERM #577 and it is not
//! fixed here.
//!
//! This registry is therefore keyed on [`CallerKey`] — a caller-identity
//! ABSTRACTION — rather than on a bare principal string:
//!
//! * **Today** it is correct for separately-authenticated principals: two
//!   principals get two records and neither can see the other's.
//! * **When #577 closes** the same key gains a person component
//!   ([`CallerKey::for_person`]) and becomes genuinely per-person, with no
//!   rewrite of the store, the tools, or any consumer.
//! * **The migration is deliberately non-silent.** A person-scoped key is a
//!   DIFFERENT storage key from the service-scoped one, so records written
//!   before #577 become orphaned rather than shared with everyone in the
//!   household. Orphaned data is a re-entry prompt; silently shared data is the
//!   leak. See [`CallerKey::storage_key`].
//!
//! Because of the gap, nothing in this module describes itself to the user as
//! per-PERSON. The tool descriptions say "your saved locations" scoped to the
//! assistant, and that is the literal truth today.
//!
//! # Entitlement
//!
//! Reuses the existing [`CallerContext`](crate::tool::CallerContext) mechanism —
//! specifically [`CallerContext::may_infer_from_routine`], the flag the gateway
//! derives from the caller's grant on `commute_estimate`. No second mechanism is
//! invented, and `caller_context.rs` is untouched.
//!
//! That flag's original meaning was "may a tool consult the OPERATOR's
//! configured home/work routine". The registry IS the modern form of that
//! configured routine, so it is the right gate — and because the registry is
//! keyed per caller, holding the flag grants access to YOUR OWN record only. The
//! flag stops meaning "may see the operator's addresses" and comes to mean "may
//! use stored-location context at all". A household guest, who is deliberately
//! denied that grant, reads nothing and writes nothing.
//!
//! Widening participation to guests is therefore a GRANT change (give the guest
//! identity the `commute_estimate` grant) and not a code change — at which point
//! the per-caller keying already confines them to their own record.
//!
//! # Consumer contract
//!
//! A module that wants a location resolves it the same way every other module
//! does, and adding a consumer needs no change in here:
//!
//! ```no_run
//! # use terminus_rs::locations::{self, CallerKey, Lookup, HOME};
//! # use terminus_rs::locations::store::LocationStore;
//! # use terminus_rs::tool::CallerContext;
//! # fn demo(store: &dyn LocationStore, key: Option<&CallerKey>, caller: CallerContext) {
//! match locations::lookup(store, key, caller, HOME) {
//!     Lookup::Found(entry) => { /* use entry.value, and SAY where it came from */ }
//!     Lookup::NotSet      => { /* ask — never invent, never infer */ }
//!     Lookup::Denied      => { /* this caller may not use stored locations */ }
//!     Lookup::Unavailable(_) => { /* say "I couldn't read your saved locations" */ }
//! }
//! # }
//! ```
//!
//! The four outcomes are the whole contract, and the last two are the point:
//!
//! * [`Lookup::NotSet`] means the registry was READ and holds nothing under that
//!   name (or holds only an expired entry).
//! * [`Lookup::Unavailable`] means the registry could not be read.
//!
//! A consumer must keep them distinct in what it says, and **must never invent
//! or infer a location to fill either gap**. Earlier this sprint the assistant
//! confidently named a city the operator had no connection to; that bug came in
//! through exactly this door and must not come back through it.
//!
//! `weather` is the proving consumer ([`crate::weather::location::Routine`]).

pub mod store;

use crate::tool::CallerContext;
use store::{CallerRecord, LocationStore, Registry, StoreError, StoredLocation};

/// The well-known name for where the caller lives.
pub const HOME: &str = "home";
/// The well-known name for where the caller works.
pub const WORK: &str = "work";
/// The well-known name for "where I am right now" — the travel override.
///
/// A consumer that resolves a routine location should prefer a non-expired
/// `current` over `home`/`work`: it is what makes *"I'm in Denver this week"*
/// work, and because temporary entries expire it is also what stops that
/// answer outliving the trip.
pub const CURRENT: &str = "current";

/// Names a user cannot invent, because a consumer gives them meaning.
pub const WELL_KNOWN: [&str; 3] = [HOME, WORK, CURRENT];

/// Longest accepted name and value. Generous for a real address, short enough
/// that the document stays small and a pasted essay is rejected as the mistake
/// it is.
const MAX_NAME_LEN: usize = 48;
const MAX_VALUE_LEN: usize = 200;

/// Upper bound on a temporary entry's lifetime (one year). A "temporary"
/// location with a ten-year expiry is a permanent one wearing a disguise, and
/// the whole point of the temporary kind is that it cannot become permanent by
/// accident.
pub const MAX_TEMPORARY_HOURS: i64 = 24 * 365;

// ── Caller identity ─────────────────────────────────────────────────────────

/// WHO a registry record belongs to.
///
/// Not a bare principal string, deliberately — see this module's TERM #577
/// note. The key has a SCHEME, so the day a human identity is threaded through
/// the gateway the key shape changes visibly instead of silently re-pointing
/// existing records at a different set of people.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CallerKey {
    /// The server-verified principal (`crate::mesh::Principal::name`).
    principal: String,
    /// The human behind that principal, once TERM #577 makes one available.
    /// `None` today, for every caller, because no such identity reaches
    /// authorization yet.
    person: Option<String>,
}

impl CallerKey {
    /// The key for a server-verified principal.
    ///
    /// This is what the dispatch layer builds today. It is honest about what it
    /// knows: a SERVICE identity, not a person.
    pub fn for_principal(principal: &crate::mesh::Principal) -> Option<Self> {
        Self::for_principal_name(principal.name())
    }

    /// Same, from an already-canonical principal NAME.
    ///
    /// `None` for an empty/blank name: with no identity there is no record to
    /// own, and the fail-closed answer is "this caller has no registry" rather
    /// than "this caller shares the blank-named one".
    pub fn for_principal_name(name: &str) -> Option<Self> {
        let name = name.trim();
        if name.is_empty() {
            return None;
        }
        Some(Self { principal: name.to_ascii_lowercase(), person: None })
    }

    /// The key for a specific PERSON behind a principal — the shape this
    /// becomes once TERM #577 propagates a human identity to authorization.
    ///
    /// Nothing calls this in production yet; it exists so the store, the tools
    /// and every consumer are already written against the final key shape. When
    /// #577 lands, the ONLY change needed is that the dispatch layer builds the
    /// key with this constructor instead of [`CallerKey::for_principal`].
    pub fn for_person(principal: &str, person: &str) -> Option<Self> {
        let mut key = Self::for_principal_name(principal)?;
        let person = person.trim();
        if person.is_empty() {
            return Some(key);
        }
        key.person = Some(person.to_ascii_lowercase());
        Some(key)
    }

    /// The principal this key is scoped to — used by the legacy `COMMUTE_*`
    /// fallback, which applies to exactly one principal (see
    /// [`crate::weather::location::Routine::resolve_for`]).
    pub fn principal(&self) -> &str {
        &self.principal
    }

    /// Whether this key names a person, or only a service.
    ///
    /// A consumer that wants to say "your locations" versus "this assistant's
    /// locations" can ask; today the answer is always `false`.
    pub fn is_person_scoped(&self) -> bool {
        self.person.is_some()
    }

    /// The string this caller's record is filed under.
    ///
    /// `svc:<principal>` today; `svc:<principal>#person:<person>` once #577
    /// lands. They are different strings ON PURPOSE: after the migration a
    /// service-scoped record is reachable by nobody rather than by everybody.
    /// Orphaning data costs one "where's home again?"; sharing it hands one
    /// person's home address to whoever is in the room.
    pub fn storage_key(&self) -> String {
        match &self.person {
            None => format!("svc:{}", self.principal),
            Some(p) => format!("svc:{}#person:{p}", self.principal),
        }
    }
}

// ── Outcomes ────────────────────────────────────────────────────────────────

/// The result of asking the registry for ONE name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup {
    /// A live (non-expired) entry.
    Found(StoredLocation),
    /// The registry was read and holds nothing usable under that name.
    NotSet,
    /// This caller may not use stored locations at all. No read happened.
    Denied,
    /// The registry could not be read. NOT the same as [`Lookup::NotSet`].
    Unavailable(StoreError),
}

/// The result of listing everything a caller has stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Listing {
    /// Live entries first, then expired ones (surfaced, not hidden — an expired
    /// travel location the user forgot about should be visible and clearable).
    Entries { live: Vec<(String, StoredLocation)>, expired: Vec<(String, StoredLocation)> },
    Denied,
    Unavailable(StoreError),
}

/// The result of a write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteOutcome {
    /// Stored. `replaced` is the previous entry, when there was one.
    Stored { name: String, entry: StoredLocation, replaced: Option<StoredLocation> },
    /// There is already a DIFFERENT value under this name and the caller did
    /// not confirm. Nothing was written. The existing value is NOT returned —
    /// the caller already knows it if they are entitled to it, and echoing it
    /// into a confirmation prompt is a needless extra place for an address to
    /// appear.
    NeedsConfirmation { name: String, existing_is_temporary: bool },
    /// The name or the value was not acceptable. Carries a reason that never
    /// quotes the value.
    Rejected(String),
    Denied,
    Unavailable(StoreError),
}

/// The result of a clear.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClearOutcome {
    /// Removed `count` entries (1 for a named clear, N for a confirmed
    /// clear-everything).
    Cleared { count: usize },
    /// There was nothing under that name to clear.
    NotSet,
    /// A clear-everything was requested without confirmation. Nothing removed.
    NeedsConfirmation,
    Rejected(String),
    Denied,
    Unavailable(StoreError),
}

// ── The gate ────────────────────────────────────────────────────────────────

/// The single entitlement decision, and the ONLY way any function in this
/// module obtains a storage key.
///
/// Two conditions, both required:
///
/// * the caller holds `may_infer_from_routine` (the gateway-derived,
///   unforgeable entitlement — see [`crate::tool::CallerContext`]), and
/// * the dispatch layer knew who they were.
///
/// It returns `None` — not an error, not a default key — for anything else, and
/// every public function checks it BEFORE constructing or touching a store.
/// That ordering is what makes "an unentitled caller causes zero reads"
/// assertable rather than aspirational.
fn entitled_key(caller: CallerContext, key: Option<&CallerKey>) -> Option<String> {
    if !caller.may_infer_from_routine() {
        return None;
    }
    key.map(CallerKey::storage_key)
}

fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

// ── Validation ──────────────────────────────────────────────────────────────

/// Canonicalise a user-supplied name, or explain why it is unusable.
///
/// Lowercase + trimmed so "Home", " home " and "HOME" are one entry rather than
/// three — a user who says "remember this is Home" and later asks for "home"
/// must get their answer.
pub fn canonical_name(raw: &str) -> Result<String, String> {
    let n = raw.trim().to_lowercase();
    if n.is_empty() {
        return Err("a location needs a name — for example \"home\", \"work\", or \"the cabin\"".into());
    }
    if n.chars().count() > MAX_NAME_LEN {
        return Err(format!("that name is too long (limit {MAX_NAME_LEN} characters)"));
    }
    if !n.chars().all(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_' | '\'' | '.')) {
        return Err("a location name can use letters, numbers, spaces, apostrophes, dots, hyphens and underscores".into());
    }
    Ok(n)
}

/// Validate a location VALUE. The error never quotes the value: an error string
/// is one of the places a home address must never appear.
fn validate_value(raw: &str) -> Result<String, String> {
    let v = raw.trim();
    if v.is_empty() {
        return Err("tell me the place — a city, an address, or 'lat,lon'".into());
    }
    if v.chars().count() > MAX_VALUE_LEN {
        return Err(format!("that location is too long (limit {MAX_VALUE_LEN} characters)"));
    }
    if v.chars().any(|c| c.is_control()) {
        return Err("a location cannot contain line breaks or control characters".into());
    }
    Ok(v.to_string())
}

// ── Read API ────────────────────────────────────────────────────────────────

/// Resolve ONE named location for this caller.
///
/// An expired temporary entry resolves as [`Lookup::NotSet`], never as its stale
/// value — expiry is enforced on READ, not merely on a later cleanup pass, so it
/// holds even if nothing ever writes again.
pub fn lookup(
    store: &dyn LocationStore,
    key: Option<&CallerKey>,
    caller: CallerContext,
    name: &str,
) -> Lookup {
    let Some(storage_key) = entitled_key(caller, key) else {
        return Lookup::Denied;
    };
    let Ok(name) = canonical_name(name) else {
        // An unusable name cannot match anything that was stored under a usable
        // one, so this is genuinely "nothing set", not an error.
        return Lookup::NotSet;
    };
    let registry = match store.load() {
        Ok(r) => r,
        Err(e) => return Lookup::Unavailable(e),
    };
    let now = now_unix();
    match registry.caller(&storage_key).and_then(|c| c.locations.get(&name)) {
        Some(entry) if !entry.is_expired(now) => Lookup::Found(entry.clone()),
        _ => Lookup::NotSet,
    }
}

/// Everything this caller has stored, live and expired.
pub fn list(store: &dyn LocationStore, key: Option<&CallerKey>, caller: CallerContext) -> Listing {
    let Some(storage_key) = entitled_key(caller, key) else {
        return Listing::Denied;
    };
    let registry = match store.load() {
        Ok(r) => r,
        Err(e) => return Listing::Unavailable(e),
    };
    let now = now_unix();
    let record = registry.caller(&storage_key).cloned().unwrap_or_default();
    let (mut live, mut expired) = (Vec::new(), Vec::new());
    for (name, entry) in record.locations {
        if entry.is_expired(now) {
            expired.push((name, entry));
        } else {
            live.push((name, entry));
        }
    }
    Listing::Entries { live, expired }
}

// ── Write API ───────────────────────────────────────────────────────────────

/// Store (or replace) a named location.
///
/// * `expires_in_hours` `None` = permanent; `Some(h)` = temporary, expiring
///   `h` hours from now (`1..=`[`MAX_TEMPORARY_HOURS`]).
/// * `confirm` must be `true` to replace an existing entry whose value differs.
///   Writes are USER DATA: overwriting one silently is how a stored home
///   quietly becomes last week's hotel. Re-storing an IDENTICAL value is a
///   no-op-shaped write and needs no confirmation — asking there is noise.
pub fn set(
    store: &dyn LocationStore,
    key: Option<&CallerKey>,
    caller: CallerContext,
    name: &str,
    value: &str,
    expires_in_hours: Option<i64>,
    confirm: bool,
) -> WriteOutcome {
    let Some(storage_key) = entitled_key(caller, key) else {
        return WriteOutcome::Denied;
    };
    let name = match canonical_name(name) {
        Ok(n) => n,
        Err(e) => return WriteOutcome::Rejected(e),
    };
    let value = match validate_value(value) {
        Ok(v) => v,
        Err(e) => return WriteOutcome::Rejected(e),
    };
    let now = now_unix();
    let expires_at = match expires_in_hours {
        None => None,
        Some(h) if h >= 1 && h <= MAX_TEMPORARY_HOURS => Some(now + h * 3600),
        Some(_) => {
            return WriteOutcome::Rejected(format!(
                "a temporary location lasts between 1 and {MAX_TEMPORARY_HOURS} hours — \
                 for anything longer, save it as a permanent one"
            ))
        }
    };

    let mut registry = match store.load() {
        Ok(r) => r,
        Err(e) => return WriteOutcome::Unavailable(e),
    };

    let existing = registry.caller(&storage_key).and_then(|c| c.locations.get(&name)).cloned();
    if let Some(prev) = &existing {
        // An EXPIRED entry is not a value the user still stands behind, so
        // replacing it is not an overwrite worth interrupting for.
        let live = !prev.is_expired(now);
        if live && prev.value != value && !confirm {
            return WriteOutcome::NeedsConfirmation {
                name,
                existing_is_temporary: prev.is_temporary(),
            };
        }
    }

    let entry = match expires_at {
        Some(t) => StoredLocation::temporary(value, t, now),
        None => StoredLocation::permanent(value, now),
    };
    registry.caller_mut(&storage_key).locations.insert(name.clone(), entry.clone());
    prune_expired(&mut registry, now);

    match store.save(&registry) {
        Ok(()) => WriteOutcome::Stored { name, entry, replaced: existing },
        Err(e) => WriteOutcome::Unavailable(e),
    }
}

/// Remove one named location, or — with `confirm_all` — everything this caller
/// has stored.
///
/// Clearing is EXPLICIT: `name` is required unless `confirm_all` is set, and
/// there is no "clear everything" shortcut that a mis-parsed sentence can reach
/// by accident.
pub fn clear(
    store: &dyn LocationStore,
    key: Option<&CallerKey>,
    caller: CallerContext,
    name: Option<&str>,
    confirm_all: bool,
) -> ClearOutcome {
    let Some(storage_key) = entitled_key(caller, key) else {
        return ClearOutcome::Denied;
    };

    let target = match name.map(str::trim).filter(|s| !s.is_empty()) {
        Some(raw) => match canonical_name(raw) {
            Ok(n) => Some(n),
            Err(e) => return ClearOutcome::Rejected(e),
        },
        None if confirm_all => None,
        None => return ClearOutcome::NeedsConfirmation,
    };

    let mut registry = match store.load() {
        Ok(r) => r,
        Err(e) => return ClearOutcome::Unavailable(e),
    };
    let record = registry.caller_mut(&storage_key);
    let count = match &target {
        Some(n) => {
            if record.locations.remove(n).is_none() {
                return ClearOutcome::NotSet;
            }
            1
        }
        None => {
            let n = record.locations.len();
            if n == 0 {
                return ClearOutcome::NotSet;
            }
            record.locations.clear();
            n
        }
    };
    match store.save(&registry) {
        Ok(()) => ClearOutcome::Cleared { count },
        Err(e) => ClearOutcome::Unavailable(e),
    }
}

/// Drop expired entries from EVERY caller's record on any write.
///
/// Expiry is already enforced on read, so this is hygiene rather than
/// correctness — but it means an expired address stops being AT REST on disk
/// shortly after it stops being usable, instead of lingering indefinitely in a
/// file that holds home addresses.
fn prune_expired(registry: &mut Registry, now: i64) {
    for record in registry.callers.values_mut() {
        record.locations.retain(|_, e| !e.is_expired(now));
    }
    registry.callers.retain(|_, r: &mut CallerRecord| !r.locations.is_empty());
}

pub mod tools;

pub use tools::{register, shared_store};

#[cfg(test)]
mod tests {
    use super::store::fake::{BrokenStore, CountingStore};
    use super::store::LocationStore as _;
    use super::*;

    // ── fixtures ────────────────────────────────────────────────────────────
    //
    // Every location below is an obvious placeholder. This file is in a repo
    // that publishes a PII-scrubbed public mirror, and a "realistic" fixture
    // address is indistinguishable from a real leak to anyone reading it later.

    /// A caller entitled to stored-location context — what the gateway derives
    /// for a principal holding the `commute_estimate` grant.
    fn entitled() -> CallerContext {
        CallerContext::entitled_for_test_only(false, true)
    }

    /// A household guest: may call the tool, entitled to nothing.
    fn guest() -> CallerContext {
        CallerContext::default()
    }

    fn key_a() -> CallerKey {
        CallerKey::for_principal_name("alpha").unwrap()
    }

    fn key_b() -> CallerKey {
        CallerKey::for_principal_name("bravo").unwrap()
    }

    /// Stands in for a home address. Never a real one.
    const A_HOME: &str = "1 Placeholder Way, Examplecity"; // pii-test-fixture: obvious placeholder standing in for caller A's home address
    const B_HOME: &str = "2 Otherplace Road, Examplecity"; // pii-test-fixture: obvious placeholder standing in for caller B's home address

    // ── round trip ──────────────────────────────────────────────────────────

    #[test]
    fn set_update_clear_list_round_trip_for_one_caller() {
        let s = CountingStore::new();
        let k = key_a();

        assert!(matches!(
            set(&s, Some(&k), entitled(), "Home", A_HOME, None, false),
            WriteOutcome::Stored { .. }
        ));
        match lookup(&s, Some(&k), entitled(), "home") {
            Lookup::Found(e) => assert_eq!(e.value, A_HOME),
            other => panic!("expected the stored home, got {other:?}"),
        }

        // Update needs confirmation, then lands.
        assert!(matches!(
            set(&s, Some(&k), entitled(), "home", "3 Newplace Lane", None, false), // pii-test-fixture: obvious placeholder standing in for a new home address
            WriteOutcome::NeedsConfirmation { .. }
        ));
        match lookup(&s, Some(&k), entitled(), "home") {
            Lookup::Found(e) => assert_eq!(e.value, A_HOME, "an unconfirmed update must not write"),
            other => panic!("got {other:?}"),
        }
        assert!(matches!(
            set(&s, Some(&k), entitled(), "home", "3 Newplace Lane", None, true), // pii-test-fixture: obvious placeholder standing in for a new home address
            WriteOutcome::Stored { replaced: Some(_), .. }
        ));

        // Arbitrary user-chosen names work, not just the well-known ones.
        assert!(matches!(
            set(&s, Some(&k), entitled(), "the cabin", "Somewhere Pines", None, false),
            WriteOutcome::Stored { .. }
        ));
        match list(&s, Some(&k), entitled()) {
            Listing::Entries { live, expired } => {
                let names: Vec<_> = live.iter().map(|(n, _)| n.as_str()).collect();
                assert!(names.contains(&"home") && names.contains(&"the cabin"));
                assert!(expired.is_empty());
            }
            other => panic!("got {other:?}"),
        }

        // Clear is explicit and per-name.
        assert_eq!(
            clear(&s, Some(&k), entitled(), Some("the cabin"), false),
            ClearOutcome::Cleared { count: 1 }
        );
        assert_eq!(lookup(&s, Some(&k), entitled(), "the cabin"), Lookup::NotSet);
        assert_eq!(clear(&s, Some(&k), entitled(), Some("the cabin"), false), ClearOutcome::NotSet);
    }

    #[test]
    fn re_storing_the_same_value_needs_no_confirmation() {
        let s = CountingStore::new();
        let k = key_a();
        set(&s, Some(&k), entitled(), "home", A_HOME, None, false);
        assert!(matches!(
            set(&s, Some(&k), entitled(), "home", &format!("  {A_HOME} "), None, false),
            WriteOutcome::Stored { .. },
        ));
    }

    #[test]
    fn clearing_everything_requires_explicit_confirmation() {
        let s = CountingStore::new();
        let k = key_a();
        set(&s, Some(&k), entitled(), "home", A_HOME, None, false);
        assert_eq!(clear(&s, Some(&k), entitled(), None, false), ClearOutcome::NeedsConfirmation);
        match lookup(&s, Some(&k), entitled(), "home") {
            Lookup::Found(_) => {}
            other => panic!("an unconfirmed clear-all must remove nothing, got {other:?}"),
        }
        assert_eq!(clear(&s, Some(&k), entitled(), None, true), ClearOutcome::Cleared { count: 1 });
    }

    // ── isolation between callers ───────────────────────────────────────────

    #[test]
    fn caller_a_cannot_read_or_write_caller_bs_locations() {
        let s = CountingStore::new();
        set(&s, Some(&key_b()), entitled(), "home", B_HOME, None, false);

        // A reads: nothing of B's, and specifically not B's address.
        assert_eq!(lookup(&s, Some(&key_a()), entitled(), "home"), Lookup::NotSet);
        match list(&s, Some(&key_a()), entitled()) {
            Listing::Entries { live, expired } => {
                assert!(live.is_empty() && expired.is_empty());
            }
            other => panic!("got {other:?}"),
        }

        // A writes: B's record is untouched.
        set(&s, Some(&key_a()), entitled(), "home", A_HOME, None, true);
        match lookup(&s, Some(&key_b()), entitled(), "home") {
            Lookup::Found(e) => assert_eq!(e.value, B_HOME, "A's write must not reach B's record"),
            other => panic!("got {other:?}"),
        }

        // And nowhere in anything A can observe does B's address appear.
        let a_view = format!("{:?}", list(&s, Some(&key_a()), entitled()));
        assert!(!a_view.contains(B_HOME), "caller B's home address leaked into caller A's view");
    }

    // ── entitlement ─────────────────────────────────────────────────────────

    #[test]
    fn an_unentitled_caller_causes_zero_reads_and_zero_writes() {
        let s = CountingStore::new();
        // Seed through an entitled caller so there IS something to leak.
        set(&s, Some(&key_a()), entitled(), "home", A_HOME, None, false);
        let writes_before = s.writes();
        let reads_before = s.reads();

        // A guest holding a valid key of their own, and a guest holding A's key
        // — neither may touch the store.
        for k in [Some(&key_b()), Some(&key_a()), None] {
            assert_eq!(lookup(&s, k, guest(), "home"), Lookup::Denied);
            assert!(matches!(list(&s, k, guest()), Listing::Denied));
            assert!(matches!(
                set(&s, k, guest(), "home", "Anywhere", None, true),
                WriteOutcome::Denied
            ));
            assert_eq!(clear(&s, k, guest(), Some("home"), false), ClearOutcome::Denied);
        }

        assert_eq!(s.reads(), reads_before, "an unentitled caller must cause ZERO reads");
        assert_eq!(s.writes(), writes_before, "an unentitled caller must cause ZERO writes");
        assert!(
            !format!("{:?}", lookup(&s, Some(&key_a()), guest(), "home")).contains("Placeholder"),
            "a denial must not carry the value it is denying"
        );
    }

    #[test]
    fn an_entitled_caller_with_no_identity_is_still_denied() {
        // Entitlement without a key would otherwise have to fall back to some
        // shared record — i.e. everyone's locations in one bucket.
        let s = CountingStore::new();
        assert_eq!(lookup(&s, None, entitled(), "home"), Lookup::Denied);
        assert_eq!(s.reads(), 0);
    }

    // ── positive control ────────────────────────────────────────────────────

    #[test]
    fn positive_control_an_entitled_caller_gets_their_stored_home_back() {
        // Without this, every negative test above would still pass if the
        // registry had simply been disabled for everyone.
        let s = CountingStore::new();
        let k = key_a();
        set(&s, Some(&k), entitled(), HOME, A_HOME, None, false);
        match lookup(&s, Some(&k), entitled(), HOME) {
            Lookup::Found(e) => {
                assert_eq!(e.value, A_HOME);
                assert!(!e.is_temporary());
            }
            other => panic!("an entitled caller MUST get their own home back, got {other:?}"),
        }
        assert!(s.reads() > 0, "the entitled path must actually read the store");
    }

    // ── temporary locations ─────────────────────────────────────────────────

    #[test]
    fn a_temporary_location_expires_and_stops_resolving() {
        let s = CountingStore::new();
        let k = key_a();
        assert!(matches!(
            set(&s, Some(&k), entitled(), CURRENT, "Denver", Some(24), false),
            WriteOutcome::Stored { .. }
        ));
        match lookup(&s, Some(&k), entitled(), CURRENT) {
            Lookup::Found(e) => assert!(e.is_temporary()),
            other => panic!("got {other:?}"),
        }

        // Reach into the document and move the expiry into the past — the same
        // thing the passage of time does, without sleeping for a day.
        let mut reg = s.snapshot();
        reg.caller_mut(&k.storage_key())
            .locations
            .get_mut(CURRENT)
            .unwrap()
            .expires_at_unix = Some(now_unix() - 1);
        s.save(&reg).unwrap();

        assert_eq!(
            lookup(&s, Some(&k), entitled(), CURRENT),
            Lookup::NotSet,
            "an expired temporary location must NOT resolve"
        );
        match list(&s, Some(&k), entitled()) {
            Listing::Entries { live, expired } => {
                assert!(live.is_empty());
                assert_eq!(expired.len(), 1, "an expired entry stays visible so it can be cleared");
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn an_absurd_expiry_is_refused_rather_than_silently_permanent() {
        let s = CountingStore::new();
        let k = key_a();
        for h in [0, -1, MAX_TEMPORARY_HOURS + 1] {
            assert!(
                matches!(set(&s, Some(&k), entitled(), CURRENT, "Denver", Some(h), false), WriteOutcome::Rejected(_)),
                "expires_in_hours={h} must be refused, not coerced to permanent"
            );
        }
        assert_eq!(lookup(&s, Some(&k), entitled(), CURRENT), Lookup::NotSet);
    }

    #[test]
    fn a_write_prunes_expired_entries_from_disk() {
        let s = CountingStore::new();
        let k = key_a();
        set(&s, Some(&k), entitled(), CURRENT, "Denver", Some(24), false);
        let mut reg = s.snapshot();
        reg.caller_mut(&k.storage_key()).locations.get_mut(CURRENT).unwrap().expires_at_unix =
            Some(now_unix() - 1);
        s.save(&reg).unwrap();

        set(&s, Some(&k), entitled(), HOME, A_HOME, None, false);
        let after = s.snapshot();
        assert!(
            !after.caller(&k.storage_key()).unwrap().locations.contains_key(CURRENT),
            "an expired address should not linger at rest"
        );
    }

    // ── honest absence ──────────────────────────────────────────────────────

    #[test]
    fn empty_means_not_set_and_a_broken_store_means_could_not_read() {
        let empty = CountingStore::new();
        let k = key_a();
        assert_eq!(lookup(&empty, Some(&k), entitled(), HOME), Lookup::NotSet);
        assert!(matches!(list(&empty, Some(&k), entitled()), Listing::Entries { .. }));

        let broken = BrokenStore;
        assert!(
            matches!(lookup(&broken, Some(&k), entitled(), HOME), Lookup::Unavailable(_)),
            "a store failure must NEVER present as 'no location set'"
        );
        assert!(matches!(list(&broken, Some(&k), entitled()), Listing::Unavailable(_)));
        assert!(matches!(
            set(&broken, Some(&k), entitled(), HOME, A_HOME, None, true),
            WriteOutcome::Unavailable(_)
        ));
        assert!(matches!(
            clear(&broken, Some(&k), entitled(), Some(HOME), false),
            ClearOutcome::Unavailable(_)
        ));
    }

    // ── caller key / TERM #577 ──────────────────────────────────────────────

    #[test]
    fn a_person_scoped_key_is_a_different_record_from_the_service_key() {
        // The #577 migration property: person-scoped records do not silently
        // inherit the shared service-scoped one.
        let svc = CallerKey::for_principal_name("lumina").unwrap();
        let person = CallerKey::for_person("lumina", "someone").unwrap();
        assert_ne!(svc.storage_key(), person.storage_key());
        assert!(!svc.is_person_scoped() && person.is_person_scoped());
        assert_eq!(svc.principal(), person.principal());

        let s = CountingStore::new();
        set(&s, Some(&svc), entitled(), HOME, A_HOME, None, false);
        assert_eq!(
            lookup(&s, Some(&person), entitled(), HOME),
            Lookup::NotSet,
            "after #577 an old service-scoped record must be orphaned, never shared"
        );
    }

    #[test]
    fn caller_keys_are_case_insensitive_and_reject_blanks() {
        assert_eq!(
            CallerKey::for_principal_name("Lumina").unwrap().storage_key(),
            CallerKey::for_principal_name("lumina").unwrap().storage_key()
        );
        assert!(CallerKey::for_principal_name("   ").is_none());
        assert!(CallerKey::for_principal_name("").is_none());
        assert!(CallerKey::for_person("", "someone").is_none());
    }

    // ── validation ──────────────────────────────────────────────────────────

    #[test]
    fn names_are_canonicalised_and_junk_is_refused() {
        assert_eq!(canonical_name("  Home ").unwrap(), "home");
        assert_eq!(canonical_name("Mum's House").unwrap(), "mum's house");
        assert!(canonical_name("").is_err());
        assert!(canonical_name(&"x".repeat(MAX_NAME_LEN + 1)).is_err());
        assert!(canonical_name("home\nwork").is_err());
    }

    #[test]
    fn a_rejection_never_quotes_the_value_it_rejected() {
        let s = CountingStore::new();
        let k = key_a();
        let sensitive = format!("{A_HOME}\nsecond line");
        match set(&s, Some(&k), entitled(), HOME, &sensitive, None, false) {
            WriteOutcome::Rejected(msg) => {
                assert!(!msg.contains("Placeholder"), "the rejection echoed the address back: {msg}");
            }
            other => panic!("expected a rejection, got {other:?}"),
        }
    }
}
