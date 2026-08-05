//! Tool groups — the named, operator-managed grouping over the tool catalog
//! that makes connector scoping expressible in human terms.
//!
//! ## Why a group is not just a list of tool names
//! The fleet exports several hundred tools. Asking an operator to enumerate
//! them when minting a connector guarantees one of two outcomes: a
//! hand-authored list that goes stale the day a tool is added, or a shrug and a
//! wildcard. A group ("media", "home automation") is a small set of PATTERNS,
//! resolved against the LIVE merged catalog rather than expanded once and
//! stored — so a newly registered tool matching an existing pattern is included
//! without a config edit, and no pattern is ever frozen into a snapshot that
//! quietly diverges from what the server actually serves.
//!
//! **Where that resolution runs today is a separate question, and the answer is
//! "not here yet".** See "Status" below before reading any of this as a
//! description of what currently authorizes a request.
//!
//! ## Three rules this module exists to enforce
//!
//! **1. The syntax is deliberately minimal.** An exact name, a trailing-`*`
//! prefix, or a namespace form
//! (`<namespace>::*`, delimiter [`PATTERN_NS_SEP`] — deliberately NOT the `__`
//! that separates the halves of an advertised name; see [`Pattern::parse`]).
//! Nothing else parses. No regex: a pattern here may be authored by a DELEGATED
//! federation user (RMCP-12), and a regex from an untrusted author is a
//! denial-of-service against the dispatch path, which is where this matcher is
//! designed to run — once per request, per pattern. No negation either — negation is the existing deny layer's job
//! ([`crate::gateway_framework::DEFAULT_SENSITIVE_DENY_PREFIXES`]), and a second
//! subtractive mechanism in a different file is how two authorization systems
//! come to disagree.
//!
//! **2. Validation happens at WRITE time; matching is pure and TOTAL.**
//! [`Pattern::parse`] can fail; [`Pattern::matches`] cannot. Matching runs on
//! the dispatch path, where an error is not a safety property but an
//! availability failure in the authorization system — a tool call that returns
//! "your group's pattern is malformed" is an outage with extra steps. So a
//! malformed pattern is refused when it is stored, and every value that reaches
//! the matcher is one that already parsed.
//!
//! **3. Nothing empty ever means "everything".** An empty group, and a pattern
//! that happens to match no tool in the current catalog, both resolve to the
//! EMPTY set. This is the single most important invariant in the item and is
//! asserted directly by tests below, because "empty means unrestricted" is the
//! intuitive reading and the catastrophic one — it is the shape of every
//! authorization bug where a scoping record that failed to load silently became
//! full access.
//!
//! ## The namespace boundary, and the two separators
//!
//! **The rule, in one sentence covering all three pattern kinds: an unqualified
//! EXACT or PREFIX pattern matches local (unqualified) tools only; a
//! namespace-qualified pattern (`<ns>::*`, `<ns>::<prefix>*`) matches only
//! within the namespace it names; and the bare `*` matches the whole merged
//! catalog, local and federated alike, bounded by the client's allowed
//! namespaces at RMCP-07's intersection rather than by this matcher.**
//!
//! The boundary exists because a bare prefix is letters, and letters collide:
//! `peer*` cannot be allowed to reach `peerhub__alerts_list` merely because a
//! namespace shares its opening characters. That is a widening the author
//! cannot see in what they wrote, so absence of a qualifier means "local only",
//! never "anything that starts this way" — the same fail-closed reading of
//! absence this module applies everywhere else.
//!
//! **Which side of the boundary a tool is on is read from the catalog, never
//! inferred from its name.** [`Pattern::matches`] takes a [`CatalogTool`] and
//! consults [`CatalogTool::namespace`]; a name is a string and can lie about its
//! provenance, and round 7 found both directions in which it did.
//!
//! **`*` is deliberately NOT local-only**, and the distinction is not an
//! inconsistency. `*` has no letters, so there is no coincidence to fall foul
//! of and no near-miss to mistake for a hit: an author who writes it has said
//! "everything" and can mean nothing else. It is also the most heavily gated
//! pattern here — operator-only at write time, and re-derived against the
//! owner's CURRENT state on every resolution — so making it the one pattern
//! that could not reach a federated tool would leave the strongest-gated shape
//! weaker than shapes with fewer gates. It is what makes the namespace
//! dimension of RMCP-07's intersection do real work: `namespaces(client)` is
//! the thing that bounds a broad group, and if `*` stopped at the local
//! registry that dimension would constrain only patterns that already name
//! their own namespace. And operationally, "this connector gets what I get"
//! has to be expressible without enumerating every upstream — otherwise it
//! silently under-grants the moment a new one is federated, which is precisely
//! the hand-authoring this item exists to remove.
//!
//! [`Pattern::matches`] carries the per-arm reasoning.
//!
//! RMCP-07 used to hold a SECOND copy of this matcher in its `scope.rs`, and
//! that copy was the one wired into `decide()`. The two used DIFFERENT
//! namespace delimiters — this one `__`, that one `::` — which meant every
//! namespace-qualified pattern written here resolved to nothing there, while a
//! `::` pattern written here passed validation as an innocuous local prefix and
//! was expanded by the enforcer into a whole federated namespace. That is TERM
//! #637. Standardising on `::` closed the vocabulary half; TERM #643 was the
//! second instance of the same divergence (the copy still inferred provenance
//! from a name's shape after this one had stopped), and the copy is now
//! DELETED. `decide()` matches with [`Pattern::matches`] — this one.
//!
//! ## Status: authored here, and enforced with this matcher
//!
//! Scoped to THIS MODULE. The assembled subsystem's wiring state — which
//! endpoints are mounted, what a connector can actually reach — has one account
//! and it is not this one: see *Exactly what is wired today* in the README. It
//! already records that scope resolution is not wired into `terminus_primary`,
//! so nothing below should be read as describing a live authorization path.
//!
//! **What this module does today:** groups are authored, validated and stored
//! through it, with every semantic below applying at WRITE time.
//!
//! **What the ENFORCEMENT path uses (TERM #637, done):**
//! [`Pattern::parse_stored`] and [`Pattern::matches`], through
//! [`crate::oauth::scope::ClientScope::from_rows`] and
//! [`crate::oauth::scope::decide`]. The pattern semantics below therefore
//! govern a dispatch as well as a write. What is still authoring-only is the
//! resolve-a-whole-catalog entry point: [`resolve`] and [`resolve_groups`] have
//! no caller on the request path, because `decide()` asks about ONE tool at a
//! time and applies two further dimensions ([`crate::oauth::scope`]'s
//! intersection) that this module deliberately knows nothing about.
//!
//! **There is still exactly one enforcement SITE.** [`crate::oauth::scope::decide`]
//! backs both the list filter and the call guard, and nothing here is wired
//! into `tools/list` or `tools/call` directly. Two authorization sites over one
//! decision is how they come to disagree — silently, in the widening direction
//! — which is the failure TERM #637 documented. The collapse removed a
//! duplicated MATCHER; it did not add a second door.
//!
//! ## Where authority comes from
//! [`GroupOwner`] is an input to PURE validation here; it is not a claim a
//! caller gets to make. The store derives it from the authoring account's own
//! row inside the transaction that performs the write — see
//! [`crate::oauth::store::OauthStore::insert_tool_group`]. An earlier revision
//! took it as a store parameter, which made the delegated-wildcard rule
//! advisory: a caller that passed [`GroupOwner::Operator`] stored a `*` that the
//! read path then honoured for the life of the row.
//!
//! ## The general rule, for RMCP-12 and anything else that delegates
//! **A write-time authorization check is point-in-time. Any authority that can
//! be REVOKED must be re-derived on the read path.** Checking at write time
//! answers "were you allowed to write this?"; it says nothing about "are you
//! still allowed to have it?", and the gap between those two is permanent.
//!
//! This item pays that twice over: the store derives operator-ness from
//! `rmcp_account` when a group is WRITTEN, and [`resolve_groups`] re-derives it
//! when the group is READ, so a `*` written by an operator who was later
//! demoted expands to nothing. RMCP-01 learned the same lesson on group and
//! namespace ownership — the namespace case being the sharper one, since
//! clearing a delegation would otherwise leave a connector reaching an entire
//! federated server.
//!
//! RMCP-12 layers more revocable delegation on top of all of this. Every
//! authority it introduces should be assumed revocable and read at the point of
//! use, not cached into a row and trusted afterwards.

use std::collections::btree_map::Entry;
use std::collections::BTreeMap;

use sqlx::FromRow;

use crate::error::ToolError;
use crate::mesh::merge::{split_namespaced, MESH_NS_SEP};
use crate::oauth::delegation::{owner_may_hold, Authoring, PatternShape};

/// The delimiter between a namespace and a tool WITHIN A PATTERN.
///
/// Deliberately NOT [`MESH_NS_SEP`], which separates the two halves of an
/// advertised NAME. Keeping them distinct is what makes `a::b__*` unambiguous;
/// see the grammar on [`Pattern::parse`] for the full reasoning, and TERM #637
/// for the divergence that settled it.
pub const PATTERN_NS_SEP: &str = "::";
use crate::oauth::model::ToolGroup;

/// Longest accepted group name, in CHARS.
///
/// Counted in chars rather than bytes so the bound means the same thing for a
/// name written in any script; [`MAX_GROUP_NAME_BYTES`] separately caps the
/// encoded size, since a char bound alone permits a four-times-larger row than
/// it appears to.
pub const MAX_GROUP_NAME_CHARS: usize = 64;

/// Hard cap on a group name's encoded size, defending the storage layer
/// independently of the char count.
pub const MAX_GROUP_NAME_BYTES: usize = 256;

/// Longest accepted description, in chars. Descriptions are shown on the
/// consent screen; this is a label, not a document.
pub const MAX_DESCRIPTION_CHARS: usize = 512;

/// Longest accepted single pattern. Real tool names are far shorter; the bound
/// exists so a stored pattern list has a knowable worst case.
pub const MAX_PATTERN_CHARS: usize = 96;

/// Most patterns one group may hold.
///
/// Resolution costs one pass over the catalog per pattern, so this is what
/// makes the cost of a group BOUNDED rather than something a delegated author
/// can grow without limit. See [`resolve`].
pub const MAX_PATTERNS_PER_GROUP: usize = 128;

/// Most tool groups one client may be scoped to.
///
/// The per-group cap alone bounds nothing at the point that matters:
/// [`resolve_groups`] concatenates the patterns of EVERY group a client holds,
/// and [`resolve`] walks that whole list once per catalog tool, so the real cost
/// is `tools x total_patterns` and the group count is the unbounded factor.
/// RMCP-07 caches resolutions, but a cache MISS still pays the full cost and
/// scoping writes invalidate the cache deliberately — so "the cache absorbs it"
/// is not an answer; an operator with very many groups would make the first
/// resolution after every scope edit expensive, on the request path.
///
/// Capping the group count is the cheaper half of the fix: it is checked once at
/// write time, where the operator is present to read the error, rather than on
/// every dispatch.
pub const MAX_GROUPS_PER_CLIENT: usize = 32;

/// Hard ceiling on the patterns [`resolve_groups`] will consider for one client.
///
/// Exactly the product of the two write-time caps, so a client scoped within
/// them can never trip it. It exists because the write-time caps are
/// POINT-IN-TIME — rows can predate a cap, and a cap can be lowered — which is
/// the same reasoning that made this item re-derive operator authority on the
/// read path rather than trust the write check.
pub const MAX_RESOLVED_PATTERNS: usize = MAX_GROUPS_PER_CLIENT * MAX_PATTERNS_PER_GROUP;

/// Who is authoring a group, for the one rule that depends on it.
///
/// Not a general permission model — RMCP-12 owns delegation. This exists
/// because a bare `*` is the one pattern whose meaning changes with the author:
/// an operator writing "everything" is stating a policy, whereas a delegated
/// federation user writing it is granting themselves the whole fleet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupOwner {
    /// The fleet operator.
    Operator,
    /// A delegated account — a federated namespace's administrator, or any
    /// account that is not the operator.
    Delegated,
}

/// One tool as it appears in the merged catalog being resolved against.
///
/// `name` is the ADVERTISED name — already namespaced for a federated tool, per
/// [`crate::mesh::merge::namespaced`] — because that is the name a caller
/// actually invokes, and therefore the only name a grant can honestly be
/// written against. `namespace` records where the entry came from, which
/// matters solely for the collision rule in [`resolve`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogTool {
    pub name: String,
    /// `Some` when this entry was contributed by a mesh upstream.
    pub namespace: Option<String>,
}

impl CatalogTool {
    /// A locally registered tool.
    pub fn local(name: impl Into<String>) -> Self {
        Self { name: name.into(), namespace: None }
    }

    /// This tool's name WITHIN its own namespace — what a qualified pattern's
    /// `bare` half is compared against.
    ///
    /// For a local tool that is simply its name. For a federated one it is the
    /// advertised name with its `<namespace>__` prefix removed. A federated
    /// entry whose name does not carry that prefix is MALFORMED (nothing built
    /// through [`Self::from_upstream`] can be), and yields the full name — which
    /// no bare pattern will equal, so the failure direction is a non-match.
    pub fn bare_name(&self) -> &str {
        match &self.namespace {
            Some(namespace) => self
                .name
                .strip_prefix(namespace.as_str())
                .and_then(|rest| rest.strip_prefix(MESH_NS_SEP))
                .unwrap_or(&self.name),
            None => &self.name,
        }
    }

    /// A tool contributed by mesh upstream `namespace`, under its advertised
    /// (namespaced) name — built through [`crate::mesh::merge::namespaced`] so
    /// this cannot drift from how the merge layer names things.
    pub fn from_upstream(namespace: &str, bare: &str) -> Self {
        Self {
            name: crate::mesh::merge::namespaced(namespace, bare),
            namespace: Some(namespace.to_string()),
        }
    }
}

/// A parsed pattern. Constructing one is the only way to get a matcher, which
/// is what makes "validated at write time" structural rather than a convention.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pattern {
    /// `*` — every tool in the merged catalog, LOCAL AND FEDERATED.
    ///
    /// Operator-only at write time (see [`GroupOwner`]) and re-checked against
    /// the owner's current state on every resolution. It is not subject to the
    /// local-only rule that governs unqualified exact and prefix patterns: what
    /// bounds it is the client's allowed namespaces at RMCP-07's intersection,
    /// not this matcher.
    Everything,
    /// `<namespace>::*` — every tool advertised by one mesh upstream.
    Namespace(String),
    /// `<namespace>::<prefix>*` — tools from ONE upstream whose bare name
    /// starts with `prefix`. Reaching into a namespace requires naming it.
    NamespacedPrefix { namespace: String, prefix: String },
    /// `<prefix>*` with no `::` — every LOCAL (unqualified) tool whose
    /// name starts with `prefix`.
    ///
    /// Local only. See [`Pattern::matches`] for why an unqualified prefix must
    /// not be allowed to span the mesh separator.
    Prefix(String),
    /// `<namespace>::<bare>` — one federated tool, exactly. Holds the two
    /// halves separately (as RMCP-07's enforcing matcher does), because the
    /// namespace must be compared against the catalog entry's PROVENANCE, not
    /// recovered by splitting its name.
    NamespacedExact { namespace: String, bare: String },
    /// One exact LOCAL tool name.
    Exact(String),
}

impl Pattern {
    /// Parse and authorize one pattern for `owner`.
    ///
    /// Fails on anything the three accepted shapes do not cover. The error text
    /// names the offending shape and what is accepted, because the operator
    /// reading it is mid-edit and the alternative — a silently dropped pattern
    /// — is a group that quietly grants less (or, with the wrong default, more)
    /// than it appears to.
    pub fn parse(raw: &str, owner: GroupOwner) -> Result<Self, ToolError> {
        let parsed = Self::parse_syntax_checked(raw)?;
        // The SHAPE rule, and the only place it is spelled out for authoring:
        // `crate::oauth::delegation::owner_may_hold`. RMCP-12 widened it from
        // "a delegated author may not write `*`" to "a delegated author may
        // write ONLY namespace-qualified patterns", because an unqualified
        // pattern addresses the LOCAL namespace — the fleet's own tools — which
        // no delegated account owns and which no client-side namespace row ever
        // bounds (`decide` applies the namespace dimension only to namespaced
        // NAMES). The same function runs again on every resolution, so a demoted
        // author's stored local patterns stop resolving rather than living on.
        if !owner_may_hold(owner, parsed.shape()) {
            return Err(ToolError::InvalidArgument(match parsed.shape() {
                PatternShape::Everything => "the bare `*` pattern is reserved for \
                     operator-owned groups; name the namespaces this group needs instead"
                    .to_string(),
                _ => "a group owned by a delegated account may contain only \
                     namespace-qualified patterns (`<server>::*`, `<server>::<prefix>*`, \
                     `<server>::<tool>`); an unqualified pattern addresses the fleet's own \
                     tools, which belong to the operator"
                    .to_string(),
            }));
        }
        Ok(parsed)
    }

    /// This pattern reduced to the shape delegation reasons about.
    ///
    /// The mapping is exhaustive by construction (no `_` arm), so a pattern
    /// form added later cannot silently default into the permissive class.
    pub fn shape(&self) -> PatternShape<'_> {
        match self {
            Pattern::Everything => PatternShape::Everything,
            Pattern::Namespace(namespace)
            | Pattern::NamespacedPrefix { namespace, .. }
            | Pattern::NamespacedExact { namespace, .. } => PatternShape::Namespaced(namespace),
            Pattern::Prefix(_) | Pattern::Exact(_) => PatternShape::Local,
        }
    }

    /// Parse a pattern READ BACK FROM STORAGE: syntax only, no error, and no
    /// authority decision.
    ///
    /// There is no error because this is on the dispatch path (rule 2 in the
    /// module docs); an unparseable stored row yields `None` and is then SKIPPED
    /// by [`resolve_groups`], which matches nothing — the fail-closed direction.
    /// A row that cannot be understood must never widen anything.
    ///
    /// **It does not re-check `*` authority, and must not be "simplified" into
    /// doing so.** Not because the check is unnecessary — it is REQUIRED, and
    /// round 2 of review was right that a write-time check alone is not enough
    /// — but because this function is a pure parser with no database handle,
    /// and the authority it would need is a live property of the owning account
    /// that only a query can answer. Inventing an answer here is how the check
    /// would come to be wrong.
    ///
    /// The authority check therefore lives one level up, in
    /// [`resolve_groups`], which takes each group's owner authority as read from
    /// `rmcp_account` at resolution time (see
    /// [`crate::oauth::store::OauthStore::client_authorized_groups`]). A `*`
    /// whose owner is not CURRENTLY an enabled operator expands to nothing.
    pub fn parse_stored(raw: &str) -> Option<Self> {
        Self::parse_syntax_checked(raw).ok()
    }

    /// The shared syntax check, and the one place the GRAMMAR is enforced.
    ///
    /// ```text
    /// pattern   := "*"                        -- Everything (operator-only)
    ///            | namespace "::" "*"         -- Namespace
    ///            | namespace "::" bare "*"    -- NamespacedPrefix
    ///            | namespace "::" bare        -- Exact, qualified
    ///            | local "*"                  -- Prefix, LOCAL ONLY
    ///            | local                      -- Exact, local
    ///
    /// namespace := ASCII-graphic+, no "*", no "::", and must round-trip through
    ///              `split_namespaced` (so: no "__" inside it, no trailing "_")
    /// bare      := ASCII-graphic+, no "*", no "::"
    ///              (it MAY contain and even end with "__" — see below)
    /// local     := ASCII-graphic+, no "*", no "::", no "__"
    /// ```
    ///
    /// ## Two separators, and why they are different characters
    /// `::` delimits a PATTERN; `__` ([`MESH_NS_SEP`]) separates an ADVERTISED
    /// NAME. They are deliberately not the same character, and this is the
    /// second vocabulary this item has had.
    ///
    /// The first used `__` for both, matching MESH-08's allow-entry syntax. That
    /// looked like the conservative choice — reuse the vocabulary rather than
    /// invent one — but it made `a__b__*` genuinely ambiguous: namespace `a__b`,
    /// or namespace `a` with bare prefix `b__`? Both readings are reasonable,
    /// two careful readers reached different ones, and the tie could only be
    /// broken by declaring a precedence rule. A pattern whose meaning depends on
    /// a precedence rule is a pattern an author cannot read.
    ///
    /// A distinct delimiter dissolves it rather than adjudicating it. `a::b__*`
    /// is namespace `a`, bare prefix `b__`, and there is no second reading — so
    /// a bare name may contain `__`, and may even END with it, harmlessly. The
    /// precedence rule is not documented here because it no longer needs to
    /// exist. (See also TERM #637: the enforcing matcher in RMCP-07 already used
    /// `::`, so this also makes the two agree.)
    ///
    /// ## `__` in an unqualified pattern is REFUSED, not reinterpreted
    /// A local pattern containing `__` can never match anything: any advertised
    /// name carrying `__` splits into a namespace, and is therefore not local.
    /// It is also exactly what a pattern written under the OLD vocabulary looks
    /// like (`peerhub__*`). Silently treating it as an unmatchable local prefix
    /// would turn a previously-working scope into one that grants nothing, with
    /// no error to explain it — so it is refused, and the error names the `::`
    /// form it should have been.
    ///
    /// Rejected, exhaustively: the empty pattern; anything over
    /// [`MAX_PATTERN_CHARS`]; any non-ASCII-graphic character (whitespace,
    /// control bytes, homoglyphs); a `*` anywhere but as the single FINAL
    /// character; `__` in an unqualified pattern; an empty or repeated `::`
    /// section; and any namespace that cannot round-trip.
    ///
    /// ## Why every rejection is a rejection
    /// Each one is a shape that would otherwise parse to something the author
    /// did not write. The direction is usually UNDER-granting — a pattern that
    /// silently matches nothing, so a connector is quietly missing tools with no
    /// error to explain it — but "means something other than what was written"
    /// is the defect either way.
    fn parse_syntax_checked(raw: &str) -> Result<Self, ToolError> {
        // ---- whole-string checks, before any shape is considered ----
        if raw.is_empty() {
            return Err(ToolError::InvalidArgument("an empty pattern matches nothing; remove it".into()));
        }
        if raw.chars().count() > MAX_PATTERN_CHARS {
            return Err(ToolError::InvalidArgument(format!(
                "a pattern may be at most {MAX_PATTERN_CHARS} characters"
            )));
        }
        // Tool names are ASCII graphic throughout the registry, so anything else
        // — whitespace, a control byte, a homoglyph from another script — can
        // never match a real tool. Accepting it would store a pattern that
        // silently matches nothing while reading, to whoever authored it, as
        // though it does something.
        if !raw.chars().all(|c| c.is_ascii_graphic()) {
            return Err(ToolError::InvalidArgument(
                "a pattern may contain only printable ASCII (no spaces, control \
                 characters, or non-ASCII text) — it must be able to match a real tool name"
                    .into(),
            ));
        }
        // `*` is understood ONLY as the single trailing character. Counted over
        // the WHOLE string and decided before any shape is chosen, so a leading
        // (`*weather`) or interior (`weather*foo`, `**`) star is refused as such
        // rather than falling through to some other branch and being accepted as
        // an exact name no tool could ever have.
        let stars = raw.matches('*').count();
        let trailing_star = raw.ends_with('*');
        if stars > 1 || (stars == 1 && !trailing_star) {
            return Err(ToolError::InvalidArgument(
                "`*` is only meaningful as the LAST character of a pattern, and only once; \
                 there is no general glob, suffix, or regex syntax here"
                    .into(),
            ));
        }
        if raw == "*" {
            return Ok(Pattern::Everything);
        }

        // ---- qualified vs local, decided on the PATTERN delimiter ----
        let Some((namespace, rest)) = raw.split_once(PATTERN_NS_SEP) else {
            return Self::parse_local(raw, trailing_star);
        };
        check_namespace(namespace)?;
        if rest.contains(PATTERN_NS_SEP) {
            return Err(ToolError::InvalidArgument(format!(
                "a pattern names at most one namespace: `{PATTERN_NS_SEP}` may appear once"
            )));
        }
        if rest.is_empty() {
            return Err(ToolError::InvalidArgument(format!(
                "`{namespace}{PATTERN_NS_SEP}` names a namespace but no tool; write \
                 `{namespace}{PATTERN_NS_SEP}*` for all of it"
            )));
        }
        if !trailing_star {
            return Ok(Pattern::NamespacedExact {
                namespace: namespace.to_string(),
                bare: rest.to_string(),
            });
        }
        let prefix = &rest[..rest.len() - 1];
        if prefix.is_empty() {
            return Ok(Pattern::Namespace(namespace.to_string()));
        }
        Ok(Pattern::NamespacedPrefix {
            namespace: namespace.to_string(),
            prefix: prefix.to_string(),
        })
    }

    /// An unqualified pattern: local tools only.
    fn parse_local(raw: &str, trailing_star: bool) -> Result<Self, ToolError> {
        // The old-vocabulary migration guard, and the unmatchable-pattern rule,
        // are the same check: see the `__` note in `parse_syntax_checked`.
        if raw.contains(MESH_NS_SEP) {
            let suggestion = raw.replacen(MESH_NS_SEP, PATTERN_NS_SEP, 1);
            return Err(ToolError::InvalidArgument(format!(
                "`{MESH_NS_SEP}` separates an advertised NAME, not a pattern, and no local tool \
                 name contains it — to name an upstream write `{suggestion}`"
            )));
        }
        let head = if trailing_star { &raw[..raw.len() - 1] } else { raw };
        // `head` cannot be empty: the bare `*` was handled by the caller.
        if trailing_star {
            Ok(Pattern::Prefix(head.to_string()))
        } else {
            Ok(Pattern::Exact(head.to_string()))
        }
    }

    /// Whether this pattern covers `tool`.
    ///
    /// Total by construction: every arm is a string comparison, so there is no
    /// input on which this can fail, panic, or take super-linear time.
    ///
    /// ## It takes a CATALOG ENTRY, not a name — this is the fix for round 7
    /// The namespace boundary is a fact about a tool's PROVENANCE, and the only
    /// place that fact exists is [`CatalogTool::namespace`]. An earlier revision
    /// matched against the advertised name alone and inferred provenance from
    /// its shape, via `split_namespaced`. That inference is not sound in either
    /// direction, because `split_namespaced` is purely syntactic:
    ///
    /// - A LOCAL tool literally named `peerhub__tool` splits to
    ///   `("peerhub", "tool")`, so it satisfied `peerhub::tool`, `peerhub::*`
    ///   and `peerhub::t*` — a qualified pattern reaching a tool from no
    ///   namespace at all, contradicting this module's central rule. [`resolve`]
    ///   masked it whenever a real federated entry of the same advertised name
    ///   existed, since the de-duplication prefers the federated one; with no
    ///   such entry, the local tool matched.
    /// - The mirror image: that same local tool was UNREACHABLE by `peer*`,
    ///   because the local arm required `split_namespaced` to return `None`.
    ///
    /// Both disappear once provenance is read rather than guessed. Note that
    /// holding the two halves separately (as RMCP-07 does) does NOT by itself
    /// fix this — `split_namespaced` gives that same false `Some` to a
    /// split-halves comparison. Only the catalog's own `namespace` field can
    /// answer the question, so that is what every arm below consults.
    ///
    /// A local tool whose name contains `__` is not something this crate's write
    /// path controls: patterns are refused for it, catalogs are not.
    pub fn matches(&self, tool: &CatalogTool) -> bool {
        let namespace = tool.namespace.as_deref();
        match self {
            Pattern::Everything => true,
            Pattern::Namespace(ns) => namespace == Some(ns.as_str()),
            Pattern::NamespacedPrefix { namespace: ns, prefix } => {
                namespace == Some(ns.as_str()) && tool.bare_name().starts_with(prefix.as_str())
            }
            Pattern::NamespacedExact { namespace: ns, bare } => {
                namespace == Some(ns.as_str()) && tool.bare_name() == bare.as_str()
            }
            // The local arms. `namespace.is_none()` IS the definition of local,
            // replacing the string-shape guess that used to stand in for it.
            Pattern::Prefix(prefix) => {
                namespace.is_none() && tool.name.starts_with(prefix.as_str())
            }
            Pattern::Exact(name) => namespace.is_none() && tool.name == name.as_str(),
        }
    }

    /// The canonical stored form. Round-trips through [`Self::parse_stored`],
    /// so storing the rendering of a parse is idempotent — which is what lets
    /// the write path normalise without changing meaning.
    pub fn render(&self) -> String {
        match self {
            Pattern::Everything => "*".to_string(),
            Pattern::Namespace(ns) => format!("{ns}{PATTERN_NS_SEP}*"),
            Pattern::NamespacedPrefix { namespace, prefix } => {
                format!("{namespace}{PATTERN_NS_SEP}{prefix}*")
            }
            Pattern::Prefix(prefix) => format!("{prefix}*"),
            Pattern::NamespacedExact { namespace, bare } => {
                format!("{namespace}{PATTERN_NS_SEP}{bare}")
            }
            Pattern::Exact(name) => name.clone(),
        }
    }
}

/// A namespace must survive a round-trip through the splitter the MATCHER uses.
///
/// Checked by construction — build a namespaced name and split it again —
/// rather than by a hand-derived character rule, so this cannot drift from
/// [`split_namespaced`]'s actual behaviour. It rejects three shapes at once,
/// all of which parse cleanly and then match NOTHING:
///
/// - **empty** (`::*`): absence of a namespace that compares equal to something
///   is the absence-means-permission failure this whole item exists to prevent.
/// - **an embedded name separator** (`a__b::*`): no advertised name resolves to
///   a namespace containing `__`, since the splitter cuts at the first one.
/// - **a TRAILING underscore** (`foo_::*` → namespace `foo_`): the subtle one.
///   `split_namespaced` always splits at the FIRST `__`, so an advertised
///   `foo___bar` yields namespace `foo`, never `foo_`.
///   A pattern naming `foo_` is therefore unmatchable — it stores cleanly,
///   reads as meaningful, and silently grants nothing.
fn check_namespace(namespace: &str) -> Result<(), ToolError> {
    let probe = crate::mesh::merge::namespaced(namespace, "x");
    if !namespace.is_empty()
        && !namespace.contains(PATTERN_NS_SEP)
        && split_namespaced(&probe) == Some((namespace, "x"))
    {
        return Ok(());
    }
    Err(ToolError::InvalidArgument(format!(
        "`{namespace}` is not a usable namespace: it must be non-empty, contain no \
         `{MESH_NS_SEP}` or `{PATTERN_NS_SEP}`, and not end in `_` — otherwise no advertised \
         tool name can ever resolve to it"
    )))
}

/// A group that has passed write-time validation. The store accepts only this,
/// so there is no path by which an unvalidated name or pattern reaches a row.
#[derive(Clone, Debug)]
pub struct ValidatedGroup {
    pub name: String,
    pub description: String,
    pub patterns: Vec<Pattern>,
}

impl ValidatedGroup {
    /// The canonical pattern strings to store.
    pub fn rendered_patterns(&self) -> Vec<String> {
        self.patterns.iter().map(Pattern::render).collect()
    }
}

/// Validate a group for storage: normalise the name, bound the description, and
/// parse every pattern under `owner`'s authority.
///
/// An EMPTY pattern list is accepted. A group being built up is a legitimate
/// state, and refusing it here would push callers toward seeding a new group
/// with a placeholder pattern — which is how an empty group accidentally
/// becomes a broad one. It resolves to nothing until it is filled in.
pub fn validate_group(
    name: &str,
    description: &str,
    patterns: &[String],
    authoring: &Authoring<'_>,
) -> Result<ValidatedGroup, ToolError> {
    Ok(ValidatedGroup {
        name: normalize_group_name(name)?,
        description: normalize_description(description)?,
        patterns: validate_patterns(patterns, authoring)?,
    })
}

/// Parse and bound a pattern list on its own — the half of
/// [`validate_group`] an edit that leaves the name alone needs.
///
/// Split out so an update path does not have to invent a placeholder name to
/// reach pattern validation: the one thing that must never be skippable is the
/// parse, so it lives where every write can reach it directly.
///
/// Two authorization questions are asked here, and they are separate on
/// purpose (RMCP-12): [`Pattern::parse`] asks the SHAPE question, which the
/// read path re-asks on every resolution, and
/// [`Authoring::authorize_ownership`] asks the OWNERSHIP question, which only a
/// write can ask because the answer is a live row that resolution bounds
/// differently — by the client's own `rmcp_client_server` rows, re-derived per
/// call.
pub fn validate_patterns(
    patterns: &[String],
    authoring: &Authoring<'_>,
) -> Result<Vec<Pattern>, ToolError> {
    let owner = authoring.owner();
    if patterns.len() > MAX_PATTERNS_PER_GROUP {
        return Err(ToolError::InvalidArgument(format!(
            "a group may hold at most {MAX_PATTERNS_PER_GROUP} patterns"
        )));
    }

    let mut parsed: Vec<Pattern> = Vec::with_capacity(patterns.len());
    for raw in patterns {
        let pattern = Pattern::parse(raw, owner)?;
        authoring.authorize_ownership(pattern.shape())?;
        // Deduplicated so a group's cost reflects its meaning: a list repeating
        // one pattern fifty times resolves identically and should not cost
        // fifty passes over the catalog.
        if !parsed.contains(&pattern) {
            parsed.push(pattern);
        }
    }
    Ok(parsed)
}

/// Normalise a human-authored group name.
///
/// Three things happen, each for its own reason:
///
/// - **Whitespace is trimmed and internal runs collapse to one space.** The
///   name column carries a UNIQUE constraint, and `"media"` versus `"media "`
///   defeats it — two groups with the same name to every human who reads them
///   and different rows to the database is precisely the confusion a uniqueness
///   constraint exists to prevent.
/// - **Invisible and direction-changing characters are REFUSED, not
///   stripped.** A group name is shown on the consent screen where a human
///   decides whether to approve a connector. A zero-width space or a
///   right-to-left override lets a name render as something other than what it
///   is, which turns that screen into a deception. Refusing says so; stripping
///   would silently rewrite what the author asked for.
/// - **Length is bounded in chars and bytes**, see [`MAX_GROUP_NAME_CHARS`].
pub fn normalize_group_name(raw: &str) -> Result<String, ToolError> {
    if raw.chars().any(is_deceptive_char) {
        return Err(ToolError::InvalidArgument(
            "a group name may not contain invisible or direction-changing characters — \
             it has to read on the consent screen as what it is"
                .into(),
        ));
    }
    let normalized = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return Err(ToolError::InvalidArgument("a group name may not be blank".into()));
    }
    if normalized.chars().count() > MAX_GROUP_NAME_CHARS
        || normalized.len() > MAX_GROUP_NAME_BYTES
    {
        return Err(ToolError::InvalidArgument(format!(
            "a group name may be at most {MAX_GROUP_NAME_CHARS} characters"
        )));
    }
    Ok(normalized)
}

/// Bound and clean a description. Newlines survive (a description may be two
/// lines); other control characters do not, for the same consent-screen reason
/// as the name.
pub fn normalize_description(raw: &str) -> Result<String, ToolError> {
    if raw.chars().any(|c| is_deceptive_char(c) || (c.is_control() && c != '\n')) {
        return Err(ToolError::InvalidArgument(
            "a description may not contain control, invisible, or direction-changing characters"
                .into(),
        ));
    }
    let trimmed = raw.trim();
    if trimmed.chars().count() > MAX_DESCRIPTION_CHARS {
        return Err(ToolError::InvalidArgument(format!(
            "a description may be at most {MAX_DESCRIPTION_CHARS} characters"
        )));
    }
    Ok(trimmed.to_string())
}

/// Characters that render as nothing, or that change how the text around them
/// renders. `char::is_control` catches the C0/C1 sets but NOT these — they are
/// the format (Cf) characters, and they are exactly the ones a spoofed name
/// would use.
fn is_deceptive_char(c: char) -> bool {
    matches!(c,
        '\u{200B}'..='\u{200F}'   // zero-width space/joiners, LRM/RLM
        | '\u{202A}'..='\u{202E}' // bidi embedding and OVERRIDE
        | '\u{2060}'..='\u{2064}' // word joiner and invisible operators
        | '\u{2066}'..='\u{2069}' // bidi isolates
        | '\u{FEFF}'              // zero-width no-break space / BOM
    ) || c.is_control()
}

/// Resolve patterns against a live catalog, returning the concrete matching
/// tools in name order.
///
/// ## The empty case, stated first because it is the one that matters
/// An empty `patterns` returns an EMPTY vector. So does a pattern list that
/// matches nothing. There is no branch anywhere in this function that turns an
/// absence into the full catalog.
///
/// ## Namespace collision
/// A local tool may be named such that it collides with a federated tool's
/// advertised name (a local `ns__foo` against upstream `ns`'s `foo`). The
/// NAMESPACED entry wins: the caller invoking that name is reaching the mesh
/// route, so that is the entry a grant over it must describe. Resolving to the
/// local entry would report a grant over a tool the call never reaches.
///
/// ## Cost
/// One pass over the catalog to build the collision-resolved index, then one
/// pattern-list scan per surviving name. Patterns are bounded by
/// [`MAX_PATTERNS_PER_GROUP`] and each match is a string comparison, so the
/// work is linear in catalog size (with the map's log factor), never quadratic
/// — no step compares tools against other tools. Allocation is bounded by the
/// catalog size: the index holds borrowed `&str` keys and the result holds
/// references, so resolving copies no tool name at all.
pub fn resolve<'a>(patterns: &[Pattern], catalog: &'a [CatalogTool]) -> Vec<&'a CatalogTool> {
    if patterns.is_empty() {
        return Vec::new();
    }

    let mut index: BTreeMap<&'a str, &'a CatalogTool> = BTreeMap::new();
    for tool in catalog {
        match index.entry(tool.name.as_str()) {
            Entry::Vacant(slot) => {
                slot.insert(tool);
            }
            Entry::Occupied(mut slot) => {
                if slot.get().namespace.is_none() && tool.namespace.is_some() {
                    slot.insert(tool);
                }
            }
        }
    }

    index
        .into_values()
        .filter(|tool| patterns.iter().any(|p| p.matches(tool)))
        .collect()
}

/// A stored group paired with its owner's authority AS IT IS NOW.
///
/// The pairing exists so the two can never be read at different times or from
/// different rows. Authority is per GROUP, not per request: a client may draw on
/// groups owned by different accounts, and each one's `*` stands or falls on its
/// own owner.
#[derive(Clone, Debug)]
pub struct AuthorizedGroup {
    pub group: ToolGroup,
    /// [`GroupOwner::Operator`] only if the owning account is currently flagged
    /// as an operator AND is not disabled.
    pub owner: GroupOwner,
}

// Decoded by hand for the same reason as every row type in
// `crate::oauth::model`: this workspace builds sqlx without the derive feature.
// The authority column is projected by the query rather than stored on the
// group, so it cannot be read from a stale row.
impl<'r> sqlx::FromRow<'r, sqlx::sqlite::SqliteRow> for AuthorizedGroup {
    fn from_row(row: &'r sqlx::sqlite::SqliteRow) -> Result<Self, sqlx::Error> {
        use sqlx::Row;
        let owner = if row.try_get::<bool, _>("owner_is_operator")? {
            GroupOwner::Operator
        } else {
            GroupOwner::Delegated
        };
        Ok(Self { group: ToolGroup::from_row(row)?, owner })
    }
}

/// Resolve a client's groups — the UNION of their patterns — against the live
/// catalog.
///
/// The union is taken over the parsed patterns rather than over per-group
/// result sets, so the catalog is indexed once no matter how many groups a
/// client holds, and a tool matched by two groups appears once.
///
/// ## Why authority is an argument here
/// A write-time authorization check is POINT-IN-TIME. The check that refuses a
/// bare `*` from a delegated author runs when the row is written, and nothing
/// about that check survives into the future: a row written before the
/// `is_operator` column existed, or one whose author was later DEMOTED, would
/// otherwise leave a non-operator holding an unrestricted group forever —
/// exactly the outcome the write-side check was added to prevent. Any authority
/// that can be REVOKED has to be re-derived on the read path.
///
/// So a [`Pattern::Everything`] expands only when its group's owner is an
/// operator RIGHT NOW. For any other owner it is DROPPED — the whole group
/// collapses to whatever its remaining patterns match, which for a group whose
/// only pattern was `*` is the empty set. Fail-closed, like every other
/// resolution rule here; it is never downgraded to "everything except…" or to
/// an error.
///
/// A stored pattern that no longer parses is skipped (see
/// [`Pattern::parse_stored`]): it contributes nothing, the same direction. A
/// group whose patterns ALL fail to parse, or whose only pattern is a
/// now-unauthorized `*`, therefore behaves exactly like an empty group.
///
/// ## Why this returns a `Result` when matching is total
/// [`Pattern::matches`] is still infallible, and that is the property that
/// matters on the dispatch path: no tool name can make a match fail. The error
/// here is not about a request at all — it is a fixed property of the stored
/// CONFIGURATION (how many groups this client holds), so it cannot be triggered
/// by traffic, cannot flap between two calls, and cannot depend on which tool is
/// being called. A caller that hits it has a client an operator must re-scope,
/// which is worth saying out loud rather than absorbing into a silent denial.
pub fn resolve_groups<'a>(
    groups: &[AuthorizedGroup],
    catalog: &'a [CatalogTool],
) -> Result<Vec<&'a CatalogTool>, ToolError> {
    // Counted from the STORED rows before any parsing, so the bound is on the
    // work this call is about to do rather than on what survives validation.
    let declared: usize = groups.iter().map(|g| g.group.patterns.len()).sum();
    if groups.len() > MAX_GROUPS_PER_CLIENT || declared > MAX_RESOLVED_PATTERNS {
        // Refuse the whole resolution rather than truncating it. A truncated
        // pattern list is a scope that silently differs from the configured one
        // — and since the surviving prefix would depend on row ordering, two
        // resolutions of the same configuration could differ from each other.
        // Denying is the safe direction and, unlike a truncation, it is visible.
        return Err(ToolError::InvalidArgument(format!(
            "this client is scoped to {} group(s) holding {declared} pattern(s), over the \
             limit of {MAX_GROUPS_PER_CLIENT} groups / {MAX_RESOLVED_PATTERNS} patterns; \
             resolution is refused rather than truncated — reduce the client's groups",
            groups.len()
        )));
    }

    let patterns: Vec<Pattern> = groups
        .iter()
        .flat_map(|authorized| {
            let owner = authorized.owner;
            authorized.group.patterns.iter().filter_map(move |raw| {
                // The revocation check. Not a filter over the RESULT set —
                // dropping the pattern is what makes an unauthorized pattern
                // resolve to nothing rather than to the catalog minus
                // something.
                //
                // RMCP-12: the rule is now `delegation::owner_may_hold`, the
                // same function the authoring path calls, so a delegated
                // owner's stored LOCAL patterns collapse here exactly as a
                // stale `*` always did. That matters for the demotion case an
                // authoring check cannot cover: an operator authors `pg_*`, is
                // later demoted, and their groups must stop reaching the
                // fleet's own tools on the very next resolution.
                Pattern::parse_stored(raw).filter(|parsed| owner_may_hold(owner, parsed.shape()))
            })
        })
        .collect();
    Ok(resolve(&patterns, catalog))
}

/// A starter group: a name, a description, and prefix patterns over tool
/// families that already exist in the registry.
pub struct StarterGroup {
    pub name: &'static str,
    pub description: &'static str,
    pub patterns: &'static [&'static str],
}

/// Seeded, operator-editable starter groups.
///
/// These exist so the FIRST connector is usable without hand-authoring several
/// hundred tool names — the failure mode this whole item exists to prevent is
/// an operator who, faced with that, reaches for a wildcard instead.
///
/// They are a starting point, deliberately not a policy: every one is an
/// ordinary row after seeding, editable and deletable like any other, and none
/// uses the bare `*` (asserted by a test, so a later edit here cannot quietly
/// seed full access). Each pattern is a tool-name prefix that the registry
/// already exports; nothing here encodes a host, an address, or an account.
///
/// Note that a family prefix includes that family's WRITE tools — `media_*`
/// covers deletion, for instance. That is correct at this layer: a group says
/// which tools a connector may see, and the sensitive-action deny list
/// ([`crate::gateway_framework::DEFAULT_SENSITIVE_DENY_PREFIXES`]) plus the
/// account's own grant still apply underneath. A group cannot widen either.
pub const STARTER_GROUPS: &[StarterGroup] = &[
    StarterGroup {
        name: "daily briefing",
        description: "Weather, news, commute and the authoritative clock.",
        patterns: &["weather_*", "news_*", "commute_*", "time_*"],
    },
    StarterGroup {
        name: "home",
        description: "Household planning: meals, pantry and recipes.",
        patterns: &["hearth_*"],
    },
    StarterGroup {
        name: "media",
        description: "The media library and its request/cleanup tooling.",
        patterns: &["media_*"],
    },
    StarterGroup {
        name: "personal records",
        description: "Ledger entries, reminders and health vitals.",
        patterns: &["ledger_*", "reminder_*", "vitals_*"],
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use uuid::Uuid;

    fn catalog() -> Vec<CatalogTool> {
        vec![
            CatalogTool::local("weather_get"),
            CatalogTool::local("weather_alerts"),
            CatalogTool::local("news_headlines"),
            CatalogTool::local("ledger_add"),
            // The adversarial pair: a LOCAL tool whose name starts with `peer`,
            // alongside a NAMESPACE that also starts with `peer`. Any matcher
            // that treats a bare prefix as a raw `starts_with` over the
            // advertised name conflates the two.
            CatalogTool::local("peer_status"),
            CatalogTool::from_upstream("peerhub", "alerts_list"),
            CatalogTool::from_upstream("peerhub", "ledger_add"),
            CatalogTool::from_upstream("sensors", "node_status"),
        ]
    }

    fn names(resolved: &[&CatalogTool]) -> Vec<String> {
        resolved.iter().map(|t| t.name.clone()).collect()
    }

    fn resolve_raw(raw: &[&str], owner: GroupOwner) -> Vec<String> {
        let patterns: Vec<Pattern> =
            raw.iter().map(|r| Pattern::parse(r, owner).expect("valid pattern")).collect();
        let cat = catalog();
        names(&resolve(&patterns, &cat))
    }

    /// A stored row, as it comes back from the database — patterns as text,
    /// with no authority attached to them.
    /// A LOCAL catalog entry. Note the name is taken verbatim: a local tool may
    /// legitimately contain `__`, which is the shape round 7 turned on.
    fn loc(name: &str) -> CatalogTool {
        CatalogTool::local(name)
    }

    /// A FEDERATED catalog entry, built the way the merge layer builds one.
    fn fed(namespace: &str, bare: &str) -> CatalogTool {
        CatalogTool::from_upstream(namespace, bare)
    }

    fn stored_group(patterns: Vec<String>) -> ToolGroup {
        ToolGroup {
            id: Uuid::nil(),
            name: "g".into(),
            description: String::new(),
            patterns,
            owner_account_id: Uuid::nil(),
            created_at: Utc::now(),
        }
    }

    fn authorized(group: ToolGroup, owner: GroupOwner) -> AuthorizedGroup {
        AuthorizedGroup { group, owner }
    }

    fn resolve_stored(groups: Vec<AuthorizedGroup>) -> Vec<String> {
        let cat = catalog();
        names(&resolve_groups(&groups, &cat).expect("within the aggregate bound"))
    }

    // ── The empty cases, first: this is the invariant the item is about ──────

    /// An empty group resolves to the empty set. Not "unrestricted", not the
    /// catalog — nothing. If this test ever fails, every connector scoped by an
    /// unfilled group has full access.
    #[test]
    fn empty_group_resolves_to_the_empty_set() {
        let cat = catalog();
        assert!(resolve(&[], &cat).is_empty(), "an empty pattern list must grant nothing");

        let group = stored_group(vec![]);
        assert!(group.is_empty());
        assert!(resolve_groups(&[authorized(group, GroupOwner::Operator)], &cat).unwrap().is_empty());
    }

    /// A well-formed pattern that matches no tool in the current catalog is the
    /// other half of the same invariant: zero matches is zero, never all.
    #[test]
    fn a_pattern_matching_nothing_resolves_to_the_empty_set() {
        assert!(resolve_raw(&["no_such_tool"], GroupOwner::Operator).is_empty());
        assert!(resolve_raw(&["nothing_starts_with_this_*"], GroupOwner::Operator).is_empty());
        assert!(resolve_raw(&["absent_namespace::*"], GroupOwner::Operator).is_empty());
    }

    /// A stored pattern that no longer parses must contribute nothing, rather
    /// than being read as a wildcard or erroring on the dispatch path.
    #[test]
    fn an_unparseable_stored_pattern_grants_nothing() {
        let cat = catalog();
        let group = stored_group(vec!["we*ther_*".into(), "".into()]);
        assert!(!group.is_empty(), "the row has patterns; they simply do not parse");
        assert!(resolve_groups(&[authorized(group, GroupOwner::Operator)], &cat).unwrap().is_empty());
    }

    // ── Revocation: authority is re-derived on the READ path ─────────────────

    /// The round-2 finding. A `*` was legitimately written by an operator who
    /// has since been DEMOTED — the row is unchanged, the authority is gone, and
    /// the pattern must stop expanding. Anything else leaves a delegated account
    /// holding an unrestricted group permanently, which is the exact outcome the
    /// write-side check exists to prevent.
    #[test]
    fn a_stored_wildcard_collapses_when_its_owner_is_no_longer_an_operator() {
        let star = || stored_group(vec!["*".into()]);
        assert_eq!(
            resolve_stored(vec![authorized(star(), GroupOwner::Operator)]).len(),
            catalog().len(),
            "an operator's wildcard still means everything"
        );
        assert!(
            resolve_stored(vec![authorized(star(), GroupOwner::Delegated)]).is_empty(),
            "a demoted owner's wildcard must resolve to the EMPTY set, not to everything"
        );
    }

    /// A row written before the `is_operator` column existed has an owner that
    /// reads as delegated, because the column defaults to false. Its `*` must
    /// collapse — that is the safe reading of a pre-migration row, and it is
    /// what the default is chosen to produce.
    #[test]
    fn a_pre_migration_wildcard_reads_as_delegated_and_collapses() {
        // `is_operator` DEFAULT false → `Account::group_owner_kind` → Delegated.
        let owner = crate::oauth::model::Account {
            id: Uuid::nil(),
            name: "legacy".into(),
            password_hash: "<REDACTED-SECRET>".into(),
            totp_secret_enc: None,
            disabled: false,
            is_operator: false,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
        .group_owner_kind();
        assert_eq!(owner, GroupOwner::Delegated);
        assert!(resolve_stored(vec![authorized(stored_group(vec!["*".into()]), owner)]).is_empty());
    }

    /// The other revocation, end to end: DISABLING an operator collapses their
    /// stored `*` to the empty set.
    ///
    /// Driven through [`crate::oauth::model::Account::group_owner_kind`] rather
    /// than by naming `GroupOwner::Delegated` directly, so this test fails if
    /// that conversion ever stops treating a disabled account as delegated —
    /// which is precisely how the wildcard would come back to life while every
    /// test that hardcodes the authority kept passing.
    #[test]
    fn disabling_an_operator_collapses_their_stored_wildcard() {
        let disabled_operator = crate::oauth::model::Account {
            id: Uuid::nil(),
            name: "compromised".into(),
            password_hash: "<REDACTED-SECRET>".into(),
            totp_secret_enc: None,
            disabled: true,
            is_operator: true,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        assert!(
            resolve_stored(vec![authorized(
                stored_group(vec!["*".into()]),
                disabled_operator.group_owner_kind(),
            )])
            .is_empty(),
            "a disabled operator's wildcard must reach the EMPTY set"
        );
    }

    /// Dropping an unauthorized `*` must not take the rest of the group with it,
    /// and must not leak across groups. Only the wildcard is revoked.
    #[test]
    fn revoking_a_wildcard_leaves_the_groups_other_patterns_alone() {
        assert_eq!(
            resolve_stored(vec![
                authorized(
                    stored_group(vec!["*".into(), "peerhub::*".into()]),
                    GroupOwner::Delegated,
                ),
                authorized(stored_group(vec!["news_headlines".into()]), GroupOwner::Operator),
            ]),
            vec!["news_headlines", "peerhub__alerts_list", "peerhub__ledger_add"],
            "the delegated group keeps its namespaced pattern; only its `*` is dropped"
        );
    }

    /// Authority is per GROUP. One operator-owned wildcard in the set must not
    /// bless a different owner's group, and one delegated group must not
    /// suppress an operator's legitimate wildcard.
    #[test]
    fn authority_is_evaluated_per_group_not_per_request() {
        let resolved = resolve_stored(vec![
            authorized(stored_group(vec!["*".into()]), GroupOwner::Delegated),
            authorized(stored_group(vec!["*".into()]), GroupOwner::Operator),
        ]);
        assert_eq!(resolved.len(), catalog().len(), "the operator's wildcard still applies");

        let resolved = resolve_stored(vec![
            authorized(stored_group(vec!["*".into()]), GroupOwner::Delegated),
            authorized(stored_group(vec!["peerhub::alerts_list".into()]), GroupOwner::Delegated),
        ]);
        assert_eq!(resolved, vec!["peerhub__alerts_list"], "no wildcard survives here");
    }

    // ── The three accepted shapes ────────────────────────────────────────────

    #[test]
    fn exact_pattern_matches_that_tool_and_nothing_else() {
        assert_eq!(resolve_raw(&["weather_get"], GroupOwner::Operator), vec!["weather_get"]);
    }

    #[test]
    fn prefix_pattern_matches_the_family_and_nothing_else() {
        assert_eq!(
            resolve_raw(&["weather_*"], GroupOwner::Operator),
            vec!["weather_alerts", "weather_get"]
        );
    }

    #[test]
    fn namespace_pattern_matches_exactly_one_upstream() {
        assert_eq!(
            resolve_raw(&["peerhub::*"], GroupOwner::Operator),
            vec!["peerhub__alerts_list", "peerhub__ledger_add"]
        );
    }

    /// The headline scoping hazard: a prefix must stay on THIS side of the
    /// namespace boundary. `a*` matching `peerhub__alerts_list` would mean any
    /// short local prefix silently reaches into every federated server.
    #[test]
    fn a_prefix_does_not_reach_into_another_namespace() {
        assert!(resolve_raw(&["a*"], GroupOwner::Operator).is_empty());
        assert!(!Pattern::parse("a*", GroupOwner::Operator)
            .unwrap()
            .matches(&fed("peerhub", "alerts_list")));
        assert!(!Pattern::parse("ledger_*", GroupOwner::Operator)
            .unwrap()
            .matches(&fed("peerhub", "ledger_add")));
        // Reaching an upstream tool takes a pattern that names the upstream.
        assert!(Pattern::parse("peerhub::ledger*", GroupOwner::Operator)
            .unwrap()
            .matches(&fed("peerhub", "ledger_add")));
    }

    /// The round-3 finding, exactly as reported: a bare prefix that is a STRICT
    /// PREFIX OF A NAMESPACE NAME. `peer*` was written for local `peer_*` tools
    /// and silently swept in an entire federated server, because a raw
    /// `starts_with` over the advertised name cannot tell `peer_status` from
    /// `peerhub__alerts_list`. This is the shape that made the widening
    /// reachable at all, so it is pinned literally.
    ///
    /// Both near-misses are asserted alongside it, because a fix that broke
    /// either would be worse than the bug: the local tool must still match, and
    /// the deliberately-qualified pattern must still reach the upstream.
    #[test]
    fn a_bare_prefix_that_is_a_prefix_of_a_namespace_stays_local() {
        let bare = Pattern::parse("peer*", GroupOwner::Operator).unwrap();
        assert!(bare.matches(&loc("peer_status")), "a LOCAL tool named peer... must still match");
        assert!(
            !bare.matches(&fed("peerhub", "alerts_list")),
            "a bare prefix must not cross the mesh separator into a namespace \
             that merely shares its letters"
        );
        assert!(!bare.matches(&fed("peerhub", "ledger_add")));

        // Written deliberately, with the upstream named, it still reaches in.
        let qualified = Pattern::parse("peerhub::*", GroupOwner::Operator).unwrap();
        assert!(qualified.matches(&fed("peerhub", "alerts_list")));
        assert!(!qualified.matches(&loc("peer_status")), "and it does NOT reach back out to local");

        // End to end through resolution, which is what actually authorizes.
        assert_eq!(resolve_raw(&["peer*"], GroupOwner::Operator), vec!["peer_status"]);
        assert_eq!(
            resolve_raw(&["peerhub::*"], GroupOwner::Operator),
            vec!["peerhub__alerts_list", "peerhub__ledger_add"]
        );
    }

    /// `*` REACHES FEDERATED TOOLS. This is the one pattern deliberately exempt
    /// from the local-only rule, and RMCP-07's copy of the matcher must agree:
    /// its `All` arm stays catalog-wide.
    ///
    /// The exemption is coherent rather than inconsistent. The local-only rule
    /// exists because letters collide — a prefix can sweep in a namespace that
    /// merely shares its opening characters, invisibly to the author. `*` has no
    /// letters and so no coincidence to fall foul of; it is also the most
    /// heavily gated pattern in the item (operator-only, re-derived on read), so
    /// making it the ONLY shape that could not reach a federated tool would
    /// leave the strongest-gated pattern weaker than weaker-gated ones.
    ///
    /// What bounds it is `namespaces(client)` at RMCP-07's intersection. If `*`
    /// stopped at the local registry, that dimension would only ever constrain
    /// patterns that already name their own namespace — nearly vestigial.
    #[test]
    fn the_bare_wildcard_reaches_federated_tools_and_is_bounded_elsewhere() {
        let all = Pattern::parse("*", GroupOwner::Operator).unwrap();
        assert!(all.matches(&loc("weather_get")), "local");
        assert!(all.matches(&fed("peerhub", "alerts_list")), "federated — NOT local-only");
        assert!(all.matches(&fed("sensors", "node_status")));

        // Every advertised name in the fixture, both sides of the separator.
        let cat = catalog();
        assert_eq!(resolve(&[all], &cat).len(), cat.len());
        assert!(cat.iter().any(|t| t.namespace.is_some()), "fixture must contain federated tools");

        // The contrast that makes the rule unambiguous: same author, same
        // group, one pattern local-only and one catalog-wide.
        assert_eq!(resolve_raw(&["peer*"], GroupOwner::Operator), vec!["peer_status"]);
        assert_eq!(resolve_raw(&["*"], GroupOwner::Operator).len(), catalog().len());
    }

    /// A qualified prefix is anchored to ONE namespace and to the bare name
    /// inside it — it is not a loose `starts_with` that happens to contain a
    /// separator.
    #[test]
    fn a_qualified_prefix_is_anchored_to_its_namespace() {
        let p = Pattern::parse("peerhub::ledger*", GroupOwner::Operator).unwrap();
        assert!(p.matches(&fed("peerhub", "ledger_add")));
        assert!(!p.matches(&fed("sensors", "ledger_add")), "a different upstream is a different namespace");
        assert!(!p.matches(&loc("ledger_add")), "and it never matches the LOCAL tool of that name");
        assert!(!p.matches(&fed("peerhub", "alerts_list")));
    }

    /// Confirms the behaviour RMCP-07 observed independently: `ledger_*` does
    /// NOT reach a federated `ledger_accounts`. It agreed with the old matcher
    /// for the wrong reason (the advertised name started with the namespace);
    /// it agrees with this one for the right reason (a bare prefix is local by
    /// definition). Same verdict, and now it holds for every namespace name
    /// rather than only the ones that happen not to share the prefix.
    #[test]
    fn a_bare_family_prefix_never_reaches_a_federated_tool_of_the_same_family() {
        let p = Pattern::parse("ledger_*", GroupOwner::Operator).unwrap();
        assert!(p.matches(&loc("ledger_add")), "the local tool");
        assert!(!p.matches(&fed("peerone", "ledger_accounts")));
        assert!(!p.matches(&fed("peerhub", "ledger_add")));
        // The pathological namespace: one literally named after the family.
        // `ledger___accounts` splits as namespace `ledger` + bare `_accounts`,
        // so it is federated and a bare prefix must not reach it.
        assert!(!p.matches(&fed("ledger", "_accounts")));
    }

    /// A namespace pattern is anchored on the FIRST separator, so it cannot be
    /// satisfied by a namespace that merely starts with the same letters.
    #[test]
    fn namespace_pattern_is_not_a_loose_prefix() {
        let p = Pattern::parse("peerhub::*", GroupOwner::Operator).unwrap();
        assert!(p.matches(&fed("peerhub", "anything")));
        assert!(!p.matches(&fed("peerhub0", "anything")), "a longer namespace is a different namespace");
        assert!(!p.matches(&loc("peerhub_local_tool")), "a single underscore is not the separator");
        assert!(!p.matches(&loc("peerhub")));
    }

    /// Several patterns union, and a tool matched twice appears once.
    #[test]
    fn patterns_union_without_duplicating_a_tool() {
        assert_eq!(
            resolve_raw(&["weather_*", "weather_get", "news_headlines"], GroupOwner::Operator),
            vec!["news_headlines", "weather_alerts", "weather_get"]
        );
    }

    // ── Collision ────────────────────────────────────────────────────────────

    /// A local tool whose literal name collides with a federated tool's
    /// advertised name resolves to the FEDERATED entry — that is the route the
    /// call actually takes, so it is the one a grant must describe.
    #[test]
    fn namespaced_entry_wins_a_collision_with_a_local_name() {
        for order in [false, true] {
            let mut cat = vec![
                CatalogTool::local("peerhub__ledger_add"),
                CatalogTool::from_upstream("peerhub", "ledger_add"),
            ];
            if order {
                cat.reverse();
            }
            let patterns = vec![Pattern::parse("peerhub::*", GroupOwner::Operator).unwrap()];
            let resolved = resolve(&patterns, &cat);
            assert_eq!(resolved.len(), 1, "one advertised name is one entry");
            assert_eq!(
                resolved[0].namespace.as_deref(),
                Some("peerhub"),
                "the namespaced form must win regardless of catalog order"
            );
        }
    }

    // ── Write-time validation ────────────────────────────────────────────────

    #[test]
    fn invalid_patterns_are_rejected_at_write_time() {
        for bad in [
            "",                      // matches nothing; a config error, not a grant
            "wea*her_*",             // interior star: reads as a glob, is not one
            "**",                    // ditto
            "*weather",              // leading star: reads as a suffix match, is not one
            "weather*foo",           // interior star with no trailing star at all
            "*a*",                   // both at once
            "weather get",           // whitespace can never match a tool name
            "weather\u{200B}_*",     // invisible character
            "weather\n_*",           // control character
            "wéather_*",             // non-ASCII cannot match an ASCII registry
            "::*",                   // empty namespace
            "::foo*",                // empty namespace in the split-prefix form
            "::foo",                 // ...and in the exact form
            "peerhub::",             // a namespace naming no tool
            "a::b::c*",              // more than one namespace
            "a__b::*",               // `__` inside a namespace: unresolvable
            "foo_::*",               // namespace `foo_` — unmatchable, see check_namespace
            "peerhub__*",            // OLD vocabulary; `__` is not a pattern delimiter
            "peerhub__ledger_add",   // ...in the exact form too
            "a__b__*",               // the formerly-ambiguous shape, now simply not a pattern
        ] {
            assert!(
                Pattern::parse(bad, GroupOwner::Operator).is_err(),
                "{bad:?} must be refused at write time"
            );
        }
        let too_long = "x".repeat(MAX_PATTERN_CHARS + 1);
        assert!(Pattern::parse(&too_long, GroupOwner::Operator).is_err());
    }

    /// The star rule is decided over the WHOLE string before any shape is
    /// chosen, so a star that is not the single trailing character is refused as
    /// such — it never falls through to another branch and gets accepted as
    /// something else.
    ///
    /// `*weather` is the case worth naming: an author writing it means a SUFFIX
    /// match. There is no suffix syntax, so the only honest answers are "refuse
    /// it" or "silently store an exact pattern for a tool literally named
    /// `*weather`", which exists nowhere and leaves a connector quietly missing
    /// tools with no error to explain it. This asserts the first.
    #[test]
    fn a_star_anywhere_but_the_end_is_refused_whatever_the_shape() {
        for bad in ["*weather", "weather*foo", "*a*", "**", "a*b*", "*"] {
            let parsed = Pattern::parse(bad, GroupOwner::Operator);
            if bad == "*" {
                assert_eq!(parsed.unwrap(), Pattern::Everything, "the bare `*` is the one legal star");
            } else {
                assert!(parsed.is_err(), "{bad:?} must be refused, not reinterpreted");
            }
        }
        // And it is refused as a STAR problem, not misreported as something else.
        let err = Pattern::parse("*weather", GroupOwner::Operator).unwrap_err().to_string();
        assert!(err.contains("LAST character"), "the error must name the real problem: {err}");
    }

    /// A namespace that cannot round-trip through `split_namespaced` is refused,
    /// because it would store cleanly, read as meaningful, and match nothing.
    ///
    /// The trailing-underscore case (`foo_::*` → namespace `foo_`) is the one
    /// that was not previously caught: the splitter always cuts at the FIRST
    /// `__`, so an advertised `foo___bar` resolves to namespace `foo`, and no
    /// tool can ever resolve to `foo_`.
    #[test]
    fn an_unusable_namespace_is_refused_rather_than_stored_unmatchable() {
        for bad in ["::*", "::foo*", "a__b::*", "foo_::*", "::foo", "a::b::c*", "peerhub::"] {
            assert!(
                Pattern::parse(bad, GroupOwner::Operator).is_err(),
                "{bad:?} can never match a real tool and must be refused at write time"
            );
        }
        // The usable forms still parse. A bare name may contain the ADVERTISED
        // separator freely now that the pattern delimiter is a different
        // character — including ending with it, which the old vocabulary could
        // not express at all.
        assert_eq!(
            Pattern::parse("a::b__c*", GroupOwner::Operator).unwrap(),
            Pattern::NamespacedPrefix { namespace: "a".into(), prefix: "b__c".into() },
        );
        assert_eq!(
            Pattern::parse("a::b__*", GroupOwner::Operator).unwrap(),
            Pattern::NamespacedPrefix { namespace: "a".into(), prefix: "b__".into() },
            "a bare prefix may END with `__`: there is no second reading to collide with"
        );
        assert_eq!(
            Pattern::parse("_foo*", GroupOwner::Operator).unwrap(),
            Pattern::Prefix("_foo".into()),
            "a single leading underscore is not a separator"
        );
    }

    /// The round-5 ambiguity is GONE, not adjudicated.
    ///
    /// Under the old `__` vocabulary `a__b__*` had two honest readings —
    /// namespace `a__b`, or namespace `a` with bare prefix `b__` — and two
    /// careful readers reached different ones. A distinct pattern delimiter
    /// dissolves it: `a::b__*` can only be namespace `a`, bare prefix `b__`.
    /// The precedence rule that used to settle it no longer exists, which is
    /// the point — a pattern whose meaning depends on a precedence rule is a
    /// pattern an author cannot read.
    #[test]
    fn the_pattern_delimiter_removes_the_double_separator_ambiguity() {
        assert_eq!(
            Pattern::parse("a::b__*", GroupOwner::Operator).unwrap(),
            Pattern::NamespacedPrefix { namespace: "a".into(), prefix: "b__".into() },
        );
        assert_eq!(
            Pattern::parse("a__b::*", GroupOwner::Operator).ok(),
            None,
            "the other reading is not a rival parse, it is an unresolvable namespace"
        );
        // And it matches the tool the reading implies.
        let p = Pattern::parse("a::b__*", GroupOwner::Operator).unwrap();
        assert!(p.matches(&fed("a", "b__c")), "namespace `a`, bare name `b__c`");
        assert!(!p.matches(&fed("a", "c")));
    }

    /// TERM #637B symmetry: the AUTHORING matcher here and the ENFORCING
    /// matcher in RMCP-07's `scope.rs` must select the same set for every
    /// pattern this validator accepts. Mirrors 637B's own qualified-forms test,
    /// case for case, so a divergence shows up on whichever side changes first.
    ///
    /// Both sides now hold the two halves separately. They still differ in the
    /// question they ask: 637B recovers the namespace by SPLITTING the
    /// advertised name, while this side reads [`CatalogTool::namespace`]. Those
    /// agree for every FEDERATED entry — the mapping between `(ns, bare)` and an
    /// advertised name is a bijection — and disagree for a LOCAL entry whose
    /// name merely looks namespaced, which is the hole recorded on TERM #637 and
    /// still open on the enforcing side.
    #[test]
    fn qualified_forms_agree_with_the_enforcing_matcher() {
        let exact = Pattern::parse("peerone::weather_now", GroupOwner::Operator).unwrap();
        assert_eq!(
            exact,
            Pattern::NamespacedExact { namespace: "peerone".into(), bare: "weather_now".into() }
        );
        assert!(exact.matches(&fed("peerone", "weather_now")));
        assert!(!exact.matches(&fed("peertwo", "weather_now")), "not another namespace");
        assert!(!exact.matches(&loc("weather_now")), "not the local tool of the same name");
        assert!(!exact.matches(&fed("peerone", "weather_forecast")), "not a sibling");

        let prefix = Pattern::parse("peerone::weather_*", GroupOwner::Operator).unwrap();
        assert_eq!(
            prefix,
            Pattern::NamespacedPrefix { namespace: "peerone".into(), prefix: "weather_".into() }
        );
        assert!(prefix.matches(&fed("peerone", "weather_now")));
        assert!(prefix.matches(&fed("peerone", "weather_forecast")));
        assert!(!prefix.matches(&fed("peertwo", "weather_now")), "cannot leak across the boundary");
        assert!(!prefix.matches(&loc("weather_now")), "and does not reach local tools");
        assert!(!prefix.matches(&fed("peerone", "media_search")));

        // The qualified prefix matches the BARE name, so a prefix equal to the
        // namespace's own text does not match by accident.
        let bare = Pattern::parse("peerone::peer*", GroupOwner::Operator).unwrap();
        assert!(bare.matches(&fed("peerone", "peer_status")));
        assert!(!bare.matches(&fed("peerone", "weather_now")));

        // And the exact form round-trips back to `::` for display.
        assert_eq!(exact.render(), "peerone::weather_now");
        assert_eq!(Pattern::parse_stored(&exact.render()), Some(exact));
    }

    /// ROUND 7: a qualified pattern must not match a LOCAL tool, even one whose
    /// literal name looks namespaced.
    ///
    /// The catalog here holds ONLY the local `peerhub__tool` — no federated
    /// entry at all. That absence is the whole point: `resolve` de-duplicates by
    /// advertised name and prefers a federated entry, so with both present the
    /// federated one won and the bug was invisible. The de-dup is a tie-breaker,
    /// and a rule that holds only when a tie exists is not a rule.
    ///
    /// Every qualified form is checked, because they shared one root cause —
    /// provenance was inferred from the NAME via `split_namespaced` rather than
    /// read from the catalog entry.
    #[test]
    fn a_qualified_pattern_never_matches_a_local_tool_that_looks_namespaced() {
        let sneaky = loc("peerhub__tool");
        assert!(sneaky.namespace.is_none(), "it is local; only its name looks otherwise");
        assert_eq!(sneaky.bare_name(), "peerhub__tool", "a local tool's bare name is its name");

        for pattern in ["peerhub::tool", "peerhub::*", "peerhub::t*"] {
            let p = Pattern::parse(pattern, GroupOwner::Operator).unwrap();
            assert!(
                !p.matches(&sneaky),
                "{pattern:?} must not reach a tool from no namespace at all"
            );
        }

        // End to end, with NO federated entry to mask it.
        let catalog = vec![sneaky.clone()];
        for pattern in ["peerhub::tool", "peerhub::*", "peerhub::t*"] {
            let p = Pattern::parse(pattern, GroupOwner::Operator).unwrap();
            assert!(
                resolve(&[p], &catalog).is_empty(),
                "{pattern:?} resolved to a local tool with no federated entry present"
            );
        }

        // The genuine federated tool of that advertised name still matches.
        let real = fed("peerhub", "tool");
        assert_eq!(real.name, "peerhub__tool", "same advertised name, different provenance");
        for pattern in ["peerhub::tool", "peerhub::*", "peerhub::t*"] {
            let p = Pattern::parse(pattern, GroupOwner::Operator).unwrap();
            assert!(p.matches(&real), "{pattern:?} must still reach the real federated tool");
        }
    }

    /// The mirror image of the same root cause: a LOCAL tool whose name contains
    /// `__` was UNREACHABLE by local patterns, because the local arms required
    /// `split_namespaced` to return `None` and it does not for such a name.
    ///
    /// Reading provenance instead of guessing it fixes both directions at once.
    #[test]
    fn a_local_tool_whose_name_contains_the_separator_is_still_reachable_locally() {
        let sneaky = loc("peerhub__tool");
        assert!(Pattern::parse("peer*", GroupOwner::Operator).unwrap().matches(&sneaky));
        assert!(Pattern::parse("peerhub_*", GroupOwner::Operator).unwrap().matches(&sneaky));
        // And an exact local pattern for it — which no pattern grammar can
        // spell, since `__` is refused in an unqualified pattern. Constructed
        // directly to show the matcher itself is provenance-correct.
        assert!(Pattern::Exact("peerhub__tool".into()).matches(&sneaky));
        assert!(!Pattern::Exact("peerhub__tool".into()).matches(&fed("peerhub", "tool")));
    }

    /// CARRY-OVER 1 from TERM #637: the over-grant hole must stay closed.
    ///
    /// Under the old vocabulary `peerhub::*` passed write-time validation as an
    /// innocuous LOCAL prefix — not a bare `*`, so the operator-only rule never
    /// fired — while RMCP-07's enforcing matcher read the same string as a whole
    /// federated namespace. A delegated author could grant themselves an
    /// upstream through a pattern this validator called harmless.
    ///
    /// It is closed by CLASSIFICATION: `peerhub::*` is now a namespace pattern
    /// on both sides, so whatever rules apply to namespace patterns apply to it.
    /// This asserts the classification, because that is the thing that was
    /// wrong — an `is_err`/`is_ok` check would not have caught it.
    #[test]
    fn a_namespace_pattern_is_never_classified_as_a_local_prefix() {
        let p = Pattern::parse("peerhub::*", GroupOwner::Delegated).unwrap();
        assert_eq!(p, Pattern::Namespace("peerhub".into()), "NOT Prefix(\"peerhub::\")");
        assert!(p.matches(&fed("peerhub", "alerts_list")), "it means the upstream, and says so");
        assert!(!p.matches(&loc("peerhub_local_tool")), "and not a local tool of a similar name");

        // No pattern can be a local prefix that carries the pattern delimiter,
        // so there is no spelling left that hides a namespace inside a local
        // classification.
        for sneaky in ["peerhub::", "peerhub::*", "::*"] {
            match Pattern::parse(sneaky, GroupOwner::Delegated) {
                Ok(parsed) => assert!(
                    !matches!(parsed, Pattern::Prefix(_) | Pattern::Exact(_)),
                    "{sneaky:?} parsed as an unqualified pattern: {parsed:?}"
                ),
                Err(_) => {}
            }
        }
    }

    /// CARRY-OVER 2 from TERM #637: an OLD-vocabulary pattern is refused, and
    /// the error names the form it should have been.
    ///
    /// A stored `peerhub__*` must not quietly become a local prefix that matches
    /// nothing — that would turn a scope which used to grant an upstream into
    /// one granting zero tools, with no error anywhere. Refusing is the
    /// deliberate choice over migrating: an operator sees it at the point of
    /// edit, and the suggestion tells them exactly what to write.
    #[test]
    fn an_old_vocabulary_pattern_is_refused_with_the_correct_form_named() {
        let err = Pattern::parse("peerhub__*", GroupOwner::Operator).unwrap_err().to_string();
        assert!(err.contains("peerhub::*"), "the error must name the correct form: {err}");

        let err = Pattern::parse("peerhub__ledger_add", GroupOwner::Operator).unwrap_err().to_string();
        assert!(err.contains("peerhub::ledger_add"), "including for an exact name: {err}");

        // Only the FIRST `__` is the one that should have been a delimiter, so
        // the suggestion keeps a bare name's own separators intact.
        let err = Pattern::parse("a__b__c*", GroupOwner::Operator).unwrap_err().to_string();
        assert!(err.contains("a::b__c*"), "the suggestion preserves the bare name: {err}");

        // A local pattern with no `__` is unaffected.
        assert!(Pattern::parse("weather_*", GroupOwner::Operator).is_ok());
    }

    /// An EXACT qualified name must stay EXACT. Splitting it into a namespaced
    /// prefix would widen it — `peerhub__ledger_add` would start matching
    /// `peerhub__ledger_add_v2` — which is the same "parses to something other
    /// than what was written" defect, in the granting direction.
    #[test]
    fn an_exact_qualified_name_does_not_become_a_prefix() {
        let p = Pattern::parse("peerhub::ledger_add", GroupOwner::Operator).unwrap();
        assert_eq!(
            p,
            Pattern::NamespacedExact { namespace: "peerhub".into(), bare: "ledger_add".into() }
        );
        assert!(p.matches(&fed("peerhub", "ledger_add")));
        assert!(!p.matches(&fed("peerhub", "ledger_add_v2")), "an exact pattern must not act as a prefix");
    }

    /// Matching is TOTAL: whatever a name looks like, matching answers
    /// yes-or-no. An error here would be an availability failure on dispatch.
    #[test]
    fn matching_never_fails_on_a_strange_name() {
        let p = Pattern::parse("weather_*", GroupOwner::Operator).unwrap();
        let long = "w".repeat(4096);
        for name in ["", "__", "___", "*", "weather_", "wéather_x", "\u{200B}", long.as_str()] {
            // Both provenances, since matching now branches on it.
            let _ = p.matches(&loc(name));
            let _ = p.matches(&CatalogTool { name: name.to_string(), namespace: Some("ns".into()) });
        }
        assert!(p.matches(&loc("weather_")), "the prefix itself is covered");
        assert!(!p.matches(&loc("")));
    }

    /// The owned set a delegated author in these tests holds.
    fn delegated_owns() -> std::collections::BTreeSet<String> {
        ["peerhub".to_string()].into_iter().collect()
    }

    /// RMCP-12's authoring rule, at the `groups.rs` entry point: a delegated
    /// author may write patterns over a server they OWN, and nothing else.
    #[test]
    fn a_delegated_author_may_write_only_patterns_over_a_server_they_own() {
        let owned = delegated_owns();
        let authoring = Authoring::Delegated { owned: &owned };
        for allowed in ["peerhub::*", "peerhub::ledger*", "peerhub::ledger_add"] {
            validate_patterns(&[allowed.to_string()], &authoring)
                .unwrap_or_else(|e| panic!("{allowed:?} must be authorable: {e}"));
        }
        for refused in ["otherpeer::*", "otherpeer::ledger_add"] {
            assert!(
                validate_patterns(&[refused.to_string()], &authoring).is_err(),
                "{refused:?} names a server this author does not own"
            );
        }
    }

    /// The other half, and the one that is a WIDENING if it is missing: an
    /// unqualified pattern addresses the fleet's own tools.
    #[test]
    fn a_delegated_author_may_not_write_an_unqualified_pattern() {
        let owned = delegated_owns();
        let authoring = Authoring::Delegated { owned: &owned };
        for refused in ["weather_*", "weather_get", "*"] {
            assert!(
                validate_patterns(&[refused.to_string()], &authoring).is_err(),
                "{refused:?} must not be authorable by a delegated owner"
            );
        }
        // The operator may write every one of them — the rule SEPARATES the two
        // authorities rather than refusing everything.
        for allowed in ["weather_*", "weather_get", "*"] {
            assert!(validate_patterns(&[allowed.to_string()], &Authoring::Operator).is_ok());
        }
    }

    /// A delegated owner's stored LOCAL pattern stops resolving, which is what
    /// covers the case an authoring check cannot: an operator authored it and
    /// was later demoted.
    #[test]
    fn a_demoted_owners_local_patterns_stop_resolving() {
        assert_eq!(
            resolve_stored(vec![authorized(
                stored_group(vec!["weather_*".into(), "peerhub::*".into()]),
                GroupOwner::Operator,
            )]),
            vec!["peerhub__alerts_list", "peerhub__ledger_add", "weather_alerts", "weather_get"],
        );
        // Same rows, same patterns — only the owner's live authority changed.
        assert_eq!(
            resolve_stored(vec![authorized(
                stored_group(vec!["weather_*".into(), "peerhub::*".into()]),
                GroupOwner::Delegated,
            )]),
            vec!["peerhub__alerts_list", "peerhub__ledger_add"],
            "the local patterns must collapse, and the namespaced one must survive"
        );
    }

    /// A federation user must not be able to grant themselves the fleet.
    #[test]
    fn bare_star_is_operator_only() {
        assert_eq!(
            Pattern::parse("*", GroupOwner::Operator).unwrap(),
            Pattern::Everything,
            "an operator may state a policy of everything"
        );
        assert!(
            Pattern::parse("*", GroupOwner::Delegated).is_err(),
            "a delegated owner may not grant themselves everything"
        );
        assert!(
            validate_group("g", "", &["*".to_string()], &Authoring::Delegated { owned: &delegated_owns() })
                .is_err(),
            "the refusal must hold through the group-level entry point too"
        );
    }

    #[test]
    fn patterns_round_trip_through_their_stored_form() {
        for raw in ["*", "peerhub::*", "peerhub::ledger*", "weather_*", "weather_get"] {
            let parsed = Pattern::parse(raw, GroupOwner::Operator).unwrap();
            assert_eq!(parsed.render(), raw);
            assert_eq!(Pattern::parse_stored(&parsed.render()), Some(parsed));
        }
    }

    #[test]
    fn group_names_are_normalized_and_bounded() {
        assert_eq!(normalize_group_name("  home   automation ").unwrap(), "home automation");
        assert_eq!(
            normalize_group_name("media ").unwrap(),
            normalize_group_name("media").unwrap(),
            "otherwise a trailing space defeats the UNIQUE constraint"
        );
        assert!(normalize_group_name("   ").is_err());
        assert!(normalize_group_name("").is_err());
        assert!(normalize_group_name(&"n".repeat(MAX_GROUP_NAME_CHARS + 1)).is_err());
        // Refused, not stripped: a name must read as what it is.
        assert!(normalize_group_name("me\u{202E}dia").is_err());
        assert!(normalize_group_name("me\u{200B}dia").is_err());
        assert!(normalize_group_name("me\u{0007}dia").is_err());
        // Non-ASCII is fine in a NAME (it is a human label, not a matcher).
        assert_eq!(normalize_group_name("médias").unwrap(), "médias");
    }

    #[test]
    fn validation_dedupes_and_bounds_patterns() {
        // Dedup is the subject; an OPERATOR authors it because RMCP-12 made
        // unqualified patterns operator-only (see
        // `a_delegated_author_may_not_write_an_unqualified_pattern`).
        let group = validate_group(
            " media ",
            "  the library  ",
            &["media_*".into(), "media_*".into()],
            &Authoring::Operator,
        )
        .unwrap();
        assert_eq!(group.name, "media");
        assert_eq!(group.description, "the library");
        assert_eq!(group.rendered_patterns(), vec!["media_*"]);

        // And the same for a delegated author, in the vocabulary they may use.
        let owned = delegated_owns();
        let delegated_group = validate_group(
            "peer media",
            "",
            &["peerhub::media_*".into(), "peerhub::media_*".into()],
            &Authoring::Delegated { owned: &owned },
        )
        .unwrap();
        assert_eq!(delegated_group.rendered_patterns(), vec!["peerhub::media_*"]);

        let too_many: Vec<String> = (0..=MAX_PATTERNS_PER_GROUP).map(|i| format!("t{i}_*")).collect();
        assert!(validate_group("g", "", &too_many, &Authoring::Operator).is_err());
    }

    /// An empty group is a legitimate stored state — it just grants nothing.
    #[test]
    fn an_empty_group_is_storable() {
        let owned = delegated_owns();
        let group =
            validate_group("new", "", &[], &Authoring::Delegated { owned: &owned }).unwrap();
        assert!(group.patterns.is_empty());
    }

    // ── Starter groups ───────────────────────────────────────────────────────

    /// Every seeded group must be valid, and none may smuggle in a bare `*`. A
    /// later edit that adds one fails here rather than seeding full access into
    /// a fresh install.
    ///
    /// Authored as an OPERATOR, which is what `seed_starter_groups` verifies its
    /// target account to be. RMCP-12 is why this is no longer expressed as
    /// "storable by a delegated owner": the starter groups are local prefixes
    /// over the FLEET's own tools, which a delegated owner may not hold at all
    /// now, so that formulation would test the wrong property. The `*` refusal
    /// it was standing in for is asserted directly below instead.
    #[test]
    fn starter_groups_are_valid_and_never_wildcard() {
        assert!(!STARTER_GROUPS.is_empty());
        for group in STARTER_GROUPS {
            let patterns: Vec<String> = group.patterns.iter().map(|p| (*p).to_string()).collect();
            assert!(
                !patterns.iter().any(|p| p.trim() == "*"),
                "starter group {:?} smuggles in a bare wildcard",
                group.name
            );
            let validated =
                validate_group(group.name, group.description, &patterns, &Authoring::Operator)
                    .unwrap_or_else(|e| panic!("starter group {:?} is invalid: {e}", group.name));
            assert!(!validated.patterns.is_empty(), "a seeded group that grants nothing is noise");
            assert!(!validated.patterns.contains(&Pattern::Everything));
            assert_eq!(validated.name, group.name, "seeded names should already be canonical");
        }
    }

    /// The seeded names are what an operator sees in a picker, so they must be
    /// distinct after normalisation — the name column is UNIQUE, and a
    /// duplicate would make seeding partially fail on a fresh install.
    #[test]
    fn starter_group_names_are_unique() {
        let mut seen: Vec<String> = STARTER_GROUPS
            .iter()
            .map(|g| normalize_group_name(g.name).unwrap())
            .collect();
        let total = seen.len();
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), total);
    }

    /// The aggregate bound. A per-group cap does not bound resolution, because
    /// resolution concatenates every group a client holds — so this refuses,
    /// rather than truncating, when the total is over the ceiling.
    ///
    /// Truncation is the tempting alternative and the wrong one: the surviving
    /// prefix would depend on row ordering, so two resolutions of one unchanged
    /// configuration could hand back different scopes, and neither would be the
    /// scope an operator configured. Denial is at least visible and stable.
    #[test]
    fn resolution_refuses_rather_than_truncates_past_the_aggregate_bound() {
        let cat = catalog();

        // At the ceiling: still resolves, and still resolves CORRECTLY.
        let ok: Vec<AuthorizedGroup> = (0..MAX_GROUPS_PER_CLIENT)
            .map(|_| authorized(stored_group(vec!["weather_*".into()]), GroupOwner::Operator))
            .collect();
        let resolved = resolve_groups(&ok, &cat).expect("exactly at the bound must resolve");
        assert_eq!(names(&resolved), vec!["weather_alerts", "weather_get"]);

        // One group past it: refused, and the error says why and by how much.
        let over: Vec<AuthorizedGroup> = (0..MAX_GROUPS_PER_CLIENT + 1)
            .map(|_| authorized(stored_group(vec!["weather_*".into()]), GroupOwner::Operator))
            .collect();
        let err = resolve_groups(&over, &cat).unwrap_err().to_string();
        assert!(err.contains(&MAX_GROUPS_PER_CLIENT.to_string()), "names the limit: {err}");
        assert!(err.contains("refused rather than truncated"), "and the disposition: {err}");

        // The pattern total is bounded independently of the group count, so a
        // few groups each holding a huge stored list cannot slip through.
        let fat: Vec<AuthorizedGroup> = (0..4)
            .map(|_| {
                let pats: Vec<String> =
                    (0..MAX_RESOLVED_PATTERNS / 3).map(|i| format!("t{i}_*")).collect();
                authorized(stored_group(pats), GroupOwner::Operator)
            })
            .collect();
        assert!(fat.len() <= MAX_GROUPS_PER_CLIENT, "the group count alone is within bounds");
        assert!(resolve_groups(&fat, &cat).is_err(), "the PATTERN total must bound it too");
    }

    /// The ceiling is exactly the product of the two write-time caps, so a
    /// client scoped within them can never trip the read-time refusal. If a
    /// future edit lowers a cap without thinking, this is what notices.
    #[test]
    fn the_read_bound_cannot_reject_a_write_bounded_client() {
        assert_eq!(MAX_RESOLVED_PATTERNS, MAX_GROUPS_PER_CLIENT * MAX_PATTERNS_PER_GROUP);
    }

    // ── Scale ────────────────────────────────────────────────────────────────

    /// Resolution over a fleet-sized catalog returns exactly the matching set.
    /// The point is not timing — it is that the answer stays exact at scale and
    /// that resolution does one pass per tool, never a pass per tool PAIR.
    #[test]
    fn resolution_stays_exact_over_a_fleet_sized_catalog() {
        let mut cat: Vec<CatalogTool> = (0..400).map(|i| CatalogTool::local(format!("fam{}_tool{i}", i % 40))).collect();
        cat.extend((0..200).map(|i| CatalogTool::from_upstream("up", &format!("tool{i}"))));

        let patterns = vec![
            Pattern::parse("fam7_*", GroupOwner::Operator).unwrap(),
            Pattern::parse("up::*", GroupOwner::Operator).unwrap(),
        ];
        let resolved = resolve(&patterns, &cat);
        let local_hits = cat.iter().filter(|t| t.name.starts_with("fam7_")).count();
        assert_eq!(resolved.len(), local_hits + 200);
        assert!(resolved.iter().all(|t| t.name.starts_with("fam7_") || t.name.starts_with("up__")));

        // And the bare wildcard covers the collision-resolved catalog exactly.
        let everything = resolve(&[Pattern::Everything], &cat);
        assert_eq!(everything.len(), cat.len(), "no duplicate advertised names in this fixture");
    }
}
