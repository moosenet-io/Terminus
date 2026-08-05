//! RMCP-07 — the effective-permission resolver.
//!
//! ## The one rule
//! Everything in this file exists to make one sentence true:
//!
//! ```text
//! effective = grant_of(account)          // what the HUMAN may do  (existing)
//!           ∩ tools_of(client.groups)    // what THIS connector may do
//!           ∩ namespaces(client.servers) // which federated servers it sees
//! ```
//!
//! An intersection can only ever REMOVE. There is deliberately no code path
//! here by which a client scoping record grants a tool the account's own grant
//! would have denied, and [`effective_is_subset_of_grant`] asserts that over
//! randomised inputs rather than trusting the reading of the code.
//!
//! ## Why the shape is what it is
//! **One function, two call sites.** [`decide`] is the whole decision. The
//! catalog filter ([`effective`]) is literally a `filter` over [`decide`], and
//! the call guard calls [`decide`] directly. This is not a stylistic
//! preference: filtering the catalog without gating the call is a disclosure
//! bug, gating the call without filtering the catalog leaks what exists, and
//! two *similar* functions is how those two drift apart six months later. The
//! same discipline (and the same reasoning) as
//! [`crate::gateway_framework::AllowlistPolicy`]'s own list/call parity.
//!
//! **Absence is the empty set, never a default.** A missing client, a disabled
//! client, a client with no scope rows, a group whose patterns are empty, a
//! pattern that matches nothing — every one of those resolves to zero tools.
//! The tempting refactor is `unwrap_or(full_grant)`, and that is precisely the
//! widening bug; [`ClientScope::empty`] is the only "default" this module has
//! and it permits nothing. There is no `Default` impl, deliberately.
//!
//! **Authority is re-derived on the READ path.** This is the defect class this
//! sprint keeps producing: a write-time authorization check is point-in-time,
//! and any authority that can be REVOKED later must be re-derived when it is
//! used. Group ownership and namespace ownership are both revocable
//! (`set_server_owner` / `clear_server_owner` / a client changing hands), so
//! [`ScopeResolver::resolve`] reads them through
//! [`crate::oauth::store::OauthStore::client_tool_groups`] and
//! [`crate::oauth::store::OauthStore::client_namespaces`], which re-join
//! ownership in SQL on every read rather than trusting the row that was
//! validated when it was written. Nothing in this file caches an ownership
//! CONCLUSION past [`ScopeResolver`]'s explicitly-invalidated, short-lived
//! entry — see [`ScopeResolver`]'s own docs for why that cache is bounded the
//! way it is.
//!
//! ## Relationship to RMCP-06 — the collapse, now done (TERM #637)
//! RMCP-06 owns tool GROUPS: their CRUD, their seeded starter set, the pattern
//! GRAMMAR, and the matcher. It had not landed when this item was built, so
//! this file carried a second copy of that vocabulary — `ScopePattern` — in
//! order to be usable at all.
//!
//! **Two matchers that must agree eventually disagree.** TERM #637 is the
//! proof: this side qualified with `::` and the authoring side with `__`, so
//! authored patterns were dead or over-granting the moment both shipped. TERM
//! #643 is the second instance of the same divergence, in the other direction:
//! the authoring matcher had already been fixed to read provenance from the
//! catalog entry, while this copy was still inferring it from the NAME's shape,
//! so a local tool named `peerhub__tool` satisfied `peerhub::tool`. Neither
//! divergence was noticed by either author; both were found by verification.
//! That is the argument for collapsing rather than for documenting harder.
//!
//! So there is now exactly ONE matcher — [`crate::oauth::groups::Pattern`] —
//! and this file holds none of the grammar. [`ClientScope::from_rows`] parses
//! stored rows with [`crate::oauth::groups::Pattern::parse_stored`] and
//! [`decide`] matches with [`crate::oauth::groups::Pattern::matches`], which is
//! the same code path the authoring side validates against.
//!
//! **The tests were RE-POINTED, not deleted with the parser.** They encode
//! ENFORCEMENT properties an authoring-side suite has no reason to cover: that
//! an unqualified prefix cannot cross a namespace boundary, that a bare `*` is
//! still clamped by the account grant, that `tools/list` and `tools/call` never
//! disagree, and that absence is the empty set. They are the half of the
//! contract that guards a live door, so what changed in them is the matcher
//! they call — never what they assert. The pure GRAMMAR tests (which strings
//! parse, which are refused) went with the parser, because they now test
//! `groups.rs`'s code and `groups.rs` already tests it.
//!
//! Parsing on this path stays TOTAL and fail-closed: an unparseable stored
//! pattern is dropped and matches nothing rather than erroring at match time (a
//! match-time error on the dispatch path is a denial-of-service, and a pattern
//! that cannot be understood must certainly not be read as "allow"). RMCP-06
//! rejects such a pattern at write time so it never reaches storage; this is
//! the second layer.
//!
//! ## Provenance, not name shape (TERM #643)
//! Whether a tool is LOCAL or belongs to a federated namespace is a fact about
//! where the catalog entry came FROM, and the only place that fact exists is
//! the entry itself. [`decide`] therefore takes a
//! [`crate::oauth::groups::CatalogTool`] — a name PLUS its provenance — and not
//! a `&str`. Both dimensions it applies read the provenance: the namespace
//! check compares [`crate::oauth::groups::CatalogTool::namespace`] against the
//! client's namespaces, and the matcher decides the local/qualified boundary
//! from the same field.
//!
//! `crate::mesh::split_namespaced` cannot stand in for this, and adopting the
//! split-halves REPRESENTATION does not help either: the split is purely
//! syntactic, so it hands the same false `Some("peerhub")` to a local tool
//! literally named `peerhub__tool`. The two cases are byte-identical as
//! strings. Only the catalog can tell them apart, so the catalog is asked.
//!
//! The provenance a request is decided against is the provenance it will
//! DISPATCH to — `tools/list` reads
//! [`crate::mesh::RoutingTable::namespace_of`] and `tools/call` reads
//! [`crate::mesh::CallRoute::namespace`], the merge layer's own two answers —
//! so an entry cannot be authorized as one thing and routed as another.

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use uuid::Uuid;

use crate::error::ToolError;
use crate::oauth::delegation::owner_may_hold;
use crate::oauth::groups::{AuthorizedGroup, CatalogTool, Pattern};
use crate::oauth::store::OauthStore;

// ---------------------------------------------------------------------------
// Decisions
// ---------------------------------------------------------------------------

/// Why a tool was denied, as a stable machine-readable code.
///
/// These strings land in the audit log and are the difference between an
/// operator diagnosing a misconfiguration in one query and guessing. They are
/// treated as a wire contract: rename one and a log query silently stops
/// matching, so the mapping is tested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenyReason {
    /// The ACCOUNT's own grant does not permit this tool. The client scoping is
    /// irrelevant — no client record can widen past this.
    DeniedByGrant,
    /// The tool belongs to a federated namespace this client is not scoped to.
    NoNamespace,
    /// No tool group attached to this client matches the name.
    NoGroup,
}

impl DenyReason {
    /// The audit code. Stable; see the type's docs.
    pub fn code(self) -> &'static str {
        match self {
            Self::DeniedByGrant => "denied_by_grant",
            Self::NoNamespace => "no_namespace",
            Self::NoGroup => "no_group",
        }
    }
}

/// The outcome of [`decide`].
///
/// Deliberately not `bool`: the reason is required by the acceptance criteria
/// and a `bool` would have thrown it away at the one place it is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny(DenyReason),
}

impl Decision {
    pub fn is_allowed(self) -> bool {
        matches!(self, Self::Allow)
    }

    /// The audit code for a denial, or `None` when allowed.
    pub fn deny_code(self) -> Option<&'static str> {
        match self {
            Self::Allow => None,
            Self::Deny(reason) => Some(reason.code()),
        }
    }
}

/// Counts of one `tools/list` evaluation, for a single aggregate audit record.
///
/// ## Why counts and not one record per hidden tool
/// Round 3 of review found that list-path denials never reached the audit
/// stream at all: [`effective`] silently dropped everything [`decide`] refused,
/// so the three reason codes existed internally and were emitted only on the
/// call path. An operator diagnosing "my connector sees nothing" — the single
/// most likely support question this feature will generate — had no trace to
/// read.
///
/// The naive fix is one record per denied tool. That is the wrong shape: a
/// 400-tool catalog would emit hundreds of records on EVERY list, burying the
/// call-path denials that describe something a caller actually attempted, and
/// turning the audit stream into a firehose nobody greps twice. One aggregate
/// row per evaluation answers the real diagnostic question — *which dimension
/// is eliminating my tools, and how many* — and answers it in a single line.
///
/// The deliberate cost: the NAMES of hidden tools are not enumerated on the
/// list path. That is a real limitation, not an oversight, and the README says
/// so rather than claiming completeness. The names are recoverable anyway — a
/// call to any one of them emits a per-tool record with its exact reason.
///
/// Not a permission-bearing type, so unlike everything else in this module it
/// may derive `Default`: a zeroed tally is an evaluation that has not counted
/// anything yet, which is exactly right and grants nothing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ListFilterTally {
    considered: usize,
    allowed: usize,
    denied_by_grant: usize,
    no_namespace: usize,
    no_group: usize,
    no_account_grant: usize,
}

impl ListFilterTally {
    /// Count one tool's decision.
    pub fn record(&mut self, decision: Decision) {
        self.considered += 1;
        match decision {
            Decision::Allow => self.allowed += 1,
            Decision::Deny(DenyReason::DeniedByGrant) => self.denied_by_grant += 1,
            Decision::Deny(DenyReason::NoNamespace) => self.no_namespace += 1,
            Decision::Deny(DenyReason::NoGroup) => self.no_group += 1,
        }
    }

    /// Count one tool refused because the process has no gateway, and therefore
    /// no account grant to intersect with ([`DENY_NO_ACCOUNT_GRANT`]).
    /// [`decide`] never produces this, so it cannot arrive through
    /// [`Self::record`].
    pub fn record_no_account_grant(&mut self) {
        self.considered += 1;
        self.no_account_grant += 1;
    }

    /// How many tools were hidden.
    pub fn denied(&self) -> usize {
        self.considered.saturating_sub(self.allowed)
    }

    pub fn considered(&self) -> usize {
        self.considered
    }

    pub fn allowed(&self) -> usize {
        self.allowed
    }

    /// The machine-readable body of the audit record.
    ///
    /// `key=value` pairs so a log query can filter and sum without parsing
    /// prose. Every reason is present even at zero — a query for
    /// `no_namespace=` must not silently miss rows where the count happened to
    /// be zero, and a reader comparing two lines should not have to work out
    /// whether a missing key means zero or means an older format.
    pub fn summary(&self) -> String {
        format!(
            "considered={} allowed={} denied={} denied_by_grant={} no_namespace={} \
             no_group={} no_account_grant={}",
            self.considered,
            self.allowed,
            self.denied(),
            self.denied_by_grant,
            self.no_namespace,
            self.no_group,
            self.no_account_grant,
        )
    }
}

// ---------------------------------------------------------------------------
// The account's own grant
// ---------------------------------------------------------------------------

/// What the ACCOUNT behind the token may do, independent of any connector.
///
/// A trait rather than a concrete dependency on
/// [`crate::gateway_framework::AllowlistPolicy`] for one reason that matters:
/// it lets the anti-widening property be property-tested against arbitrary
/// generated grants, including pathological ones, without constructing a whole
/// gateway. The production implementation is a thin adapter over the existing
/// allowlist in `crate::gateway_framework` — this module introduces no second
/// way to decide what an account may do.
pub trait AccountGrant {
    /// Whether the account may call `tool`, by its ADVERTISED (possibly
    /// namespaced) name — the same name the allowlist and the deny layer see.
    fn permits_tool(&self, tool: &str) -> bool;
}

/// Any `Fn(&str) -> bool` is a grant. Exists for tests and for call sites that
/// already hold a closure; production uses the allowlist adapter.
///
/// `?Sized` so that a `&dyn Fn(&str) -> bool` is also a grant — the property
/// test needs a heterogeneous collection of grants, and without this the
/// blanket impl's implicit `Sized` bound excludes the trait objects it holds.
impl<F: ?Sized> AccountGrant for F
where
    F: Fn(&str) -> bool,
{
    fn permits_tool(&self, tool: &str) -> bool {
        self(tool)
    }
}


// ---------------------------------------------------------------------------
// A client's resolved scope
// ---------------------------------------------------------------------------

/// The scoping half of the intersection for ONE connector, resolved from the
/// store at a point in time.
///
/// Holds no account authority whatsoever — deliberately. Keeping the grant out
/// of this type is what makes it impossible for a stale cached scope to carry a
/// stale grant with it: [`decide`] always takes the grant separately and reads
/// it live, so "the account's grant narrows after a token is issued" takes
/// effect on the very next call regardless of anything cached here.
#[derive(Debug, Clone)]
pub struct ClientScope {
    /// The public OAuth `client_id`, for audit attribution only.
    client_id: String,
    /// Every pattern from every group attached to this client, flattened.
    ///
    /// Flattened because groups are a UNION with each other and the decision
    /// never needs to know which group matched. Keeping the group boundary
    /// would only invite a per-group rule that does not exist.
    ///
    /// [`crate::oauth::groups::Pattern`] — the ONE matcher, shared with the
    /// authoring path (TERM #637). There is no enforcing-side pattern type.
    patterns: Vec<Pattern>,
    /// The mesh namespaces this client may see, as owner-verified by the store.
    namespaces: BTreeSet<String>,
}

impl ClientScope {
    /// The scope of a client that reaches nothing.
    ///
    /// This — not `None`, and not the account's grant — is what an unknown
    /// client, a disabled client, an unscoped client, or a failed store read
    /// resolves to. It is the module's only "default" and it permits zero
    /// tools.
    pub fn empty(client_id: impl Into<String>) -> Self {
        Self {
            client_id: client_id.into(),
            patterns: Vec::new(),
            namespaces: BTreeSet::new(),
        }
    }

    /// Build a scope from rows the store returned.
    ///
    /// Both inputs come from queries that RE-JOIN ownership (see the module
    /// docs), so this constructor's job is only to flatten and parse. An
    /// unparseable pattern is DROPPED with a warning rather than propagated:
    /// on the dispatch path the alternatives are "match nothing" and "fail the
    /// request", and dropping is the one that keeps the rest of a group
    /// working while still never granting anything.
    pub fn from_rows(
        client_id: impl Into<String>,
        groups: &[AuthorizedGroup],
        namespaces: Vec<String>,
    ) -> Self {
        let client_id = client_id.into();
        let mut patterns = Vec::new();
        for authorized in groups {
            let group = &authorized.group;
            for raw in &group.patterns {
                // `parse_stored` is the dispatch-path parser: syntax only, no
                // error, and no authority decision — the authority question is
                // asked immediately below, where a live owner is in hand.
                match Pattern::parse_stored(raw) {
                    Some(parsed) if !owner_may_hold(authorized.owner, parsed.shape()) => {
                        // RMCP-12's read-path re-derivation, and the reason this
                        // constructor now takes AUTHORIZED groups rather than
                        // bare rows. `authorized.owner` is projected by
                        // `client_authorized_groups` from `rmcp_account` in the
                        // same statement as the group, so it is the owner's
                        // CURRENT authority — not the authority they had when
                        // the pattern was written.
                        //
                        // Dropping the pattern (never the group, never the
                        // request) is what makes a revoked authority resolve to
                        // less rather than to an error or to everything.
                        tracing::warn!(
                            group = %group.name,
                            "rmcp scope: dropping a pattern its group's owner is no longer \
                             authorized to hold"
                        );
                    }
                    Some(parsed) => patterns.push(parsed),
                    None => {
                        // The pattern text itself is operator-authored config,
                        // not caller input, but it is still not echoed: the
                        // group NAME is what an operator needs to find it.
                        tracing::warn!(
                            group = %group.name,
                            "rmcp scope: dropping an unparseable tool-group pattern (it matches nothing)"
                        );
                    }
                }
            }
        }
        Self {
            client_id,
            patterns,
            namespaces: namespaces.into_iter().collect(),
        }
    }

    /// The public client identifier, for audit attribution.
    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    /// Whether this scope can match anything at all.
    ///
    /// A client with no patterns reaches nothing even if it has namespaces, so
    /// this is keyed on the patterns. Exists so call sites can short-circuit
    /// without reaching for `patterns.is_empty()`, which invites the wrong
    /// default.
    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    /// A scope permitting the whole catalog, for tests in OTHER modules whose
    /// subject is not the connector ceiling.
    ///
    /// `cfg(test)`-only and named so it cannot be mistaken for a production
    /// constructor: there is no code path by which a real connector becomes
    /// unrestricted without an operator writing a `*` group and owning it.
    #[cfg(test)]
    pub fn unrestricted_for_test(client_id: impl Into<String>) -> Self {
        Self {
            client_id: client_id.into(),
            patterns: vec![Pattern::Everything],
            namespaces: BTreeSet::new(),
        }
    }

    /// The namespaces this client may see. Test/diagnostic accessor.
    pub fn namespaces(&self) -> &BTreeSet<String> {
        &self.namespaces
    }
}

// ---------------------------------------------------------------------------
// The decision
// ---------------------------------------------------------------------------

/// **The** authorization decision for one tool under one connector.
///
/// Both the `tools/list` filter and the `tools/call` guard route through this
/// function — [`effective`] is a filter over it, and the guard calls it
/// directly. There is intentionally no second implementation.
///
/// Order of the three checks is grant → namespace → group. The order does not
/// change the OUTCOME (it is a conjunction), but it does decide which reason is
/// reported, and "the account itself may not do this" is the most actionable
/// answer when more than one thing is wrong.
///
/// ## What is NOT here
/// The deny layer. [`crate::gateway_framework`]'s deny prefixes compose on top
/// of this and override unconditionally — they are already inside
/// `grant.permits_tool`, which is why a deny can never be lost by anything this
/// function does. Re-implementing denial here would create a second, weaker
/// copy of it.
///
/// ## Namespaces and LOCAL tools (TERM #643)
/// A LOCAL tool has no namespace, so the namespace dimension does not apply to
/// it — requiring one would mean no connector could ever call a local tool.
/// Which tools those are is read from [`CatalogTool::namespace`], the catalog's
/// own record of where the entry came from, and NOT from the name's shape.
///
/// The earlier version asked `crate::mesh::split_namespaced` instead, and that
/// is unsound in both directions because the split is purely syntactic: a local
/// tool literally named `peerhub__tool` was treated as federated here (denied
/// unless the client happened to hold `peerhub`, which reads as fail-closed)
/// and, in the matcher, as a MEMBER of `peerhub` — so `peerhub::tool` and
/// `peerhub::*` reached a tool from no namespace at all, while a client holding
/// that namespace passed this check. That is the widening, and only provenance
/// closes it: holding the two halves separately does not, since the splitter
/// hands the split form the same false answer.
pub fn decide<G: AccountGrant + ?Sized>(
    grant: &G,
    scope: &ClientScope,
    tool: &CatalogTool,
) -> Decision {
    // 1. The account's own grant, asked by ADVERTISED name — the name a caller
    //    invokes, and the one the allowlist and deny layers are written
    //    against. Checked first and never skipped: this is the ceiling, and
    //    everything below can only narrow it.
    if !grant.permits_tool(&tool.name) {
        return Decision::Deny(DenyReason::DeniedByGrant);
    }

    // 2. The federated-server dimension. A tool from an upstream this client is
    //    not scoped to is invisible and uncallable no matter which groups match.
    if let Some(namespace) = tool.namespace.as_deref() {
        if !scope.namespaces.contains(namespace) {
            return Decision::Deny(DenyReason::NoNamespace);
        }
    }

    // 3. The tool-group dimension. `any` over an EMPTY pattern list is `false`,
    //    which is exactly the fail-closed default — an unscoped client lands
    //    here and is denied.
    if scope.patterns.iter().any(|p| p.matches(tool)) {
        Decision::Allow
    } else {
        Decision::Deny(DenyReason::NoGroup)
    }
}

/// The effective set: every tool in `catalog` that [`decide`] allows.
///
/// A thin filter over [`decide`] by construction, so `tools/list` and
/// `tools/call` cannot disagree. Linear in the catalog and allocation-bounded
/// (one `String` per PERMITTED tool, not per candidate).
pub fn effective<'a, G, I>(grant: &G, scope: &ClientScope, catalog: I) -> BTreeSet<String>
where
    G: AccountGrant + ?Sized,
    I: IntoIterator<Item = &'a CatalogTool>,
{
    catalog
        .into_iter()
        .filter(|tool| decide(grant, scope, tool).is_allowed())
        .map(|tool| tool.name.clone())
        .collect()
}

// ---------------------------------------------------------------------------
// Resolution and caching
// ---------------------------------------------------------------------------

/// Env var overriding [`DEFAULT_CACHE_TTL_SECS`].
pub const CACHE_TTL_ENV: &str = "RMCP_SCOPE_CACHE_TTL_SECONDS";

/// How long a resolved scope may be reused without re-reading the store.
///
/// Short on purpose. Invalidation on write (below) is the PRIMARY mechanism and
/// the TTL is only a backstop for a mutation that did not come through this
/// resolver — an operator editing the tables by hand, or a future write path
/// that forgets to invalidate. A stale permit is a security bug, so the
/// backstop is measured in seconds rather than minutes.
pub const DEFAULT_CACHE_TTL_SECS: u64 = 15;

/// Upper bound on cached clients, so a hostile or buggy caller presenting an
/// endless stream of unknown `client_id`s cannot grow this map without limit.
const MAX_CACHED_CLIENTS: usize = 1024;

/// Audit reason for a denial [`decide`] can never produce: the process has no
/// gateway, so there is no account grant to intersect a connector scope with.
///
/// Part of the audit vocabulary rather than a [`DenyReason`] variant precisely
/// because it is NOT an outcome of the decision — it is the absence of one of
/// the decision's inputs. Making it a `DenyReason` would imply `decide` could
/// return it, and the next reader would go looking for the branch that does.
pub const DENY_NO_ACCOUNT_GRANT: &str = "no_account_grant";

// ---------------------------------------------------------------------------
// The scope generation — the invalidation epoch
// ---------------------------------------------------------------------------

/// Monotone counter bumped by EVERY write that can narrow what some connector
/// reaches.
///
/// ## Why this is process-global rather than owned by a resolver
/// Round 1 of review (gpt56, HIGH) found that invalidation hung off
/// [`ScopeResolver`]'s own write-through mutators, so a write made directly
/// against [`crate::oauth::store::OauthStore`] — or an ownership/delegation
/// change, which narrows an arbitrary number of clients at once — left a
/// PERMITTING scope cached until the TTL expired. The finding is correct and
/// the distinction it rests on is the important part: a stale DENIAL costs
/// someone a retry, but a stale PERMIT is revoked authority that still works.
/// A TTL is an acceptable backstop for the first and not for the second.
///
/// The fix is to stop making invalidation depend on the caller choosing the
/// polite door. The counter lives here, next to the cache that reads it, and
/// the STORE bumps it from inside its own write methods — so any code path
/// that can narrow authority invalidates by construction, whether or not it
/// went through a resolver.
///
/// Process-global is the right scope: it is monotone, a spurious bump costs
/// only a re-read, and two resolvers in one process sharing one epoch is
/// strictly safer than each keeping its own.
///
/// ## How future writes are held to this
/// Not by asking them to remember, and not only in the store. Every mutation of
/// a table in [`SCOPE_AFFECTING_TABLES`] — anywhere in the crate — is bracketed
/// by a held [`ScopeWrite`] guard, and
/// `store::tests::every_scope_affecting_write_bumps_the_generation` reads the
/// WHOLE CRATE's source and fails, naming the file and the function, if one
/// ever appears outside it. Round 2 of review found that round 1 had left the
/// tool-group DEFINITION writes uncovered and had addressed the residual by
/// documenting an obligation for RMCP-06 — which is the fake-guard shape: a
/// comment describing a rule is not a rule, because the author who needs it is
/// the one who did not read it. Round 6 found the guard itself carried a
/// narrower version of the same flaw: it scanned one FILE, so its real claim
/// was "provided the write lives in `store.rs`", while the README claimed the
/// general rule. A detector blind to the case it exists to catch is the same
/// shape again, one level up.
///
/// FOLLOW-UP (not done here, and not claimed): making these tables reachable
/// only through the store, so a write from another module fails to COMPILE
/// rather than failing a test. Rust cannot fully deliver that — nothing stops a
/// module opening its own pool and issuing SQL — so it would raise the cost of
/// bypassing without closing it. The crate-wide scan is what holds the line.
///
/// The one thing it cannot see is an out-of-process write (an operator editing
/// the tables by hand). That is what the short TTL backstop remains for, and
/// it is the only residual — stated plainly rather than left implied.
static SCOPE_EPOCH: ScopeEpoch = ScopeEpoch::new();

/// The two counters that together make invalidation linearizable with reads.
///
/// ## Why a generation alone is not enough
/// A generation says *something changed*. It does not say *a write is in
/// progress*, and round 5 of review found the interval that distinction leaves
/// open:
///
/// 1. `ScopeWrite::begin` bumps the generation to `g+1`.
/// 2. A resolver misses the cache, observes `g+1`, and reads the OLD rows while
///    the transaction is still open.
/// 3. The write commits.
/// 4. Before `Drop` bumps to `g+2`, that resolver inserts what it read — at
///    generation `g+1`, which is *still current*, so the entry is accepted.
/// 5. A later request is served that entry and uses revoked authority.
///
/// The pre-write bump stops old RESIDENT entries being served and the
/// post-write bump stops a stale result surviving past `Drop`, but neither
/// stops a read that BEGAN after the pre-bump from populating in the gap.
///
/// ## The distinction that fixes it
/// "May I compute an answer?" is not "may I persist it?". A read that began
/// before the commit is entitled to compute and serve a pre-write answer —
/// that is ordinary concurrency, and the request genuinely preceded the
/// revocation. What must not happen is that answer being CACHED, because
/// caching is what converts a legitimately-concurrent read into authority
/// served *after* the commit, to callers who arrived after it.
///
/// So [`writes_in_flight`](Self::writes_in_flight) is tracked as its own state
/// and cache POPULATION refuses while it is non-zero, in addition to the
/// generation check. Reads still compute; they just do not persist.
///
/// A COUNT, not a flag: with two concurrent writers a boolean would let the
/// second writer's drop clear the first writer's exclusion while its
/// transaction was still open. The count only returns to zero when the last
/// guard drops.
///
/// This is deliberately NOT a lock. Nothing waits on anything, and no lock is
/// held across a database round trip — holding one over a transaction is how a
/// stall appears under load on the exact path an operator is using during an
/// incident. The only cost is that concurrent readers re-derive for the
/// duration of a write.
pub(crate) struct ScopeEpoch {
    generation: AtomicU64,
    writes_in_flight: AtomicU64,
}

impl ScopeEpoch {
    pub(crate) const fn new() -> Self {
        Self {
            generation: AtomicU64::new(0),
            writes_in_flight: AtomicU64::new(0),
        }
    }

    fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    fn bump(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::SeqCst) + 1
    }

    fn writes_in_flight(&self) -> u64 {
        self.writes_in_flight.load(Ordering::SeqCst)
    }

    /// Whether a resolution that observed `observed_generation` may be cached.
    ///
    /// Both conditions are required and neither implies the other: the
    /// generation check catches a read whose answer has been superseded, and
    /// the in-flight check catches a read whose answer is about to be, in the
    /// window where the generation has not moved yet.
    fn may_populate(&self, observed_generation: u64) -> bool {
        self.writes_in_flight() == 0 && self.generation() == observed_generation
    }
}

/// The tables whose contents determine what a connector resolves to.
///
/// Any mutation of one of these can NARROW some client's effective set, so any
/// mutation of one of these must bump the epoch. This list is the single source
/// of truth for that rule and is consumed by
/// `store::tests::every_scope_affecting_write_bumps_the_generation`, which reads
/// the store's own source and fails if a write against one of these tables ever
/// appears outside the invalidating chokepoint.
///
/// Round 2 of review is why that guard exists. Round 1 closed the client-scoping
/// writes but left the tool-group DEFINITION writes uncovered — editing a
/// group's patterns narrows every client the group is attached to, and that is a
/// revocation — and the residual was addressed by DOCUMENTING an obligation on
/// RMCP-06 rather than enforcing one. A comment describing a rule is not a rule;
/// the next author does not read it, and nothing fails when they do not.
///
/// Deliberately NOT listed, because none of them feeds
/// [`ScopeResolver::load`]: `rmcp_account` (account state is the GRANT side,
/// re-derived per request and never cached here), `rmcp_auth_code`,
/// `rmcp_refresh_token`, `rmcp_login_session_use` and `rmcp_consent` (all
/// per-token state checked on their own paths). Including them would be
/// harmless for correctness but would flush this cache on every token issuance,
/// which is frequent — the list is narrow because it is exact, not to save
/// work.
pub const SCOPE_AFFECTING_TABLES: &[&str] = &[
    "rmcp_client",
    "rmcp_client_scope",
    "rmcp_client_server",
    "rmcp_server_owner",
    "rmcp_tool_group",
];

/// The current invalidation epoch.
///
/// `SeqCst` throughout. The cheaper orderings would be sufficient for a plain
/// monotone counter, but the property being protected is an authorization
/// invariant and the counter is read at most once per cache miss — this is not
/// a hot path worth shaving, and a total order is the version of this that is
/// obviously correct rather than the version that needs an argument.
pub fn scope_generation() -> u64 {
    SCOPE_EPOCH.generation()
}

/// Advance the epoch, invalidating every cached scope and — crucially — every
/// resolution ALREADY IN FLIGHT that read an earlier value.
///
/// That second half is the whole point, and is what round 1 finding 2 was
/// about: invalidating a map only AFTER an awaited write loses to a resolve
/// that STARTED before the write. That resolve read the old rows, and it
/// repopulates the cache after the invalidation has already run — reinserting
/// the stale permit for a full TTL, with nothing in the system reporting
/// anything wrong. A comment observing that the window is small would not be a
/// fix: the window is exactly the moment a revocation is happening, which is
/// the only moment that matters.
///
/// With an epoch, a resolve that began at generation *g* may only populate the
/// cache AT generation *g*, and an entry is served only while its generation is
/// current. A bump anywhere in between makes the in-flight result unusable
/// rather than authoritative. The cost of losing the race is one extra store
/// read; the cost of winning it the old way was a live revoked permission.
pub fn bump_scope_generation() -> u64 {
    SCOPE_EPOCH.bump()
}

/// Brackets one scope-affecting write, advancing the epoch on BOTH sides of it.
///
/// ## Why both sides — the window a post-write bump leaves open
/// Round 4 of review found that invalidating only AFTER the database operation
/// is not linearizable with reads. Between the moment the write COMMITS and the
/// moment the bump lands, a cache hit still finds an entry whose generation is
/// current, so it is served without consulting the database — and a revocation
/// that has already committed is not yet enforced. The read-side generation
/// check cannot close this: the entry genuinely was valid a microsecond ago,
/// and nothing has told the cache otherwise yet.
///
/// This is a DIFFERENT window from the one round 2 closed. That one was an
/// in-flight resolve repopulating the cache after an invalidation; this one is
/// a plain hit on a resident entry. Both are now closed, by the two bumps:
///
/// - **On `begin`** — from the instant the revocation starts, every resident
///   entry is stale, so no cached answer can be served while the write is in
///   progress. There is no interval in which a committed revocation is served
///   from cache, because the invalidation precedes the commit rather than
///   trailing it.
/// - **On `drop`** — for the round-2 case: a resolve that read the OLD rows
///   before the commit may still be in flight and about to cache them, and the
///   trailing bump makes whatever it cached unusable. It also runs when the
///   write fails or panics partway, so a half-applied write never leaves a
///   cache the process believes is fresh.
///
/// Neither bump closes the interval BETWEEN them — a read that began after the
/// pre-bump and returns before the drop finds its observed generation still
/// current. That is what [`ScopeEpoch`]'s in-flight count is for; read its docs
/// next. The guard maintains both: the count is incremented before the
/// generation is bumped and decremented after it is bumped again, so there is
/// never an instant in which the generation reads as changed while the write
/// reads as finished.
///
/// The cost is re-derivation for concurrent readers for the duration of the
/// write: a re-read, not a wrong answer. That is the correct direction of error
/// for an authorization control and the same trade made everywhere else here.
///
/// `Drop` rather than an explicit trailing call so the guarantee survives the
/// paths that are easy to forget — an early `return`, a `?` propagation, or a
/// panic. `set_client_tool_groups` has two early returns that write nothing;
/// they bump anyway, which costs a re-read and removes any need to reason about
/// which exit paths matter.
#[must_use = "the guard must be HELD for the duration of the write — binding it to `_` \
              drops it immediately, so both bumps happen before the write and the \
              post-write invalidation is lost"]
pub(crate) struct ScopeWrite {
    epoch: &'static ScopeEpoch,
}

impl ScopeWrite {
    /// Begin a scope-affecting write against the process-global epoch.
    pub(crate) fn begin() -> Self {
        Self::on(&SCOPE_EPOCH)
    }

    /// Begin against an explicit epoch, so the ordering can be tested without
    /// racing every other test in the binary — the same injection the cache
    /// uses. This is the production code path; only the epoch differs.
    fn on(epoch: &'static ScopeEpoch) -> Self {
        // ORDER MATTERS. The exclusion is established BEFORE the change is
        // announced. Bumping first would leave an instant in which the
        // generation already reads `g+1` while `writes_in_flight` is still 0 —
        // and a resolver that observed `g+1` in exactly that instant could
        // populate, which is the hole this guard exists to close.
        epoch.writes_in_flight.fetch_add(1, Ordering::SeqCst);
        epoch.bump();
        Self { epoch }
    }
}

impl Drop for ScopeWrite {
    fn drop(&mut self) {
        // The exact mirror, and equally order-sensitive. Decrementing first
        // would leave an instant with `writes_in_flight == 0` while the
        // generation still reads `g+1` — so a resolver holding `observed ==
        // g+1` with pre-write rows would find both checks satisfied and cache
        // a revoked permit. Bump first, so no such instant exists.
        self.epoch.bump();
        self.epoch.writes_in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

struct CacheEntry {
    inserted: Instant,
    /// The epoch this entry was resolved at. An entry whose generation is no
    /// longer current is never served, however fresh its timestamp.
    generation: u64,
    scope: Arc<ClientScope>,
}

/// The cache itself, deliberately separated from the store.
///
/// Split out for one reason: the invalidation behaviour is the security-
/// relevant half of [`ScopeResolver`], and a cache welded to a `PgPool` can
/// only be tested against a live database. Here it is a plain unit with no I/O,
/// so "a scoping write invalidates immediately", "an expired entry is not
/// served" and "the map cannot grow without bound" are all asserted directly.
struct ScopeCache {
    entries: RwLock<HashMap<String, CacheEntry>>,
    ttl: Duration,
    /// The epoch this cache is keyed against.
    ///
    /// A `&'static ScopeEpoch` rather than a direct reference to
    /// [`SCOPE_EPOCH`] for one reason: it lets a test give a cache its OWN
    /// epoch. The generation tests assert exact epoch transitions, and on the
    /// process-global epoch they would race every other test that performs a
    /// scoping write — a guard against a race must not itself be racy.
    epoch: &'static ScopeEpoch,
}

impl ScopeCache {
    /// A cache keyed against the process-global epoch — the only form used
    /// outside tests.
    fn new(ttl: Duration) -> Self {
        Self::with_epoch(ttl, &SCOPE_EPOCH)
    }

    fn with_epoch(ttl: Duration, epoch: &'static ScopeEpoch) -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
            ttl,
            epoch,
        }
    }

    fn generation(&self) -> u64 {
        self.epoch.generation()
    }

    fn bump(&self) -> u64 {
        self.epoch.bump()
    }

    /// A live entry, or `None` when absent, expired, or resolved at a
    /// superseded epoch.
    ///
    /// The generation check is the guarantee, and it is deliberately on the
    /// READ side: an entry that was inserted before a bump — or that raced a
    /// bump and slipped past the write-side check below — is still never
    /// served. Removing this check is what the
    /// `a_stale_generation_entry_is_never_served` test exists to catch.
    ///
    /// A poisoned lock reads as a MISS rather than as an error: the caller then
    /// re-reads the store and gets the current answer, which is the safe
    /// direction. Serving a possibly-torn entry would not be.
    fn get(&self, client_id: &str) -> Option<Arc<ClientScope>> {
        let entries = self.entries.read().ok()?;
        let entry = entries.get(client_id)?;
        if entry.generation != self.generation() {
            return None;
        }
        if entry.inserted.elapsed() >= self.ttl {
            return None;
        }
        Some(Arc::clone(&entry.scope))
    }

    /// Cache a scope that was resolved starting at `observed_generation`.
    ///
    /// Refuses to insert when the epoch has moved on since the resolve began —
    /// the in-flight-resolve half of finding 2. This is the second layer, not
    /// the guarantee: even if an entry does land at a superseded generation,
    /// [`Self::get`] will not serve it.
    fn put_at(&self, client_id: &str, scope: Arc<ClientScope>, observed_generation: u64) {
        // Two conditions, neither implying the other (see `ScopeEpoch`):
        // the generation must not have moved since this resolve began, AND no
        // write may be in progress. The second is what closes the round-5
        // interval — a read that began after the pre-write bump, read pre-write
        // rows, and is now trying to persist them while the write's guard is
        // still held. It computed an answer, which is fine; it must not cache
        // one, which would serve it to callers who arrived after the commit.
        if !self.epoch.may_populate(observed_generation) {
            return;
        }
        self.insert(client_id, scope, observed_generation);
    }

    fn insert(&self, client_id: &str, scope: Arc<ClientScope>, generation: u64) {
        let Ok(mut entries) = self.entries.write() else {
            // A poisoned lock means some thread panicked mid-update. Losing the
            // cache is a latency problem; guessing at its contents would be a
            // correctness one, so this simply declines to cache.
            return;
        };
        if entries.len() >= MAX_CACHED_CLIENTS {
            // Drop expired entries first; if that is not enough, drop
            // everything. Clearing is safe in the direction that matters — the
            // next request re-reads the store and gets the CURRENT answer — and
            // it avoids an eviction policy whose bugs would be permission bugs.
            let ttl = self.ttl;
            entries.retain(|_, e| e.inserted.elapsed() < ttl);
            if entries.len() >= MAX_CACHED_CLIENTS {
                entries.clear();
            }
        }
        entries.insert(
            client_id.to_string(),
            CacheEntry {
                inserted: Instant::now(),
                generation,
                scope,
            },
        );
    }

    /// Drop one client's entry AND advance the epoch.
    ///
    /// The epoch bump is what makes this race-safe, so the two are one
    /// operation and there is no way to do the map removal without it. Bumping
    /// globally for a single-client removal is deliberately conservative: it
    /// costs other clients one store read each and removes any need to reason
    /// about whether a given write could have affected a different client than
    /// the one named — which, for an ownership change, it very much can.
    fn remove(&self, client_id: &str) {
        self.bump();
        if let Ok(mut entries) = self.entries.write() {
            entries.remove(client_id);
        }
    }

    /// Drop every entry AND advance the epoch.
    fn clear(&self) {
        self.bump();
        if let Ok(mut entries) = self.entries.write() {
            entries.clear();
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.read().map(|e| e.len()).unwrap_or(0)
    }

    /// Insert bypassing the write-side generation check, to construct the state
    /// that would exist if that check were absent. Lets the read-side guarantee
    /// be tested on its own.
    #[cfg(test)]
    fn force_insert_at(&self, client_id: &str, scope: Arc<ClientScope>, generation: u64) {
        self.insert(client_id, scope, generation);
    }
}

/// Resolves a `client_id` to its [`ClientScope`], with an explicitly
/// invalidated cache in front of the store.
///
/// ## Why there is a cache at all, and why it is keyed the way it is
/// This is the first per-request database I/O on the authorization path;
/// everything else the gateway consults is an in-memory map. Two round trips
/// per `tools/list` on a 400-tool catalog is not acceptable latency for the
/// dispatch path, so a cache is required.
///
/// The spec asks for a cache keyed on `(client, catalog-generation)`. What is
/// cached here is the client's SCOPE ROWS keyed on the client alone, and the
/// intersection against the catalog and the grant is recomputed on every list
/// and every call. That is a deliberate deviation, and it is the safer of the
/// two:
///
/// - Keying on a catalog generation would require hashing the catalog on every
///   request, which costs the same linear walk as the resolution it would be
///   saving. The database round trip is the only expensive part, and that is
///   what is actually cached.
/// - Re-deriving against the LIVE catalog and the LIVE grant on every request
///   makes three of the spec's edge cases true by construction rather than by
///   invalidation: a catalog that changes between list and call is re-evaluated
///   at the call, a stale list never authorizes anything, and an account grant
///   that narrows after a token was issued takes effect on the next call.
///
/// ## Invalidation
/// Invalidation is driven by the EPOCH ([`SCOPE_EPOCH`]), which the store
/// bumps from inside its own narrowing writes. It therefore does not depend on
/// a caller choosing to go through this type: a direct
/// [`crate::oauth::store::OauthStore`] write, and an ownership or delegation
/// change that narrows many clients at once, both invalidate by construction.
/// Round 1 of review found the earlier design — invalidation coupled to this
/// type's own mutators — left a stale PERMIT live until the TTL expired, which
/// is revoked authority that still works.
///
/// The mutators below remain as conveniences that keep a write and its
/// invalidation in one call, but they are no longer what makes invalidation
/// correct, and nothing depends on a caller using them.
///
/// A failed store read is NEVER cached: it resolves to the empty scope for that
/// one request, so a transient database blip denies rather than poisoning the
/// cache with a denial that outlives it.
pub struct ScopeResolver {
    store: Arc<OauthStore>,
    cache: ScopeCache,
}

impl ScopeResolver {
    /// Wrap a store, reading the cache TTL from the environment.
    pub fn new(store: Arc<OauthStore>) -> Self {
        Self::with_ttl(store, Duration::from_secs(cache_ttl_secs()))
    }

    /// Wrap a store with an explicit TTL.
    pub fn with_ttl(store: Arc<OauthStore>, ttl: Duration) -> Self {
        Self {
            store,
            cache: ScopeCache::new(ttl),
        }
    }

    /// Resolve a connector's scope.
    ///
    /// Infallible by design — it returns a scope, never an error. Every failure
    /// (unknown client, disabled client, unreadable store) becomes
    /// [`ClientScope::empty`], because there is no failure whose correct
    /// handling is "permit something". A caller that received an error would
    /// have to decide what to do with it, and the only correct decision is the
    /// one already made here.
    pub async fn resolve(&self, client_id: &str) -> Arc<ClientScope> {
        if let Some(hit) = self.cache.get(client_id) {
            return hit;
        }

        // Read the epoch BEFORE the store round trip, never after. This value
        // is what the result is allowed to be cached AT, so a narrowing write
        // that lands while the load below is in flight makes the result
        // uncacheable rather than authoritative. Reading it afterwards would
        // reintroduce finding 2 exactly.
        // Read through the CACHE's own counter, so the resolver and the cache
        // can never end up reasoning about two different epochs.
        let observed_generation = self.cache.generation();

        let scope = match self.load(client_id).await {
            Ok(scope) => Arc::new(scope),
            Err(err) => {
                // Deny for this request without caching the denial: the next
                // request retries the store rather than inheriting a blip.
                tracing::warn!(
                    client_id = %client_id,
                    error = %err,
                    "rmcp scope: store read failed; resolving to the empty scope for this request"
                );
                return Arc::new(ClientScope::empty(client_id));
            }
        };

        self.cache
            .put_at(client_id, Arc::clone(&scope), observed_generation);
        scope
    }

    /// Read a client's scope straight from the store, re-deriving ownership.
    async fn load(&self, client_id: &str) -> Result<ClientScope, ToolError> {
        // `find_active_client` filters disabled clients, so a client an
        // operator just switched off resolves to the empty scope here rather
        // than to its last-known groups.
        let Some(client) = self.store.find_active_client(client_id).await? else {
            return Ok(ClientScope::empty(client_id));
        };
        // Both of these re-join ownership in SQL (see their docs in `store`):
        // a group whose owner no longer matches the client's owner, and a
        // namespace whose delegation was cleared, both drop out HERE, on the
        // read path, rather than living on because they were valid when they
        // were written.
        // `client_authorized_groups`, NOT `client_tool_groups`: the former
        // projects the owner's CURRENT authority in the same statement as the
        // rows, and resolving from the latter would drop it — honouring a `*`
        // (or, since RMCP-12, a local pattern) whose owner is no longer entitled
        // to hold one. Its own doc comment says it is the resolution entry
        // point; this call site is the one that has to agree with that.
        let groups = self.store.client_authorized_groups(client.id).await?;
        let namespaces = self.store.client_namespaces(client.id).await?;
        Ok(ClientScope::from_rows(client_id, &groups, namespaces))
    }

    /// Drop one client's cached scope. Idempotent.
    pub fn invalidate(&self, client_id: &str) {
        self.cache.remove(client_id);
    }

    /// Drop every cached scope.
    ///
    /// The correct response to a change whose blast radius is not one client —
    /// a namespace delegation being reassigned or cleared (RMCP-12), which can
    /// narrow any number of clients at once.
    pub fn invalidate_all(&self) {
        self.cache.clear();
    }

    /// Replace a client's tool groups.
    ///
    /// A convenience that keeps a write and its cache drop in one call. The
    /// store bumps the epoch itself, so a caller that skips this method and
    /// goes straight to the store is equally safe — that is the point of
    /// finding 1's fix. Ownership enforcement lives in the store, inside the
    /// write transaction.
    pub async fn set_client_tool_groups(
        &self,
        actor_account_id: Uuid,
        client: &str,
        client_uuid: Uuid,
        group_ids: &[Uuid],
    ) -> Result<(), ToolError> {
        let result = self
            .store
            .set_client_tool_groups(actor_account_id, client_uuid, group_ids)
            .await;
        // Invalidated on the FAILURE path too. A write that returned an error
        // may still have committed part of a transaction in some future
        // refactor, and re-reading a correct answer costs one query.
        self.invalidate(client);
        result
    }

    /// Replace a client's namespaces. See [`Self::set_client_tool_groups`].
    pub async fn set_client_namespaces(
        &self,
        actor_account_id: Uuid,
        client: &str,
        client_uuid: Uuid,
        namespaces: &[String],
    ) -> Result<(), ToolError> {
        let result = self
            .store
            .set_client_namespaces(actor_account_id, client_uuid, namespaces)
            .await;
        self.invalidate(client);
        result
    }

    /// Grant server ownership AS `actor_account_id`, then drop EVERY cached
    /// scope.
    ///
    /// This and [`Self::revoke_server_owner`] are why [`Self::invalidate_all`]
    /// exists. A delegation change's blast radius is not one client: it can
    /// narrow every connector owned by the account that just lost the
    /// namespace, and this process does not know which those are without asking
    /// the store. Dropping the whole map costs a re-read per client and is the
    /// only version of this that cannot miss one.
    ///
    /// ## The actor is an argument because round 1 found it missing
    ///
    /// The first version of this method took only a namespace and an owner, and
    /// called the store's raw mutator. That made the resolver a SECOND,
    /// unauthorized door onto delegation: every rule
    /// [`crate::oauth::delegation`] enforces was bypassable by calling this
    /// instead of [`crate::oauth::delegation::DelegationService`]. The store's
    /// mutators are now private and demand a proof value that can only be
    /// produced by running the check, so this method could not be written that
    /// way today — but it also no longer wants to be. It delegates to the same
    /// service every other caller uses, and adds exactly one thing: the cache
    /// drop.
    ///
    /// The epoch bump alone is already sufficient for CORRECTNESS (see
    /// [`ScopeWrite`]) — no stale entry can be served across a delegation write
    /// whether or not this method is the one used. What the explicit drop adds
    /// is that the entries are FREED rather than left resident-but-unusable.
    pub async fn set_server_owner(
        &self,
        actor_account_id: Uuid,
        namespace: &str,
        grantee_name: &str,
    ) -> Result<crate::oauth::delegation::DelegationChange, ToolError> {
        let result = self
            .delegation()
            .grant(actor_account_id, namespace, grantee_name)
            .await;
        // On the FAILURE path too, for the same reason as every other write
        // here: re-reading a correct answer costs one query, and a missed
        // invalidation leaves revoked authority live.
        self.invalidate_all();
        result
    }

    /// Revoke server ownership AS `actor_account_id`, then drop every cached
    /// scope. See [`Self::set_server_owner`].
    pub async fn revoke_server_owner(
        &self,
        actor_account_id: Uuid,
        namespace: &str,
    ) -> Result<crate::oauth::delegation::DelegationChange, ToolError> {
        let result = self.delegation().revoke(actor_account_id, namespace).await;
        self.invalidate_all();
        result
    }

    /// The delegation service over this resolver's own store.
    ///
    /// Constructed per call rather than held: it is a handle around the same
    /// `Arc`, and building it here means there is no second place where an
    /// authority could be cached across requests.
    fn delegation(&self) -> crate::oauth::delegation::DelegationService {
        crate::oauth::delegation::DelegationService::new(
            Arc::clone(&self.store) as Arc<dyn crate::oauth::delegation::DelegationStore>
        )
    }

    /// Enable or disable a client, then invalidate.
    ///
    /// Disabling is the fastest revocation an operator has, so it must not wait
    /// out a TTL.
    pub async fn set_client_disabled(
        &self,
        client: &str,
        client_uuid: Uuid,
        disabled: bool,
    ) -> Result<(), ToolError> {
        let result = self.store.set_client_disabled(client_uuid, disabled).await;
        self.invalidate(client);
        result
    }
}

/// The connector scope for a `client_id`, as `crate::mcp_server` needs it.
///
/// A trait rather than a direct [`ScopeResolver`] for the same reason
/// [`crate::oauth::resource::TokenState`] is one, and stated there: this crate
/// stands up no Postgres in tests, so a concrete store on the request path
/// would make the end-to-end behaviour of the scoping gate assertable only by
/// reading the code — which, for the control that decides what an
/// internet-facing connector may reach, is not good enough.
///
/// Production is [`ScopeResolver`]. There is deliberately no default
/// implementation and no blanket impl: a type that cannot answer this question
/// must not silently answer it permissively.
#[async_trait::async_trait]
pub trait ClientScopeSource: Send + Sync {
    /// The scope for `client_id`. Infallible by contract — every failure is the
    /// EMPTY scope, never an error the caller might mishandle into a permit.
    async fn scope_for(&self, client_id: &str) -> Arc<ClientScope>;
}

#[async_trait::async_trait]
impl ClientScopeSource for ScopeResolver {
    async fn scope_for(&self, client_id: &str) -> Arc<ClientScope> {
        self.resolve(client_id).await
    }
}

/// THE way a binary turns its OAuth door into a scope source (TERM #631, item 5).
///
/// Before this existed, `terminus_primary` built its gateway state with
/// `scope_resolver: None` while the door itself was fully mounted: a connector
/// could authenticate, reach `handle_mcp`, resolve to [`ClientScope::empty`]
/// and be refused every tool. Fail-closed and therefore not a security hole —
/// but the feature reached nothing, and no unit test could see it, because
/// every unit-level piece passed while the assembled process permitted nothing.
///
/// ## Why it is a function and not three lines in each `main`
/// There must be exactly ONE answer to "where does a connector's ceiling come
/// from", for the same reason [`decide`] is one function with two call sites.
/// A second binary that grew its own version of this would be one refactor away
/// from a `unwrap_or_else(|| unrestricted)` that no reviewer of THIS file would
/// ever see.
///
/// ## The three branches, and why each is the safe one
/// - The door is OFF (`None`): no request can carry a connector at all, so
///   there is nothing to scope. Not a widening — [`crate::mcp_server`] only
///   consults a scope when [`crate::oauth::resource::OauthCaller`] bound one.
/// - The door is ON and owns a store: a real [`ScopeResolver`] over THAT store.
///   The same handle the door authenticates through, never a second pool.
/// - The door is ON and owns no store (a door built over a fake
///   [`crate::oauth::resource::TokenState`]): `None`, which
///   [`crate::mcp_server`] reads as the EMPTY scope and refuses. There is
///   deliberately no branch here that opens a store, and none that substitutes
///   a permissive source: a missing store must never read as permission.
///
/// Note what this function does NOT do: it never returns a source that permits
/// more than the store says, and absence at every level collapses to the empty
/// set rather than to a default.
pub fn scope_source_for_door(
    door: Option<&Arc<crate::oauth::resource::OauthResourceServer>>,
) -> Option<Arc<dyn ClientScopeSource>> {
    let store = door?.store()?;
    Some(Arc::new(ScopeResolver::new(Arc::clone(store))) as Arc<dyn ClientScopeSource>)
}

/// Resolve the cache TTL, falling back to the default on absent or unparseable
/// input.
///
/// A zero is honoured — it means "never serve from cache", which is a
/// legitimate (if slow) operator choice and the safe direction. An unparseable
/// value falls back to the default rather than failing the door: this knob
/// grants nothing, so the fail-closed rule that governs PERMISSIONS does not
/// apply to it.
fn cache_ttl_secs() -> u64 {
    std::env::var(CACHE_TTL_ENV)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_CACHE_TTL_SECS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn group(patterns: &[&str], owner: crate::oauth::groups::GroupOwner) -> AuthorizedGroup {
        AuthorizedGroup {
            group: crate::oauth::model::ToolGroup {
                id: Uuid::nil(),
                name: "g".into(),
                description: String::new(),
                patterns: patterns.iter().map(|p| (*p).to_string()).collect(),
                owner_account_id: Uuid::nil(),
                created_at: Utc::now(),
            },
            owner,
        }
    }

    /// The default test scope is OPERATOR-owned, because most of these tests
    /// are about the matcher rather than about delegation.
    fn scope(patterns: &[&str], namespaces: &[&str]) -> ClientScope {
        scope_owned_by(patterns, namespaces, crate::oauth::groups::GroupOwner::Operator)
    }

    fn scope_owned_by(
        patterns: &[&str],
        namespaces: &[&str],
        owner: crate::oauth::groups::GroupOwner,
    ) -> ClientScope {
        ClientScope::from_rows(
            "a-client-id",
            &[group(patterns, owner)],
            namespaces.iter().map(|n| (*n).to_string()).collect(),
        )
    }

    /// A catalog entry from the ORDINARY world, where a `__`-shaped advertised
    /// name really was contributed by the upstream it names.
    ///
    /// This is a FIXTURE BUILDER, and the only place in these tests that reads
    /// a name's shape. That inference is exactly what TERM #643 removed from
    /// the production path, so it is spent once here, building the input, and
    /// never inside anything under test. The interesting cases — a LOCAL tool
    /// whose name looks federated — are built with [`CatalogTool::local`]
    /// explicitly, because there is no name from which that could be inferred.
    fn advertised(name: &str) -> CatalogTool {
        match crate::mesh::split_namespaced(name) {
            Some((namespace, bare)) => CatalogTool::from_upstream(namespace, bare),
            None => CatalogTool::local(name),
        }
    }

    fn catalog(names: &[&str]) -> Vec<CatalogTool> {
        names.iter().map(|n| advertised(n)).collect()
    }

    /// Parse a pattern the way the dispatch path does.
    fn pattern(raw: &str) -> Pattern {
        Pattern::parse_stored(raw).expect("the fixture pattern must parse")
    }

    /// RMCP-12 at the ENFORCING matcher: the same stored rows resolve to less
    /// when their owner is no longer an operator. This is the read-path
    /// re-derivation, and it is what makes a demotion or a revoked delegation
    /// effective on the next call rather than at a TTL.
    #[test]
    fn a_delegated_owners_local_patterns_do_not_reach_the_enforcing_matcher() {
        use crate::oauth::groups::GroupOwner;
        let operator_scope =
            scope_owned_by(&["weather_*", "peerone::*"], &["peerone"], GroupOwner::Operator);
        assert!(decide(&allow_all, &operator_scope, &advertised("weather_get")).is_allowed());
        assert!(decide(&allow_all, &operator_scope, &advertised("peerone__weather_get")).is_allowed());

        let delegated_scope =
            scope_owned_by(&["weather_*", "peerone::*"], &["peerone"], GroupOwner::Delegated);
        assert_eq!(
            decide(&allow_all, &delegated_scope, &advertised("weather_get")),
            Decision::Deny(DenyReason::NoGroup),
            "an unqualified pattern must not reach the fleet's own tools for a delegated owner"
        );
        assert!(
            decide(&allow_all, &delegated_scope, &advertised("peerone__weather_get")).is_allowed(),
            "and the namespaced pattern must still work — the rule separates them"
        );
    }

    /// Mutation-verified: delete the `owner_may_hold` arm in
    /// `ClientScope::from_rows` and this goes red, because a `*` stored by a
    /// non-operator would expand to the whole catalog again.
    #[test]
    fn a_delegated_owners_stored_wildcard_expands_to_nothing_here_too() {
        use crate::oauth::groups::GroupOwner;
        let delegated_scope = scope_owned_by(&["*"], &[], GroupOwner::Delegated);
        assert!(delegated_scope.is_empty(), "the group must collapse to no patterns at all");
        assert_eq!(
            decide(&allow_all, &delegated_scope, &advertised("weather_get")),
            Decision::Deny(DenyReason::NoGroup)
        );
        assert!(decide(
            &allow_all,
            &scope_owned_by(&["*"], &[], GroupOwner::Operator),
            &advertised("weather_get")
        )
        .is_allowed());
    }

    /// A grant that permits everything — used to isolate the CLIENT half of the
    /// intersection in tests that are not about the grant.
    fn allow_all(_tool: &str) -> bool {
        true
    }

    // -- patterns -----------------------------------------------------------
    //
    // The GRAMMAR — which strings parse and which are refused — is
    // `crate::oauth::groups`'s, and is tested there. TERM #637 deleted the
    // second parser that used to live in this file, so the parse-shape tests
    // went with it rather than being kept as a second, drifting copy of
    // someone else's contract. What stayed is every test below: they assert
    // ENFORCEMENT properties, which no authoring-side suite has a reason to
    // cover, and they were RE-POINTED at the surviving matcher rather than
    // deleted alongside the parser.

    /// The acceptance criterion spelled out by name: an unqualified prefix
    /// matches LOCAL tools only, so it cannot reach across a namespace boundary
    /// into a federated server's tool whose bare name happens to start the same
    /// way.
    #[test]
    fn a_prefix_does_not_reach_into_another_namespace() {
        let p = pattern("a*");
        assert!(p.matches(&advertised("agent_selftest")));
        assert!(!p.matches(&advertised("peerone__agent_selftest")));

        // And the namespace form matches only its own namespace.
        let namespaced = pattern("peerone::*");
        assert!(namespaced.matches(&advertised("peerone__anything")));
        assert!(!namespaced.matches(&advertised("peertwo__anything")));
        assert!(!namespaced.matches(&advertised("anything")));
        // A bare name that itself contains the advertised separator stays
        // inside its OWN namespace — the catalog says which that is.
        assert!(namespaced.matches(&advertised("peerone__deep__name")));
        assert!(!pattern("deep::*").matches(&advertised("peerone__deep__name")));
    }

    /// TERM #637 part B, findings 1 and 2: the qualified forms.
    ///
    /// Before these existed the only way to reach a federated tool was to grant
    /// its whole namespace, because the unqualified forms are local-only. That
    /// is a scoping feature that cannot express the narrowest thing an operator
    /// would ask for first.
    #[test]
    fn qualified_patterns_address_one_federated_tool_or_one_prefix_within_a_namespace() {
        // ONE specific federated tool — and nothing else, in either namespace.
        let exact = pattern("peerone::weather_now");
        assert_eq!(
            exact,
            Pattern::NamespacedExact {
                namespace: "peerone".into(),
                bare: "weather_now".into()
            }
        );
        assert!(exact.matches(&advertised("peerone__weather_now")));
        assert!(!exact.matches(&advertised("peertwo__weather_now")), "not another namespace");
        assert!(!exact.matches(&advertised("weather_now")), "not the local tool of the same name");
        assert!(!exact.matches(&advertised("peerone__weather_forecast")), "not a sibling");

        // A prefix WITHIN one namespace.
        let prefix = pattern("peerone::weather_*");
        assert_eq!(
            prefix,
            Pattern::NamespacedPrefix {
                namespace: "peerone".into(),
                prefix: "weather_".into()
            }
        );
        assert!(prefix.matches(&advertised("peerone__weather_now")));
        assert!(prefix.matches(&advertised("peerone__weather_forecast")));
        assert!(
            !prefix.matches(&advertised("peertwo__weather_now")),
            "cannot leak across the boundary"
        );
        assert!(!prefix.matches(&advertised("weather_now")), "and does not reach local tools");
        assert!(!prefix.matches(&advertised("peerone__media_search")));

        // The prefix matches the BARE name, so a namespace that merely starts
        // the same way is not reachable — the round-3 boundary lesson, applied
        // to the qualified form.
        let peer = pattern("peer::w*");
        assert!(peer.matches(&advertised("peer__weather_now")));
        assert!(!peer.matches(&advertised("peerone__weather_now")));

        // A bare name containing the advertised separator is addressable,
        // which is the whole reason the qualifier is `::` and not `__`.
        assert!(pattern("peerone::deep__name").matches(&advertised("peerone__deep__name")));
        assert!(pattern("peerone::deep*").matches(&advertised("peerone__deep__name")));
    }

    /// The two halves of the vocabulary must be able to express the SAME set —
    /// that is what TERM #637 was filed about. For every shape, a qualified
    /// pattern reaches exactly the federated tools an unqualified one reaches
    /// locally.
    #[test]
    fn qualified_and_unqualified_forms_are_symmetric() {
        let local = catalog(&["weather_now", "weather_forecast", "media_search"]);
        let federated = catalog(&[
            "peerone__weather_now",
            "peerone__weather_forecast",
            "peerone__media_search",
        ]);
        for (unqualified, qualified) in [
            ("weather_now", "peerone::weather_now"),
            ("weather_*", "peerone::weather_*"),
        ] {
            let u = pattern(unqualified);
            let q = pattern(qualified);
            let matched_local: Vec<&str> =
                local.iter().filter(|t| u.matches(t)).map(|t| t.name.as_str()).collect();
            let matched_federated: Vec<&str> =
                federated.iter().filter(|t| q.matches(t)).map(CatalogTool::bare_name).collect();
            assert_eq!(
                matched_local, matched_federated,
                "{unqualified} and {qualified} must select the same names"
            );
        }
    }

    /// End to end through `decide`: a connector scoped to ONE federated tool
    /// reaches that tool and nothing else on that peer — the expressiveness gap
    /// finding 1 was about, at the level that actually gates a call.
    #[test]
    fn a_connector_can_be_scoped_to_a_single_federated_tool() {
        let sc = scope(&["peerone::weather_now"], &["peerone"]);
        assert!(decide(&allow_all, &sc, &advertised("peerone__weather_now")).is_allowed());
        assert_eq!(
            decide(&allow_all, &sc, &advertised("peerone__media_search")).deny_code(),
            Some("no_group"),
            "the rest of the peer stays out of reach"
        );
        assert_eq!(
            decide(&allow_all, &sc, &advertised("peertwo__weather_now")).deny_code(),
            Some("no_namespace"),
            "and another peer is refused on the namespace dimension first"
        );
        assert_eq!(
            decide(&allow_all, &sc, &advertised("weather_now")).deny_code(),
            Some("no_group"),
            "and the local tool of the same name is NOT granted by a qualified pattern"
        );
    }

    /// **The adversarial shape RMCP-06's review found.** A bare prefix that is
    /// also a strict prefix of a NAMESPACE must not reach into that namespace.
    ///
    /// This is the case an unrestricted `starts_with` gets wrong, and the case
    /// the older `ledger_*` / `peerone__…` test did NOT cover: there the
    /// pattern failed to match for the incidental reason that the namespace
    /// spelled differently. Here the namespace begins with the pattern's own
    /// prefix, so only an actual boundary rule can refuse it.
    #[test]
    fn a_bare_prefix_cannot_reach_a_namespace_it_is_a_prefix_of() {
        let p = pattern("peer*");

        // Genuinely LOCAL tools starting with the prefix still match — the fix
        // must narrow the boundary, not break the pattern.
        assert!(p.matches(&advertised("peermetrics")));
        assert!(p.matches(&advertised("peer")));

        // But nothing across a namespace boundary, however the namespace is
        // spelled — including when it starts with the prefix itself.
        assert!(!p.matches(&advertised("peerhub__alerts_list")));
        assert!(!p.matches(&advertised("peer__alerts_list")));
        assert!(!p.matches(&advertised("peermetrics__alerts_list")));

        // A qualified pattern still reaches in, when written deliberately.
        let qualified = pattern("peerhub::*");
        assert!(qualified.matches(&advertised("peerhub__alerts_list")));
        assert!(!qualified.matches(&advertised("peermetrics")));

        // And a bare pattern spelling out a namespaced name is REFUSED
        // outright by the surviving matcher, rather than parsing into
        // something that can never match. That is the stricter write-time rule
        // from the authoring side, and it now also governs a stored row: an
        // old-vocabulary `peerhub__alerts_list` drops instead of quietly
        // becoming an unmatchable local exact.
        assert!(
            Pattern::parse_stored("peerhub__alerts_list").is_none(),
            "`__` in an unqualified pattern must be refused, not reinterpreted"
        );
    }

    /// The same boundary at the level that matters — a full [`decide`] where
    /// the client IS scoped to the namespace, so the namespace dimension passes
    /// and the pattern is the only thing left to refuse the crossing.
    ///
    /// Without the local-only rule this is a genuine unintended permit: the
    /// connector was granted one federated server and a local-looking prefix,
    /// and would have received that server's entire catalog.
    #[test]
    fn a_bare_prefix_does_not_widen_into_an_allowed_namespace() {
        let sc = scope(&["peer*"], &["peerhub"]);

        assert!(
            decide(&allow_all, &sc, &advertised("peermetrics")).is_allowed(),
            "the local tool the pattern was written for still works"
        );
        assert_eq!(
            decide(&allow_all, &sc, &advertised("peerhub__alerts_list")).deny_code(),
            Some("no_group"),
            "the namespace check PASSES here — only the pattern boundary refuses this, \
             which is exactly why the boundary has to be a rule and not a coincidence"
        );

        // Written deliberately, the qualified form does reach it.
        let deliberate = scope(&["peer*", "peerhub::*"], &["peerhub"]);
        assert!(decide(&allow_all, &deliberate, &advertised("peerhub__alerts_list")).is_allowed());
        assert!(decide(&allow_all, &deliberate, &advertised("peermetrics")).is_allowed());
    }

    /// Namespace collision: a local tool and a mesh-prefixed tool of the same
    /// bare name are distinct, and the namespaced form addresses only the
    /// namespaced one.
    #[test]
    fn namespaced_and_local_tools_of_the_same_bare_name_are_distinct() {
        let local = scope(&["shared_tool"], &["peerone"]);
        assert!(decide(&allow_all, &local, &advertised("shared_tool")).is_allowed());
        assert_eq!(
            decide(&allow_all, &local, &advertised("peerone__shared_tool")),
            Decision::Deny(DenyReason::NoGroup),
            "an exact local pattern must not match the namespaced tool"
        );

        let remote = scope(&["peerone::*"], &["peerone"]);
        assert!(decide(&allow_all, &remote, &advertised("peerone__shared_tool")).is_allowed());
        assert_eq!(
            decide(&allow_all, &remote, &advertised("shared_tool")),
            Decision::Deny(DenyReason::NoGroup),
        );
    }

    // -- provenance, not name shape (TERM #643) ------------------------------

    /// **The defect.** A LOCAL tool whose literal name contains the mesh
    /// separator belongs to no namespace, and no namespace-qualified pattern
    /// may reach it — however exactly that name matches the qualifier.
    ///
    /// Mutation-verified: change `Pattern::matches`'s qualified arms to compare
    /// `split_namespaced(&tool.name)` instead of `tool.namespace`, and every
    /// assertion here goes red while the rest of the suite stays green — which
    /// is precisely how this shipped. The client below holds the `peerhub`
    /// namespace, so the namespace dimension does NOT refuse it either: the
    /// provenance read is the only thing standing between a `peerhub` grant and
    /// a tool the fleet itself registered.
    #[test]
    fn a_qualified_pattern_cannot_reach_a_local_tool_that_merely_looks_federated() {
        let impostor = CatalogTool::local("peerhub__tool");
        assert!(impostor.namespace.is_none());

        for qualifier in ["peerhub::tool", "peerhub::*", "peerhub::t*"] {
            assert!(
                !pattern(qualifier).matches(&impostor),
                "{qualifier} must not reach a local tool named {}",
                impostor.name
            );
            let sc = scope(&[qualifier], &["peerhub"]);
            assert_eq!(
                decide(&allow_all, &sc, &impostor).deny_code(),
                Some("no_group"),
                "{qualifier} reached a LOCAL tool through decide()"
            );
        }

        // The genuinely federated tool of the same advertised name is still
        // reachable — the fix narrows provenance, it does not break the form.
        let real = CatalogTool::from_upstream("peerhub", "tool");
        assert_eq!(real.name, impostor.name, "the two are byte-identical as strings");
        for qualifier in ["peerhub::tool", "peerhub::*", "peerhub::t*"] {
            assert!(pattern(qualifier).matches(&real));
            assert!(decide(&allow_all, &scope(&[qualifier], &["peerhub"]), &real).is_allowed());
        }
    }

    /// The mirror image of the same defect, and the reason a fix that only
    /// tightened the qualified arms would be half a fix: the local tool was
    /// also UNREACHABLE by the unqualified pattern that legitimately covers it,
    /// because the local arms demanded that its name not split.
    ///
    /// Mutation-verified: restore `namespace.is_none()` to
    /// `split_namespaced(&tool.name).is_none()` in the local arms and this goes
    /// red.
    #[test]
    fn an_unqualified_pattern_still_reaches_a_local_tool_that_looks_federated() {
        let impostor = CatalogTool::local("peerhub__tool");
        assert!(pattern("peer*").matches(&impostor));
        assert!(pattern("*").matches(&impostor));

        let sc = scope(&["peer*"], &[]);
        assert!(
            decide(&allow_all, &sc, &impostor).is_allowed(),
            "a local tool must be reachable by the local pattern that covers it"
        );
        // And no namespace row is needed for it, because it came from no
        // namespace. Note the client above holds NONE.
        assert!(sc.namespaces().is_empty());
    }

    /// The namespace DIMENSION reads provenance too, not just the matcher.
    ///
    /// Mutation-verified: change `decide`'s step 2 back to
    /// `split_namespaced(&tool.name)` and the first assertion goes red — a
    /// local tool would need a namespace grant it can never legitimately
    /// require.
    #[test]
    fn the_namespace_dimension_is_bounded_by_provenance() {
        let sc = scope(&["*"], &[]);
        assert!(
            decide(&allow_all, &sc, &CatalogTool::local("peerhub__tool")).is_allowed(),
            "a local tool needs no namespace grant, whatever it is called"
        );
        assert_eq!(
            decide(&allow_all, &sc, &CatalogTool::from_upstream("peerhub", "tool")).deny_code(),
            Some("no_namespace"),
            "and the federated tool of that exact name still needs one"
        );
    }

    // -- the empty set ------------------------------------------------------

    /// The headline fail-closed property, asserted directly because the natural
    /// refactor to `unwrap_or(full_grant)` is exactly the widening bug.
    #[test]
    fn a_client_with_no_scoping_rows_reaches_nothing() {
        let none = ClientScope::empty("a-client-id");
        assert!(none.is_empty());
        let entries = catalog(&["weather_now", "peerone__weather_now", "media_search"]);
        assert!(effective(&allow_all, &none, entries.iter()).is_empty());
        for tool in &entries {
            assert!(
                !decide(&allow_all, &none, tool).is_allowed(),
                "an unscoped client must reach nothing, even under an unrestricted grant: {}",
                tool.name
            );
        }
        // The reason differs by dimension, and BOTH dimensions are empty for an
        // unscoped client: a local tool has no group, a federated one has no
        // namespace (checked first, since it is the more specific refusal).
        assert_eq!(
            decide(&allow_all, &none, &advertised("weather_now")).deny_code(),
            Some("no_group")
        );
        assert_eq!(
            decide(&allow_all, &none, &advertised("peerone__weather_now")).deny_code(),
            Some("no_namespace")
        );
    }

    /// An empty group, and a group whose patterns match nothing, are both the
    /// empty set — never a wildcard.
    #[test]
    fn empty_and_zero_match_groups_are_the_empty_set() {
        let empty_group = scope(&[], &["peerone"]);
        assert!(empty_group.is_empty());
        assert!(effective(&allow_all, &empty_group, catalog(&["weather_now"]).iter()).is_empty());

        let matches_nothing = scope(&["nothing_matches_this_*"], &["peerone"]);
        assert!(!matches_nothing.is_empty(), "it has a pattern");
        assert!(
            effective(
                &allow_all,
                &matches_nothing,
                catalog(&["weather_now", "media_search"]).iter()
            )
            .is_empty(),
            "a pattern matching zero tools is the empty set, not everything"
        );
    }

    /// An unparseable stored pattern must match nothing rather than error on
    /// the dispatch path — and must not take the rest of its group with it.
    #[test]
    fn an_unparseable_pattern_is_dropped_not_widened() {
        let with_junk = scope(&["a*b", "weather_*"], &["peerone"]);
        assert!(decide(&allow_all, &with_junk, &advertised("weather_now")).is_allowed());
        assert_eq!(
            decide(&allow_all, &with_junk, &advertised("media_search")),
            Decision::Deny(DenyReason::NoGroup)
        );
    }

    // -- the intersection ---------------------------------------------------

    /// The anti-widening invariant, over a randomised (deterministic, seedless)
    /// cross product of grants, scopes and catalogs. No input may produce a tool
    /// the account's own grant would deny.
    #[test]
    fn effective_is_subset_of_grant() {
        let mut entries = catalog(&[
            "weather_now",
            "weather_forecast",
            "media_search",
            "media_play",
            "admin_reset",
            "peerone__weather_now",
            "peerone__media_play",
            "peertwo__weather_now",
            "peertwo__admin_reset",
            "odd__name__with__separators",
        ]);
        // A LOCAL tool that looks federated, so the property is asserted over
        // both readings of the same string shape.
        entries.push(CatalogTool::local("peerthree__local_impostor"));
        let names: Vec<&str> = entries.iter().map(|t| t.name.as_str()).collect();

        let pattern_sets: Vec<Vec<&str>> = vec![
            vec![],
            vec!["*"],
            vec!["weather_*"],
            vec!["weather_now"],
            vec!["peerone::*"],
            vec!["peertwo::*", "media_*"],
            vec!["*", "peerone::*", "weather_now"],
            vec!["nothing_here_*"],
            vec!["odd::*"],
            vec!["peerthree::*"],
            vec!["peer*"],
        ];
        let namespace_sets: Vec<Vec<&str>> = vec![
            vec![],
            vec!["peerone"],
            vec!["peertwo"],
            vec!["peerone", "peertwo"],
            vec!["odd"],
            vec!["peerthree"],
        ];
        // A spread of grants including the pathological ends: deny-all,
        // allow-all, and grants keyed on properties orthogonal to the patterns.
        let grants: Vec<Box<dyn Fn(&str) -> bool>> = vec![
            Box::new(|_| false),
            Box::new(|_| true),
            Box::new(|t: &str| t.starts_with("weather_")),
            Box::new(|t: &str| !t.contains("admin")),
            Box::new(|t: &str| crate::mesh::split_namespaced(t).is_some()),
            Box::new(|t: &str| crate::mesh::split_namespaced(t).is_none()),
            Box::new(|t: &str| t.len() % 2 == 0),
        ];

        let mut saw_allow = false;
        for patterns in &pattern_sets {
            for namespaces in &namespace_sets {
                let sc = scope(patterns, namespaces);
                for grant in &grants {
                    let allowed = effective(grant.as_ref(), &sc, entries.iter());
                    for tool in &allowed {
                        assert!(
                            grant.permits_tool(tool),
                            "WIDENING: {tool} was permitted by the client scope but denied by the \
                             account grant (patterns={patterns:?}, namespaces={namespaces:?})"
                        );
                        saw_allow = true;
                    }
                    // And the set is genuinely a subset of the catalog.
                    for tool in &allowed {
                        assert!(names.contains(&tool.as_str()));
                    }
                }
            }
        }
        assert!(saw_allow, "the property test must actually exercise allows");
    }

    /// `tools/list` and `tools/call` must agree for every tool in the catalog —
    /// they route through the same function, and this asserts that they still
    /// do rather than that they were written to.
    #[test]
    fn list_and_call_never_disagree() {
        let mut entries = catalog(&[
            "weather_now",
            "media_search",
            "admin_reset",
            "peerone__weather_now",
            "peertwo__media_search",
            "odd__name__with__separators",
        ]);
        // TERM #643: a LOCAL entry whose name looks federated. It is the shape
        // that made the two paths capable of disagreeing at all, so it belongs
        // in the parity property rather than only in its own test.
        entries.push(CatalogTool::local("peerone__local_only"));
        for patterns in [
            vec![],
            vec!["*"],
            vec!["weather_*", "peerone::*"],
            vec!["media_search"],
            vec!["peer*"],
        ] {
            for namespaces in [vec![], vec!["peerone"], vec!["peerone", "peertwo"]] {
                let sc = scope(&patterns, &namespaces);
                let grant = |t: &str| !t.contains("admin");
                let listed = effective(&grant, &sc, entries.iter());
                for tool in &entries {
                    let callable = decide(&grant, &sc, tool).is_allowed();
                    assert_eq!(
                        listed.contains(&tool.name),
                        callable,
                        "list/call drift on {} (patterns={patterns:?}, ns={namespaces:?})",
                        tool.name
                    );
                }
            }
        }
    }

    /// A tool inside an allowed group but outside the client's namespaces is
    /// BOTH invisible and uncallable — the two halves of the same requirement.
    #[test]
    fn a_disallowed_namespace_hides_and_blocks() {
        let sc = scope(&["*"], &["peerone"]);
        let entries =
            catalog(&["weather_now", "peerone__weather_now", "peertwo__weather_now"]);

        let listed = effective(&allow_all, &sc, entries.iter());
        assert!(listed.contains("weather_now"));
        assert!(listed.contains("peerone__weather_now"));
        assert!(
            !listed.contains("peertwo__weather_now"),
            "a tool from an unscoped upstream must be invisible"
        );
        assert_eq!(
            decide(&allow_all, &sc, &advertised("peertwo__weather_now")),
            Decision::Deny(DenyReason::NoNamespace),
            "and uncallable, with the diagnosable reason"
        );
    }

    /// A client scoped to a namespace whose upstream is down sees the tool
    /// ABSENT from the catalog (availability removes it upstream of here) —
    /// never present-and-permitted-with-an-error. Modelled by resolving against
    /// a catalog the tool is missing from.
    #[test]
    fn a_down_upstream_yields_absence_not_a_permitted_error() {
        let sc = scope(&["*"], &["peerone"]);
        let up = catalog(&["peerone__weather_now", "weather_now"]);
        let down = catalog(&["weather_now"]);
        assert!(effective(&allow_all, &sc, up.iter()).contains("peerone__weather_now"));
        assert!(!effective(&allow_all, &sc, down.iter()).contains("peerone__weather_now"));
    }

    /// A bare `*` cannot widen past the account grant. This is what makes the
    /// pattern safe at match time independently of RMCP-06's write-time
    /// restriction on authoring one.
    #[test]
    fn a_bare_star_is_still_clamped_by_the_grant() {
        let sc = scope(&["*"], &["peerone"]);
        let narrow = |t: &str| t == "weather_now";
        let entries = catalog(&["weather_now", "media_search", "peerone__x"]);
        let allowed = effective(&narrow, &sc, entries.iter());
        assert_eq!(allowed.len(), 1);
        assert!(allowed.contains("weather_now"));
        assert_eq!(
            decide(&narrow, &sc, &advertised("media_search")),
            Decision::Deny(DenyReason::DeniedByGrant)
        );
    }

    /// Grant is checked first, so the most actionable reason wins when more
    /// than one dimension denies.
    #[test]
    fn denial_reasons_are_machine_readable_and_ordered() {
        let sc = scope(&["weather_*"], &["peerone"]);
        let deny_all = |_: &str| false;
        assert_eq!(
            decide(&deny_all, &sc, &advertised("peertwo__media_search")).deny_code(),
            Some("denied_by_grant")
        );
        assert_eq!(
            decide(&allow_all, &sc, &advertised("peertwo__weather_now")).deny_code(),
            Some("no_namespace")
        );
        assert_eq!(
            decide(&allow_all, &sc, &advertised("media_search")).deny_code(),
            Some("no_group")
        );
        assert_eq!(decide(&allow_all, &sc, &advertised("weather_now")).deny_code(), None);
    }

    /// An account grant that narrows takes effect immediately, because the
    /// grant is never carried inside a `ClientScope`.
    #[test]
    fn a_narrowed_grant_takes_effect_without_touching_the_scope() {
        let sc = scope(&["*"], &[]);
        let entries = catalog(&["weather_now", "media_search"]);
        let before = effective(&allow_all, &sc, entries.iter());
        assert_eq!(before.len(), 2);
        let after = effective(&|t: &str| t == "weather_now", &sc, entries.iter());
        assert_eq!(after.len(), 1);
    }

    #[test]
    fn client_id_is_carried_for_audit() {
        assert_eq!(ClientScope::empty("cid-1").client_id(), "cid-1");
        assert_eq!(scope(&["*"], &[]).client_id(), "a-client-id");
    }

    #[test]
    fn namespaces_are_deduplicated_and_ordered() {
        let sc = scope(&["*"], &["peertwo", "peerone", "peertwo"]);
        let ns: Vec<&str> = sc.namespaces().iter().map(String::as_str).collect();
        assert_eq!(ns, vec!["peerone", "peertwo"]);
    }

    // -- the cache ----------------------------------------------------------

    /// A cache with its OWN epoch counter, so a generation test asserts exact
    /// transitions without racing every other test in the binary that performs
    /// a scoping write. Leaking one counter per test is a few bytes and buys a
    /// guard against a race that is not itself racy.
    fn isolated_cache(ttl: Duration) -> ScopeCache {
        ScopeCache::with_epoch(ttl, leaked_epoch())
    }

    /// A fresh process-independent epoch for one test.
    fn leaked_epoch() -> &'static ScopeEpoch {
        Box::leak(Box::new(ScopeEpoch::new()))
    }

    /// Cache a scope the way a resolve that started just now would.
    fn put_now(cache: &ScopeCache, client_id: &str, scope: Arc<ClientScope>) {
        let observed = cache.generation();
        cache.put_at(client_id, scope, observed);
    }

    /// The acceptance criterion "scoping writes invalidate cached decisions
    /// immediately", asserted on the mechanism itself: after an invalidation
    /// the old permission does not survive, so the next resolution re-reads.
    #[test]
    fn an_invalidation_drops_the_old_permission_immediately() {
        let cache = isolated_cache(Duration::from_secs(3600));
        let permissive = Arc::new(scope(&["*"], &["peerone"]));
        put_now(&cache, "cid", Arc::clone(&permissive));
        assert!(
            cache.get("cid").is_some_and(|s| !s.is_empty()),
            "the permissive scope is cached"
        );

        // A scoping write happened.
        cache.remove("cid");
        assert!(
            cache.get("cid").is_none(),
            "the stale permit must be gone the moment the write lands, not a TTL later"
        );

        // And the narrowed scope is what is served next.
        put_now(&cache, "cid", Arc::new(ClientScope::empty("cid")));
        assert!(cache.get("cid").is_some_and(|s| s.is_empty()));
    }

    /// **Round 1 finding 2 — the race.** A resolve that STARTED before an
    /// invalidation must not be able to repopulate the cache after it.
    ///
    /// The old design invalidated only after the awaited write, so this
    /// interleaving reinserted the stale PERMIT and it lived for a full TTL
    /// with nothing reporting anything wrong. Remove the generation check in
    /// `put_at` and this test fails: the permitting scope becomes visible
    /// again after the revocation.
    #[test]
    fn a_resolve_that_started_before_an_invalidation_cannot_populate() {
        let cache = isolated_cache(Duration::from_secs(3600));
        let permitting = Arc::new(scope(&["*"], &["peerone"]));

        // T0: a resolve begins and reads the epoch, then blocks on the store.
        let observed = cache.generation();

        // T1: a narrowing write lands and invalidates while that load is in
        // flight — the exact moment a revocation is happening.
        cache.remove("cid");

        // T2: the in-flight resolve returns with rows read BEFORE the write and
        // tries to cache them.
        cache.put_at("cid", Arc::clone(&permitting), observed);

        assert!(
            cache.get("cid").is_none(),
            "a scope resolved before a revocation must never become cached after it"
        );
        assert_eq!(
            cache.len(),
            0,
            "and it must not even be inserted — the epoch moved on while it was loading"
        );

        // A resolve that starts AFTER the write caches normally, so the fix
        // closes the race without disabling the cache.
        let fresh = cache.generation();
        cache.put_at("cid", Arc::clone(&permitting), fresh);
        assert!(cache.get("cid").is_some(), "a post-write resolve still caches");
    }

    /// The read-side half of the same guarantee, tested on its own: even if an
    /// entry DID land at a superseded epoch (i.e. if `put_at`'s check were
    /// removed), it is still never served. Remove the generation check in `get`
    /// and this test fails.
    #[test]
    fn a_stale_generation_entry_is_never_served() {
        let cache = isolated_cache(Duration::from_secs(3600));
        let permitting = Arc::new(scope(&["*"], &["peerone"]));
        let stale_generation = cache.generation();

        // A narrowing write moves the epoch on.
        cache.bump();

        // Simulate the entry having been inserted anyway, at the old epoch.
        cache.force_insert_at("cid", permitting, stale_generation);
        assert_eq!(cache.len(), 1, "the entry is physically present");
        assert!(
            cache.get("cid").is_none(),
            "an entry from a superseded epoch must never be served, however fresh its timestamp"
        );
    }

    /// **Round 4 finding — the commit-to-bump window.**
    ///
    /// A post-write-only bump leaves an interval that starts when the write
    /// COMMITS and ends when the bump lands. In it, a plain cache hit finds an
    /// entry whose generation is still current and serves it without touching
    /// the database — so a revocation that has already committed is not yet
    /// enforced. The read-side generation check cannot help: the entry really
    /// was valid a moment ago.
    ///
    /// This interleaves the read at exactly that point: guard begun, write
    /// committed, guard NOT yet dropped. Remove the bump in `ScopeWrite::on`
    /// (the pre-write half) and this test fails — the cached permitting scope
    /// is served after the revocation committed.
    ///
    /// It observes the PRODUCTION path: the guard constructed here is the same
    /// `ScopeWrite` the store's seven write methods hold, with the same
    /// pre-write bump and the same `Drop`. Only the counter is injected, which
    /// is the same technique the cache tests use so a test of an ordering
    /// property is not itself racing every other test in the binary.
    #[test]
    fn a_read_between_the_commit_and_the_post_write_bump_is_not_served() {
        let epoch = leaked_epoch();
        let cache = ScopeCache::with_epoch(Duration::from_secs(3600), epoch);

        // A connector's scope is resolved and cached, permitting a tool.
        let permitting = Arc::new(scope(&["media_*"], &[]));
        put_now(&cache, "cid", Arc::clone(&permitting));
        let hit = cache.get("cid").expect("precondition: the scope is cached");
        assert!(decide(&allow_all, &hit, &advertised("media_search")).is_allowed());

        // An operator revokes it. The store method opens by taking the guard,
        // and the database write then commits...
        let guard = ScopeWrite::on(epoch);

        // ...and HERE, after the commit and before the guard drops, a
        // concurrent request reads the cache. This is the window.
        assert!(
            cache.get("cid").is_none(),
            "a revocation that has already committed must not be served from cache while the \
             write is still in progress"
        );

        // The trailing bump still happens, for the in-flight-resolve case.
        let before_drop = epoch.generation();
        drop(guard);
        assert!(
            epoch.generation() > before_drop,
            "dropping the guard must still advance the epoch"
        );
        assert!(cache.get("cid").is_none());
    }

    /// **Round 5 finding — the commit-to-drop interval.**
    ///
    /// The generation alone says *something changed*; it does not say *a write
    /// is in progress*. So a resolver that began AFTER the pre-write bump, read
    /// the old rows, and returns before `Drop` finds its observed generation
    /// still current and caches a revoked permit.
    ///
    /// This is the exact interleaving: pre-bump, a reader that observes `g+1`
    /// and reads old rows, the commit, then the reader attempting to populate
    /// while the guard is still held. Remove the `writes_in_flight` half of
    /// `ScopeEpoch::may_populate` and this test fails — the generation check on
    /// its own is satisfied, so the stale scope is cached and then served.
    ///
    /// ## How this observes the production path
    /// Every object here is the production one. The guard is `ScopeWrite`, with
    /// the real `begin` ordering and the real `Drop`. The population attempt
    /// goes through `ScopeCache::put_at` — the same method
    /// `ScopeResolver::resolve` calls, with the same `observed` value taken the
    /// same way (`cache.generation()` before the read). The read is asserted
    /// through `cache.get`. Nothing is reimplemented: only the epoch is
    /// injected, so the test does not race the rest of the binary. The mutation
    /// run is the proof — deleting the production check turns this red.
    #[test]
    fn a_read_that_spans_a_write_cannot_populate_before_the_guard_drops() {
        let epoch = leaked_epoch();
        let cache = ScopeCache::with_epoch(Duration::from_secs(3600), epoch);

        // 1. The write begins: the store method takes the guard.
        let guard = ScopeWrite::on(epoch);

        // 2. A resolver misses the cache and observes the CURRENT generation —
        //    `g+1`, already bumped — then reads the OLD rows, because the
        //    transaction has not committed yet.
        let observed = cache.generation();
        let stale_permitting = Arc::new(scope(&["media_*"], &[]));
        assert!(
            decide(&allow_all, &stale_permitting, &advertised("media_search")).is_allowed(),
            "precondition: what the reader read still permits the tool"
        );

        // 3. The write COMMITS. (The commit is in the database; from this
        //    module's point of view nothing observable changes — which is
        //    precisely why the generation cannot detect this moment, and why
        //    the in-flight count has to.)

        // 4. The reader returns and tries to cache what it read, before Drop.
        cache.put_at("cid", Arc::clone(&stale_permitting), observed);

        assert!(
            cache.get("cid").is_none(),
            "a read that spanned a committed write must not be CACHED — it may compute and \
             serve its own answer, but persisting it would serve revoked authority to callers \
             who arrived after the commit"
        );
        assert_eq!(cache.len(), 0, "and it must not be inserted at all");

        // 5. After the guard drops, the entry is still absent, and a fresh
        //    resolution caches normally — the fix excludes population during a
        //    write, it does not disable the cache.
        drop(guard);
        assert!(cache.get("cid").is_none());
        put_now(&cache, "cid", Arc::new(scope(&["weather_*"], &[])));
        assert!(
            cache.get("cid").is_some(),
            "once no write is in flight, resolutions cache again"
        );
    }

    /// Concurrent writers: the exclusion must last until the LAST guard drops.
    /// A boolean instead of a count would let the second writer's drop clear
    /// the first writer's exclusion while its transaction was still open.
    #[test]
    fn concurrent_writes_hold_the_exclusion_until_the_last_one_drops() {
        let epoch = leaked_epoch();
        let cache = ScopeCache::with_epoch(Duration::from_secs(3600), epoch);

        let first = ScopeWrite::on(epoch);
        let second = ScopeWrite::on(epoch);
        assert_eq!(epoch.writes_in_flight(), 2);

        // The second writer finishes; the first is still mid-transaction.
        drop(second);
        assert_eq!(epoch.writes_in_flight(), 1, "one write is still in progress");

        let observed = cache.generation();
        cache.put_at("cid", Arc::new(scope(&["media_*"], &[])), observed);
        assert!(
            cache.get("cid").is_none(),
            "a boolean would have been cleared by the second writer's drop, letting a stale \
             scope be cached while the first writer's transaction was still open"
        );

        drop(first);
        assert_eq!(epoch.writes_in_flight(), 0);
        put_now(&cache, "cid", Arc::new(scope(&["media_*"], &[])));
        assert!(cache.get("cid").is_some());
    }

    /// The guard advances the epoch on both sides, and the production
    /// constructor is wired to the same logic as the injected one.
    #[test]
    fn the_write_guard_bumps_before_and_after() {
        let epoch = leaked_epoch();
        let start = epoch.generation();

        {
            let _guard = ScopeWrite::on(epoch);
            assert_eq!(
                epoch.generation(),
                start + 1,
                "the epoch must advance BEFORE the write, not only after it"
            );
            assert_eq!(epoch.writes_in_flight(), 1, "and the write is marked in progress");
        }
        assert_eq!(epoch.generation(), start + 2, "and again when the guard drops");
        assert_eq!(epoch.writes_in_flight(), 0, "and the write is no longer in progress");

        // The production entry point advances the global epoch the same way.
        // Only the direction is asserted, because that counter is shared with
        // the rest of the test binary and only ever climbs.
        let global_before = scope_generation();
        {
            let _guard = ScopeWrite::begin();
            assert!(scope_generation() > global_before);
        }
        assert!(scope_generation() > global_before + 1);
    }

    /// A guard dropped early — `let _ = ScopeWrite::begin()` — puts both bumps
    /// before the write and loses the trailing invalidation. The source guard
    /// in `store` rejects that binding; this pins why it matters.
    #[test]
    fn dropping_the_guard_early_collapses_both_bumps_to_the_front() {
        let epoch = leaked_epoch();
        let start = epoch.generation();
        let _ = ScopeWrite::on(epoch); // dropped immediately
        assert_eq!(
            epoch.generation(),
            start + 2,
            "both bumps land up front, so nothing invalidates work that lands later"
        );
        assert_eq!(
            epoch.writes_in_flight(),
            0,
            "and the write reads as finished while the database work has not started"
        );
    }

    /// **Round 1 finding 1 — invalidation must not depend on the polite door.**
    ///
    /// A production cache is keyed against the process-global epoch, which the
    /// STORE bumps from inside its own narrowing writes. So a write made
    /// directly against `OauthStore` — bypassing `ScopeResolver` entirely —
    /// still invalidates. This asserts that coupling: the same call the store
    /// makes drops a cached permit from a default cache.
    ///
    /// Only the invalidation DIRECTION is asserted, never "it is still cached",
    /// because the global counter is shared with the rest of the test binary
    /// and only ever climbs — so a concurrent bump can make this test's
    /// assertion more true, never less.
    #[test]
    fn a_store_side_bump_invalidates_a_default_cache() {
        let cache = ScopeCache::new(Duration::from_secs(3600));
        let permitting = Arc::new(scope(&["*"], &["peerone"]));
        put_now(&cache, "cid", permitting);

        // Exactly what `OauthStore::set_client_namespaces` and friends call.
        bump_scope_generation();

        assert!(
            cache.get("cid").is_none(),
            "a store write must invalidate without anyone calling the resolver's mutators"
        );
    }

    /// **Round 2 finding — narrowing a GROUP's patterns is a revocation and
    /// must take effect on the very next call.**
    ///
    /// A group is shared: editing its patterns changes what every client it is
    /// attached to resolves to. Round 1 bumped the epoch inside the five
    /// client-scoping writes but not inside the group-definition writes, so a
    /// scope cached before such an edit kept permitting the removed tools until
    /// the TTL expired.
    ///
    /// The link from `insert_tool_group` (and any future group edit) to this
    /// bump is enforced separately, and by construction, by
    /// `store::tests::every_scope_affecting_write_bumps_the_generation` — there
    /// is no live database in this test binary, so what is asserted HERE is the
    /// half that does not need one: once the bump happens, the previously
    /// cached permit is gone immediately rather than a TTL later.
    #[test]
    fn narrowing_a_groups_patterns_revokes_the_removed_tool_on_the_next_call() {
        let cache = isolated_cache(Duration::from_secs(3600));

        // A client scoped through a group whose patterns cover two families.
        let before = Arc::new(scope(&["weather_*", "media_*"], &[]));
        put_now(&cache, "cid", Arc::clone(&before));

        let cached = cache.get("cid").expect("the scope is cached");
        assert!(
            decide(&allow_all, &cached, &advertised("media_search")).is_allowed(),
            "precondition: the cached scope permits the tool"
        );

        // The operator narrows the GROUP itself, dropping `media_*`. This is
        // the call every scope-affecting store write funnels through.
        cache.bump();

        assert!(
            cache.get("cid").is_none(),
            "the cached scope must not survive a group edit — it still permits a tool the \
             operator just revoked"
        );

        // And what the next call re-resolves to no longer permits it.
        let after = scope(&["weather_*"], &[]);
        assert_eq!(
            decide(&allow_all, &after, &advertised("media_search")).deny_code(),
            Some("no_group"),
            "the re-resolved scope refuses the removed tool"
        );
        assert!(
            decide(&allow_all, &after, &advertised("weather_now")).is_allowed(),
            "and the patterns that survived still work"
        );
    }

    /// Deleting a group outright is the same revocation in its strongest form:
    /// the client is left with nothing.
    #[test]
    fn deleting_a_group_leaves_the_client_reaching_nothing() {
        let cache = isolated_cache(Duration::from_secs(3600));
        put_now(&cache, "cid", Arc::new(scope(&["media_*"], &[])));
        assert!(cache.get("cid").is_some());

        cache.bump();
        assert!(cache.get("cid").is_none());

        // With its only group gone, the client resolves to the empty scope.
        let after = ClientScope::empty("cid");
        assert!(after.is_empty());
        assert!(!decide(&allow_all, &after, &advertised("media_search")).is_allowed());
    }

    /// The epoch only ever moves forward, and every invalidation moves it.
    /// A counter that could repeat a value would let a stale entry become
    /// current again.
    #[test]
    fn the_epoch_is_monotone_and_every_invalidation_advances_it() {
        let cache = isolated_cache(Duration::from_secs(3600));
        let before = cache.generation();
        cache.remove("cid");
        let after_remove = cache.generation();
        assert!(after_remove > before, "a per-client drop advances the epoch");
        cache.clear();
        let after_clear = cache.generation();
        assert!(after_clear > after_remove, "a whole-cache drop advances the epoch");
        assert_eq!(cache.bump(), after_clear + 1, "and the counter only ever climbs");
    }

    /// A namespace delegation change can narrow many clients at once, so the
    /// whole-cache drop must actually drop everything.
    #[test]
    fn invalidate_all_drops_every_client() {
        let cache = isolated_cache(Duration::from_secs(3600));
        for cid in ["a", "b", "c"] {
            put_now(&cache, cid, Arc::new(scope(&["*"], &["peerone"])));
        }
        assert_eq!(cache.len(), 3);
        cache.clear();
        assert_eq!(cache.len(), 0);
        for cid in ["a", "b", "c"] {
            assert!(cache.get(cid).is_none());
        }
    }

    /// The TTL backstop: an expired entry is never served, even though it is
    /// still physically present.
    #[test]
    fn an_expired_entry_is_not_served() {
        let cache = isolated_cache(Duration::ZERO);
        put_now(&cache, "cid", Arc::new(scope(&["*"], &[])));
        assert!(
            cache.get("cid").is_none(),
            "a zero TTL means never serve from cache"
        );
        assert_eq!(cache.len(), 1, "and the entry is present but not honoured");
    }

    /// An endless stream of unknown client ids must not grow the map without
    /// bound — an unauthenticated caller can choose the key.
    #[test]
    fn the_cache_is_bounded() {
        let cache = isolated_cache(Duration::from_secs(3600));
        for i in 0..(MAX_CACHED_CLIENTS * 2) {
            put_now(&cache, &format!("cid-{i}"), Arc::new(ClientScope::empty("x")));
        }
        assert!(
            cache.len() <= MAX_CACHED_CLIENTS,
            "cache grew to {} entries",
            cache.len()
        );
    }

    /// A miss must never be a permit: an absent entry resolves through the
    /// store, and a store failure resolves to the empty scope (see
    /// `ScopeResolver::resolve`). Asserted here for the cache half.
    #[test]
    fn a_cache_miss_is_not_a_permit() {
        let cache = isolated_cache(Duration::from_secs(3600));
        assert!(cache.get("never-inserted").is_none());
    }

    /// A blank or unparseable TTL falls back to the default rather than
    /// disabling the door; a zero is honoured as "never serve from cache".
    #[test]
    fn cache_ttl_parsing_falls_back_safely() {
        let parse = |raw: &str| {
            raw.trim()
                .parse::<u64>()
                .ok()
                .unwrap_or(DEFAULT_CACHE_TTL_SECS)
        };
        assert_eq!(parse("30"), 30);
        assert_eq!(parse("0"), 0);
        assert_eq!(parse(""), DEFAULT_CACHE_TTL_SECS);
        assert_eq!(parse("not-a-number"), DEFAULT_CACHE_TTL_SECS);
        assert_eq!(parse("-1"), DEFAULT_CACHE_TTL_SECS);
    }

    // ── TERM #631 item 5: the binary's wiring ────────────────────────────

    // pii-test-fixture: an invented signing key, an RFC 6761 `.invalid`
    // database host that can never resolve, and an RFC 6761 `.test` connector
    // URI. None of them names a real host, database or credential.
    const WIRE_KEY: &str = "term631-scope-wiring-test-key-32b!!"; // pii-test-fixture
    const WIRE_ISSUER: &str = "https://connector.example.test"; // pii-test-fixture
    const WIRE_RESOURCE: &str = "https://connector.example.test/mcp"; // pii-test-fixture

    fn wire_resource_config() -> crate::oauth::resource::ResourceServerConfig {
        let signer = crate::oauth::jwt::JwtSigner::new(
            WIRE_KEY.to_string(),
            None,
            WIRE_ISSUER.to_string(),
            900,
            30,
        )
        .expect("valid signer");
        crate::oauth::resource::ResourceServerConfig::new(WIRE_RESOURCE, signer)
            .expect("valid resource-server config")
    }

    /// A door built the PRODUCTION way, over a pool that is never connected.
    ///
    /// `connect_lazy_with` performs no I/O and opens no file, so this is
    /// hermetic: it stands up the real `OauthStore` type over a real
    /// `SqlitePool` handle without a database, which is exactly enough to
    /// assert the OWNERSHIP arrangement this item changed. Nothing here issues
    /// a query.
    ///
    /// It does need a tokio runtime, though — the pool is built eagerly (only
    /// the CONNECTING is deferred) and spawns its idle reaper, so its callers
    /// below are `#[tokio::test]`. That is a runtime requirement, not a
    /// filesystem one.
    fn production_shaped_door() -> Arc<crate::oauth::resource::OauthResourceServer> {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_lazy_with(sqlx::sqlite::SqliteConnectOptions::new().in_memory(true));
        let store = Arc::new(OauthStore::from_pool(pool));
        Arc::new(crate::oauth::resource::OauthResourceServer::new(
            wire_resource_config(),
            store,
        ))
    }

    /// THE regression this item exists for: a production-shaped door yields a
    /// scope source, so a binary that derives its state from the door no longer
    /// constructs `scope_resolver: None`.
    ///
    /// Before TERM #631 item 5 this was impossible to express — the door
    /// CONSUMED the store, so there was no handle left to build a resolver
    /// from, and `terminus_primary` hardcoded `None` for exactly that reason.
    #[tokio::test]
    async fn a_production_door_yields_a_scope_source_from_its_own_store() {
        let door = production_shaped_door();
        assert!(
            door.store().is_some(),
            "the production constructor must KEEP its store handle; without it the \
             resolver has nothing to read and the connector reaches no tools"
        );
        assert!(
            scope_source_for_door(Some(&door)).is_some(),
            "a mounted door must produce a scope source"
        );
    }

    /// The store is SHARED, not duplicated: the door and the resolver read the
    /// same handle, so there is one connection budget and one answer to whether
    /// the database is reachable.
    #[tokio::test]
    async fn the_resolver_shares_the_doors_store_rather_than_opening_a_second_one() {
        let door = production_shaped_door();
        let store = door.store().expect("production door owns a store");
        let before = Arc::strong_count(store);
        let source = scope_source_for_door(Some(&door)).expect("a source");
        assert_eq!(
            Arc::strong_count(door.store().expect("still owns a store")),
            before + 1,
            "the resolver must hold a CLONE of the door's handle, not a fresh store"
        );
        drop(source);
    }

    /// Absence is the empty set, in the wiring too.
    ///
    /// Neither branch may manufacture a store or substitute a permissive
    /// source: a closed door has no connector to scope, and a door with no
    /// store cannot read a ceiling, which `mcp_server` refuses on.
    #[test]
    fn a_door_with_no_store_and_no_door_at_all_both_yield_no_source() {
        assert!(
            scope_source_for_door(None).is_none(),
            "a closed door must not fabricate a scope source"
        );

        struct NeverLive;
        #[async_trait::async_trait]
        impl crate::oauth::resource::TokenState for NeverLive {
            async fn active_client_row(
                &self,
                _: &str,
            ) -> Result<Option<Uuid>, ToolError> {
                Ok(None)
            }
            async fn active_account_name(
                &self,
                _: Uuid,
            ) -> Result<Option<String>, ToolError> {
                Ok(None)
            }
            async fn consent_is_live(&self, _: Uuid, _: Uuid) -> Result<bool, ToolError> {
                Ok(false)
            }
            async fn any_session_is_live(&self, _: Uuid, _: Uuid) -> Result<bool, ToolError> {
                Ok(false)
            }
        }

        let storeless = Arc::new(crate::oauth::resource::OauthResourceServer::with_state(
            wire_resource_config(),
            Arc::new(NeverLive),
        ));
        assert!(storeless.store().is_none());
        assert!(
            scope_source_for_door(Some(&storeless)).is_none(),
            "a door with no store must yield no source — which reads as the EMPTY scope, \
             never as an unscoped door"
        );
    }
}
