//! `WorkerTransport` — the pluggable in-box transport a broker uses to reach
//! a worker (TMOD-02).
//!
//! ## The three tiers
//! Loopback (`127.0.0.1`) is deliberately NEVER treated as a trust boundary
//! anywhere in this module — every tier either stays entirely inside the
//! kernel (a Unix Domain Socket, whose peer identity the kernel itself
//! attests via `SO_PEERCRED`) or authenticates cryptographically (mTLS),
//! never "it came from localhost so it must be us".
//!
//! - **T2 (default)** — [`uds_mtls`]: UDS + mTLS-over-UDS. The strongest
//!   tier: the connection stays on-host (no network exposure at all) AND is
//!   mutually authenticated with certificates, AND the two independent
//!   identity signals — the kernel-attested `SO_PEERCRED` peer uid and the
//!   TLS peer leaf certificate's Subject CN — must BOTH resolve to the
//!   worker's configured identity. Either one disagreeing with the
//!   configured identity is a fail-closed rejection (see [`uds_mtls`]'s
//!   module doc for the exact check and why "agree with config" rather than
//!   "agree with each other" is the chosen semantics).
//! - **T1** — [`uds_peercred`]: UDS + `SO_PEERCRED` only, no TLS. Same-host
//!   only, no cryptographic identity — appropriate only for low-risk,
//!   read-only tools where the kernel's peer-uid attestation is judged
//!   sufficient. Weakest tier this module offers.
//! - **T0** — [`mtls_tcp`]: mTLS over TCP, for a worker that is off-box (not
//!   reachable via a shared filesystem for a UDS at all). Cryptographically
//!   authenticated like T2, but network-exposed (TCP, possibly crossing a
//!   host boundary) rather than kernel-local like T2/T1 — ranked between T1
//!   and T2 by [`TransportTier::security_rank`], not equal to either.
//!
//! ## The minimum-tier floor
//! [`MinTierPolicy`] maps a worker's declared capability class to the
//! lowest tier it may register at. A `write_scoped` or `secret_holding`
//! worker is refused below T2 — see [`MinTierPolicy::permits`]. This is
//! enforced by [`crate::config`]'s worker-transport registry at config-load
//! time (a misconfigured floor violation is rejected before any connection
//! is ever attempted), and independently re-checked here so any future
//! caller that constructs a transport directly (bypassing that registry)
//! still can't silently under-provision a sensitive worker.

pub mod mtls_tcp;
pub mod uds_mtls;
pub mod uds_peercred;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::error::ToolError;
use crate::tool::ToolOutput;

/// One of the three transport tiers a worker can be reached over. See the
/// module doc for what each tier actually does.
///
/// Deliberately does NOT derive [`Ord`] from declaration order — tier
/// declaration order here is arbitrary/documentation order (T2 default is
/// listed first), while the *security* ordering used by [`MinTierPolicy`] is
/// a distinct, explicit ranking via [`TransportTier::security_rank`]. Mixing
/// the two up (e.g. via a derived `Ord`) is exactly the kind of subtle bug
/// this module's tests guard against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TransportTier {
    T0,
    T1,
    T2,
}

impl TransportTier {
    /// Security rank used ONLY for [`MinTierPolicy`] floor comparisons —
    /// higher is stronger. T1 (no cryptographic identity, kernel peer-uid
    /// attestation only) is the weakest; T0 (mTLS, but network-exposed/
    /// off-box) is stronger than T1 but weaker than T2 (mTLS AND kernel
    /// peer-uid attestation AND on-host-only). NOT the enum's declaration
    /// order — see the module doc.
    pub fn security_rank(self) -> u8 {
        match self {
            TransportTier::T1 => 0,
            TransportTier::T0 => 1,
            TransportTier::T2 => 2,
        }
    }
}

impl std::fmt::Display for TransportTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransportTier::T0 => write!(f, "T0"),
            TransportTier::T1 => write!(f, "T1"),
            TransportTier::T2 => write!(f, "T2"),
        }
    }
}

/// A worker's declared capability class — what the [`MinTierPolicy`] floor
/// is keyed on. Declared per worker in `crate::config`'s worker-transport
/// registry, never inferred from the tool names a worker happens to expose
/// (an operator-authored, auditable declaration, not a heuristic).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityClass {
    /// No side effects outside the worker's own process; never touches a
    /// secret. The only class permitted at T1.
    ReadOnly,
    /// Can mutate state (filesystem, an external API, etc.) outside its own
    /// process. Floored at T2.
    WriteScoped,
    /// Holds or can access secret material (credentials, keys, tokens).
    /// Floored at T2, same as `write_scoped` — a secret-holding worker is at
    /// least as sensitive as a write-scoped one.
    SecretHolding,
}

/// The broker-side minimum-tier floor: maps a [`CapabilityClass`] to the
/// lowest [`TransportTier`] a worker of that class may register at, and
/// checks a proposed (class, tier) pair against it.
///
/// A pure policy table — no I/O, no state. `crate::config`'s worker registry
/// calls [`MinTierPolicy::permits`] at config-load time so a misconfigured
/// floor violation (e.g. a `write_scoped` worker declared at T1) is rejected
/// before the broker ever attempts to dial it.
pub struct MinTierPolicy;

impl MinTierPolicy {
    /// The lowest tier `class` may register at.
    pub fn minimum_tier(class: CapabilityClass) -> TransportTier {
        match class {
            CapabilityClass::ReadOnly => TransportTier::T1,
            CapabilityClass::WriteScoped | CapabilityClass::SecretHolding => TransportTier::T2,
        }
    }

    /// Whether `tier` meets or exceeds the floor for `class`.
    pub fn permits(class: CapabilityClass, tier: TransportTier) -> bool {
        tier.security_rank() >= Self::minimum_tier(class).security_rank()
    }
}

/// Errors from the TRANSPORT itself never producing a worker-shaped answer —
/// distinct from [`ToolError`], which is a tool-level failure a worker
/// deliberately reported. Every variant's `Display` is safe to log verbatim;
/// none interpolate certificate/key material.
#[derive(Debug, Error)]
pub enum TransportError {
    /// The worker could not be reached at all (socket/port absent, refused,
    /// or the transport-level handshake failed). Maps to a clean "worker
    /// unavailable" [`ToolError`] at the [`WorkerTransport::call`] boundary
    /// — never a panic.
    #[error("worker unavailable: {0}")]
    Unavailable(String),
    /// A T1/T2 UDS peer's `SO_PEERCRED` uid, or a T0/T2 peer's TLS leaf
    /// certificate CN, did not match this worker's configured identity.
    /// Always fail-closed: the connection is dropped, nothing is dispatched.
    #[error("worker identity mismatch: {0}")]
    IdentityMismatch(String),
    /// The wire protocol response could not be parsed, or the worker sent a
    /// structurally invalid reply.
    #[error("worker transport protocol error: {0}")]
    Protocol(String),
}

/// The transport over which a broker reaches one worker. Implemented by
/// [`uds_peercred::UdsPeercredTransport`] (T1), [`uds_mtls::UdsMtlsTransport`]
/// (T2), and [`mtls_tcp::MtlsTcpTransport`] (T0) — one instance per worker,
/// selected by that worker's configured [`TransportTier`].
///
/// A routed tool call is deliberately made to return the SAME
/// [`ToolOutput`]/[`ToolError`] shapes the in-proc [`crate::registry`] uses,
/// so a caller dispatching to a worker-routed tool cannot tell the
/// difference from a compiled-in one except by latency.
#[async_trait::async_trait]
pub trait WorkerTransport: Send + Sync {
    /// Establish (or confirm) connectivity to the worker, performing
    /// whatever tier-specific handshake/identity validation applies. Does
    /// NOT need to be called before [`Self::call`]/[`Self::list`] — those
    /// dial fresh per call (workers are short-lived-connection-oriented, not
    /// pooled, in this item's scope) — it exists so a caller can eagerly
    /// probe reachability + identity without making a tool call.
    async fn connect(&self) -> Result<(), TransportError>;

    /// Invoke tool `name` on the worker with `args`, returning the same
    /// [`ToolOutput`]/[`ToolError`] shapes an in-proc [`crate::tool::RustTool`]
    /// would. A transport-level failure (worker unreachable, identity
    /// mismatch, protocol error) is mapped to a clean [`ToolError::Execution`]
    /// — never a panic, never propagated as a raw I/O error.
    async fn call(&self, name: &str, args: Value) -> Result<ToolOutput, ToolError>;

    /// List the tool names this worker currently advertises.
    async fn list(&self) -> Result<Vec<String>, TransportError>;

    /// Lightweight liveness check. Never returns an `Err` — a failed probe
    /// just means "not healthy right now" (mirrors
    /// `crate::mesh::client::UpstreamClient::health_probe`'s contract).
    async fn health(&self) -> bool;
}

// ── Shared wire protocol ────────────────────────────────────────────────
//
// All three tiers speak the SAME minimal newline-delimited-JSON
// request/response protocol once their byte stream is established — they
// differ only in how that stream is dialed + authenticated (plain UDS,
// TLS-over-UDS, TLS-over-TCP). Centralizing the framing here means each
// tier's module is only responsible for producing an
// `AsyncRead + AsyncWrite` stream it has already verified the identity of.

#[derive(Debug, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub(crate) enum WireRequest {
    Call { name: String, args: Value },
    List,
    Health,
}

#[derive(Debug, Deserialize, Default)]
pub(crate) struct WireResponse {
    #[serde(default)]
    pub ok: bool,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub structured: Option<Value>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub tools: Option<Vec<String>>,
    #[serde(default)]
    pub healthy: Option<bool>,
}

/// Write one newline-delimited JSON request and read back one
/// newline-delimited JSON response, over an already-authenticated stream.
/// Shared by all three tier implementations.
pub(crate) async fn roundtrip<S>(
    mut stream: S,
    req: &WireRequest,
) -> Result<WireResponse, TransportError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let mut line = serde_json::to_string(req)
        .map_err(|e| TransportError::Protocol(format!("encoding request: {e}")))?;
    line.push('\n');
    stream
        .write_all(line.as_bytes())
        .await
        .map_err(|e| TransportError::Unavailable(format!("write: {e}")))?;
    stream
        .flush()
        .await
        .map_err(|e| TransportError::Unavailable(format!("flush: {e}")))?;

    let mut reader = BufReader::new(stream);
    let mut resp_line = String::new();
    let n = reader
        .read_line(&mut resp_line)
        .await
        .map_err(|e| TransportError::Unavailable(format!("read: {e}")))?;
    if n == 0 {
        return Err(TransportError::Unavailable(
            "worker closed the connection with no response".to_string(),
        ));
    }

    serde_json::from_str(resp_line.trim_end())
        .map_err(|e| TransportError::Protocol(format!("decoding response: {e}")))
}

/// Send a `Call` request and turn the result into the same
/// [`ToolOutput`]/[`ToolError`] shape an in-proc tool would produce. Shared
/// by all three tier implementations' [`WorkerTransport::call`].
pub(crate) async fn call_over<S>(stream: S, name: &str, args: Value) -> Result<ToolOutput, ToolError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let req = WireRequest::Call { name: name.to_string(), args };
    let resp = roundtrip(stream, &req)
        .await
        .map_err(|e| ToolError::Execution(e.to_string()))?;
    if resp.ok {
        Ok(ToolOutput {
            text: resp.text.unwrap_or_default(),
            structured: resp.structured,
        })
    } else {
        Err(ToolError::Execution(
            resp.error.unwrap_or_else(|| "worker returned no error detail".to_string()),
        ))
    }
}

/// Send a `List` request and extract the tool-name list. Shared by all three
/// tier implementations' [`WorkerTransport::list`].
pub(crate) async fn list_over<S>(stream: S) -> Result<Vec<String>, TransportError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let resp = roundtrip(stream, &WireRequest::List).await?;
    if !resp.ok {
        return Err(TransportError::Protocol(
            resp.error.unwrap_or_else(|| "worker rejected list request".to_string()),
        ));
    }
    Ok(resp.tools.unwrap_or_default())
}

/// Send a `Health` request and extract the boolean. Shared by all three tier
/// implementations' [`WorkerTransport::health`].
pub(crate) async fn health_over<S>(stream: S) -> bool
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    match roundtrip(stream, &WireRequest::Health).await {
        Ok(resp) => resp.ok && resp.healthy.unwrap_or(false),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── TransportTier::security_rank ordering ───────────────────────────

    #[test]
    fn security_rank_orders_t1_below_t0_below_t2() {
        assert!(TransportTier::T1.security_rank() < TransportTier::T0.security_rank());
        assert!(TransportTier::T0.security_rank() < TransportTier::T2.security_rank());
    }

    // ── MinTierPolicy ────────────────────────────────────────────────────

    #[test]
    fn read_only_permits_t1_and_above() {
        assert!(MinTierPolicy::permits(CapabilityClass::ReadOnly, TransportTier::T1));
        assert!(MinTierPolicy::permits(CapabilityClass::ReadOnly, TransportTier::T0));
        assert!(MinTierPolicy::permits(CapabilityClass::ReadOnly, TransportTier::T2));
    }

    #[test]
    fn write_scoped_rejects_t1() {
        assert!(!MinTierPolicy::permits(CapabilityClass::WriteScoped, TransportTier::T1));
    }

    #[test]
    fn write_scoped_permits_t2() {
        assert!(MinTierPolicy::permits(CapabilityClass::WriteScoped, TransportTier::T2));
    }

    #[test]
    fn write_scoped_rejects_t0() {
        // T0 is off-box/network-exposed -- not equivalent to T2's on-host +
        // dual-identity-check strength, so a write-scoped worker must not be
        // satisfied by T0 either.
        assert!(!MinTierPolicy::permits(CapabilityClass::WriteScoped, TransportTier::T0));
    }

    #[test]
    fn secret_holding_rejects_t1_and_t0_permits_t2() {
        assert!(!MinTierPolicy::permits(CapabilityClass::SecretHolding, TransportTier::T1));
        assert!(!MinTierPolicy::permits(CapabilityClass::SecretHolding, TransportTier::T0));
        assert!(MinTierPolicy::permits(CapabilityClass::SecretHolding, TransportTier::T2));
    }

    #[test]
    fn minimum_tier_table() {
        assert_eq!(MinTierPolicy::minimum_tier(CapabilityClass::ReadOnly), TransportTier::T1);
        assert_eq!(MinTierPolicy::minimum_tier(CapabilityClass::WriteScoped), TransportTier::T2);
        assert_eq!(MinTierPolicy::minimum_tier(CapabilityClass::SecretHolding), TransportTier::T2);
    }

    // ── Wire protocol round trip (over an in-memory duplex pipe) ────────

    #[tokio::test]
    async fn call_over_success_produces_tool_output() {
        let (mut client, mut server) = tokio::io::duplex(4096);
        let server_task = tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
            let mut reader = BufReader::new(&mut server);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let req: Value = serde_json::from_str(line.trim_end()).unwrap();
            assert_eq!(req["op"], "call");
            assert_eq!(req["name"], "echo");
            let resp = serde_json::json!({"ok": true, "text": "echoed", "structured": {"n": 1}});
            server.write_all(format!("{resp}\n").as_bytes()).await.unwrap();
        });

        let out = call_over(&mut client, "echo", serde_json::json!({"a": 1})).await.unwrap();
        assert_eq!(out.text, "echoed");
        assert_eq!(out.structured, Some(serde_json::json!({"n": 1})));
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn call_over_tool_level_error_is_tool_error_not_panic() {
        let (mut client, mut server) = tokio::io::duplex(4096);
        let server_task = tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
            let mut reader = BufReader::new(&mut server);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let resp = serde_json::json!({"ok": false, "error": "boom"});
            server.write_all(format!("{resp}\n").as_bytes()).await.unwrap();
        });

        let err = call_over(&mut client, "echo", serde_json::json!({})).await.unwrap_err();
        assert!(matches!(err, ToolError::Execution(msg) if msg == "boom"));
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn list_over_extracts_tool_names() {
        let (mut client, mut server) = tokio::io::duplex(4096);
        let server_task = tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
            let mut reader = BufReader::new(&mut server);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let resp = serde_json::json!({"ok": true, "tools": ["a", "b"]});
            server.write_all(format!("{resp}\n").as_bytes()).await.unwrap();
        });

        let tools = list_over(&mut client).await.unwrap();
        assert_eq!(tools, vec!["a".to_string(), "b".to_string()]);
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn health_over_false_on_closed_connection_no_panic() {
        let (client, server) = tokio::io::duplex(4096);
        drop(server);
        assert!(!health_over(client).await);
    }
}
