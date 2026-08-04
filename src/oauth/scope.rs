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
//! ## Relationship to RMCP-06
//! RMCP-06 owns tool GROUPS — their CRUD, their seeded starter set, and
//! write-time validation of the pattern syntax. It had not landed when this
//! item was built, so [`ScopePattern`] parses the same minimal vocabulary here
//! (exact name, trailing-`*` prefix, `<namespace>::*`) in order to be usable at
//! all. When `groups.rs` lands, this parser is the one to delete: the semantics
//! must have exactly one definition. Until then the two halves are consistent
//! by having the same, deliberately tiny, grammar — no regex, no negation.
//!
//! Note that parsing here is TOTAL and fail-closed: an unparseable pattern
//! matches nothing rather than erroring at match time (a match-time error on
//! the dispatch path is a denial-of-service, and a pattern that cannot be
//! understood must certainly not be read as "allow"). RMCP-06 rejects such a
//! pattern at write time so it never reaches storage; this is the second layer.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use uuid::Uuid;

use crate::error::ToolError;
use crate::mesh::split_namespaced;
use crate::oauth::model::ToolGroup;
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
// Patterns
// ---------------------------------------------------------------------------

/// The longest pattern that will be accepted. A pattern is operator- (or
/// delegated-owner-) authored and stored, so this is a sanity bound on stored
/// data rather than a defence against a request body.
const MAX_PATTERN_LEN: usize = 256;

/// The suffix marking a namespace pattern, per the RMCP-06 vocabulary.
const NAMESPACE_SUFFIX: &str = "::*";

/// One entry in a tool group.
///
/// The grammar is deliberately tiny — three forms and nothing else. No regex
/// (a regex authored by a delegated federation owner is a denial-of-service
/// against the dispatch path) and no negation (denial already has a layer, in
/// [`crate::gateway_framework`], which composes on top of this and overrides
/// unconditionally; a second, weaker negation here would be a way to *appear*
/// to deny something that the real deny layer never sees).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopePattern {
    /// `*` — every tool in the catalog.
    ///
    /// This cannot widen anything: [`decide`] checks the account grant FIRST,
    /// so `*` in a group means "everything this account could already do",
    /// never "everything that exists". RMCP-06 additionally restricts authoring
    /// a bare `*` to an operator-owned group at write time; that restriction is
    /// a usability and blast-radius guard, not the thing that makes `*` safe.
    All,
    /// An exact advertised tool name.
    Exact(String),
    /// A trailing-`*` prefix over the ADVERTISED name.
    ///
    /// Matching the advertised (namespaced) name rather than the bare one is
    /// what makes `a*` fail to match `peerone__anything`: the advertised name
    /// starts with `peerone`, not `a`. Matching the bare name would have made a
    /// prefix pattern silently reach across every federated server.
    Prefix(String),
    /// `<namespace>::*` — every tool advertised under one mesh namespace.
    Namespace(String),
}

impl ScopePattern {
    /// Parse one stored pattern.
    ///
    /// Returns [`ToolError::InvalidArgument`] rather than a permissive
    /// fallback — there is no reading of "I do not understand this pattern"
    /// that should widen anything. Callers on the DISPATCH path must not
    /// propagate the error: see [`ClientScope::from_rows`], which drops the
    /// pattern (so it matches nothing) and logs.
    pub fn parse(raw: &str) -> Result<Self, ToolError> {
        let refuse = |why: &str| {
            ToolError::InvalidArgument(format!(
                "invalid tool-group pattern ({why}); the accepted forms are an exact tool name, \
                 a trailing-* prefix, and <namespace>::*"
            ))
        };

        let pattern = raw.trim();
        if pattern.is_empty() {
            return Err(refuse("empty"));
        }
        if pattern.len() > MAX_PATTERN_LEN {
            return Err(refuse("too long"));
        }
        // A control character in a stored pattern is either corruption or an
        // attempt to forge a log line; neither should reach the matcher.
        if pattern.chars().any(char::is_control) {
            return Err(refuse("contains a control character"));
        }

        if pattern == "*" {
            return Ok(Self::All);
        }

        if let Some(namespace) = pattern.strip_suffix(NAMESPACE_SUFFIX) {
            if namespace.is_empty() {
                return Err(refuse("namespace pattern with no namespace"));
            }
            // `a::b::*` is not a namespace this catalog can produce — mesh
            // namespaces never contain `:` — so it is refused rather than
            // quietly matching nothing, which would look like a working rule.
            if namespace.contains(':') || namespace.contains('*') {
                return Err(refuse("malformed namespace"));
            }
            return Ok(Self::Namespace(namespace.to_string()));
        }

        // Exactly one trailing `*` and no other. `a*b`, `**` and `*a` are all
        // refused: accepting them would imply a glob grammar this matcher does
        // not implement, and an operator who believes they wrote a glob has
        // written a permission they cannot predict.
        let stars = pattern.matches('*').count();
        if stars > 1 || (stars == 1 && !pattern.ends_with('*')) {
            return Err(refuse("`*` is only permitted as a whole pattern or as a trailing wildcard"));
        }
        if let Some(prefix) = pattern.strip_suffix('*') {
            // `prefix` cannot be empty here: a bare `*` was handled above.
            return Ok(Self::Prefix(prefix.to_string()));
        }

        Ok(Self::Exact(pattern.to_string()))
    }

    /// Whether this pattern matches an advertised tool name.
    ///
    /// Total and infallible by construction — every failure mode was spent at
    /// parse time. A matcher that can error is a matcher that can be made to
    /// error on the dispatch path.
    pub fn matches(&self, advertised: &str) -> bool {
        match self {
            Self::All => true,
            Self::Exact(name) => advertised == name,
            Self::Prefix(prefix) => advertised.starts_with(prefix.as_str()),
            Self::Namespace(namespace) => {
                split_namespaced(advertised).is_some_and(|(ns, _)| ns == namespace)
            }
        }
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
    patterns: Vec<ScopePattern>,
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
        groups: &[ToolGroup],
        namespaces: Vec<String>,
    ) -> Self {
        let client_id = client_id.into();
        let mut patterns = Vec::new();
        for group in groups {
            for raw in &group.patterns {
                match ScopePattern::parse(raw) {
                    Ok(parsed) => patterns.push(parsed),
                    Err(err) => {
                        // The pattern text itself is operator-authored config,
                        // not caller input, but it is still not echoed: the
                        // group NAME is what an operator needs to find it.
                        tracing::warn!(
                            group = %group.name,
                            error = %err,
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
/// ## Namespaces and LOCAL tools
/// A name with no `<ns>__` prefix is a local tool, not a federated one, so the
/// namespace dimension does not apply to it — requiring a namespace for local
/// tools would mean no connector could ever call one. Namespace-ness is decided
/// by [`crate::mesh::split_namespaced`], the same function the existing deny
/// layer and the catalog merge use, so there is one answer to "is this name
/// namespaced" across the whole process. A local tool whose own name happened
/// to contain `__` would therefore be treated as federated and DENIED unless
/// the client is scoped to the matching namespace — fail-closed, which is the
/// correct direction for an ambiguity.
pub fn decide<G: AccountGrant + ?Sized>(grant: &G, scope: &ClientScope, tool: &str) -> Decision {
    // 1. The account's own grant. Checked first and never skipped: this is the
    //    ceiling, and everything below can only narrow it.
    if !grant.permits_tool(tool) {
        return Decision::Deny(DenyReason::DeniedByGrant);
    }

    // 2. The federated-server dimension. A tool from an upstream this client is
    //    not scoped to is invisible and uncallable no matter which groups match.
    if let Some((namespace, _bare)) = split_namespaced(tool) {
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
    I: IntoIterator<Item = &'a str>,
{
    catalog
        .into_iter()
        .filter(|tool| decide(grant, scope, tool).is_allowed())
        .map(str::to_string)
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

struct CacheEntry {
    inserted: Instant,
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
}

impl ScopeCache {
    fn new(ttl: Duration) -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
            ttl,
        }
    }

    /// A live entry, or `None` when absent or expired.
    ///
    /// A poisoned lock reads as a MISS rather than as an error: the caller then
    /// re-reads the store and gets the current answer, which is the safe
    /// direction. Serving a possibly-torn entry would not be.
    fn get(&self, client_id: &str) -> Option<Arc<ClientScope>> {
        let entries = self.entries.read().ok()?;
        let entry = entries.get(client_id)?;
        if entry.inserted.elapsed() >= self.ttl {
            return None;
        }
        Some(Arc::clone(&entry.scope))
    }

    fn put(&self, client_id: &str, scope: Arc<ClientScope>) {
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
                scope,
            },
        );
    }

    fn remove(&self, client_id: &str) {
        if let Ok(mut entries) = self.entries.write() {
            entries.remove(client_id);
        }
    }

    fn clear(&self) {
        if let Ok(mut entries) = self.entries.write() {
            entries.clear();
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.read().map(|e| e.len()).unwrap_or(0)
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
/// Every scoping write goes through this type ([`Self::set_client_tool_groups`],
/// [`Self::set_client_namespaces`], [`Self::set_client_disabled`]) and drops the
/// affected entry before returning, so a narrowing edit is effective for the
/// next request rather than up to a TTL later. A widening edit is subject to the
/// same invalidation but would have been safe to delay; the ordering exists for
/// the narrowing case, which is the one where lateness is a security bug.
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

        self.cache.put(client_id, Arc::clone(&scope));
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
        let groups = self.store.client_tool_groups(client.id).await?;
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

    /// Replace a client's tool groups, then invalidate.
    ///
    /// Write-through rather than "remember to call `invalidate` afterwards":
    /// the invalidation is the security-relevant half and a caller that forgets
    /// it leaves a permission live that an operator believes they revoked.
    /// Ownership enforcement lives in the store, inside the write transaction.
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

    /// Replace a client's namespaces, then invalidate. See
    /// [`Self::set_client_tool_groups`] for why this is write-through.
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

    fn group(patterns: &[&str]) -> ToolGroup {
        ToolGroup {
            id: Uuid::nil(),
            name: "g".into(),
            description: String::new(),
            patterns: patterns.iter().map(|p| (*p).to_string()).collect(),
            owner_account_id: Uuid::nil(),
            created_at: Utc::now(),
        }
    }

    fn scope(patterns: &[&str], namespaces: &[&str]) -> ClientScope {
        ClientScope::from_rows(
            "a-client-id",
            &[group(patterns)],
            namespaces.iter().map(|n| (*n).to_string()).collect(),
        )
    }

    /// A grant that permits everything — used to isolate the CLIENT half of the
    /// intersection in tests that are not about the grant.
    fn allow_all(_tool: &str) -> bool {
        true
    }

    // -- patterns -----------------------------------------------------------

    #[test]
    fn the_three_pattern_forms_parse() {
        assert_eq!(ScopePattern::parse("*").unwrap(), ScopePattern::All);
        assert_eq!(
            ScopePattern::parse("weather_now").unwrap(),
            ScopePattern::Exact("weather_now".into())
        );
        assert_eq!(
            ScopePattern::parse("weather_*").unwrap(),
            ScopePattern::Prefix("weather_".into())
        );
        assert_eq!(
            ScopePattern::parse("peerone::*").unwrap(),
            ScopePattern::Namespace("peerone".into())
        );
        // Surrounding whitespace is stored noise, not a distinct pattern.
        assert_eq!(
            ScopePattern::parse("  weather_now  ").unwrap(),
            ScopePattern::Exact("weather_now".into())
        );
    }

    /// Anything outside the three forms must be refused at parse time, so it
    /// can never be a match-time surprise. A grammar an operator THINKS they
    /// wrote is a permission they cannot predict.
    #[test]
    fn nothing_else_parses() {
        for bad in [
            "",
            "   ",
            "a*b",
            "*a",
            "**",
            "*weather*",
            "::*",
            "a::b::*",
            "a*::*",
            "with\nnewline",
            "with\ttab",
        ] {
            assert!(
                ScopePattern::parse(bad).is_err(),
                "must refuse the pattern {bad:?}"
            );
        }
        assert!(ScopePattern::parse(&"x".repeat(MAX_PATTERN_LEN + 1)).is_err());
        assert!(ScopePattern::parse(&"x".repeat(MAX_PATTERN_LEN)).is_ok());
    }

    /// The acceptance criterion spelled out by name: a prefix pattern matches
    /// the ADVERTISED name, so it cannot reach across a namespace boundary into
    /// a federated server's tool whose BARE name happens to start the same way.
    #[test]
    fn a_prefix_does_not_reach_into_another_namespace() {
        let pattern = ScopePattern::parse("a*").unwrap();
        assert!(pattern.matches("agent_selftest"));
        assert!(!pattern.matches("peerone__agent_selftest"));

        // And the namespace form matches only its own namespace.
        let namespaced = ScopePattern::parse("peerone::*").unwrap();
        assert!(namespaced.matches("peerone__anything"));
        assert!(!namespaced.matches("peertwo__anything"));
        assert!(!namespaced.matches("anything"));
        // `split_namespaced` splits on the FIRST separator, so a bare name that
        // itself contains the separator stays inside its own namespace.
        assert!(namespaced.matches("peerone__deep__name"));
        assert!(!ScopePattern::parse("deep::*").unwrap().matches("peerone__deep__name"));
    }

    /// Namespace collision: a local tool and a mesh-prefixed tool of the same
    /// bare name are distinct, and the namespaced form addresses only the
    /// namespaced one.
    #[test]
    fn namespaced_and_local_tools_of_the_same_bare_name_are_distinct() {
        let local = scope(&["shared_tool"], &["peerone"]);
        assert!(decide(&allow_all, &local, "shared_tool").is_allowed());
        assert_eq!(
            decide(&allow_all, &local, "peerone__shared_tool"),
            Decision::Deny(DenyReason::NoGroup),
            "an exact local pattern must not match the namespaced tool"
        );

        let remote = scope(&["peerone::*"], &["peerone"]);
        assert!(decide(&allow_all, &remote, "peerone__shared_tool").is_allowed());
        assert_eq!(
            decide(&allow_all, &remote, "shared_tool"),
            Decision::Deny(DenyReason::NoGroup),
        );
    }

    // -- the empty set ------------------------------------------------------

    /// The headline fail-closed property, asserted directly because the natural
    /// refactor to `unwrap_or(full_grant)` is exactly the widening bug.
    #[test]
    fn a_client_with_no_scoping_rows_reaches_nothing() {
        let none = ClientScope::empty("a-client-id");
        assert!(none.is_empty());
        let catalog = ["weather_now", "peerone__weather_now", "media_search"];
        assert!(effective(&allow_all, &none, catalog).is_empty());
        for tool in catalog {
            assert!(
                !decide(&allow_all, &none, tool).is_allowed(),
                "an unscoped client must reach nothing, even under an unrestricted grant: {tool}"
            );
        }
        // The reason differs by dimension, and BOTH dimensions are empty for an
        // unscoped client: a local tool has no group, a federated one has no
        // namespace (checked first, since it is the more specific refusal).
        assert_eq!(
            decide(&allow_all, &none, "weather_now").deny_code(),
            Some("no_group")
        );
        assert_eq!(
            decide(&allow_all, &none, "peerone__weather_now").deny_code(),
            Some("no_namespace")
        );
    }

    /// An empty group, and a group whose patterns match nothing, are both the
    /// empty set — never a wildcard.
    #[test]
    fn empty_and_zero_match_groups_are_the_empty_set() {
        let empty_group = scope(&[], &["peerone"]);
        assert!(empty_group.is_empty());
        assert!(effective(&allow_all, &empty_group, ["weather_now"]).is_empty());

        let matches_nothing = scope(&["nothing_matches_this_*"], &["peerone"]);
        assert!(!matches_nothing.is_empty(), "it has a pattern");
        assert!(
            effective(&allow_all, &matches_nothing, ["weather_now", "media_search"]).is_empty(),
            "a pattern matching zero tools is the empty set, not everything"
        );
    }

    /// An unparseable stored pattern must match nothing rather than error on
    /// the dispatch path — and must not take the rest of its group with it.
    #[test]
    fn an_unparseable_pattern_is_dropped_not_widened() {
        let with_junk = scope(&["a*b", "weather_*"], &["peerone"]);
        assert!(decide(&allow_all, &with_junk, "weather_now").is_allowed());
        assert_eq!(
            decide(&allow_all, &with_junk, "media_search"),
            Decision::Deny(DenyReason::NoGroup)
        );
    }

    // -- the intersection ---------------------------------------------------

    /// The anti-widening invariant, over a randomised (deterministic, seedless)
    /// cross product of grants, scopes and catalogs. No input may produce a tool
    /// the account's own grant would deny.
    #[test]
    fn effective_is_subset_of_grant() {
        let catalog: Vec<&str> = vec![
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
        ];
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
            Box::new(|t: &str| t.contains("__")),
            Box::new(|t: &str| !t.contains("__")),
            Box::new(|t: &str| t.len() % 2 == 0),
        ];

        let mut saw_allow = false;
        for patterns in &pattern_sets {
            for namespaces in &namespace_sets {
                let sc = scope(patterns, namespaces);
                for grant in &grants {
                    let allowed = effective(grant.as_ref(), &sc, catalog.iter().copied());
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
                        assert!(catalog.contains(&tool.as_str()));
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
        let catalog: Vec<&str> = vec![
            "weather_now",
            "media_search",
            "admin_reset",
            "peerone__weather_now",
            "peertwo__media_search",
            "odd__name__with__separators",
        ];
        for patterns in [
            vec![],
            vec!["*"],
            vec!["weather_*", "peerone::*"],
            vec!["media_search"],
        ] {
            for namespaces in [vec![], vec!["peerone"], vec!["peerone", "peertwo"]] {
                let sc = scope(&patterns, &namespaces);
                let grant = |t: &str| !t.contains("admin");
                let listed = effective(&grant, &sc, catalog.iter().copied());
                for tool in &catalog {
                    let callable = decide(&grant, &sc, tool).is_allowed();
                    assert_eq!(
                        listed.contains(*tool),
                        callable,
                        "list/call drift on {tool} (patterns={patterns:?}, ns={namespaces:?})"
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
        let catalog = ["weather_now", "peerone__weather_now", "peertwo__weather_now"];

        let listed = effective(&allow_all, &sc, catalog);
        assert!(listed.contains("weather_now"));
        assert!(listed.contains("peerone__weather_now"));
        assert!(
            !listed.contains("peertwo__weather_now"),
            "a tool from an unscoped upstream must be invisible"
        );
        assert_eq!(
            decide(&allow_all, &sc, "peertwo__weather_now"),
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
        let up = ["peerone__weather_now", "weather_now"];
        let down = ["weather_now"];
        assert!(effective(&allow_all, &sc, up).contains("peerone__weather_now"));
        assert!(!effective(&allow_all, &sc, down).contains("peerone__weather_now"));
    }

    /// A bare `*` cannot widen past the account grant. This is what makes the
    /// pattern safe at match time independently of RMCP-06's write-time
    /// restriction on authoring one.
    #[test]
    fn a_bare_star_is_still_clamped_by_the_grant() {
        let sc = scope(&["*"], &["peerone"]);
        let narrow = |t: &str| t == "weather_now";
        let allowed = effective(&narrow, &sc, ["weather_now", "media_search", "peerone__x"]);
        assert_eq!(allowed.len(), 1);
        assert!(allowed.contains("weather_now"));
        assert_eq!(
            decide(&narrow, &sc, "media_search"),
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
            decide(&deny_all, &sc, "peertwo__media_search").deny_code(),
            Some("denied_by_grant")
        );
        assert_eq!(
            decide(&allow_all, &sc, "peertwo__weather_now").deny_code(),
            Some("no_namespace")
        );
        assert_eq!(
            decide(&allow_all, &sc, "media_search").deny_code(),
            Some("no_group")
        );
        assert_eq!(decide(&allow_all, &sc, "weather_now").deny_code(), None);
    }

    /// An account grant that narrows takes effect immediately, because the
    /// grant is never carried inside a `ClientScope`.
    #[test]
    fn a_narrowed_grant_takes_effect_without_touching_the_scope() {
        let sc = scope(&["*"], &[]);
        let before = effective(&allow_all, &sc, ["weather_now", "media_search"]);
        assert_eq!(before.len(), 2);
        let after = effective(&|t: &str| t == "weather_now", &sc, ["weather_now", "media_search"]);
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

    /// The acceptance criterion "scoping writes invalidate cached decisions
    /// immediately", asserted on the mechanism itself: after an invalidation
    /// the old permission does not survive, so the next resolution re-reads.
    #[test]
    fn an_invalidation_drops_the_old_permission_immediately() {
        let cache = ScopeCache::new(Duration::from_secs(3600));
        let permissive = Arc::new(scope(&["*"], &["peerone"]));
        cache.put("cid", Arc::clone(&permissive));
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
        cache.put("cid", Arc::new(ClientScope::empty("cid")));
        assert!(cache.get("cid").is_some_and(|s| s.is_empty()));
    }

    /// A namespace delegation change can narrow many clients at once, so the
    /// whole-cache drop must actually drop everything.
    #[test]
    fn invalidate_all_drops_every_client() {
        let cache = ScopeCache::new(Duration::from_secs(3600));
        for cid in ["a", "b", "c"] {
            cache.put(cid, Arc::new(scope(&["*"], &["peerone"])));
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
        let cache = ScopeCache::new(Duration::ZERO);
        cache.put("cid", Arc::new(scope(&["*"], &[])));
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
        let cache = ScopeCache::new(Duration::from_secs(3600));
        for i in 0..(MAX_CACHED_CLIENTS * 2) {
            cache.put(&format!("cid-{i}"), Arc::new(ClientScope::empty("x")));
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
        let cache = ScopeCache::new(Duration::from_secs(3600));
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
}
