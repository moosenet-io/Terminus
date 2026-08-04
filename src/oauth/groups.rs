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
//! (`<namespace>__*`, the separator [`crate::mesh::merge::MESH_NS_SEP`]).
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
    /// `*` — every tool in the catalog. Operator-only (see [`GroupOwner`]).
    Everything,
    /// `<namespace>__*` — every tool advertised by one mesh upstream.
    Namespace(String),
    /// `<prefix>*` — every advertised name starting with `prefix`.
    Prefix(String),
    /// An exact advertised name.
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

    /// The shared syntax check. Order matters: character legality is checked
    /// before shape, so a pattern with a control character is rejected as such
    /// rather than being reported as an odd prefix.
    fn parse_syntax_checked(raw: &str) -> Result<Self, ToolError> {
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
        // `*` is understood ONLY as a trailing wildcard. `a*b` and `**` read to
        // a human as globs and would be matched here as neither, so they are
        // refused rather than quietly reinterpreted — the same fail-closed
        // stance `gateway_framework`'s grant validation takes on the same
        // character, for the same reason.
        if raw[..raw.len() - 1].contains('*') {
            return Err(ToolError::InvalidArgument(
                "`*` is only meaningful as the LAST character of a pattern; \
                 there is no general glob or regex syntax here"
                    .into(),
            ));
        }

        let Some(head) = raw.strip_suffix('*') else {
            return Ok(Pattern::Exact(raw.to_string()));
        };
        if head.is_empty() {
            return Ok(Pattern::Everything);
        }
        // `ns__*` is the namespace form. Recognising it here (rather than
        // leaving it as a prefix that happens to end in the separator) is what
        // lets it render back identically and lets the matcher use the mesh's
        // own splitter instead of a second, parallel notion of "namespaced".
        if let Some(namespace) = head.strip_suffix(MESH_NS_SEP) {
            if namespace.is_empty() || namespace.contains(MESH_NS_SEP) {
                return Err(ToolError::InvalidArgument(format!(
                    "the namespace form is `<namespace>{MESH_NS_SEP}*` with a single, non-empty namespace"
                )));
            }
            return Ok(Pattern::Namespace(namespace.to_string()));
        }
        Ok(Pattern::Prefix(head.to_string()))
    }

    /// Whether this pattern covers `advertised` — the name a caller invokes.
    ///
    /// Total by construction: every arm is a string comparison, so there is no
    /// input on which this can fail, panic, or take super-linear time in the
    /// length of the name.
    ///
    /// Matching is against the ADVERTISED name, never a bare tool name, and
    /// that is what keeps a prefix inside its own namespace: `a*` cannot reach
    /// `peerhub__alerts_list`, because that name starts with its namespace, not
    /// with `a`. A pattern that means to cross into an upstream has to say so.
    ///
    /// A prefix that DOES span the separator (`peerhub__ledger*`) reaches into
    /// that namespace, and that is intended — this is MESH-08's prefix
    /// semantics, unchanged. The distinction worth being precise about is what
    /// the boundary is FOR: it protects against a short bare prefix
    /// ACCIDENTALLY sweeping in every federated tool whose namespace happens to
    /// start with the same letters, which is a mistake an author cannot see in
    /// the pattern they wrote. It is not a barrier against an author who
    /// deliberately types an upstream's name — that author has said exactly
    /// what they meant, the namespace is right there in the text, and the
    /// client's own `rmcp_client_server` rows still have to permit that
    /// namespace before any of it resolves (RMCP-07). Forbidding it would only
    /// mean an operator scoping one upstream's ledger tools had to enumerate
    /// them by hand — the very thing groups exist to avoid.
    pub fn matches(&self, advertised: &str) -> bool {
        match self {
            Pattern::Everything => true,
            // Uses the mesh's own splitter rather than a `starts_with` on
            // `"ns__"`, so `ns__*` means exactly what the merge layer means by
            // "advertised by ns" and cannot drift if the separator changes.
            Pattern::Namespace(ns) => {
                matches!(split_namespaced(advertised), Some((found, _)) if found == ns.as_str())
            }
            Pattern::Prefix(prefix) => advertised.starts_with(prefix.as_str()),
            Pattern::Exact(name) => advertised == name.as_str(),
        }
    }

    /// The canonical stored form. Round-trips through [`Self::parse_stored`],
    /// so storing the rendering of a parse is idempotent — which is what lets
    /// the write path normalise without changing meaning.
    pub fn render(&self) -> String {
        match self {
            Pattern::Everything => "*".to_string(),
            Pattern::Namespace(ns) => format!("{ns}{MESH_NS_SEP}*"),
            Pattern::Prefix(prefix) => format!("{prefix}*"),
            Pattern::Exact(name) => name.clone(),
        }
    }
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
pub fn resolve_groups<'a>(
    groups: &[AuthorizedGroup],
    catalog: &'a [CatalogTool],
) -> Vec<&'a CatalogTool> {
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
    resolve(&patterns, catalog)
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
        names(&resolve_groups(&groups, &cat))
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
        assert!(resolve_groups(&[authorized(group, GroupOwner::Operator)], &cat).is_empty());
    }

    /// A well-formed pattern that matches no tool in the current catalog is the
    /// other half of the same invariant: zero matches is zero, never all.
    #[test]
    fn a_pattern_matching_nothing_resolves_to_the_empty_set() {
        assert!(resolve_raw(&["no_such_tool"], GroupOwner::Operator).is_empty());
        assert!(resolve_raw(&["nothing_starts_with_this_*"], GroupOwner::Operator).is_empty());
        assert!(resolve_raw(&["absent_namespace__*"], GroupOwner::Operator).is_empty());
    }

    /// A stored pattern that no longer parses must contribute nothing, rather
    /// than being read as a wildcard or erroring on the dispatch path.
    #[test]
    fn an_unparseable_stored_pattern_grants_nothing() {
        let cat = catalog();
        let group = stored_group(vec!["we*ther_*".into(), "".into()]);
        assert!(!group.is_empty(), "the row has patterns; they simply do not parse");
        assert!(resolve_groups(&[authorized(group, GroupOwner::Operator)], &cat).is_empty());
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
            resolve_raw(&["peerhub__*"], GroupOwner::Operator),
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
        assert!(Pattern::parse("peerhub__ledger*", GroupOwner::Operator)
            .unwrap()
            .matches("peerhub__ledger_add"));
    }

    /// A namespace pattern is anchored on the FIRST separator, so it cannot be
    /// satisfied by a namespace that merely starts with the same letters.
    #[test]
    fn namespace_pattern_is_not_a_loose_prefix() {
        let p = Pattern::parse("peerhub__*", GroupOwner::Operator).unwrap();
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
            let patterns = vec![Pattern::parse("peerhub__*", GroupOwner::Operator).unwrap()];
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
            "*weather",              // leading star is not supported syntax
            "weather get",           // whitespace can never match a tool name
            "weather\u{200B}_*",     // invisible character
            "weather\n_*",           // control character
            "wéather_*",             // non-ASCII cannot match an ASCII registry
            "__*",                   // empty namespace
            "a__b__*",               // ambiguous double namespace
        ] {
            assert!(
                Pattern::parse(bad, GroupOwner::Operator).is_err(),
                "{bad:?} must be refused at write time"
            );
        }
        let too_long = "x".repeat(MAX_PATTERN_CHARS + 1);
        assert!(Pattern::parse(&too_long, GroupOwner::Operator).is_err());
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
        for raw in ["*", "peerhub__*", "weather_*", "weather_get"] {
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
            Pattern::parse("up__*", GroupOwner::Operator).unwrap(),
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
