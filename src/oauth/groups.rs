//! Tool groups — the named, operator-managed grouping over the tool catalog
//! that makes connector scoping expressible in human terms.
//!
//! ## Why a group is not just a list of tool names
//! The fleet exports several hundred tools. Asking an operator to enumerate
//! them when minting a connector guarantees one of two outcomes: a
//! hand-authored list that goes stale the day a tool is added, or a shrug and a
//! wildcard. A group ("media", "home automation") is a small set of PATTERNS
//! resolved against the LIVE merged catalog on every list and every call, so a
//! newly registered tool that matches an existing pattern is included without a
//! config edit, and no pattern is ever frozen into a snapshot that quietly
//! diverges from what the server actually serves.
//!
//! ## Three rules this module exists to enforce
//!
//! **1. The syntax is deliberately minimal, and it is the syntax MESH-08
//! already uses.** An exact name, a trailing-`*` prefix, or a namespace form
//! (`<namespace>::*`, delimiter [`PATTERN_NS_SEP`] — deliberately NOT the `__`
//! that separates the halves of an advertised name; see [`Pattern::parse`]).
//! Nothing else parses. No regex: a pattern here may be authored by a DELEGATED
//! federation user (RMCP-12), and a regex from an untrusted author is a
//! denial-of-service against the dispatch path, which runs the matcher on every
//! request. No negation either — negation is the existing deny layer's job
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
//! RMCP-07 holds a SECOND copy of this matcher in its `scope.rs`, and it is the
//! copy wired into `decide()`. The two used DIFFERENT namespace delimiters —
//! this one `__`, that one `::` — which meant every namespace-qualified pattern
//! written here resolved to nothing there, while a `::` pattern written here
//! passed validation as an innocuous local prefix and was expanded by the
//! enforcer into a whole federated namespace. That is TERM #637; the resolution
//! was to standardise on `::`, which is what this module now uses.
//!
//! ## This module has no caller yet, and that is the design
//! [`resolve`] is not wired into `tools/list` or `tools/call`, and must not be
//! wired in here. Enforcement is RMCP-07's single `effective()` function, which
//! intersects the account's own grant with the client's groups and its visible
//! namespaces, and which backs BOTH the list filter and the call guard from one
//! definition. Calling this resolver directly from a dispatch path would create
//! a SECOND enforcement point — which is precisely the thing RMCP-07 exists to
//! prevent, because two authorization sites over one decision is how they come
//! to disagree, silently, in the widening direction. So this item ships the
//! matcher and the store and stops there, deliberately caller-less.
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
    /// One exact ADVERTISED name. A qualified pattern (`<ns>::<bare>`) is
    /// stored in advertised form (`<ns>__<bare>`), so matching is a plain
    /// comparison against the name a caller actually invokes.
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
        if parsed == Pattern::Everything && owner != GroupOwner::Operator {
            return Err(ToolError::InvalidArgument(
                "the bare `*` pattern is reserved for operator-owned groups; \
                 name the prefixes or namespaces this group needs instead"
                    .into(),
            ));
        }
        Ok(parsed)
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
            // A qualified EXACT pattern is stored as the ADVERTISED name it
            // denotes, so matching stays a plain string comparison and cannot
            // drift from how the merge layer builds that name. `render` splits
            // it back for display.
            return Ok(Pattern::Exact(crate::mesh::merge::namespaced(namespace, rest)));
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

    /// Whether this pattern covers `advertised` — the name a caller invokes.
    ///
    /// Total by construction: every arm is a string comparison, so there is no
    /// input on which this can fail, panic, or take super-linear time in the
    /// length of the name.
    ///
    /// ## The namespace boundary
    /// Matching is against the ADVERTISED name (already namespaced for a
    /// federated tool), and **an unqualified EXACT or PREFIX pattern matches
    /// only unqualified names**. A bare [`Pattern::Prefix`] is checked against
    /// LOCAL tools alone; it does not span [`MESH_NS_SEP`].
    ///
    /// [`Pattern::Everything`] is deliberately excluded from that rule and DOES
    /// reach federated tools — see the module docs for why, and note the
    /// wording: it is unqualified EXACT and PREFIX patterns that are local-only,
    /// not "any bare pattern", which read literally would swallow `*` too.
    ///
    /// Review round 3 found why this has to be structural rather than
    /// incidental. The previous revision ran a plain `starts_with` over the
    /// advertised name and reasoned that a prefix therefore "stays on its own
    /// side" — which is true only until a namespace happens to share the
    /// prefix's letters. `peer*`, written to scope local `peer_*` tools,
    /// silently matched `peerhub__alerts_list`: an entire federated server
    /// swept in because of a string coincidence the author cannot see in what
    /// they wrote. The reachable shape is a bare prefix that is a strict prefix
    /// of a namespace NAME, and it widens authorization purely by accident.
    ///
    /// So absence of a namespace qualifier means "local only", never "anything
    /// that happens to start this way" — the same fail-closed rule the rest of
    /// this module applies: absence is the empty set, never a wider one.
    /// Reaching a federated tool takes a pattern that NAMES the upstream
    /// ([`Pattern::Namespace`] or [`Pattern::NamespacedPrefix`]), which an
    /// author can only write deliberately, and which the client's own
    /// `rmcp_client_server` rows must still permit before any of it resolves
    /// (RMCP-07). Two patterns that look alike are now firmly different:
    /// `peer*` is local-only, `peerhub::*` is that upstream.
    ///
    /// **RMCP-07 must adopt these semantics.** It currently carries its own
    /// copy of this matcher in `scope.rs`, written before this item landed and
    /// documented in-file as the thing to delete once it did. Until that
    /// collapse happens the two must not disagree on this rule — a bare prefix
    /// honoured by one matcher and refused by the other is exactly the split
    /// that produces a permit nobody intended.
    pub fn matches(&self, advertised: &str) -> bool {
        match self {
            Pattern::Everything => true,
            // Every arm below uses the mesh's own splitter rather than a
            // `starts_with` on `"ns__"`, so "advertised by ns" means exactly
            // what the merge layer means by it and cannot drift if the
            // separator ever changes.
            Pattern::Namespace(ns) => {
                matches!(split_namespaced(advertised), Some((found, _)) if found == ns.as_str())
            }
            Pattern::NamespacedPrefix { namespace, prefix } => {
                matches!(
                    split_namespaced(advertised),
                    Some((found, bare)) if found == namespace.as_str() && bare.starts_with(prefix.as_str())
                )
            }
            // The boundary. `split_namespaced` returning `None` IS the
            // definition of a local name, so this cannot disagree with the
            // arms above about where a namespace begins.
            Pattern::Prefix(prefix) => {
                split_namespaced(advertised).is_none() && advertised.starts_with(prefix.as_str())
            }
            // An exact pattern names the whole advertised string, separator and
            // all, so it is already explicit about which side it is on.
            Pattern::Exact(name) => advertised == name.as_str(),
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
            // An exact pattern stores the ADVERTISED name. A qualified one is
            // split back onto the pattern delimiter so it round-trips; a local
            // one carries no `__` at all (the parser refuses that), so this
            // cannot mangle it.
            Pattern::Exact(name) => match split_namespaced(name) {
                Some((namespace, bare)) => format!("{namespace}{PATTERN_NS_SEP}{bare}"),
                None => name.clone(),
            },
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
    owner: GroupOwner,
) -> Result<ValidatedGroup, ToolError> {
    Ok(ValidatedGroup {
        name: normalize_group_name(name)?,
        description: normalize_description(description)?,
        patterns: validate_patterns(patterns, owner)?,
    })
}

/// Parse and bound a pattern list on its own — the half of
/// [`validate_group`] an edit that leaves the name alone needs.
///
/// Split out so an update path does not have to invent a placeholder name to
/// reach pattern validation: the one thing that must never be skippable is the
/// parse, so it lives where every write can reach it directly.
pub fn validate_patterns(patterns: &[String], owner: GroupOwner) -> Result<Vec<Pattern>, ToolError> {
    if patterns.len() > MAX_PATTERNS_PER_GROUP {
        return Err(ToolError::InvalidArgument(format!(
            "a group may hold at most {MAX_PATTERNS_PER_GROUP} patterns"
        )));
    }

    let mut parsed: Vec<Pattern> = Vec::with_capacity(patterns.len());
    for raw in patterns {
        let pattern = Pattern::parse(raw, owner)?;
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
        .filter(|tool| patterns.iter().any(|p| p.matches(&tool.name)))
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
impl<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> for AuthorizedGroup {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
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
                match Pattern::parse_stored(raw) {
                    // The revocation check. Not a filter over the RESULT set —
                    // dropping the pattern is what makes an unauthorized `*`
                    // resolve to nothing rather than to the catalog minus
                    // something.
                    Some(Pattern::Everything) if owner != GroupOwner::Operator => None,
                    other => other,
                }
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
                    stored_group(vec!["*".into(), "weather_*".into()]),
                    GroupOwner::Delegated,
                ),
                authorized(stored_group(vec!["news_headlines".into()]), GroupOwner::Operator),
            ]),
            vec!["news_headlines", "weather_alerts", "weather_get"],
            "the delegated group keeps its explicit prefix; only its `*` is dropped"
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
            authorized(stored_group(vec!["news_*".into()]), GroupOwner::Delegated),
        ]);
        assert_eq!(resolved, vec!["news_headlines"], "no wildcard survives here");
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
            .matches("peerhub__alerts_list"));
        assert!(!Pattern::parse("ledger_*", GroupOwner::Operator)
            .unwrap()
            .matches("peerhub__ledger_add"));
        // Reaching an upstream tool takes a pattern that names the upstream.
        assert!(Pattern::parse("peerhub::ledger*", GroupOwner::Operator)
            .unwrap()
            .matches("peerhub__ledger_add"));
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
        assert!(bare.matches("peer_status"), "a LOCAL tool named peer... must still match");
        assert!(
            !bare.matches("peerhub__alerts_list"),
            "a bare prefix must not cross the mesh separator into a namespace \
             that merely shares its letters"
        );
        assert!(!bare.matches("peerhub__ledger_add"));

        // Written deliberately, with the upstream named, it still reaches in.
        let qualified = Pattern::parse("peerhub::*", GroupOwner::Operator).unwrap();
        assert!(qualified.matches("peerhub__alerts_list"));
        assert!(!qualified.matches("peer_status"), "and it does NOT reach back out to local");

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
        assert!(all.matches("weather_get"), "local");
        assert!(all.matches("peerhub__alerts_list"), "federated — NOT local-only");
        assert!(all.matches("sensors__node_status"));

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
        assert!(p.matches("peerhub__ledger_add"));
        assert!(!p.matches("sensors__ledger_add"), "a different upstream is a different namespace");
        assert!(!p.matches("ledger_add"), "and it never matches the LOCAL tool of that name");
        assert!(!p.matches("peerhub__alerts_list"));
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
        assert!(p.matches("ledger_add"), "the local tool");
        assert!(!p.matches("peerone__ledger_accounts"));
        assert!(!p.matches("peerhub__ledger_add"));
        // The pathological namespace: one literally named after the family.
        // `ledger___accounts` splits as namespace `ledger` + bare `_accounts`,
        // so it is federated and a bare prefix must not reach it.
        assert!(!p.matches("ledger___accounts"));
    }

    /// A namespace pattern is anchored on the FIRST separator, so it cannot be
    /// satisfied by a namespace that merely starts with the same letters.
    #[test]
    fn namespace_pattern_is_not_a_loose_prefix() {
        let p = Pattern::parse("peerhub::*", GroupOwner::Operator).unwrap();
        assert!(p.matches("peerhub__anything"));
        assert!(!p.matches("peerhub0__anything"), "a longer namespace is a different namespace");
        assert!(!p.matches("peerhub_local_tool"), "a single underscore is not the separator");
        assert!(!p.matches("peerhub"));
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
    /// The trailing-underscore case (`foo___*` → namespace `foo_`) is the one
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
        assert!(p.matches("a__b__c"), "namespace `a`, bare name `b__c`");
        assert!(!p.matches("a__c"));
    }

    /// TERM #637B symmetry: the AUTHORING matcher here and the ENFORCING
    /// matcher in RMCP-07's `scope.rs` must select the same set for every
    /// pattern this validator accepts. Mirrors 637B's own qualified-forms test,
    /// case for case, so a divergence shows up on whichever side changes first.
    ///
    /// The representations differ deliberately and are equivalent: 637B holds
    /// `NamespacedExact(ns, bare)` and compares the split halves; this side
    /// stores the ADVERTISED name and compares it whole. Those agree because
    /// `split_namespaced` cuts at the FIRST `__`, so exactly one advertised
    /// string splits to any given `(ns, bare)` — the mapping is a bijection, not
    /// an approximation.
    #[test]
    fn qualified_forms_agree_with_the_enforcing_matcher() {
        let exact = Pattern::parse("peerone::weather_now", GroupOwner::Operator).unwrap();
        assert_eq!(exact, Pattern::Exact("peerone__weather_now".into()));
        assert!(exact.matches("peerone__weather_now"));
        assert!(!exact.matches("peertwo__weather_now"), "not another namespace");
        assert!(!exact.matches("weather_now"), "not the local tool of the same name");
        assert!(!exact.matches("peerone__weather_forecast"), "not a sibling");

        let prefix = Pattern::parse("peerone::weather_*", GroupOwner::Operator).unwrap();
        assert_eq!(
            prefix,
            Pattern::NamespacedPrefix { namespace: "peerone".into(), prefix: "weather_".into() }
        );
        assert!(prefix.matches("peerone__weather_now"));
        assert!(prefix.matches("peerone__weather_forecast"));
        assert!(!prefix.matches("peertwo__weather_now"), "cannot leak across the boundary");
        assert!(!prefix.matches("weather_now"), "and does not reach local tools");
        assert!(!prefix.matches("peerone__media_search"));

        // The qualified prefix matches the BARE name, so a prefix equal to the
        // namespace's own text does not match by accident.
        let bare = Pattern::parse("peerone::peer*", GroupOwner::Operator).unwrap();
        assert!(bare.matches("peerone__peer_status"));
        assert!(!bare.matches("peerone__weather_now"));

        // And the exact form round-trips back to `::` for display.
        assert_eq!(exact.render(), "peerone::weather_now");
        assert_eq!(Pattern::parse_stored(&exact.render()), Some(exact));
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
        assert!(p.matches("peerhub__alerts_list"), "it means the upstream, and says so");
        assert!(!p.matches("peerhub_local_tool"), "and not a local tool of a similar name");

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
        assert_eq!(p, Pattern::Exact("peerhub__ledger_add".into()));
        assert!(p.matches("peerhub__ledger_add"));
        assert!(!p.matches("peerhub__ledger_add_v2"), "an exact pattern must not act as a prefix");
    }

    /// Matching is TOTAL: whatever a name looks like, matching answers
    /// yes-or-no. An error here would be an availability failure on dispatch.
    #[test]
    fn matching_never_fails_on_a_strange_name() {
        let p = Pattern::parse("weather_*", GroupOwner::Operator).unwrap();
        let long = "w".repeat(4096);
        for name in ["", "__", "___", "*", "weather_", "wéather_x", "\u{200B}", long.as_str()] {
            let _ = p.matches(name);
        }
        assert!(p.matches("weather_"), "the prefix itself is covered");
        assert!(!p.matches(""));
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
            validate_group("g", "", &["*".to_string()], GroupOwner::Delegated).is_err(),
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
        let group = validate_group(
            " media ",
            "  the library  ",
            &["media_*".into(), "media_*".into()],
            GroupOwner::Delegated,
        )
        .unwrap();
        assert_eq!(group.name, "media");
        assert_eq!(group.description, "the library");
        assert_eq!(group.rendered_patterns(), vec!["media_*"]);

        let too_many: Vec<String> = (0..=MAX_PATTERNS_PER_GROUP).map(|i| format!("t{i}_*")).collect();
        assert!(validate_group("g", "", &too_many, GroupOwner::Operator).is_err());
    }

    /// An empty group is a legitimate stored state — it just grants nothing.
    #[test]
    fn an_empty_group_is_storable() {
        let group = validate_group("new", "", &[], GroupOwner::Delegated).unwrap();
        assert!(group.patterns.is_empty());
    }

    // ── Starter groups ───────────────────────────────────────────────────────

    /// Every seeded group must be storable by a DELEGATED owner — which is the
    /// check that none of them smuggles in a bare `*`. A later edit that adds
    /// one fails here rather than seeding full access into a fresh install.
    #[test]
    fn starter_groups_are_valid_and_never_wildcard() {
        assert!(!STARTER_GROUPS.is_empty());
        for group in STARTER_GROUPS {
            let patterns: Vec<String> = group.patterns.iter().map(|p| (*p).to_string()).collect();
            let validated =
                validate_group(group.name, group.description, &patterns, GroupOwner::Delegated)
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
            .map(|_| authorized(stored_group(vec!["weather_*".into()]), GroupOwner::Delegated))
            .collect();
        let resolved = resolve_groups(&ok, &cat).expect("exactly at the bound must resolve");
        assert_eq!(names(&resolved), vec!["weather_alerts", "weather_get"]);

        // One group past it: refused, and the error says why and by how much.
        let over: Vec<AuthorizedGroup> = (0..MAX_GROUPS_PER_CLIENT + 1)
            .map(|_| authorized(stored_group(vec!["weather_*".into()]), GroupOwner::Delegated))
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
                authorized(stored_group(pats), GroupOwner::Delegated)
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
