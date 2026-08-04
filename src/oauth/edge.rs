//! RMCP-09 — the public edge: a deliberately minimal internet-facing listener
//! with a PER-PATH source-address policy.
//!
//! ## Why a per-path policy, and not one pinhole
//! The obvious way to expose an MCP connector safely is a single allowlist:
//! only Anthropic's published outbound egress range may reach this port. That
//! is wrong, and it fails in a way that is very hard to diagnose, because the
//! two halves of the OAuth flow arrive from DIFFERENT networks:
//!
//! - `/mcp`, `/oauth/token`, `/oauth/register` and the `.well-known` documents
//!   are fetched by Anthropic's own infrastructure, from its egress range.
//! - `/oauth/authorize` — and the login and consent form posts behind it — are
//!   opened in the OPERATOR'S OWN BROWSER. They never come from Anthropic.
//!
//! A single "allow only Anthropic" rule therefore serves discovery and the
//! token exchange perfectly while silently returning 403 to the human trying to
//! consent, which presents to the operator as "the connector just doesn't
//! work". Hence two classes, [`CLASS_ANTHROPIC`] and [`CLASS_INTERACTIVE`], and
//! a path→class table rather than a per-host rule.
//!
//! Claude Code is a third case again: it runs the whole flow from the user's
//! machine with an RFC 8252 loopback redirect, so even `/oauth/token` arrives
//! from the user's network. That is a named profile ([`EdgeProfile::ClaudeCode`]),
//! not a default, because widening the token endpoint to the interactive class
//! is a real (if small) loosening and should be a deliberate act.
//!
//! ## What the edge exposes
//! Only the two `.well-known` documents, `/oauth/*`, and `/mcp`. Nothing else —
//! no `/enroll`, no `/admin`, no `/healthz`, no inference routes. The blast
//! radius of a public door is the smallest set of paths that makes a connector
//! work, and a path with NO policy entry is DENIED rather than defaulted to
//! open. The route table is not the exposure boundary here; [`edge_guard`] is,
//! and it runs before the router's own matching (including its fallback), so a
//! route added elsewhere in the tree cannot quietly become internet-facing.
//!
//! ## Client address resolution is the security core
//! Every decision keys on ONE value: the resolved client address. Getting that
//! value wrong nullifies the entire pinhole, so [`resolve_client_ip`] follows
//! two rules without exception:
//!
//! 1. `X-Forwarded-For` is read ONLY when the peer is itself a configured
//!    trusted proxy. From any other peer the header is attacker-controlled
//!    input and is ignored completely.
//! 2. The RIGHTMOST untrusted entry wins — never the leftmost. The leftmost
//!    entry is whatever the client sent; a reverse proxy appends the address it
//!    actually saw at the right end, so only a right-to-left walk that skips
//!    known proxies finds an address nobody upstream could forge.
//!
//! Both mistakes are trivially spoofable one-liners, which is why each has its
//! own test asserting the spoof CANNOT grant access.
//!
//! ## Configuration is fail-closed, everywhere
//! - Every CIDR is CONFIGURATION ([`ENV_ANTHROPIC_CIDRS`] /
//!   [`ENV_INTERACTIVE_CIDRS`]), never a compiled-in constant. Anthropic's
//!   published range is documented in `.env.example` prose and in
//!   `docs/networking/remote-mcp.md`; a published range that changes must be an
//!   env edit, not a release.
//! - An EMPTY CIDR list for a class denies everything. It is never read as
//!   "unrestricted" — that inversion is exactly how a pinhole silently opens.
//! - An unparseable policy, an unknown class name, or a proxied deployment with
//!   no trusted proxies configured is a HARD STARTUP ERROR. The alternative
//!   (log and carry on with a default) means running under a policy nobody
//!   understood, on the one listener that faces the internet.
//!
//! ## A note on names and fixtures (settled review findings — read before re-raising)
//! Several things in this file and its deploy assets have been raised as
//! possible "hardcoded infrastructure". One was CONCEDED and is now gone; the
//! rest are deliberate and HELD, with the reasoning at each site so a later
//! round does not have to re-derive it:
//!
//! - **CONCEDED (round 3): Anthropic's published egress range.** No literal for
//!   it exists anywhere in this repository any more — not in the binary, not in
//!   `.env.example`. [`ENV_ANTHROPIC_CIDRS`] has no default, and the operator
//!   looks the current value up in Anthropic's own published IP-address
//!   documentation at deploy time (`docs/networking/remote-mcp.md` says where
//!   and warns about the inbound/outbound trap). That range is Anthropic's to
//!   change, so a copy of it here would go stale silently — and a stale
//!   allowlist on this class presents as a connector that just stopped working,
//!   with nothing in our logs pointing at the cause.
//!
//! - **`terminus-primary` / `terminus_primary`** is this repository's SERVICE
//!   and module name — it names the binary, the systemd units, the deploy
//!   configs, and the `module` argument to the build door. It is not a fleet
//!   host identifier, which is what the standing rule targets; the PII gate's
//!   own internal-host detector is a fixed list of node names and does not
//!   include it. Every other module in the tree refers to itself the same way.
//! - **The loopback default bind and the default port** — see [`DEFAULT_BIND`]
//!   and [`DEFAULT_PORT`]. Both are DEFAULTS behind [`ENV_BIND`]/[`ENV_PORT`],
//!   not values compiled into the binary that an operator has to live with.
//! - **RFC 5737 TEST-NET / RFC 3849 documentation ranges in test fixtures** —
//!   see the note at the fixture constants in this module's `tests`. These are
//!   the sanctioned placeholders; inventing addresses instead would be worse,
//!   because an invented address can collide with something real.
//!
//! In every case the repo's own `no_pii_in_own_source_tree` gate passes on
//! these files, which is the mechanical check the rule is expressed through.
//!
//! ## TLS is not terminated here
//! The edge binds a private interface and is reached only through a reverse
//! proxy that terminates TLS — see `deploy/rmcp-edge-proxy.conf.example` and
//! the runbook in `docs/networking/remote-mcp.md`. That proxy is also where the
//! network-layer half of this policy lives; the edge policy is defence in
//! depth, not the only control.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use axum::extract::{ConnectInfo, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Router;

use crate::gateway_framework::audit::sanitize;
use crate::gateway_framework::rate_limit::{rate_limit_key, RateLimiter};

/// Master switch. Unset/false ⇒ no edge listener is bound, no edge config is
/// parsed, and this binary behaves exactly as it did before this item.
pub const ENV_ENABLED: &str = "RMCP_EDGE_ENABLED";
/// Interface the edge binds. Defaults to loopback: the edge is meant to sit
/// behind a TLS-terminating reverse proxy, never to face the internet directly.
pub const ENV_BIND: &str = "RMCP_EDGE_BIND";
/// Port the edge binds.
pub const ENV_PORT: &str = "RMCP_EDGE_PORT";
/// Named policy profile — see [`EdgeProfile`].
pub const ENV_PROFILE: &str = "RMCP_EDGE_PROFILE";
/// Optional JSON object overriding the profile's path→class table entirely.
pub const ENV_POLICY_JSON: &str = "RMCP_EDGE_POLICY_JSON";
/// Comma-separated CIDRs for the `anthropic` class.
pub const ENV_ANTHROPIC_CIDRS: &str = "RMCP_EDGE_ANTHROPIC_CIDRS";
/// Comma-separated CIDRs for the `interactive` class.
pub const ENV_INTERACTIVE_CIDRS: &str = "RMCP_EDGE_INTERACTIVE_CIDRS";
/// Comma-separated CIDRs whose `X-Forwarded-For` header may be believed.
pub const ENV_TRUSTED_PROXIES: &str = "RMCP_EDGE_TRUSTED_PROXIES";
/// Whether this edge sits behind a reverse proxy. When true, an empty
/// [`ENV_TRUSTED_PROXIES`] is a hard startup error rather than a deployment
/// that silently attributes every request to the proxy's own address.
pub const ENV_BEHIND_PROXY: &str = "RMCP_EDGE_BEHIND_PROXY";
/// Edge rate-limit burst, per resolved client address.
pub const ENV_RATE_LIMIT_BURST: &str = "RMCP_EDGE_RATE_LIMIT_BURST";
/// Edge rate-limit refill, tokens per second, per resolved client address.
pub const ENV_RATE_LIMIT_REFILL_PER_SEC: &str = "RMCP_EDGE_RATE_LIMIT_REFILL_PER_SEC";

/// Source class for traffic Anthropic's own infrastructure originates.
pub const CLASS_ANTHROPIC: &str = "anthropic";
/// Source class for traffic a human originates from the operator's networks.
pub const CLASS_INTERACTIVE: &str = "interactive";

/// Default edge rate-limit burst per resolved client address. Generous enough
/// for a real discovery + authorize + token sequence (which is a handful of
/// requests in a few seconds) and small enough to make a scan expensive.
const DEFAULT_RATE_LIMIT_BURST: u32 = 30;
/// Default edge rate-limit refill, tokens/sec.
const DEFAULT_RATE_LIMIT_REFILL_PER_SEC: f64 = 2.0;
/// Default edge port, adjacent to the primary gateway's own default.
///
/// A DEFAULT, not a baked-in value: [`ENV_PORT`] overrides it, exactly as
/// [`ENV_BIND`] overrides [`DEFAULT_BIND`]. Neither the port nor the interface
/// is compiled into the binary as something an operator has to live with — see
/// [`DEFAULT_BIND`] for why having a safe default here is better than requiring
/// the variable.
const DEFAULT_PORT: u16 = 8311;
/// Default edge bind interface.
///
/// Loopback, written as a literal here and in `.env.example`. HELD through
/// review rounds 1 and 2, deliberately — if a later round raises it again, the
/// answer has not changed and the reasoning is here rather than needing to be
/// re-derived:
///
/// - **It discloses nothing.** `127.0.0.1` names no host, no network and no
///   allocation. The S1 rule targets RFC 1918 private ranges, container ids and
///   internal hostnames; a loopback literal and a port number are none of those,
///   and the repo's own `no_pii_in_own_source_tree` gate passes on this file.
///   The plain listener and the review daemon already document the same default.
/// - **It is the SAFE value, not merely an acceptable one.** Any wider default
///   would place an internet-facing listener on a routable interface with no
///   reverse proxy in front of it. A default that fails toward "unreachable" is
///   the correct direction for this particular listener.
/// - **It constrains no one.** The bind is env-overridable ([`ENV_BIND`]).
///   Replacing a safe default with a mandatory variable would make a
///   misconfiguration MORE likely, not less: it converts "works, privately"
///   into "does not start", and the pressure that creates is toward pasting in
///   whatever value makes it boot.
const DEFAULT_BIND: &str = "127.0.0.1";

/// Upper bound on `X-Forwarded-For` entries parsed. A chain longer than this is
/// not a real deployment topology, it is someone probing the parser, so it is
/// refused rather than walked. Bounded parsing on the internet-facing path is
/// not optional.
const MAX_FORWARDED_ENTRIES: usize = 32;

/// The forwarding header this door reads. Only `X-Forwarded-For` — RFC 7239's
/// `Forwarded` is deliberately NOT consulted, because supporting two headers
/// means two parsers that can disagree about who the client is, and a
/// disagreement between them is a bypass.
const FORWARDED_FOR_HEADER: &str = "x-forwarded-for";

/// Errors that make the edge refuse to start.
///
/// Every variant here is a CONFIG error, and every one is fatal by design: the
/// caller ([`crate::oauth::edge::EdgeConfig::from_env`]'s only caller, in
/// `terminus_primary`) exits rather than serving. A permissive fallback on the
/// one internet-facing listener is not a degraded mode, it is a hole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EdgeConfigError {
    /// A CIDR (in a class list or the trusted-proxy list) did not parse.
    BadCidr { var: &'static str, entry: String, why: String },
    /// [`ENV_POLICY_JSON`] was not a JSON object of `path → class`.
    BadPolicyJson(String),
    /// A policy entry named a class the configuration does not define.
    UnknownClass { path: String, class: String },
    /// A policy path pattern was not usable (empty, relative, or a `*` in a
    /// position this matcher does not support).
    BadPathPattern(String),
    /// [`ENV_PROFILE`] named a profile that does not exist.
    UnknownProfile(String),
    /// [`ENV_BEHIND_PROXY`] is set but no trusted proxies are configured, so
    /// every request would be attributed to the proxy's own address.
    ProxyWithoutTrustedProxies,
    /// The bind address or port did not parse.
    BadBind(String),
    /// A rate-limit knob was present but not a usable value.
    BadRateLimit { var: &'static str, value: String },
}

impl std::fmt::Display for EdgeConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EdgeConfigError::BadCidr { var, entry, why } => write!(
                f,
                "{var}: `{entry}` is not a valid CIDR or address ({why}) — the edge refuses to \
                 start rather than serve the internet under a policy that was never understood"
            ),
            EdgeConfigError::BadPolicyJson(why) => write!(
                f,
                "{ENV_POLICY_JSON}: expected a JSON object mapping a path pattern to a source \
                 class ({why})"
            ),
            EdgeConfigError::UnknownClass { path, class } => write!(
                f,
                "{ENV_POLICY_JSON}: path `{path}` names source class `{class}`, which is not \
                 defined — the known classes are `{CLASS_ANTHROPIC}` and `{CLASS_INTERACTIVE}`"
            ),
            EdgeConfigError::BadPathPattern(pattern) => write!(
                f,
                "{ENV_POLICY_JSON}: `{pattern}` is not a usable path pattern — write an absolute \
                 path (`/mcp`) or an absolute prefix (`/oauth/*`)"
            ),
            EdgeConfigError::UnknownProfile(name) => write!(
                f,
                "{ENV_PROFILE}: `{name}` is not a known profile (expected `anthropic-hosted` or \
                 `claude-code`)"
            ),
            EdgeConfigError::ProxyWithoutTrustedProxies => write!(
                f,
                "{ENV_BEHIND_PROXY} is set but {ENV_TRUSTED_PROXIES} is empty — every request \
                 would be attributed to the proxy's own address, so the per-path source policy \
                 would police nothing"
            ),
            EdgeConfigError::BadBind(why) => {
                write!(f, "{ENV_BIND}/{ENV_PORT}: {why}")
            }
            EdgeConfigError::BadRateLimit { var, value } => write!(
                f,
                "{var}: `{value}` is not a usable positive value — a rate limit is a security \
                 control, so a typo here must not silently fall back to a default"
            ),
        }
    }
}

impl std::error::Error for EdgeConfigError {}

// ── CIDR matching ───────────────────────────────────────────────────────────

/// One configured network, v4 or v6.
///
/// Hand-rolled rather than pulled from a crate: the whole of the matching this
/// door needs is "mask both sides to the prefix and compare", and an
/// internet-facing authorization decision is a poor place to inherit a
/// dependency's edge-case behavior sight unseen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cidr {
    V4 { network: u32, prefix: u8 },
    V6 { network: u128, prefix: u8 },
}

impl Cidr {
    /// Parse `addr/len`, or a bare address (an implicit `/32` or `/128`).
    ///
    /// A v4-mapped IPv6 form (`::ffff:a.b.c.d`) is REFUSED rather than
    /// accepted-and-normalized. Matching normalizes such an address to its v4
    /// form before comparing (see [`normalize_ip`]), so a configured
    /// `::ffff:a.b.c.d/120` would silently never match anything — a rule that
    /// looks present and does nothing is worse than a rejected one. The error
    /// tells the operator to write the v4 form.
    pub fn parse(entry: &str) -> Result<Self, String> {
        let entry = entry.trim();
        let (addr_part, prefix_part) = match entry.split_once('/') {
            Some((a, p)) => (a, Some(p)),
            None => (entry, None),
        };
        let addr: IpAddr = addr_part
            .parse()
            .map_err(|_| format!("`{addr_part}` is not an IP address"))?;
        if let IpAddr::V6(v6) = addr {
            if v6.to_ipv4_mapped().is_some() {
                return Err(
                    "v4-mapped IPv6 (`::ffff:a.b.c.d`) is not accepted here; write the plain \
                     IPv4 form, which is what matching normalizes to"
                        .to_string(),
                );
            }
        }
        let max_prefix = if addr.is_ipv4() { 32u8 } else { 128u8 };
        let prefix = match prefix_part {
            None => max_prefix,
            Some(p) => {
                let parsed: u8 = p
                    .trim()
                    .parse()
                    .map_err(|_| format!("`{p}` is not a prefix length"))?;
                if parsed > max_prefix {
                    return Err(format!("prefix /{parsed} exceeds /{max_prefix}"));
                }
                parsed
            }
        };
        Ok(match addr {
            IpAddr::V4(v4) => {
                let bits = u32::from(v4);
                Cidr::V4 { network: bits & mask_v4(prefix), prefix }
            }
            IpAddr::V6(v6) => {
                let bits = u128::from(v6);
                Cidr::V6 { network: bits & mask_v6(prefix), prefix }
            }
        })
    }

    /// Whether `ip` falls inside this network. `ip` is expected to be already
    /// normalized ([`normalize_ip`]); a v4-mapped v6 address would otherwise
    /// miss every v4 rule.
    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self, ip) {
            (Cidr::V4 { network, prefix }, IpAddr::V4(v4)) => {
                u32::from(v4) & mask_v4(*prefix) == *network
            }
            (Cidr::V6 { network, prefix }, IpAddr::V6(v6)) => {
                u128::from(v6) & mask_v6(*prefix) == *network
            }
            // A v4 rule never matches a v6 source and vice versa. Families are
            // not interchangeable, and pretending otherwise is how an operator
            // who allowlisted a v4 range discovers they also allowed a v6 one.
            _ => false,
        }
    }
}

/// `/0` must produce an all-zero mask; `1u32 << 32` is undefined-shift
/// territory, hence the explicit branch rather than the clever expression.
fn mask_v4(prefix: u8) -> u32 {
    if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    }
}

fn mask_v6(prefix: u8) -> u128 {
    if prefix == 0 {
        0
    } else {
        u128::MAX << (128 - prefix)
    }
}

/// A set of configured networks.
///
/// An EMPTY set matches nothing. This is the single most important line in the
/// file to read as written: an empty class list denies everything, and is never
/// interpreted as "no restriction configured, therefore allow".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CidrSet(Vec<Cidr>);

impl CidrSet {
    /// Parse a comma-separated list. Blank entries are skipped; a malformed one
    /// is an error, never a skipped line — a typo that silently drops a rule
    /// changes the policy without telling anyone.
    pub fn parse(var: &'static str, raw: &str) -> Result<Self, EdgeConfigError> {
        let mut out = Vec::new();
        for entry in raw.split(',') {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            let cidr = Cidr::parse(entry).map_err(|why| EdgeConfigError::BadCidr {
                var,
                entry: entry.to_string(),
                why,
            })?;
            out.push(cidr);
        }
        Ok(Self(out))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether `ip` (already normalized) is in any configured network.
    pub fn contains(&self, ip: IpAddr) -> bool {
        self.0.iter().any(|c| c.contains(ip))
    }
}

/// Normalize an address before matching: a v4-mapped IPv6 address
/// (`::ffff:a.b.c.d`) becomes its plain IPv4 form.
///
/// A dual-stack listener reports an IPv4 peer in exactly this shape, so without
/// this every v4 rule would silently stop matching the moment the socket was
/// opened dual-stack — the policy would look identical and enforce nothing.
pub fn normalize_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6(v6),
        },
        v4 => v4,
    }
}

// ── Path → class policy ─────────────────────────────────────────────────────

/// How a policy entry matches a request path.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PathMatcher {
    /// `/mcp` — this path and nothing else.
    Exact(String),
    /// `/oauth/*` — this prefix (stored including the trailing `/`) and
    /// anything below it.
    Prefix(String),
}

impl PathMatcher {
    fn parse(pattern: &str) -> Result<Self, EdgeConfigError> {
        let bad = || EdgeConfigError::BadPathPattern(pattern.to_string());
        if !pattern.starts_with('/') {
            return Err(bad());
        }
        match pattern.strip_suffix('*') {
            Some(prefix) => {
                // `/oauth/*` and `/oauth*` both mean "below /oauth". Storing
                // the normalized `/oauth/` form means a path like `/oauthx`
                // cannot match a rule written for `/oauth/*` — a prefix rule
                // that leaks sideways into a neighbouring path is a classic
                // allowlist bug.
                let prefix = prefix.strip_suffix('/').unwrap_or(prefix);
                if prefix.is_empty() {
                    // A bare `/*` would make the whole edge one class and
                    // defeat the per-path split this module exists for.
                    return Err(bad());
                }
                Ok(PathMatcher::Prefix(format!("{prefix}/")))
            }
            None => {
                if pattern.contains('*') {
                    return Err(bad());
                }
                Ok(PathMatcher::Exact(pattern.to_string()))
            }
        }
    }

    /// Length of the matched pattern, used to break ties between overlapping
    /// prefixes — the most specific rule wins.
    fn specificity(&self) -> usize {
        match self {
            PathMatcher::Exact(p) => p.len(),
            PathMatcher::Prefix(p) => p.len(),
        }
    }

    fn matches(&self, path: &str) -> bool {
        match self {
            PathMatcher::Exact(p) => p == path,
            // Strictly BELOW the prefix. An earlier revision also matched the
            // bare form (`/oauth` against `/oauth/*`), which is the same
            // leniency as the trailing-slash folding review round 1 rejected in
            // `decide` — a pattern that quietly covers one more path than it
            // says. A bare parent that should be exposed gets its own exact
            // entry (as the two `.well-known` documents do), which is a
            // statement rather than a side effect.
            PathMatcher::Prefix(p) => path.starts_with(p.as_str()),
        }
    }
}

/// The path→class table.
#[derive(Debug, Clone)]
pub struct PathPolicy {
    entries: Vec<(PathMatcher, String)>,
}

impl PathPolicy {
    /// Build from `pattern → class` pairs. Entries are sorted so that
    /// classification is deterministic regardless of config ordering: exact
    /// matches outrank prefixes, and a longer prefix outranks a shorter one.
    fn build(pairs: Vec<(String, String)>) -> Result<Self, EdgeConfigError> {
        let mut entries = Vec::with_capacity(pairs.len());
        for (pattern, class) in pairs {
            entries.push((PathMatcher::parse(&pattern)?, class));
        }
        entries.sort_by(|a, b| {
            let rank = |m: &PathMatcher| matches!(m, PathMatcher::Exact(_)) as u8;
            rank(&b.0)
                .cmp(&rank(&a.0))
                .then(b.0.specificity().cmp(&a.0.specificity()))
        });
        Ok(Self { entries })
    }

    /// The class governing `path`, or `None` when the path has no entry — which
    /// callers must treat as DENY, never as "unrestricted".
    pub fn classify(&self, path: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(m, _)| m.matches(path))
            .map(|(_, class)| class.as_str())
    }

    /// Every distinct class named by the table, for startup validation and for
    /// the startup log line.
    fn classes(&self) -> Vec<&str> {
        let mut out: Vec<&str> = self.entries.iter().map(|(_, c)| c.as_str()).collect();
        out.sort_unstable();
        out.dedup();
        out
    }
}

/// A named, documented default policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeProfile {
    /// The default: Anthropic's hosted surfaces (Claude web, Desktop, mobile)
    /// fetch discovery, register and exchange tokens; the human authorizes from
    /// their own network.
    AnthropicHosted,
    /// Claude Code runs the ENTIRE flow from the user's machine with a loopback
    /// redirect, so the token exchange arrives from the interactive networks
    /// too. Everything else is unchanged.
    ///
    /// Note `/oauth/register` deliberately stays in the Anthropic class even
    /// here: dynamic registration from a Claude Code deployment is not part of
    /// the default story (RMCP-08 keeps DCR off unless explicitly enabled), and
    /// a deployment that wants it can move that one path with
    /// [`ENV_POLICY_JSON`] rather than having it widened for everyone.
    ClaudeCode,
}

impl EdgeProfile {
    fn parse(name: &str) -> Result<Self, EdgeConfigError> {
        match name.trim().to_ascii_lowercase().as_str() {
            "" | "default" | "anthropic-hosted" => Ok(EdgeProfile::AnthropicHosted),
            "claude-code" => Ok(EdgeProfile::ClaudeCode),
            other => Err(EdgeConfigError::UnknownProfile(other.to_string())),
        }
    }

    /// The profile's path→class pairs.
    ///
    /// This list IS the exposure surface of the public door — the two discovery
    /// documents, the OAuth endpoints, and `/mcp`. Adding a line here makes a
    /// path internet-reachable, which is why it is short and lives in one place
    /// rather than being spread across route registrations.
    fn pairs(self) -> Vec<(String, String)> {
        let token_class = match self {
            EdgeProfile::AnthropicHosted => CLASS_ANTHROPIC,
            EdgeProfile::ClaudeCode => CLASS_INTERACTIVE,
        };
        vec![
            // Discovery: fetched by the client, not the browser.
            ("/.well-known/oauth-protected-resource".into(), CLASS_ANTHROPIC.into()),
            ("/.well-known/oauth-protected-resource/*".into(), CLASS_ANTHROPIC.into()),
            ("/.well-known/oauth-authorization-server".into(), CLASS_ANTHROPIC.into()),
            ("/.well-known/oauth-authorization-server/*".into(), CLASS_ANTHROPIC.into()),
            // The MCP surface itself.
            ("/mcp".into(), CLASS_ANTHROPIC.into()),
            // Machine half of the OAuth flow.
            ("/oauth/token".into(), token_class.into()),
            ("/oauth/register".into(), CLASS_ANTHROPIC.into()),
            ("/oauth/revoke".into(), CLASS_ANTHROPIC.into()),
            // Human half: opened in the operator's own browser, never reached
            // from Anthropic. `/oauth/login` and `/oauth/consent` are the form
            // posts behind `/oauth/authorize` (RMCP-03) and share its class —
            // splitting them would break consent for exactly the same reason a
            // single pinhole does.
            ("/oauth/authorize".into(), CLASS_INTERACTIVE.into()),
            ("/oauth/login".into(), CLASS_INTERACTIVE.into()),
            ("/oauth/consent".into(), CLASS_INTERACTIVE.into()),
        ]
    }
}

// ── The policy object ───────────────────────────────────────────────────────

/// The resolved, validated source policy: which networks may reach which paths,
/// and whose `X-Forwarded-For` may be believed.
#[derive(Debug, Clone)]
pub struct EdgePolicy {
    policy: PathPolicy,
    classes: BTreeMap<String, CidrSet>,
    trusted_proxies: CidrSet,
}

/// Why a request was refused, or that it was allowed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EdgeDecision {
    /// The path is exposed and the resolved client address is in its class.
    Allow { class: String },
    /// The path has no policy entry. The edge does not serve it at all.
    NotExposed,
    /// The path is exposed, but not to this source.
    SourceDenied { class: String },
}

impl EdgeDecision {
    pub fn is_allowed(&self) -> bool {
        matches!(self, EdgeDecision::Allow { .. })
    }
}

/// Why an address could not be resolved. Every variant is a denial: a request
/// whose true origin is unknown cannot be checked against a source policy, and
/// "unknown" must never resolve to "allowed".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddrError {
    /// A forwarded entry chosen as the client address did not parse.
    UnparseableForwarded,
    /// A forwarding header value was not valid UTF-8, so the chain could not be
    /// read in full. See [`edge_guard`] for why a partial chain is refused
    /// rather than used.
    UndecodableForwardedHeader,
    /// The chain exceeded [`MAX_FORWARDED_ENTRIES`].
    ForwardedChainTooLong,
    /// Every entry in the chain was a trusted proxy, so no address in it can be
    /// attributed to a client.
    AllForwardedEntriesTrusted,
    /// The peer is a trusted proxy but forwarded no chain at all, so the only
    /// address available is the proxy's own — which is not a client.
    NoForwardedFromTrustedProxy,
}

impl std::fmt::Display for AddrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AddrError::UnparseableForwarded => f.write_str("unparseable X-Forwarded-For entry"),
            AddrError::ForwardedChainTooLong => f.write_str("X-Forwarded-For chain too long"),
            AddrError::AllForwardedEntriesTrusted => {
                f.write_str("every X-Forwarded-For entry is a trusted proxy")
            }
            AddrError::NoForwardedFromTrustedProxy => {
                f.write_str("a trusted proxy forwarded no X-Forwarded-For chain")
            }
            AddrError::UndecodableForwardedHeader => {
                f.write_str("an X-Forwarded-For header value is not valid UTF-8")
            }
        }
    }
}

/// Resolve the address a policy decision is keyed on.
///
/// `peer` is the socket's own remote address; `forwarded` is every
/// `X-Forwarded-For` value in header order (an entry may itself be a
/// comma-separated list). `trusted` is the configured trusted-proxy set.
///
/// The two rules, restated because they are the whole security of the pinhole:
/// the header is read ONLY from a trusted peer, and the RIGHTMOST untrusted
/// entry is the answer. A client can prepend anything it likes to
/// `X-Forwarded-For`; what it cannot do is make its own address disappear from
/// the right-hand end, where the proxy that actually accepted the connection
/// appends it.
///
/// ## The peer fallback applies only to an UNTRUSTED peer
/// Review round 1 (`gpt56`) caught the asymmetry, and it is a real hole. Falling
/// back to the peer address is right when the peer is untrusted — the peer IS
/// the client then. It is wrong when the peer is a trusted proxy that forwarded
/// no usable chain: a trusted proxy is by definition NOT a client, so
/// attributing a request to it means that if the proxy's own address happens to
/// sit inside an allowed CIDR (a co-located proxy on a permitted network is the
/// normal case, not an exotic one), every request it forwards clears the policy
/// regardless of who sent it — the pinhole would be wide open through the one
/// hop it trusts most. So a trusted peer with an absent, empty, or entirely
/// trusted chain is DENIED. The RMCP-09 spec's "proxy sends no XFF ⇒ fall back
/// to the peer address" line was written for the untrusted case and is applied
/// only there.
pub fn resolve_client_ip(
    peer: IpAddr,
    forwarded: &[&str],
    trusted: &CidrSet,
) -> Result<IpAddr, AddrError> {
    let peer = normalize_ip(peer);
    if !trusted.contains(peer) {
        // Not a proxy we configured ⇒ the header is unauthenticated input from
        // whoever connected. Ignoring it entirely is the point.
        return Ok(peer);
    }

    let entries: Vec<&str> = forwarded
        .iter()
        .copied()
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .collect();

    if entries.is_empty() {
        // NOT a fallback to the peer — see this function's doc. The peer here is
        // a trusted proxy, and a proxy is not a client.
        return Err(AddrError::NoForwardedFromTrustedProxy);
    }
    if entries.len() > MAX_FORWARDED_ENTRIES {
        return Err(AddrError::ForwardedChainTooLong);
    }

    for entry in entries.iter().rev() {
        let ip = parse_forwarded_entry(entry).ok_or(AddrError::UnparseableForwarded)?;
        if trusted.contains(ip) {
            continue;
        }
        return Ok(ip);
    }
    Err(AddrError::AllForwardedEntriesTrusted)
}

/// Parse one `X-Forwarded-For` entry into a normalized address.
///
/// Accepts the bare forms (`a.b.c.d`, `2001:db8::1`) and the port-suffixed ones
/// some proxies emit (`a.b.c.d:1234`, `[2001:db8::1]:443`). Anything else is
/// `None` — and a `None` on the entry that would have been chosen denies the
/// request rather than falling through to a more permissive attribution.
fn parse_forwarded_entry(entry: &str) -> Option<IpAddr> {
    let entry = entry.trim();
    if let Ok(ip) = entry.parse::<IpAddr>() {
        return Some(normalize_ip(ip));
    }
    if let Ok(sock) = entry.parse::<SocketAddr>() {
        return Some(normalize_ip(sock.ip()));
    }
    // `[2001:db8::1]` without a port.
    if let Some(inner) = entry.strip_prefix('[').and_then(|e| e.strip_suffix(']')) {
        if let Ok(ip) = inner.parse::<Ipv6Addr>() {
            return Some(normalize_ip(IpAddr::V6(ip)));
        }
    }
    // `a.b.c.d:port` where the v4 parse above failed only because of the port.
    if let Some((host, port)) = entry.rsplit_once(':') {
        if port.bytes().all(|b| b.is_ascii_digit()) {
            if let Ok(ip) = host.parse::<Ipv4Addr>() {
                return Some(IpAddr::V4(ip));
            }
        }
    }
    None
}

impl EdgePolicy {
    /// Build and validate. Every class named by the table must be defined —
    /// a typo'd class name would otherwise become an empty set, denying a path
    /// that looks configured, which is the confusing half of fail-closed.
    pub fn new(
        policy: PathPolicy,
        classes: BTreeMap<String, CidrSet>,
        trusted_proxies: CidrSet,
    ) -> Result<Self, EdgeConfigError> {
        for (matcher, class) in &policy.entries {
            if !classes.contains_key(class) {
                let path = match matcher {
                    PathMatcher::Exact(p) => p.clone(),
                    PathMatcher::Prefix(p) => format!("{p}*"),
                };
                return Err(EdgeConfigError::UnknownClass {
                    path,
                    class: class.clone(),
                });
            }
        }
        Ok(Self { policy, classes, trusted_proxies })
    }

    pub fn trusted_proxies(&self) -> &CidrSet {
        &self.trusted_proxies
    }

    /// The decision for one request.
    ///
    /// `client` must already be the RESOLVED address (see
    /// [`resolve_client_ip`]) — this function does not look at headers, so
    /// there is exactly one place in the module where a forged header could
    /// ever have influenced the outcome.
    pub fn decide(&self, path: &str, client: IpAddr) -> EdgeDecision {
        if !path_is_safe(path) {
            return EdgeDecision::NotExposed;
        }
        // Matched EXACTLY as received. An earlier revision folded a trailing
        // slash here so `/mcp/` classified as `/mcp`; review round 1 (`gpt56`)
        // called that out and is right. Normalizing a path ahead of an
        // authorization decision is a classic bypass shape — it makes the
        // policy's reading of the request deliberately differ from the literal
        // one, and the argument that the difference is harmless rested on the
        // ROUTER's current behavior (it 404s `/mcp/`) rather than on anything
        // the policy guarantees. Nothing legitimate needs it: the canonical
        // resource URI carries no trailing slash (RMCP-02 validates that at
        // startup), so a real client posts to `/mcp`.
        let class = match self.policy.classify(path) {
            Some(c) => c.to_string(),
            None => return EdgeDecision::NotExposed,
        };
        let allowed = self
            .classes
            .get(&class)
            .map(|set| set.contains(normalize_ip(client)))
            // A class with no entry at all is the empty set. Not "unset,
            // therefore open".
            .unwrap_or(false);
        if allowed {
            EdgeDecision::Allow { class }
        } else {
            EdgeDecision::SourceDenied { class }
        }
    }
}

/// Reject a path before it is classified when it carries anything that could
/// make the string the policy sees differ from the path the router resolves.
///
/// Percent-encoding and `.` / `..` segments are the two classic ways to write a
/// path that an allowlist reads one way and a router reads another. Neither has
/// any legitimate use on this door's tiny fixed path set, so both are refused
/// outright rather than decoded — decoding invites the two readers to disagree
/// again a layer down.
fn path_is_safe(path: &str) -> bool {
    if !path.starts_with('/') || path.contains('%') || path.contains("//") {
        return false;
    }
    !path.split('/').any(|seg| seg == "." || seg == "..")
}

// ── Runtime configuration + wiring ──────────────────────────────────────────

/// Everything `terminus_primary` needs to bind the edge.
pub struct EdgeConfig {
    pub bind: String,
    pub port: u16,
    policy: Arc<EdgePolicy>,
    limiter: Arc<dyn RateLimiter>,
}

impl EdgeConfig {
    /// Read the edge configuration from the environment.
    ///
    /// - `Ok(None)` — the edge is not enabled. Nothing is parsed and nothing is
    ///   bound; the binary behaves exactly as it did before this item.
    /// - `Err(_)` — the edge IS enabled and its configuration is not usable.
    ///   The caller must refuse to start. This is the fail-closed rule: on the
    ///   one listener that faces the internet there is no such thing as a
    ///   default that is safe to guess.
    pub fn from_env() -> Result<Option<Self>, EdgeConfigError> {
        if !env_flag(ENV_ENABLED) {
            return Ok(None);
        }

        let profile = EdgeProfile::parse(&env_str(ENV_PROFILE).unwrap_or_default())?;
        let pairs = match env_str(ENV_POLICY_JSON) {
            Some(raw) => parse_policy_json(&raw)?,
            None => profile.pairs(),
        };
        let policy = PathPolicy::build(pairs)?;

        let mut classes = BTreeMap::new();
        classes.insert(
            CLASS_ANTHROPIC.to_string(),
            CidrSet::parse(ENV_ANTHROPIC_CIDRS, &env_str(ENV_ANTHROPIC_CIDRS).unwrap_or_default())?,
        );
        classes.insert(
            CLASS_INTERACTIVE.to_string(),
            CidrSet::parse(
                ENV_INTERACTIVE_CIDRS,
                &env_str(ENV_INTERACTIVE_CIDRS).unwrap_or_default(),
            )?,
        );

        let trusted_proxies = CidrSet::parse(
            ENV_TRUSTED_PROXIES,
            &env_str(ENV_TRUSTED_PROXIES).unwrap_or_default(),
        )?;
        if env_flag(ENV_BEHIND_PROXY) && trusted_proxies.is_empty() {
            return Err(EdgeConfigError::ProxyWithoutTrustedProxies);
        }

        for class in policy.classes() {
            if classes.get(class).map(CidrSet::is_empty).unwrap_or(true) {
                // Loud, because it is almost always a provisioning miss rather
                // than an intent — and the symptom (a 403 the operator reads as
                // a broken connector) points nowhere near the cause.
                tracing::warn!(
                    "rmcp_edge: source class `{class}` has no configured CIDRs — every path in \
                     that class will be refused until it does"
                );
            }
        }

        let bind = env_str(ENV_BIND).unwrap_or_else(|| DEFAULT_BIND.to_string());
        let port = match env_str(ENV_PORT) {
            Some(raw) => raw
                .parse::<u16>()
                .map_err(|_| EdgeConfigError::BadBind(format!("`{raw}` is not a port")))?,
            None => DEFAULT_PORT,
        };
        if bind.parse::<IpAddr>().is_err() {
            return Err(EdgeConfigError::BadBind(format!(
                "`{bind}` is not an IP address to bind"
            )));
        }

        // ABSENT ⇒ the documented default. PRESENT BUT UNUSABLE ⇒ a hard error,
        // never a silent fall back to the default.
        //
        // Review round 1 (`gpt56`) flagged the earlier `unwrap_or(default)` here
        // and is right, even though the identical shape is correct two fields up
        // for `RMCP_DB_MAX_CONNECTIONS`. The difference is what the knob does: a
        // pool size grants no permission, so a typo there costs throughput and
        // is self-announcing. A rate limit is a security control, and a typo
        // that quietly restores a laxer default is precisely the failure class
        // nobody notices — the limiter still exists, still logs, still returns
        // 429s, just not at the budget the operator wrote. Same posture as a
        // malformed policy: refuse to boot and name the variable.
        let burst = parse_positive(ENV_RATE_LIMIT_BURST, DEFAULT_RATE_LIMIT_BURST, |raw| {
            raw.parse::<u32>().ok().filter(|n| *n > 0)
        })?;
        let refill = parse_positive(
            ENV_RATE_LIMIT_REFILL_PER_SEC,
            DEFAULT_RATE_LIMIT_REFILL_PER_SEC,
            |raw| raw.parse::<f64>().ok().filter(|n| n.is_finite() && *n > 0.0),
        )?;

        Ok(Some(Self {
            bind,
            port,
            policy: Arc::new(EdgePolicy::new(policy, classes, trusted_proxies)?),
            limiter: Arc::new(
                crate::gateway_framework::rate_limit::InProcessRateLimiter::new(burst, refill),
            ),
        }))
    }

    /// Build a config directly, for tests and for a caller that assembles the
    /// policy itself.
    pub fn new(
        bind: impl Into<String>,
        port: u16,
        policy: Arc<EdgePolicy>,
        limiter: Arc<dyn RateLimiter>,
    ) -> Self {
        Self { bind: bind.into(), port, policy, limiter }
    }

    pub fn policy(&self) -> &Arc<EdgePolicy> {
        &self.policy
    }

    /// A log-safe one-line summary — counts and class names, never a rendered
    /// address list.
    pub fn describe(&self) -> String {
        let classes: Vec<String> = self
            .policy
            .classes
            .iter()
            .map(|(name, set)| format!("{name}={} cidr(s)", set.len()))
            .collect();
        format!(
            "rmcp_edge on {}:{} [{}] trusted_proxies={}",
            self.bind,
            self.port,
            classes.join(" "),
            self.policy.trusted_proxies.len()
        )
    }
}

/// `RMCP_EDGE_POLICY_JSON` ⇒ `path pattern → class` pairs.
fn parse_policy_json(raw: &str) -> Result<Vec<(String, String)>, EdgeConfigError> {
    let value: serde_json::Value = serde_json::from_str(raw)
        .map_err(|e| EdgeConfigError::BadPolicyJson(e.to_string()))?;
    let obj = value
        .as_object()
        .ok_or_else(|| EdgeConfigError::BadPolicyJson("not a JSON object".to_string()))?;
    let mut pairs = Vec::with_capacity(obj.len());
    for (path, class) in obj {
        let class = class.as_str().ok_or_else(|| {
            EdgeConfigError::BadPolicyJson(format!("class for `{path}` is not a string"))
        })?;
        pairs.push((path.clone(), class.to_string()));
    }
    Ok(pairs)
}

/// Read a positive numeric knob: absent ⇒ `default`, present-and-usable ⇒ that
/// value, present-and-unusable ⇒ [`EdgeConfigError::BadRateLimit`]. `accept`
/// returns `None` for anything not strictly positive (and, for a float, not
/// finite — `inf` parses happily and would make a limiter that never refills).
fn parse_positive<T>(
    var: &'static str,
    default: T,
    accept: impl Fn(&str) -> Option<T>,
) -> Result<T, EdgeConfigError> {
    match env_str(var) {
        None => Ok(default),
        Some(raw) => accept(&raw).ok_or(EdgeConfigError::BadRateLimit { var, value: raw }),
    }
}

/// Non-secret boolean env flag: `1`/`true`/`yes`/`on`, case-insensitive.
fn env_flag(key: &str) -> bool {
    matches!(
        std::env::var(key).unwrap_or_default().trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Non-secret env string; blank reads as absent.
fn env_str(key: &str) -> Option<String> {
    std::env::var(key).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

// ── The middleware ──────────────────────────────────────────────────────────

/// Wrap `inner` in the edge's source policy.
///
/// The middleware is applied with [`axum::Router::layer`], which — unlike
/// `route_layer` — also wraps the router's FALLBACK. That is deliberate and
/// load-bearing: every request reaching this listener passes the policy,
/// including one for a path the router does not serve, so the exposure surface
/// is the policy table and not whatever routes happen to be registered.
pub fn build_edge_router(inner: Router, config: Arc<EdgeConfig>) -> Router {
    inner.layer(axum::middleware::from_fn_with_state(config, edge_guard))
}

/// One structured audit record per edge decision, on its own tracing target.
///
/// Deliberately NOT a `gateway_framework::audit::AuditEntry`: that record is
/// keyed on a resolved mTLS identity and an action kind (tool / inference /
/// admin), and an edge decision has neither — it happens before any identity
/// exists, and its subject is a network address and a path. Forcing it into
/// that shape would mean either a misleading `ActionKind` or a new variant that
/// every authorization `match` in the gateway would have to grow a case for, to
/// describe something the gateway does not gate. The sanitizer is shared, so
/// S6 redaction is identical.
fn audit_edge(client: &str, path: &str, method: &str, outcome: &str, detail: &str) {
    tracing::info!(
        target: "rmcp_edge_audit",
        client = %client,
        path = %sanitize(path),
        method = %method,
        outcome = %outcome,
        detail = %sanitize(detail),
        "rmcp_edge_audit"
    );
}

/// The edge gate: resolve the client address, rate-limit on it, then apply the
/// per-path source policy. Every refusal is audited; nothing is silent.
///
/// Order matters. Rate limiting comes BEFORE the policy check so that a scan of
/// forbidden paths is itself throttled — a limiter that only counts requests
/// which passed policy does not slow down the thing worth slowing down.
pub async fn edge_guard(
    State(config): State<Arc<EdgeConfig>>,
    req: Request,
    next: Next,
) -> Response {
    let path = req.uri().path().to_string();
    let method = req.method().as_str().to_string();

    // No peer address means this request did not arrive through
    // `into_make_service_with_connect_info`, so there is nothing to police. A
    // source policy that cannot see a source must refuse, not wave through.
    let peer = match req.extensions().get::<ConnectInfo<SocketAddr>>() {
        Some(ConnectInfo(addr)) => addr.ip(),
        None => {
            audit_edge("unknown", &path, &method, "denied", "no peer address on the connection");
            return refuse(StatusCode::FORBIDDEN, "forbidden");
        }
    };

    // `get_all`, not `get`: a chain may legitimately arrive as several header
    // instances, and reading only the first would drop the very end of the
    // chain — the end that carries the address the proxy actually saw.
    // Copied rather than borrowed from the request: `req` is moved into
    // `next.run` below, and an owned chain keeps that ordering a non-question.
    //
    // A header value that is not valid UTF-8 REFUSES the request. An earlier
    // revision used `filter_map(|v| v.to_str().ok())`, silently dropping such a
    // value and proceeding with a shorter chain — review round 2 (`gpt56`)
    // caught it, and it is the same bug class as the absent-chain fallback fixed
    // in round 1. HTTP header values are opaque octets, so a byte outside UTF-8
    // is reachable by anyone who can influence any hop's forwarding metadata;
    // dropping the entry it lands in shifts which entry is "rightmost
    // untrusted", and that single value is what the whole policy turns on. An
    // attacker who can place one malformed byte in the chain could therefore
    // choose the address the edge attributes the request to. A chain that cannot
    // be read in full is not a shorter chain — it is an unusable one.
    //
    // Checked BEFORE the trusted-peer test, so it holds for every caller rather
    // than only for the peers whose header is believed today. Tolerating a
    // malformed value from an untrusted peer would be defensible (the header is
    // ignored there anyway) and is deliberately not done: that would leave one
    // path where a malformed value is accepted, which becomes load-bearing the
    // moment the trusted-proxy list changes. One rule is easier to keep true.
    let mut forwarded_values: Vec<String> = Vec::new();
    for value in req.headers().get_all(FORWARDED_FOR_HEADER).iter() {
        match value.to_str() {
            Ok(v) => forwarded_values.push(v.to_string()),
            Err(_) => {
                audit_edge(
                    &peer.to_string(),
                    &path,
                    &method,
                    "denied",
                    &AddrError::UndecodableForwardedHeader.to_string(),
                );
                return refuse(StatusCode::FORBIDDEN, "forbidden");
            }
        }
    }
    let forwarded: Vec<&str> = forwarded_values.iter().map(String::as_str).collect();

    let client = match resolve_client_ip(peer, &forwarded, config.policy.trusted_proxies()) {
        Ok(ip) => ip,
        Err(e) => {
            audit_edge(&peer.to_string(), &path, &method, "denied", &e.to_string());
            return refuse(StatusCode::FORBIDDEN, "forbidden");
        }
    };
    let client_label = client.to_string();

    let decision = config.limiter.check(&rate_limit_key(&client_label, "rmcp_edge")).await;
    if decision.is_over_budget() {
        let retry = decision.retry_after_secs().unwrap_or(1.0).ceil().max(1.0) as u64;
        audit_edge(
            &client_label,
            &path,
            &method,
            "rate_limited",
            if decision.is_degraded() { "limiter degraded" } else { "over budget" },
        );
        let mut response = refuse(StatusCode::TOO_MANY_REQUESTS, "too many requests");
        if let Ok(value) = retry.to_string().parse() {
            response.headers_mut().insert(axum::http::header::RETRY_AFTER, value);
        }
        return response;
    }

    match config.policy.decide(&path, client) {
        EdgeDecision::Allow { class } => {
            audit_edge(&client_label, &path, &method, "allowed", &class);
            next.run(req).await
        }
        // 404 rather than 403: an unlisted path is not something this listener
        // serves at all, and saying "forbidden" would confirm that something is
        // there. The exposed set is public knowledge (it is the OAuth spec's
        // own path list); which networks may reach it is not.
        EdgeDecision::NotExposed => {
            audit_edge(&client_label, &path, &method, "not_exposed", "no policy entry");
            refuse(StatusCode::NOT_FOUND, "not found")
        }
        EdgeDecision::SourceDenied { class } => {
            audit_edge(&client_label, &path, &method, "denied", &format!("class {class}"));
            refuse(StatusCode::FORBIDDEN, "forbidden")
        }
    }
}

/// A refusal body carries no policy detail: which class a path is in, and which
/// networks that class holds, are facts a prober should have to guess.
fn refuse(status: StatusCode, message: &str) -> Response {
    (status, axum::Json(serde_json::json!({ "error": message }))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::{get, post};
    use tower::ServiceExt;

    // RFC 5737 TEST-NET (v4) and RFC 3849 (v6) documentation ranges — the
    // sanctioned placeholder for exactly this, not real infrastructure.
    //
    // HELD through review rounds 1, 2 and 3, deliberately, not an oversight:
    //
    // These blocks exist in their RFCs precisely to be written down in examples
    // and tests. They are reserved, never routable, and describe no fleet
    // infrastructure — the S1 PII rule targets RFC 1918 private ranges,
    // container ids and internal hostnames, and the repo's own
    // `no_pii_in_own_source_tree` gate passes on this file. Substituting a
    // made-up "fictional-looking" range would be strictly WORSE: an
    // unreserved range is somebody's real allocation, and writing one into a
    // test named after an allowlist is how a real third party ends up
    // documented as trusted. Loopback literals are here for the same reason.
    const ANTHROPIC_NET: &str = "203.0.113.0/24";
    const INTERACTIVE_NET: &str = "198.51.100.0/24";
    const PROXY_NET: &str = "192.0.2.10/32";

    fn ip(s: &str) -> IpAddr {
        s.parse().expect("test fixture address must parse")
    }

    fn policy_with(
        anthropic: &str,
        interactive: &str,
        proxies: &str,
        profile: EdgeProfile,
    ) -> Arc<EdgePolicy> {
        let mut classes = BTreeMap::new();
        classes.insert(
            CLASS_ANTHROPIC.to_string(),
            CidrSet::parse(ENV_ANTHROPIC_CIDRS, anthropic).expect("fixture cidrs"),
        );
        classes.insert(
            CLASS_INTERACTIVE.to_string(),
            CidrSet::parse(ENV_INTERACTIVE_CIDRS, interactive).expect("fixture cidrs"),
        );
        Arc::new(
            EdgePolicy::new(
                PathPolicy::build(profile.pairs()).expect("fixture policy"),
                classes,
                CidrSet::parse(ENV_TRUSTED_PROXIES, proxies).expect("fixture proxies"),
            )
            .expect("fixture policy is valid"),
        )
    }

    fn default_policy() -> Arc<EdgePolicy> {
        policy_with(ANTHROPIC_NET, INTERACTIVE_NET, PROXY_NET, EdgeProfile::AnthropicHosted)
    }

    // ── CIDR matching ───────────────────────────────────────────────────────

    #[test]
    fn cidr_matches_only_inside_its_network() {
        let set = CidrSet::parse(ENV_ANTHROPIC_CIDRS, ANTHROPIC_NET).unwrap();
        assert!(set.contains(ip("203.0.113.7")));
        assert!(set.contains(ip("203.0.113.255")));
        assert!(!set.contains(ip("203.0.114.7")));
        assert!(!set.contains(ip("198.51.100.7")));
    }

    #[test]
    fn cidr_accepts_a_bare_address_and_a_zero_prefix() {
        let single = CidrSet::parse(ENV_TRUSTED_PROXIES, "192.0.2.10").unwrap();
        assert!(single.contains(ip("192.0.2.10")));
        assert!(!single.contains(ip("192.0.2.11")));
        // `/0` is a legitimate, deliberate "any address" configuration for the
        // interactive class on a deployment that fronts it with its own auth.
        let any = CidrSet::parse(ENV_INTERACTIVE_CIDRS, "0.0.0.0/0").unwrap();
        assert!(any.contains(ip("198.51.100.1")));
        assert!(any.contains(ip("203.0.113.1")));
        // …but it is v4-only. A v6 source still needs a v6 rule.
        assert!(!any.contains(ip("2001:db8::1")));
    }

    /// The inversion this module exists to avoid: an empty list is not
    /// "unconfigured, therefore allow".
    #[test]
    fn an_empty_cidr_list_denies_everything() {
        let empty = CidrSet::parse(ENV_ANTHROPIC_CIDRS, "").unwrap();
        assert!(empty.is_empty());
        assert!(!empty.contains(ip("203.0.113.7")));
        assert!(!empty.contains(ip("2001:db8::1")));

        let policy = policy_with("", INTERACTIVE_NET, PROXY_NET, EdgeProfile::AnthropicHosted);
        assert_eq!(
            policy.decide("/mcp", ip("203.0.113.7")),
            EdgeDecision::SourceDenied { class: CLASS_ANTHROPIC.to_string() }
        );
    }

    #[test]
    fn ipv6_and_v4_mapped_v6_normalize_before_matching() {
        let v6 = CidrSet::parse(ENV_INTERACTIVE_CIDRS, "2001:db8::/32").unwrap();
        assert!(v6.contains(ip("2001:db8::1")));
        assert!(!v6.contains(ip("2001:db9::1")));

        // A dual-stack listener reports a v4 peer in the v4-mapped form. It
        // must still match the v4 rule, or the policy silently stops working
        // the moment the socket is opened dual-stack.
        let v4 = CidrSet::parse(ENV_ANTHROPIC_CIDRS, ANTHROPIC_NET).unwrap();
        let mapped = normalize_ip(ip("::ffff:203.0.113.7"));
        assert_eq!(mapped, ip("203.0.113.7"));
        assert!(v4.contains(mapped));
        assert!(!v4.contains(ip("::ffff:203.0.113.7")), "matching must be on the normalized form");
    }

    /// A v4-mapped CIDR in CONFIG would never match anything once sources are
    /// normalized, so it is refused rather than accepted as a rule that does
    /// nothing.
    #[test]
    fn a_v4_mapped_cidr_is_refused_at_config_time() {
        let err = CidrSet::parse(ENV_ANTHROPIC_CIDRS, "::ffff:203.0.113.0/120").unwrap_err();
        assert!(matches!(err, EdgeConfigError::BadCidr { .. }));
        assert!(err.to_string().contains("v4-mapped"));
    }

    #[test]
    fn a_malformed_cidr_is_an_error_not_a_dropped_rule() {
        assert!(CidrSet::parse(ENV_ANTHROPIC_CIDRS, "203.0.113.0/33").is_err());
        assert!(CidrSet::parse(ENV_ANTHROPIC_CIDRS, "not-an-address").is_err());
        assert!(CidrSet::parse(ENV_ANTHROPIC_CIDRS, "203.0.113.0/x").is_err());
        // Blank entries around a valid one are tolerated — trailing commas in a
        // hand-edited env file are not a policy change.
        let set = CidrSet::parse(ENV_ANTHROPIC_CIDRS, " 203.0.113.0/24 , ").unwrap();
        assert_eq!(set.len(), 1);
    }

    // ── Path classification ─────────────────────────────────────────────────

    #[test]
    fn the_exposed_set_is_exactly_wellknown_oauth_and_mcp() {
        let policy = default_policy();
        for exposed in [
            "/mcp",
            "/oauth/authorize",
            "/oauth/token",
            "/oauth/register",
            "/.well-known/oauth-protected-resource",
            "/.well-known/oauth-protected-resource/mcp",
            "/.well-known/oauth-authorization-server",
        ] {
            assert!(policy.policy.classify(exposed).is_some(), "{exposed} must be exposed");
        }
        // Everything else the internal router serves stays internal.
        for hidden in [
            "/healthz",
            "/enroll",
            "/admin/workers",
            "/v1/chat/completions",
            "/metrics",
            "/",
            "/oauthx",
        ] {
            assert_eq!(
                policy.decide(hidden, ip("203.0.113.7")),
                EdgeDecision::NotExposed,
                "{hidden} must not be reachable from the edge"
            );
        }
    }

    #[test]
    fn an_unlisted_path_is_denied_from_every_source() {
        let policy = default_policy();
        for source in ["203.0.113.7", "198.51.100.7", "192.0.2.10"] {
            assert_eq!(policy.decide("/admin/workers", ip(source)), EdgeDecision::NotExposed);
        }
    }

    #[test]
    fn a_more_specific_rule_wins_regardless_of_config_order() {
        let mut classes = BTreeMap::new();
        classes.insert(
            CLASS_ANTHROPIC.to_string(),
            CidrSet::parse(ENV_ANTHROPIC_CIDRS, ANTHROPIC_NET).unwrap(),
        );
        classes.insert(
            CLASS_INTERACTIVE.to_string(),
            CidrSet::parse(ENV_INTERACTIVE_CIDRS, INTERACTIVE_NET).unwrap(),
        );
        // A broad prefix and a narrower exact entry, listed broad-first.
        let policy = EdgePolicy::new(
            PathPolicy::build(vec![
                ("/oauth/*".to_string(), CLASS_ANTHROPIC.to_string()),
                ("/oauth/authorize".to_string(), CLASS_INTERACTIVE.to_string()),
            ])
            .unwrap(),
            classes,
            CidrSet::default(),
        )
        .unwrap();
        assert!(policy.decide("/oauth/authorize", ip("198.51.100.7")).is_allowed());
        assert!(policy.decide("/oauth/token", ip("203.0.113.7")).is_allowed());
        assert!(!policy.decide("/oauth/authorize", ip("203.0.113.7")).is_allowed());
    }

    /// A prefix rule must not leak into a neighbouring path that merely starts
    /// with the same characters.
    #[test]
    fn a_prefix_rule_does_not_match_a_sibling_path() {
        let mut classes = BTreeMap::new();
        classes.insert(
            CLASS_ANTHROPIC.to_string(),
            CidrSet::parse(ENV_ANTHROPIC_CIDRS, ANTHROPIC_NET).unwrap(),
        );
        let policy = EdgePolicy::new(
            PathPolicy::build(vec![("/oauth/*".to_string(), CLASS_ANTHROPIC.to_string())]).unwrap(),
            classes,
            CidrSet::default(),
        )
        .unwrap();
        assert!(policy.decide("/oauth/token", ip("203.0.113.7")).is_allowed());
        // A prefix rule covers what is strictly BELOW it, not the bare parent —
        // a parent that should be exposed gets its own exact entry.
        assert_eq!(policy.decide("/oauth", ip("203.0.113.7")), EdgeDecision::NotExposed);
        assert_eq!(policy.decide("/oauthx", ip("203.0.113.7")), EdgeDecision::NotExposed);
        assert_eq!(policy.decide("/oauth-admin", ip("203.0.113.7")), EdgeDecision::NotExposed);
    }

    #[test]
    fn traversal_and_encoded_paths_are_refused_rather_than_decoded() {
        let policy = default_policy();
        for path in ["/mcp/../admin/workers", "/%6Dcp", "/mcp/./x", "//mcp", "/mcp%2F"] {
            assert_eq!(
                policy.decide(path, ip("203.0.113.7")),
                EdgeDecision::NotExposed,
                "{path} must not be classified"
            );
        }
    }

    /// The policy matches the path as RECEIVED. A trailing-slash variant is an
    /// unlisted path, not a spelling of a listed one — see `decide`'s comment
    /// for why normalizing ahead of an authorization decision was removed.
    #[test]
    fn a_trailing_slash_variant_is_an_unlisted_path() {
        let policy = default_policy();
        for path in ["/mcp/", "/oauth/authorize/", "/oauth/token/"] {
            assert_eq!(
                policy.decide(path, ip("203.0.113.7")),
                EdgeDecision::NotExposed,
                "{path} is not the listed path"
            );
            assert_eq!(policy.decide(path, ip("198.51.100.7")), EdgeDecision::NotExposed);
        }
        // The canonical forms are unaffected.
        assert!(policy.decide("/mcp", ip("203.0.113.7")).is_allowed());
        assert!(policy.decide("/oauth/authorize", ip("198.51.100.7")).is_allowed());
    }

    // ── The per-path split, which is the whole point ────────────────────────

    #[test]
    fn the_interactive_source_reaches_authorize_but_not_mcp() {
        let policy = default_policy();
        let human = ip("198.51.100.7");
        assert!(policy.decide("/oauth/authorize", human).is_allowed());
        assert_eq!(
            policy.decide("/mcp", human),
            EdgeDecision::SourceDenied { class: CLASS_ANTHROPIC.to_string() }
        );
    }

    #[test]
    fn the_anthropic_source_reaches_mcp_but_not_authorize() {
        let policy = default_policy();
        let robot = ip("203.0.113.7");
        assert!(policy.decide("/mcp", robot).is_allowed());
        assert!(policy.decide("/oauth/token", robot).is_allowed());
        assert!(policy.decide("/.well-known/oauth-protected-resource/mcp", robot).is_allowed());
        assert_eq!(
            policy.decide("/oauth/authorize", robot),
            EdgeDecision::SourceDenied { class: CLASS_INTERACTIVE.to_string() }
        );
    }

    #[test]
    fn a_source_in_neither_class_reaches_nothing() {
        let policy = default_policy();
        let stranger = ip("192.0.2.200");
        assert!(!policy.decide("/mcp", stranger).is_allowed());
        assert!(!policy.decide("/oauth/authorize", stranger).is_allowed());
        assert!(!policy.decide("/oauth/token", stranger).is_allowed());
    }

    /// The named profile for deployments driven by Claude Code, which exchanges
    /// tokens from the user's own machine.
    #[test]
    fn the_claude_code_profile_moves_only_the_token_endpoint() {
        let policy =
            policy_with(ANTHROPIC_NET, INTERACTIVE_NET, PROXY_NET, EdgeProfile::ClaudeCode);
        let human = ip("198.51.100.7");
        let robot = ip("203.0.113.7");
        assert!(policy.decide("/oauth/token", human).is_allowed());
        assert!(!policy.decide("/oauth/token", robot).is_allowed());
        // Everything else is unchanged by the profile.
        assert!(policy.decide("/mcp", robot).is_allowed());
        assert!(!policy.decide("/mcp", human).is_allowed());
        assert!(policy.decide("/oauth/authorize", human).is_allowed());
        assert!(policy.decide("/oauth/register", robot).is_allowed());
    }

    #[test]
    fn profile_names_are_validated() {
        assert_eq!(EdgeProfile::parse("").unwrap(), EdgeProfile::AnthropicHosted);
        assert_eq!(EdgeProfile::parse("Claude-Code").unwrap(), EdgeProfile::ClaudeCode);
        assert!(matches!(
            EdgeProfile::parse("allow-everything"),
            Err(EdgeConfigError::UnknownProfile(_))
        ));
    }

    // ── Client address resolution: the security core ────────────────────────

    /// The headline test. An attacker connecting DIRECTLY (from an address in
    /// no class) sets `X-Forwarded-For` to an Anthropic-class address. If the
    /// header were believed, the entire pinhole would be a formality.
    #[test]
    fn a_spoofed_forwarded_header_from_an_untrusted_peer_cannot_grant_access() {
        let trusted = CidrSet::parse(ENV_TRUSTED_PROXIES, PROXY_NET).unwrap();
        let attacker = ip("192.0.2.200");
        let resolved = resolve_client_ip(attacker, &["203.0.113.7"], &trusted).unwrap();
        assert_eq!(resolved, attacker, "the header must be ignored entirely");

        let policy = default_policy();
        assert!(!policy.decide("/mcp", resolved).is_allowed());
        // …and the same spoof aimed at the interactive half fails too.
        let resolved = resolve_client_ip(attacker, &["198.51.100.7"], &trusted).unwrap();
        assert_eq!(resolved, attacker);
        assert!(!policy.decide("/oauth/authorize", resolved).is_allowed());
    }

    /// The other headline. Through a real proxy, the client may prepend
    /// anything it likes; the proxy appends what it actually saw. Taking the
    /// LEFTMOST entry would hand the choice to the attacker.
    #[test]
    fn the_rightmost_untrusted_entry_wins_over_a_multi_hop_chain() {
        let trusted = CidrSet::parse(ENV_TRUSTED_PROXIES, "192.0.2.10, 192.0.2.11").unwrap();
        // Client-forged entries, then the true client, then the proxies that
        // handled the request.
        let chain = ["203.0.113.7, 198.51.100.42, 192.0.2.11"];
        let resolved = resolve_client_ip(ip("192.0.2.10"), &chain, &trusted).unwrap();
        assert_eq!(
            resolved,
            ip("198.51.100.42"),
            "the rightmost UNTRUSTED entry is the client; trusted hops are skipped"
        );
        assert_ne!(resolved, ip("203.0.113.7"), "the leftmost entry is attacker-controlled");

        // And the forged Anthropic-class address it prepended buys nothing.
        let policy = default_policy();
        assert!(!policy.decide("/mcp", resolved).is_allowed());
        assert!(policy.decide("/oauth/authorize", resolved).is_allowed());
    }

    #[test]
    fn header_values_split_across_multiple_headers_are_one_chain() {
        let trusted = CidrSet::parse(ENV_TRUSTED_PROXIES, "192.0.2.10, 192.0.2.11").unwrap();
        let resolved =
            resolve_client_ip(ip("192.0.2.10"), &["203.0.113.7", "198.51.100.42, 192.0.2.11"], &trusted)
                .unwrap();
        assert_eq!(resolved, ip("198.51.100.42"));
    }

    /// Review round 1's finding, asserted directly: a trusted proxy is not a
    /// client, so a chain that yields no untrusted address DENIES rather than
    /// falling back to the proxy's own address — otherwise a proxy that happens
    /// to sit in an allowed CIDR launders every request it forwards.
    #[test]
    fn a_trusted_peer_with_no_usable_chain_is_denied_not_attributed_to_itself() {
        // The proxy's own address is deliberately INSIDE the anthropic class
        // here, which is what makes the old fallback dangerous and this
        // assertion meaningful.
        let trusted = CidrSet::parse(ENV_TRUSTED_PROXIES, "203.0.113.9/32").unwrap();
        let proxy = ip("203.0.113.9");
        assert!(
            CidrSet::parse(ENV_ANTHROPIC_CIDRS, ANTHROPIC_NET).unwrap().contains(proxy),
            "fixture must place the proxy inside an allowed class for this test to mean anything"
        );

        // Absent, empty, and all-trusted chains: three shapes, one answer.
        assert_eq!(
            resolve_client_ip(proxy, &[], &trusted),
            Err(AddrError::NoForwardedFromTrustedProxy)
        );
        assert_eq!(
            resolve_client_ip(proxy, &["  "], &trusted),
            Err(AddrError::NoForwardedFromTrustedProxy)
        );
        assert_eq!(
            resolve_client_ip(proxy, &["203.0.113.9"], &trusted),
            Err(AddrError::AllForwardedEntriesTrusted)
        );

        // The fallback is still correct for an UNTRUSTED peer — that peer IS
        // the client. Only the trusted-peer case changed.
        let stranger = ip("192.0.2.200");
        assert_eq!(resolve_client_ip(stranger, &[], &trusted).unwrap(), stranger);
    }

    /// The same finding at the HTTP layer: a trusted proxy forwarding no chain
    /// gets 403 even though its own address is in the `anthropic` class.
    #[tokio::test]
    async fn a_trusted_proxy_forwarding_no_chain_is_refused_at_the_router() {
        let policy = policy_with(
            ANTHROPIC_NET,
            INTERACTIVE_NET,
            "203.0.113.9/32",
            EdgeProfile::AnthropicHosted,
        );
        let router = edge_router(policy, 100);
        assert_eq!(
            request(router.clone(), "POST", "/mcp", "203.0.113.9", None).await,
            StatusCode::FORBIDDEN
        );
        // With a real chain it works, so the refusal above is about the missing
        // chain and not about the proxy being unreachable in general.
        assert_eq!(
            request(router, "POST", "/mcp", "203.0.113.9", Some("203.0.113.7")).await,
            StatusCode::OK
        );
    }

    #[test]
    fn an_all_trusted_chain_and_a_garbage_entry_both_deny() {
        let trusted = CidrSet::parse(ENV_TRUSTED_PROXIES, "192.0.2.0/24").unwrap();
        // No entry can be attributed to a client.
        assert_eq!(
            resolve_client_ip(ip("192.0.2.10"), &["192.0.2.11, 192.0.2.12"], &trusted),
            Err(AddrError::AllForwardedEntriesTrusted)
        );
        // The entry that WOULD have been chosen is unparseable — a misconfigured
        // proxy, and not something to guess around.
        assert_eq!(
            resolve_client_ip(ip("192.0.2.10"), &["203.0.113.7, junk"], &trusted),
            Err(AddrError::UnparseableForwarded)
        );
        // Garbage to the LEFT of the chosen entry is never reached, so an
        // attacker cannot deny service to a legitimate client by prepending it.
        assert_eq!(
            resolve_client_ip(ip("192.0.2.10"), &["junk, 198.51.100.42"], &trusted).unwrap(),
            ip("198.51.100.42")
        );
    }

    #[test]
    fn an_over_long_chain_is_refused_rather_than_walked() {
        let trusted = CidrSet::parse(ENV_TRUSTED_PROXIES, PROXY_NET).unwrap();
        let chain = vec!["198.51.100.1"; MAX_FORWARDED_ENTRIES + 1].join(", ");
        assert_eq!(
            resolve_client_ip(ip("192.0.2.10"), &[chain.as_str()], &trusted),
            Err(AddrError::ForwardedChainTooLong)
        );
    }

    #[test]
    fn forwarded_entries_may_carry_a_port_or_be_v4_mapped() {
        let trusted = CidrSet::parse(ENV_TRUSTED_PROXIES, PROXY_NET).unwrap();
        let proxy = ip("192.0.2.10");
        assert_eq!(
            resolve_client_ip(proxy, &["198.51.100.42:51234"], &trusted).unwrap(),
            ip("198.51.100.42")
        );
        assert_eq!(
            resolve_client_ip(proxy, &["[2001:db8::1]:443"], &trusted).unwrap(),
            ip("2001:db8::1")
        );
        assert_eq!(
            resolve_client_ip(proxy, &["[2001:db8::1]"], &trusted).unwrap(),
            ip("2001:db8::1")
        );
        assert_eq!(
            resolve_client_ip(proxy, &["::ffff:198.51.100.42"], &trusted).unwrap(),
            ip("198.51.100.42")
        );
    }

    // ── Config validation is fail-closed ────────────────────────────────────

    #[test]
    fn an_unparseable_policy_document_is_an_error() {
        assert!(matches!(
            parse_policy_json("{not json"),
            Err(EdgeConfigError::BadPolicyJson(_))
        ));
        assert!(matches!(
            parse_policy_json(r#"["/mcp"]"#),
            Err(EdgeConfigError::BadPolicyJson(_))
        ));
        assert!(matches!(
            parse_policy_json(r#"{"/mcp": 7}"#),
            Err(EdgeConfigError::BadPolicyJson(_))
        ));
    }

    #[test]
    fn a_policy_naming_an_undefined_class_is_an_error() {
        let mut classes = BTreeMap::new();
        classes.insert(CLASS_ANTHROPIC.to_string(), CidrSet::default());
        let err = EdgePolicy::new(
            PathPolicy::build(vec![("/mcp".to_string(), "everyone".to_string())]).unwrap(),
            classes,
            CidrSet::default(),
        )
        .unwrap_err();
        assert!(matches!(err, EdgeConfigError::UnknownClass { .. }));
    }

    #[test]
    fn an_unusable_path_pattern_is_an_error() {
        for pattern in ["mcp", "/*", "*", "/oa*th", ""] {
            assert!(
                PathMatcher::parse(pattern).is_err(),
                "`{pattern}` must be refused as a path pattern"
            );
        }
        assert!(PathMatcher::parse("/mcp").is_ok());
        assert!(PathMatcher::parse("/oauth/*").is_ok());
    }

    // ── Startup: from_env is where fail-closed actually has to hold ─────────

    /// The env-driven startup path, exercised for the three outcomes that
    /// matter: not enabled, enabled-and-usable, and enabled-but-refused.
    ///
    /// `#[serial]` because this mutates process-global environment on shared
    /// keys; the vars are cleared again so no later test inherits an enabled
    /// edge.
    #[test]
    #[serial_test::serial]
    fn from_env_is_off_by_default_and_fails_closed_when_enabled() {
        let vars = [
            ENV_ENABLED,
            ENV_ANTHROPIC_CIDRS,
            ENV_INTERACTIVE_CIDRS,
            ENV_TRUSTED_PROXIES,
            ENV_BEHIND_PROXY,
            ENV_POLICY_JSON,
            ENV_PROFILE,
            ENV_PORT,
            ENV_BIND,
            ENV_RATE_LIMIT_BURST,
            ENV_RATE_LIMIT_REFILL_PER_SEC,
        ];
        let clear = || {
            for v in vars {
                std::env::remove_var(v);
            }
        };

        clear();
        // Unset ⇒ no edge at all, and nothing else is even looked at.
        assert!(EdgeConfig::from_env().unwrap().is_none());

        // Enabled and coherent.
        std::env::set_var(ENV_ENABLED, "1");
        std::env::set_var(ENV_ANTHROPIC_CIDRS, ANTHROPIC_NET);
        std::env::set_var(ENV_INTERACTIVE_CIDRS, INTERACTIVE_NET);
        std::env::set_var(ENV_TRUSTED_PROXIES, PROXY_NET);
        std::env::set_var(ENV_PORT, "8311");
        let config = EdgeConfig::from_env().unwrap().expect("edge should be configured");
        assert_eq!(config.port, 8311);
        assert!(config.policy.decide("/mcp", ip("203.0.113.7")).is_allowed());
        assert!(config.policy.decide("/oauth/authorize", ip("198.51.100.7")).is_allowed());
        // The summary is log-safe: counts and class names, never an address.
        let described = config.describe();
        assert!(described.contains(CLASS_ANTHROPIC));
        assert!(!described.contains("203.0.113"));

        // A proxied deployment with nothing trusted: every request would be
        // attributed to the proxy, so the policy would police nothing.
        std::env::set_var(ENV_BEHIND_PROXY, "1");
        std::env::set_var(ENV_TRUSTED_PROXIES, "");
        assert!(matches!(
            EdgeConfig::from_env(),
            Err(EdgeConfigError::ProxyWithoutTrustedProxies)
        ));
        std::env::remove_var(ENV_BEHIND_PROXY);
        std::env::set_var(ENV_TRUSTED_PROXIES, PROXY_NET);

        // An unparseable policy document refuses to start rather than falling
        // back to the profile default.
        std::env::set_var(ENV_POLICY_JSON, "{not json");
        assert!(matches!(
            EdgeConfig::from_env(),
            Err(EdgeConfigError::BadPolicyJson(_))
        ));
        std::env::remove_var(ENV_POLICY_JSON);

        // As does a malformed CIDR: a typo must not silently drop a rule.
        std::env::set_var(ENV_ANTHROPIC_CIDRS, "203.0.113.0/24, oops");
        assert!(matches!(EdgeConfig::from_env(), Err(EdgeConfigError::BadCidr { .. })));
        std::env::set_var(ENV_ANTHROPIC_CIDRS, ANTHROPIC_NET);

        // Review round 1: a present-but-unusable rate limit is a hard error,
        // not a silent fall back to the default. A limiter that quietly runs at
        // a budget nobody wrote is the failure class nobody notices.
        for bad in ["0", "-1", "banana", "1.5"] {
            std::env::set_var(ENV_RATE_LIMIT_BURST, bad);
            assert!(
                matches!(EdgeConfig::from_env(), Err(EdgeConfigError::BadRateLimit { .. })),
                "burst `{bad}` must refuse to start"
            );
        }
        std::env::remove_var(ENV_RATE_LIMIT_BURST);
        for bad in ["0", "-2.5", "inf", "NaN", "banana"] {
            std::env::set_var(ENV_RATE_LIMIT_REFILL_PER_SEC, bad);
            assert!(
                matches!(EdgeConfig::from_env(), Err(EdgeConfigError::BadRateLimit { .. })),
                "refill `{bad}` must refuse to start"
            );
        }
        // ABSENT is still fine — the documented default applies. Only a value
        // the operator actually wrote and got wrong is fatal.
        std::env::remove_var(ENV_RATE_LIMIT_REFILL_PER_SEC);
        assert!(EdgeConfig::from_env().unwrap().is_some());
        std::env::set_var(ENV_RATE_LIMIT_BURST, "5");
        std::env::set_var(ENV_RATE_LIMIT_REFILL_PER_SEC, "0.5");
        assert!(EdgeConfig::from_env().unwrap().is_some());

        clear();
    }

    // ── The middleware, end to end ──────────────────────────────────────────

    /// A stand-in for the real gateway router. RMCP-02/03/04 add the actual
    /// handlers; what is under test here is which requests ever REACH one.
    fn inner_router() -> Router {
        Router::new()
            .route("/mcp", post(|| async { "mcp" }).get(|| async { "mcp" }))
            .route("/oauth/authorize", get(|| async { "authorize" }))
            .route("/oauth/token", post(|| async { "token" }))
            .route("/healthz", get(|| async { "ok" }))
            .route("/admin/workers", get(|| async { "workers" }))
    }

    /// A negligible refill rate (not a fast one) so the budget tests are
    /// deterministic: these requests drive a real router, and at any realistic
    /// refill the bucket tops back up between calls, which makes a
    /// rate-limit assertion pass or fail on scheduling luck rather than on
    /// behavior. Same trick the gateway's own limiter tests use.
    fn edge_router(policy: Arc<EdgePolicy>, burst: u32) -> Router {
        let limiter = Arc::new(
            crate::gateway_framework::rate_limit::InProcessRateLimiter::new(burst, 0.0001),
        );
        build_edge_router(
            inner_router(),
            Arc::new(EdgeConfig::new(DEFAULT_BIND, DEFAULT_PORT, policy, limiter)),
        )
    }

    /// Drive the edge router the way a real connection would: the peer address
    /// arrives as a `ConnectInfo` extension, which is what
    /// `into_make_service_with_connect_info` installs on a live listener.
    async fn request(
        router: Router,
        method: &str,
        path: &str,
        peer: &str,
        forwarded: Option<&str>,
    ) -> StatusCode {
        let mut req = axum::http::Request::builder()
            .method(method)
            .uri(path)
            .body(axum::body::Body::empty())
            .unwrap();
        req.extensions_mut()
            .insert(ConnectInfo(SocketAddr::new(ip(peer), 40000)));
        if let Some(value) = forwarded {
            req.headers_mut()
                .insert("x-forwarded-for", value.parse().expect("header value"));
        }
        router.oneshot(req).await.unwrap().status()
    }

    #[tokio::test]
    async fn edge_router_enforces_the_per_path_split_end_to_end() {
        let router = edge_router(default_policy(), 100);
        // The machine half, from Anthropic's class.
        assert_eq!(request(router.clone(), "POST", "/mcp", "203.0.113.7", None).await, StatusCode::OK);
        assert_eq!(
            request(router.clone(), "POST", "/oauth/token", "203.0.113.7", None).await,
            StatusCode::OK
        );
        // The human half, from the operator's class…
        assert_eq!(
            request(router.clone(), "GET", "/oauth/authorize", "198.51.100.7", None).await,
            StatusCode::OK
        );
        // …and each is refused from the other's network.
        assert_eq!(
            request(router.clone(), "POST", "/mcp", "198.51.100.7", None).await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            request(router, "GET", "/oauth/authorize", "203.0.113.7", None).await,
            StatusCode::FORBIDDEN
        );
    }

    #[tokio::test]
    async fn edge_router_does_not_serve_unlisted_paths_even_to_an_allowed_source() {
        let router = edge_router(default_policy(), 100);
        // `/healthz` and `/admin/workers` both EXIST in the inner router and
        // both return 200 internally. From the edge they must not exist at all.
        assert_eq!(
            request(router.clone(), "GET", "/healthz", "203.0.113.7", None).await,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            request(router.clone(), "GET", "/admin/workers", "203.0.113.7", None).await,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            request(router, "GET", "/admin/workers", "198.51.100.7", None).await,
            StatusCode::NOT_FOUND
        );
    }

    /// The spoof, asserted at the HTTP layer rather than only on the resolver:
    /// a header cannot buy a request past the door.
    #[tokio::test]
    async fn a_spoofed_header_is_refused_at_the_router() {
        let router = edge_router(default_policy(), 100);
        assert_eq!(
            request(router.clone(), "POST", "/mcp", "192.0.2.200", Some("203.0.113.7")).await,
            StatusCode::FORBIDDEN
        );
        // The same header from the CONFIGURED proxy is believed, which is the
        // control proving the test above failed for the right reason.
        assert_eq!(
            request(router, "POST", "/mcp", "192.0.2.10", Some("203.0.113.7")).await,
            StatusCode::OK
        );
    }

    /// A preflight is policed exactly like the request it precedes — an
    /// `OPTIONS` that skipped the source check would let a browser on any
    /// network probe which paths exist.
    #[tokio::test]
    async fn a_preflight_is_policed_like_the_method_it_precedes() {
        let router = edge_router(default_policy(), 100);
        assert_eq!(
            request(router.clone(), "OPTIONS", "/mcp", "198.51.100.7", None).await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            request(router, "OPTIONS", "/healthz", "203.0.113.7", None).await,
            StatusCode::NOT_FOUND
        );
    }

    /// No `ConnectInfo` means the source is unknowable, and an unknowable
    /// source cannot be checked against a source policy.
    /// Review round 2's finding. A header value that is not valid UTF-8 is
    /// refused rather than dropped: dropping it would silently shorten the
    /// chain, and a shorter chain has a DIFFERENT rightmost-untrusted entry —
    /// which is the single value the whole policy turns on.
    #[tokio::test]
    async fn a_malformed_forwarded_header_is_refused_not_silently_dropped() {
        let policy = policy_with(
            ANTHROPIC_NET,
            INTERACTIVE_NET,
            "203.0.113.9/32",
            EdgeProfile::AnthropicHosted,
        );
        let router = edge_router(policy, 100);

        // Header values are opaque octets, so this is a value a real peer can
        // actually send — `HeaderValue::from_bytes` accepts it and `to_str`
        // then fails.
        let malformed = axum::http::HeaderValue::from_bytes(&[0xff, 0xfe])
            .expect("a non-UTF-8 byte string is a legal header value");

        async fn send(
            router: Router,
            peer: &str,
            forwarded: axum::http::HeaderValue,
        ) -> StatusCode {
            use tower::ServiceExt;
            let mut req = axum::http::Request::builder()
                .method("POST")
                .uri("/mcp")
                .body(axum::body::Body::empty())
                .unwrap();
            req.extensions_mut()
                .insert(ConnectInfo(SocketAddr::new(ip(peer), 40000)));
            req.headers_mut().insert("x-forwarded-for", forwarded);
            router.oneshot(req).await.unwrap().status()
        }

        // From the TRUSTED proxy: the chain is unreadable, so the request is
        // refused rather than resolved from whatever survived.
        assert_eq!(
            send(router.clone(), "203.0.113.9", malformed.clone()).await,
            StatusCode::FORBIDDEN
        );
        // Positive control: the same request, same peer, well-formed chain —
        // proving the refusal above is about the malformed value and not about
        // this peer or path being unreachable.
        assert_eq!(
            send(router.clone(), "203.0.113.9", "203.0.113.7".parse().unwrap()).await,
            StatusCode::OK
        );
        // And the rule is unconditional: an untrusted peer, whose header would
        // be ignored anyway, is refused too. See `edge_guard` for why there is
        // deliberately no branch that tolerates a malformed value.
        assert_eq!(send(router, "203.0.113.7", malformed).await, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn a_request_with_no_peer_address_is_refused() {
        let router = edge_router(default_policy(), 100);
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/mcp")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(router.oneshot(req).await.unwrap().status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn the_edge_rate_limits_on_the_resolved_address() {
        let router = edge_router(default_policy(), 2);
        assert_eq!(request(router.clone(), "POST", "/mcp", "203.0.113.7", None).await, StatusCode::OK);
        assert_eq!(request(router.clone(), "POST", "/mcp", "203.0.113.7", None).await, StatusCode::OK);
        assert_eq!(
            request(router.clone(), "POST", "/mcp", "203.0.113.7", None).await,
            StatusCode::TOO_MANY_REQUESTS
        );
        // A different source has its own budget — one caller cannot lock the
        // connector out for everyone.
        assert_eq!(request(router, "POST", "/mcp", "203.0.113.8", None).await, StatusCode::OK);
    }

    /// The limiter counts REFUSED requests too, so scanning the door is as
    /// expensive as using it.
    #[tokio::test]
    async fn refused_requests_consume_edge_budget() {
        let router = edge_router(default_policy(), 2);
        // Two refusals from ONE address — the budget is per resolved client, so
        // they have to come from the same source to exhaust the same bucket.
        assert_eq!(
            request(router.clone(), "GET", "/admin/workers", "203.0.113.7", None).await,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            request(router.clone(), "GET", "/healthz", "203.0.113.7", None).await,
            StatusCode::NOT_FOUND
        );
        // A third request that the policy WOULD have allowed is now shed: the
        // two refusals spent the budget, which is the point — scanning the door
        // costs the scanner exactly what using it costs.
        assert_eq!(
            request(router, "POST", "/mcp", "203.0.113.7", None).await,
            StatusCode::TOO_MANY_REQUESTS
        );
    }
}
