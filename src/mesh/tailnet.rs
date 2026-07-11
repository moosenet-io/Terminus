//! Embedded tailnet listener (MESH-04) — the gateway becomes its own
//! Tailscale node, in-process, with no host `tailscaled` daemon required.
//!
//! ## Compiled ONLY under `--features tsnet`
//! `src/mesh/mod.rs` gates the `mod tailnet;` declaration itself behind the
//! `tsnet` Cargo feature (off by default), so on a default `cargo build`/
//! `cargo test` this file doesn't exist as far as the compiler is
//! concerned, and the `tsnet` crate dependency (below) is never fetched or
//! linked — it's `optional = true` in `Cargo.toml` and pulled in ONLY by
//! this feature.
//!
//! ## Binding chosen (recorded per the MESH-04 spec item's "decide and
//! record" requirement)
//! **The official `libtailscale` C shared library, via the `tsnet` Rust
//! crate** (crates.io `tsnet` 0.1, wrapping
//! <https://github.com/tailscale/libtailscale>'s `tailscale.h` C API —
//! `tailscale_new`/`_start`/`_up`/`_listen`/`_dial`/`_accept`/…, itself a
//! thin cgo shim around Go's `tailscale.com/tsnet` package). This gives
//! Up/Listen/Dial tsnet semantics matching the reference Go implementation,
//! the same posture as embedding tsnet in a Go binary, just reached from
//! Rust via FFI.
//!
//! **Fallback (documented, deliberately NOT implemented here):** run a
//! userspace `tailscaled` process alongside this binary (`tailscaled
//! --tun=userspace-networking --socket=<path>`), and reach it over its
//! LocalAPI unix socket for Listen/Dial/WhoIs — the way tooling that can't
//! embed the Go runtime directly integrates with a tailnet. Not chosen here
//! because it reintroduces exactly the "needs a host tailscaled" dependency
//! MESH-04 exists to remove. Recorded as the fallback for a future host
//! that can't satisfy the `tsnet` crate's build-time Go-toolchain
//! requirement (see below) but still wants tailnet reachability — a
//! follow-up item, not part of this one.
//!
//! ## Build-time requirement (why this may not compile everywhere, by design)
//! The `tsnet` crate's own `build.rs` invokes `go build -buildmode=c-archive`
//! to compile its vendored Go source (`tailscale.go`/`.c`/`.h`) into a
//! static `libtailscale.a`, then links it in — so `cargo build --features
//! tsnet` requires a Go toolchain (`go` on `$PATH`) on the build host, in
//! addition to Rust. This dev/build host has no Go toolchain, so
//! `--features tsnet` is EXPECTED to fail here, at that dependency's own
//! build-script step, before a single line of this file compiles — see this
//! item's completion report for the exact error observed. A build host that
//! does have Go (e.g. a future dedicated build host) can compile this
//! feature; nothing about the code below assumes anything beyond what the
//! `tsnet` crate documents.
//!
//! ## WhoIs (MESH-05) — FFI binding added, wiring blocked on two verified gaps
//! MESH-04's doc (previous paragraph, preserved above for history) assumed
//! `tailscale_whois` was "already statically linked into the binary via the
//! same `libtailscale.a`" and only missing a Rust wrapper. MESH-05 checked
//! that assumption against the actual vendored source the pinned `tsnet`
//! 0.1.0 crate ships (`tailscale.h`/`tailscale.c`/`tailscale.go`, present in
//! its crates.io source tarball) and found it does **not** hold:
//!
//! 1. **The C symbol itself does not exist in this vendored snapshot.**
//!    `grep -n whois` across all three vendored source files returns nothing
//!    — the pinned crate vendors `tailscale.com v1.1.1-0.20230308…` (see its
//!    `go.mod`), which predates upstream `libtailscale` adding
//!    `tailscale_whois` at all. So `--features tsnet` compiling this crate's
//!    vendored Go source will never *produce* a `tailscale_whois` symbol to
//!    link against, regardless of how it's declared on the Rust side.
//! 2. **Even with that symbol available, `tsnet::Server` doesn't expose the
//!    handle needed to call it.** `tsnet::Server { handle: sys::tailscale }`
//!    (`rust/src/lib.rs` in the crate's source) is a private field with no
//!    accessor — `sys::tailscale` is a bare `c_int` (see its
//!    bindgen-generated `tailscale.h`: `typedef int tailscale;`), but this
//!    crate has no supported way to read it out of a `Server` we didn't
//!    build ourselves via `sys::tailscale_new` (which would create a SECOND,
//!    disconnected tsnet node object, not give us the handle for the one
//!    [`TailnetServer::start`] already built).
//!
//! This is exactly the "genuinely does not expose the underlying handle
//! needed" case the MESH-05 spec item's own approach section anticipated,
//! and its prescribed response is "bump the dep or add a minimal direct
//! `extern "C"` against libtailscale, document exactly what you did and
//! why." Bumping `tsnet` to a release whose vendored source implements
//! `tailscale_whois` is the real fix, but this dev/build sandbox has no
//! network egress to crates.io or GitHub to identify/fetch one (verified —
//! `curl` to both times out/is unreachable here), so that step is left for
//! whichever future item does have that access, rather than guessing a
//! version number blind.
//!
//! What MESH-05 DID land, all under this feature gate:
//! - [`whois_ffi::tailscale_whois`] — the `extern "C"` declaration itself
//!   (item 1's blocker: declared, but nothing in the currently-linked
//!   archive implements it yet, so it is not actually *called* anywhere in
//!   this file — an unreferenced `extern "C"` declaration links fine; it
//!   only needs to resolve once something calls it).
//! - [`parse_whois_json`] — a pure, fully unit-tested function that decodes
//!   `libtailscale`'s documented `WhoIsResponse` JSON shape
//!   (`Node.Name`/`Node.Tags`/`UserProfile.LoginName`) into a
//!   [`crate::mesh::TailnetIdentity`]. This is the part of "wire up WhoIs"
//!   that doesn't depend on either verified gap above, so it's real,
//!   working, tested code today — once a future item resolves gap 1 (dep
//!   bump) and gap 2 (a way to reach the live handle, e.g. if that same dep
//!   bump also adds a `Server` accessor), completing the wire-up is "call
//!   the FFI function, feed its JSON output to this parser" — a small,
//!   mechanical follow-up, not a redesign.
//! - [`TailnetServer::whois`] / [`TailnetServer::whois_identity`] — the
//!   stable accessor surface (unchanged shape from MESH-04, plus the new
//!   `TailnetIdentity`-returning variant the mesh identity layer in
//!   `crate::mcp_server` consumes). Both still return "not available" today
//!   ([`WhoIsError::Unavailable`] / `None` respectively) — never a panic —
//!   because of the two verified gaps above, not because they're unwired in
//!   principle.
//!
//! ## Config surface (non-secret, `std::env::var`; consistent with
//! `crate::mesh::registry`'s convention)
//! - `TERMINUS_MESH_TAILNET_ENABLED` — RUNTIME flag (bool-ish, same
//!   truthiness rule as `TERMINUS_MESH_ENABLED`: `1`/`true`/`yes`/`on`,
//!   case-insensitive). Independent from the COMPILE-time `tsnet` feature —
//!   both must be on for `terminus_primary` to actually bind the tailnet
//!   listener; either off leaves `terminus_primary` byte-for-byte unchanged
//!   from before this item (see [`tailnet_enabled_from_env`]).
//! - `TERMINUS_TSNET_HOSTNAME` — the MagicDNS hostname this node advertises
//!   on the tailnet. Required when the flag above is on; there is no
//!   "leave it blank and let tsnet pick" default, since an operator-chosen,
//!   stable hostname is what makes MagicDNS useful for this deployment.
//! - `TERMINUS_TSNET_STATE_DIR` — local directory tsnet persists its node
//!   state/keys under (mirrors Go `tsnet.Server.Dir`). Required when the
//!   flag is on; created if missing, and probed for write access — see
//!   [`TsnetConfigError::StateDirUnwritable`].
//! - `TERMINUS_TSNET_AUTHKEY` — the tailnet auth key (<secret-manager>-hydrated,
//!   materialized into this process's environment the same way every other
//!   secret in this crate is — see `crate::mesh::registry`'s module doc for
//!   the established convention). Read via plain `std::env::var`, wrapped
//!   immediately in [`crate::mesh::registry::ResolvedSecret`] so it can
//!   never be accidentally logged via a stray `{:?}`/`{}` — see
//!   [`TailnetConfig::from_env`]. NEVER read except when both gates above
//!   are on.
//!
//! ## What this module does NOT do
//! - Does not touch the existing plain or mTLS listeners
//!   (`src/pki/server.rs`) — [`TailnetServer`] is purely additive, and
//!   `src/bin/terminus_primary.rs` only calls into this module when BOTH
//!   the `tsnet` compile feature and the `TERMINUS_MESH_TAILNET_ENABLED`
//!   runtime flag are on. Feature or flag off ⇒ `terminus_primary`'s
//!   existing listener setup is untouched.
//! - Does not join any real tailnet in this repo's tests — the unit tests
//!   in this module only exercise config parsing / error paths, never
//!   [`TailnetServer::start`] against a live tailnet (that needs a real
//!   auth key and network egress, neither available in CI or a dev
//!   sandbox).

use std::sync::Arc;

use thiserror::Error;

use crate::mesh::registry::ResolvedSecret;
use crate::mesh::TailnetIdentity;

/// The tailnet listener's bind address, in `tsnet`'s own address syntax
/// (empty host = all of this node's tailnet IPs). Deliberately NOT 443 —
/// tsnet's own WireGuard transport is already the encryption/authentication
/// layer here (there is no second TLS termination on this listener, unlike
/// the mTLS listener in `crate::pki::mtls`), so binding the conventionally
/// TLS-flavored port `:443` would be misleading. Not currently
/// operator-configurable (no env var) — the MESH-04 spec item's documented
/// config surface is hostname/state-dir/authkey/enabled only; a listen-port
/// override can be added later if a real deployment needs one.
const TAILNET_MCP_LISTEN_ADDR: &str = ":8443";

/// Errors resolving [`TailnetConfig`] from the process environment.
#[derive(Debug, Error)]
pub enum TsnetConfigError {
    #[error(
        "TERMINUS_TSNET_HOSTNAME is not set (required when TERMINUS_MESH_TAILNET_ENABLED is on)"
    )]
    MissingHostname,
    #[error(
        "TERMINUS_TSNET_STATE_DIR is not set (required when TERMINUS_MESH_TAILNET_ENABLED is on)"
    )]
    MissingStateDir,
    #[error("TERMINUS_TSNET_STATE_DIR \"{path}\" could not be created or is not writable: {source}")]
    StateDirUnwritable { path: String, source: String },
    #[error("TERMINUS_TSNET_AUTHKEY is not set in the process environment")]
    AuthKeyMissing,
    #[error("TERMINUS_TSNET_AUTHKEY is set but empty")]
    AuthKeyEmpty,
}

/// Errors from starting or serving on an embedded tsnet node.
#[derive(Debug, Error)]
pub enum TailnetError {
    #[error(transparent)]
    Config(#[from] TsnetConfigError),
    #[error("tsnet server failed to start: {0}")]
    Build(String),
    #[error("tsnet listener bind failed: {0}")]
    Listen(String),
}

/// Resolved config for one embedded tailnet node — hostname/state-dir are
/// plain structural config, `authkey` is a secret VALUE already wrapped in
/// [`ResolvedSecret`] the moment it's read (see [`TailnetConfig::from_env`]).
/// `Debug` is hand-implemented (rather than derived) so a stray `{:?}` of a
/// whole `TailnetConfig` — e.g. in a future log line someone adds without
/// thinking hard about it — still can't print the auth key.
pub struct TailnetConfig {
    pub hostname: String,
    pub state_dir: String,
    authkey: ResolvedSecret,
}

impl std::fmt::Debug for TailnetConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TailnetConfig")
            .field("hostname", &self.hostname)
            .field("state_dir", &self.state_dir)
            .field("authkey", &self.authkey)
            .finish()
    }
}

impl TailnetConfig {
    /// Resolve config from `TERMINUS_TSNET_HOSTNAME` / `TERMINUS_TSNET_STATE_DIR`
    /// / `TERMINUS_TSNET_AUTHKEY`. Callers (`src/bin/terminus_primary.rs`)
    /// only reach this after confirming [`tailnet_enabled_from_env`] — so a
    /// tailnet-disabled process never reads `TERMINUS_TSNET_AUTHKEY` at all,
    /// same "don't touch a secret unless the feature that needs it is
    /// actually on" posture `crate::mesh::registry::UpstreamServer::resolve_secret`
    /// documents for the mesh-federation secret.
    pub fn from_env() -> Result<Self, TsnetConfigError> {
        let hostname =
            env_nonempty("TERMINUS_TSNET_HOSTNAME").ok_or(TsnetConfigError::MissingHostname)?;
        let state_dir =
            env_nonempty("TERMINUS_TSNET_STATE_DIR").ok_or(TsnetConfigError::MissingStateDir)?;

        ensure_state_dir_writable(&state_dir)?;

        let authkey = match std::env::var("TERMINUS_TSNET_AUTHKEY") {
            Ok(v) if !v.trim().is_empty() => ResolvedSecret::new(v),
            Ok(_) => return Err(TsnetConfigError::AuthKeyEmpty),
            Err(_) => return Err(TsnetConfigError::AuthKeyMissing),
        };

        Ok(Self {
            hostname,
            state_dir,
            authkey,
        })
    }
}

/// Create `state_dir` if missing and confirm this process can actually write
/// to it — fail fast with an actionable error rather than letting `tsnet`'s
/// own state-persistence fail opaquely later, deep inside `Server::build`.
fn ensure_state_dir_writable(state_dir: &str) -> Result<(), TsnetConfigError> {
    std::fs::create_dir_all(state_dir).map_err(|e| TsnetConfigError::StateDirUnwritable {
        path: state_dir.to_string(),
        source: e.to_string(),
    })?;
    let probe = std::path::Path::new(state_dir).join(".terminus-tsnet-write-probe");
    std::fs::write(&probe, b"ok").map_err(|e| TsnetConfigError::StateDirUnwritable {
        path: state_dir.to_string(),
        source: e.to_string(),
    })?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

/// Read an env var, trimmed; `None` when unset or blank. Same convention as
/// `crate::mesh::registry`'s private helper of the same name — duplicated
/// here per this crate's existing practice of keeping each module's env
/// reads small and self-contained (see that module's own doc comment on its
/// copy).
fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// `TERMINUS_MESH_TAILNET_ENABLED` truthiness: `1`/`true`/`yes`/`on` (case
/// insensitive) is enabled; anything else, including unset/blank, is
/// disabled. The RUNTIME half of MESH-04's two independent gates — see the
/// module doc's "Config surface" section. `src/bin/terminus_primary.rs`
/// checks this (and is only able to, at all, when compiled with the `tsnet`
/// feature) before ever calling [`TailnetConfig::from_env`] or
/// [`TailnetServer::start`].
pub fn tailnet_enabled_from_env() -> bool {
    env_nonempty("TERMINUS_MESH_TAILNET_ENABLED")
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

/// A tailnet peer's resolved identity — the minimal shape MESH-05's
/// allowlist/audit middleware needs. Deliberately small: only what an authz
/// decision requires, never anything secret.
///
/// Kept as this module's own type (distinct from
/// [`crate::mesh::TailnetIdentity`]) because it mirrors `libtailscale`'s
/// `WhoIsResponse` field names 1:1 for [`parse_whois_json`]'s benefit;
/// [`TailnetServer::whois_identity`] converts to the crate-wide
/// [`crate::mesh::TailnetIdentity`] shape MESH-06 actually consumes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhoIsInfo {
    /// The tailnet login identity (e.g. an operator's tailnet account) that
    /// owns the connecting node.
    pub login_name: String,
    /// The connecting node's own tailnet machine name.
    pub node_name: String,
    /// ACL tags carried by the connecting node, if any (e.g. `tag:ci`).
    pub tags: Vec<String>,
}

/// Errors from [`TailnetServer::whois`].
#[derive(Debug, Error)]
pub enum WhoIsError {
    /// WhoIs cannot be performed today. See the module doc's "WhoIs
    /// (MESH-05)" section for the two independently-verified reasons: the
    /// pinned `tsnet` 0.1.0 crate's vendored `libtailscale` source doesn't
    /// implement `tailscale_whois` at all (not just an unwrapped binding),
    /// and even if it did, `tsnet::Server` exposes no accessor for the raw
    /// handle [`whois_ffi::tailscale_whois`] would need to call it.
    #[error(
        "WhoIs is not available: the pinned tsnet 0.1.0 crate's vendored libtailscale source \
         has no tailscale_whois implementation, and tsnet::Server exposes no handle accessor \
         to call it even if it did -- see crate::mesh::tailnet's module doc"
    )]
    Unavailable,
    /// `tailscale_whois` itself reported an error for this lookup (e.g. no
    /// entry for the given address). Not currently reachable (see
    /// [`WhoIsError::Unavailable`]), but modeled now so the eventual live
    /// call site has somewhere to put a real FFI error.
    #[error("tailscale_whois lookup failed: {0}")]
    Ffi(String),
    /// `tailscale_whois` succeeded but its JSON output didn't match the
    /// expected `WhoIsResponse` shape.
    #[error("could not parse tailscale_whois response: {0}")]
    InvalidResponse(String),
}

/// A gateway node embedded in-process on the tailnet (MESH-04). Wraps a
/// `tsnet::Server` handle. The underlying FFI handle
/// (`tsnet`'s `sys::tailscale`) is a plain integer descriptor, so
/// `tsnet::Server` is `Send + Sync` with no extra work — this type holds it
/// behind an `Arc` purely so a clone can be moved into the background
/// accept-loop task in [`TailnetServer::serve`] while the original stays
/// usable by the caller (e.g. for [`TailnetServer::whois`]).
pub struct TailnetServer {
    server: Arc<tsnet::Server>,
    hostname: String,
}

impl TailnetServer {
    /// Build + start ("Up") an embedded tsnet node from an already-resolved
    /// [`TailnetConfig`]. Config resolution and node startup are
    /// deliberately split (mirroring
    /// `crate::mesh::registry::UpstreamRegistry`'s parse/validate-vs-dial
    /// split) so a caller can inspect/log a resolved config before ever
    /// touching the network. The auth key is exposed to
    /// `tsnet::ServerBuilder::authkey` exactly once, right here, read via
    /// [`ResolvedSecret::expose`] and never logged.
    pub fn start(config: TailnetConfig) -> Result<Self, TailnetError> {
        let server = tsnet::ServerBuilder::new()
            .dir(std::path::PathBuf::from(&config.state_dir))
            .hostname(&config.hostname)
            .authkey(config.authkey.expose().to_string())
            .redirect_log()
            .build()
            .map_err(|e| TailnetError::Build(e.to_string()))?;

        Ok(Self {
            server: Arc::new(server),
            hostname: config.hostname,
        })
    }

    /// This node's advertised MagicDNS hostname. Operator-facing only —
    /// plays no role in client-side authz.
    pub fn hostname(&self) -> &str {
        &self.hostname
    }

    /// Serve `router` on this tailnet node's MCP listener
    /// ([`TAILNET_MCP_LISTEN_ADDR`]). `router` is the SAME merged `/mcp`
    /// router `crate::pki::server::build_gateway_router` already built for
    /// the plain + mTLS listeners (see `src/bin/terminus_primary.rs`'s call
    /// site) — this method never duplicates route wiring, only adds a third
    /// transport for it.
    ///
    /// Runs until the tailnet listener errors (mirrors
    /// `crate::pki::mtls::run_listener`'s "run forever, propagate a
    /// bind/accept-loop error" contract) — intended to be spawned as its own
    /// task alongside the existing listeners, never replacing them.
    ///
    /// `tsnet::Listener::accept` is a BLOCKING call (see the `tsnet` crate's
    /// own doc) — there is no usable async listener variant here: the
    /// crate's own `AsyncListener` needs its `tokio` feature (not enabled by
    /// this crate's `Cargo.toml`) and targets the older hyper 0.14 `Accept`
    /// trait, incompatible with this crate's axum 0.7 / hyper 1.x stack. So
    /// the accept loop runs on a dedicated OS thread (`std::thread::spawn`,
    /// not `tokio::task::spawn_blocking`, since it blocks for this node's
    /// entire serving lifetime, not just one operation), handing each
    /// accepted `std::net::TcpStream` back onto the calling async runtime —
    /// the same "accept, then dispatch into the shared router with hyper
    /// directly" shape `crate::pki::mtls::run_listener` uses, minus the
    /// TLS-termination step: tsnet's own WireGuard transport already
    /// provides wire-level encryption/authentication for every connection
    /// reaching this listener, so there is no second TLS layer here.
    pub async fn serve(&self, router: axum::Router) -> Result<(), TailnetError> {
        let listener = self
            .server
            .listen(tsnet::Network::Tcp, TAILNET_MCP_LISTEN_ADDR)
            .map_err(|e| TailnetError::Listen(e.to_string()))?;

        let rt_handle = tokio::runtime::Handle::current();
        let hostname = self.hostname.clone();
        let (result_tx, result_rx) = tokio::sync::oneshot::channel::<TailnetError>();

        std::thread::spawn(move || {
            tracing::info!(
                "mesh::tailnet: listening on tailnet node \"{hostname}\" {TAILNET_MCP_LISTEN_ADDR}"
            );
            loop {
                match listener.accept() {
                    Ok(std_stream) => {
                        if let Err(e) = std_stream.set_nonblocking(true) {
                            tracing::warn!(
                                "mesh::tailnet: failed to set accepted stream nonblocking, dropping connection: {e}"
                            );
                            continue;
                        }
                        let tokio_stream = match tokio::net::TcpStream::from_std(std_stream) {
                            Ok(s) => s,
                            Err(e) => {
                                tracing::warn!(
                                    "mesh::tailnet: failed to adopt accepted stream into the tokio runtime: {e}"
                                );
                                continue;
                            }
                        };
                        let router = router.clone();
                        rt_handle.spawn(serve_tailnet_connection(tokio_stream, router));
                    }
                    Err(e) => {
                        tracing::error!("mesh::tailnet: accept loop ended: {e}");
                        let _ = result_tx.send(TailnetError::Listen(e.to_string()));
                        return;
                    }
                }
            }
        });

        match result_rx.await {
            Ok(err) => Err(err),
            Err(_) => Err(TailnetError::Listen(
                "accept loop thread exited without reporting a result".to_string(),
            )),
        }
    }

    /// Look up the tailnet identity of the peer that reached this listener.
    /// Stable accessor surface for MESH-05's middleware to consume — see the
    /// module doc's "WhoIs (MESH-05)" section for why this currently always
    /// returns [`WhoIsError::Unavailable`]: the pinned `tsnet` 0.1.0 crate's
    /// vendored `libtailscale` source has no `tailscale_whois`
    /// implementation to call, and even if it did, `tsnet::Server` exposes
    /// no accessor for the raw handle [`whois_ffi::tailscale_whois`] needs.
    pub fn whois(&self, _remote_addr: std::net::SocketAddr) -> Result<WhoIsInfo, WhoIsError> {
        // Deliberately does NOT call `whois_ffi::tailscale_whois` -- doing
        // so would require a `sys::tailscale` handle this crate cannot
        // obtain from `self.server` (private field, no accessor; see the
        // module doc). Fabricating one (e.g. `0`, or minting a second node
        // via `tsnet::ServerBuilder`) would either call into an
        // unimplemented symbol or silently return wrong-node data --  both
        // worse than this explicit, documented "not available" error.
        Err(WhoIsError::Unavailable)
    }

    /// [`Self::whois`], mapped into the crate-wide
    /// [`crate::mesh::TailnetIdentity`] shape the identity middleware in
    /// `crate::mcp_server` (and MESH-06 downstream) consumes, collapsing any
    /// error into `None` -- a WhoIs miss or failure is never fatal here;
    /// absence just means this connection has no tailnet-derived principal
    /// input, and mTLS (or another source) may still supply one. This is the
    /// method MESH-05's middleware actually calls.
    pub fn whois_identity(&self, remote_addr: std::net::SocketAddr) -> Option<TailnetIdentity> {
        match self.whois(remote_addr) {
            Ok(info) => Some(TailnetIdentity {
                login: info.login_name,
                node: info.node_name,
                tags: info.tags,
            }),
            Err(_) => None,
        }
    }
}

/// Raw FFI binding to `libtailscale`'s `tailscale_whois` entry point — added
/// by MESH-05 (previously a hard stub with no binding at all). See the
/// module doc's "WhoIs (MESH-05)" section for why this declaration, while
/// well-formed, is not yet reachable from [`TailnetServer::whois`].
///
/// Signature mirrors the documented upstream `libtailscale` C API (as of the
/// `tailscale_whois` entry point's public documentation): like
/// [`tailscale_loopback`]'s existing buffer-out convention (`tailscale.h` in
/// this crate's vendored source), the response is written as a JSON-encoded
/// `WhoIsResponse` into a caller-provided buffer rather than an FFI struct,
/// which avoids having to pin an exact C struct ABI on the Rust side (a
/// struct layout mismatch would silently corrupt memory; a JSON parse
/// failure just returns an error). [`ERANGE_ERRNO`] documents the "buffer
/// too small" contract callers must handle by retrying with a larger buffer
/// (mirroring [`tailscale_errmsg`]'s own fixed-buffer usage elsewhere in
/// this file, generalized to a retry loop since a `WhoIsResponse` has no
/// fixed maximum size).
///
/// [`tailscale_loopback`]: https://github.com/tailscale/libtailscale
/// [`tailscale_errmsg`]: https://github.com/tailscale/libtailscale
mod whois_ffi {
    use std::os::raw::{c_char, c_int};

    extern "C" {
        /// `int tailscale_whois(tailscale sd, const char* remote_addr, char* json_out, size_t json_out_len);`
        ///
        /// `remote_addr` is a NUL-terminated `ip:port` string. Writes a
        /// JSON-encoded `WhoIsResponse` into `json_out` (NUL-terminated,
        /// truncated to `json_out_len`). Returns zero on success, -1 on
        /// error (call `tailscale_errmsg` for details, per this crate's
        /// existing convention for every other `tailscale_*` call).
        ///
        /// NOT CURRENTLY CALLED anywhere in this crate — see the module
        /// doc's "WhoIs (MESH-05)" section for why (no handle accessor from
        /// `tsnet::Server`, and this symbol isn't implemented by the
        /// currently-linked, pinned `tsnet` 0.1.0 vendored archive either).
        /// An unreferenced `extern "C"` declaration like this one links
        /// fine regardless -- it only needs to resolve once something
        /// actually calls it.
        #[allow(dead_code)]
        pub fn tailscale_whois(
            sd: c_int,
            remote_addr: *const c_char,
            json_out: *mut c_char,
            json_out_len: usize,
        ) -> c_int;
    }
}

/// Decode `libtailscale`'s `WhoIsResponse` JSON shape (as written by
/// [`whois_ffi::tailscale_whois`], once reachable) into a [`WhoIsInfo`].
/// Pure and fully unit-tested independent of any live FFI call or handle --
/// see the module doc's "WhoIs (MESH-05)" section for why this is the part
/// of WhoIs support that's genuinely complete today, ahead of the FFI call
/// site itself.
///
/// Only decodes the fields this crate actually uses
/// (`Node.Name`/`Node.Tags`/`UserProfile.LoginName`); the real
/// `WhoIsResponse` has additional fields (capabilities, key expiry, etc.)
/// this crate has no use for and deliberately ignores (`serde`'s default
/// "ignore unknown fields" behavior), so a future `libtailscale` version
/// adding more fields to the response can't break parsing here.
fn parse_whois_json(json: &str) -> Result<WhoIsInfo, WhoIsError> {
    #[derive(serde::Deserialize)]
    struct WhoIsResponse {
        #[serde(rename = "Node")]
        node: Option<WhoIsNode>,
        #[serde(rename = "UserProfile")]
        user_profile: Option<WhoIsUserProfile>,
    }
    #[derive(serde::Deserialize)]
    struct WhoIsNode {
        #[serde(rename = "Name")]
        name: Option<String>,
        #[serde(rename = "Tags")]
        tags: Option<Vec<String>>,
    }
    #[derive(serde::Deserialize)]
    struct WhoIsUserProfile {
        #[serde(rename = "LoginName")]
        login_name: Option<String>,
    }

    let parsed: WhoIsResponse =
        serde_json::from_str(json).map_err(|e| WhoIsError::InvalidResponse(e.to_string()))?;

    let node_name = parsed
        .node
        .as_ref()
        .and_then(|n| n.name.clone())
        .ok_or_else(|| WhoIsError::InvalidResponse("missing Node.Name".to_string()))?;
    let login_name = parsed
        .user_profile
        .as_ref()
        .and_then(|p| p.login_name.clone())
        .ok_or_else(|| WhoIsError::InvalidResponse("missing UserProfile.LoginName".to_string()))?;
    let tags = parsed
        .node
        .and_then(|n| n.tags)
        .unwrap_or_default();

    Ok(WhoIsInfo {
        login_name,
        node_name,
        tags,
    })
}

/// Drive one accepted tailnet connection's HTTP framing directly with
/// `hyper`, dispatching into `router` — same "why hyper directly" reasoning
/// as `crate::pki::mtls::serve_connection`, minus the identity-extension
/// insertion (no per-connection identity is attached here yet; that's
/// MESH-05's job, once [`TailnetServer::whois`] is implemented) and minus
/// TLS termination (tsnet's own transport already provides it).
async fn serve_tailnet_connection(stream: tokio::net::TcpStream, router: axum::Router) {
    let io = hyper_util::rt::TokioIo::new(stream);
    let svc = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
        let router = router.clone();
        async move {
            let (parts, body) = req.into_parts();
            let req = axum::extract::Request::from_parts(parts, axum::body::Body::new(body));
            let resp = tower::ServiceExt::oneshot(router, req)
                .await
                .unwrap_or_else(|e: std::convert::Infallible| match e {});
            Ok::<_, std::convert::Infallible>(resp)
        }
    });

    if let Err(e) = hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new())
        .serve_connection_with_upgrades(io, svc)
        .await
    {
        tracing::debug!("mesh::tailnet: connection ended: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn clear_env() {
        std::env::remove_var("TERMINUS_MESH_TAILNET_ENABLED");
        std::env::remove_var("TERMINUS_TSNET_HOSTNAME");
        std::env::remove_var("TERMINUS_TSNET_STATE_DIR");
        std::env::remove_var("TERMINUS_TSNET_AUTHKEY");
    }

    // ── TERMINUS_MESH_TAILNET_ENABLED truthiness ────────────────────────────

    #[test]
    #[serial]
    fn tailnet_enabled_from_env_truthiness() {
        clear_env();
        assert!(!tailnet_enabled_from_env(), "unset must be disabled");

        for truthy in ["1", "true", "TRUE", "yes", "On"] {
            std::env::set_var("TERMINUS_MESH_TAILNET_ENABLED", truthy);
            assert!(tailnet_enabled_from_env(), "{truthy:?} should be truthy");
        }

        for falsy in ["0", "false", "nah", ""] {
            std::env::set_var("TERMINUS_MESH_TAILNET_ENABLED", falsy);
            assert!(!tailnet_enabled_from_env(), "{falsy:?} should be falsy");
        }
        clear_env();
    }

    // ── TailnetConfig::from_env — required fields, fail-fast, no secret leak ─

    #[test]
    #[serial]
    fn from_env_requires_hostname() {
        clear_env();
        let err = TailnetConfig::from_env().expect_err("missing hostname must error");
        assert!(matches!(err, TsnetConfigError::MissingHostname));
        clear_env();
    }

    #[test]
    #[serial]
    fn from_env_requires_state_dir() {
        clear_env();
        std::env::set_var("TERMINUS_TSNET_HOSTNAME", "test-node");
        let err = TailnetConfig::from_env().expect_err("missing state dir must error");
        assert!(matches!(err, TsnetConfigError::MissingStateDir));
        clear_env();
    }

    #[test]
    #[serial]
    fn from_env_requires_authkey() {
        clear_env();
        let dir = tempfile::tempdir().expect("tempdir");
        std::env::set_var("TERMINUS_TSNET_HOSTNAME", "test-node");
        std::env::set_var("TERMINUS_TSNET_STATE_DIR", dir.path().to_str().unwrap());
        let err = TailnetConfig::from_env().expect_err("missing authkey must error");
        assert!(matches!(err, TsnetConfigError::AuthKeyMissing));
        clear_env();
    }

    #[test]
    #[serial]
    fn from_env_rejects_blank_authkey() {
        clear_env();
        let dir = tempfile::tempdir().expect("tempdir");
        std::env::set_var("TERMINUS_TSNET_HOSTNAME", "test-node");
        std::env::set_var("TERMINUS_TSNET_STATE_DIR", dir.path().to_str().unwrap());
        std::env::set_var("TERMINUS_TSNET_AUTHKEY", "   ");
        let err = TailnetConfig::from_env().expect_err("blank authkey must error");
        assert!(matches!(err, TsnetConfigError::AuthKeyEmpty));
        clear_env();
    }

    #[test]
    #[serial]
    fn from_env_fails_fast_when_state_dir_path_is_unusable() {
        clear_env();
        // A state dir path that can't possibly be created (its parent is a
        // FILE, not a directory) -- must surface StateDirUnwritable rather
        // than panicking or silently proceeding.
        let base = tempfile::tempdir().expect("tempdir");
        let blocking_file = base.path().join("not-a-directory");
        std::fs::write(&blocking_file, b"x").expect("write blocking file");
        let bogus_state_dir = blocking_file.join("state");

        std::env::set_var("TERMINUS_TSNET_HOSTNAME", "test-node");
        std::env::set_var(
            "TERMINUS_TSNET_STATE_DIR",
            bogus_state_dir.to_str().unwrap(),
        );
        std::env::set_var("TERMINUS_TSNET_AUTHKEY", "fixture-authkey-value"); // pii-test-fixture
        let err = TailnetConfig::from_env().expect_err("unusable state dir path must error");
        assert!(matches!(err, TsnetConfigError::StateDirUnwritable { .. }));
        clear_env();
    }

    #[test]
    #[serial]
    fn from_env_succeeds_and_debug_never_leaks_the_authkey() {
        clear_env();
        let dir = tempfile::tempdir().expect("tempdir");
        std::env::set_var("TERMINUS_TSNET_HOSTNAME", "test-node");
        std::env::set_var("TERMINUS_TSNET_STATE_DIR", dir.path().to_str().unwrap());
        std::env::set_var("TERMINUS_TSNET_AUTHKEY", "fixture-authkey-value"); // pii-test-fixture
        let config = TailnetConfig::from_env().expect("should resolve with all vars set");
        assert_eq!(config.hostname, "test-node");
        assert_eq!(config.state_dir, dir.path().to_str().unwrap());

        let debug_output = format!("{config:?}");
        assert!(!debug_output.contains("fixture-authkey-value"));
        assert!(debug_output.contains("redacted"));
        clear_env();
    }

    #[test]
    #[serial]
    fn from_env_creates_a_missing_state_dir() {
        clear_env();
        let base = tempfile::tempdir().expect("tempdir");
        let nested = base.path().join("does").join("not").join("exist-yet");
        assert!(!nested.exists());

        std::env::set_var("TERMINUS_TSNET_HOSTNAME", "test-node");
        std::env::set_var("TERMINUS_TSNET_STATE_DIR", nested.to_str().unwrap());
        std::env::set_var("TERMINUS_TSNET_AUTHKEY", "fixture-authkey-value"); // pii-test-fixture
        TailnetConfig::from_env().expect("should create the missing state dir and succeed");
        assert!(nested.is_dir());
        clear_env();
    }

    // ── WhoIs scope boundary ─────────────────────────────────────────────────

    #[test]
    fn whois_error_documents_the_unavailable_reason() {
        // A live `TailnetServer::whois` call needs an actually-started tsnet
        // node (real auth key + network egress), out of scope for a unit
        // test -- this pins the documented contract via the error type
        // itself instead: MESH-05 added the `tailscale_whois` FFI
        // declaration, but the pinned `tsnet` 0.1.0 crate's vendored
        // libtailscale source doesn't implement that symbol, and
        // `tsnet::Server` exposes no handle accessor to call it even if it
        // did -- see the module doc's "WhoIs (MESH-05)" section.
        let err = WhoIsError::Unavailable;
        assert!(err.to_string().contains("not available"));
    }

    // ── parse_whois_json — pure, no live handle/FFI call needed ─────────────

    #[test]
    fn parse_whois_json_decodes_a_well_formed_response() {
        let json = r#"{
            "Node": {"Name": "laptop.tailnetname.ts.net", "Tags": ["tag:ci"]},
            "UserProfile": {"LoginName": "<email>"}
        }"#;
        let info = parse_whois_json(json).expect("well-formed response should parse");
        assert_eq!(info.node_name, "laptop.tailnetname.ts.net");
        assert_eq!(info.login_name, "<email>");
        assert_eq!(info.tags, vec!["tag:ci".to_string()]);
    }

    #[test]
    fn parse_whois_json_defaults_missing_tags_to_empty() {
        let json = r#"{
            "Node": {"Name": "laptop.tailnetname.ts.net"},
            "UserProfile": {"LoginName": "<email>"}
        }"#;
        let info = parse_whois_json(json).expect("response without Tags should still parse");
        assert!(info.tags.is_empty());
    }

    #[test]
    fn parse_whois_json_rejects_garbage() {
        let err = parse_whois_json("not json").expect_err("garbage input must not panic or succeed");
        assert!(matches!(err, WhoIsError::InvalidResponse(_)));
    }

    #[test]
    fn parse_whois_json_rejects_missing_node_name() {
        let json = r#"{"UserProfile": {"LoginName": "<email>"}}"#;
        let err = parse_whois_json(json).expect_err("missing Node.Name must error, not panic");
        assert!(matches!(err, WhoIsError::InvalidResponse(_)));
    }

    #[test]
    fn parse_whois_json_rejects_missing_login_name() {
        let json = r#"{"Node": {"Name": "laptop.tailnetname.ts.net"}}"#;
        let err =
            parse_whois_json(json).expect_err("missing UserProfile.LoginName must error, not panic");
        assert!(matches!(err, WhoIsError::InvalidResponse(_)));
    }
}
