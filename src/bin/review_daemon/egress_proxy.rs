//! Loopback-only HTTP CONNECT egress proxy: the network half of the `agy`
//! sandbox (see `sandbox.rs` for the filesystem/capability half).
//!
//! ## Why this exists
//! `agy --dangerously-skip-permissions` auto-approves agy's own tool calls,
//! so if `review_run` feeds it adversarial content (a diff crafted as a
//! prompt-injection payload), agy could be tricked into taking real actions
//! -- including reaching an internal LAN service -- rather than just
//! producing a text verdict. agy itself needs REAL internet egress to
//! function (it calls `*.googleapis.com` / `*.azureedge.net` / a Cloud Run
//! auto-updater endpoint for its own backend, confirmed by a live
//! unsandboxed-network trace), so simply cutting all network access (e.g.
//! `bwrap --unshare-net`) breaks it. This proxy is the narrow middle ground:
//! it allows the CONNECT tunnels agy's own genuine traffic needs, while
//! denying any tunnel to an RFC1918/loopback/link-local/multicast
//! destination -- i.e. exactly the internal-LAN reach the isolation is meant
//! to close.
//!
//! ## Why a proxy instead of a network namespace
//! bwrap's own `--unshare-net` gives all-or-nothing network access; getting
//! "real internet, no LAN" at the namespace level normally means attaching a
//! user-mode NIC via `slirp4netns`, which requires a `/dev/net/tun` device
//! that this host does not expose (confirmed: `open("/dev/net/tun"): No such
//! file or directory` when tested live) -- and this box has neither a usable
//! Docker daemon socket nor real root, so a veout/iptables-in-the-host-netns
//! approach is also unavailable. agy is empirically confirmed (live test) to
//! honor `HTTPS_PROXY`/`HTTP_PROXY` (it's a Go binary using the default
//! `net/http` transport, which consults `ProxyFromEnvironment`), so a daemon
//! -owned, IP-range-filtering CONNECT proxy is the mechanism that is actually
//! usable, real, and testable on this box.
//!
//! ## Threat model notes
//! - This proxy binds `127.0.0.1` only, on an OS-assigned ephemeral port
//!   (`:0`), for the daemon's process lifetime. It is not caller-configurable
//!   in any way; `HTTPS_PROXY`/`HTTP_PROXY` pointing at it are injected by
//!   `sandbox.rs`'s fixed argv builder, never derived from request content.
//! - Only the `CONNECT` method is served (matches agy's HTTPS traffic
//!   pattern, observed live). Plain-HTTP proxying (an absolute-URI GET/POST)
//!   is rejected -- narrower is safer, and agy's own traffic is all HTTPS.
//! - The destination is resolved via async DNS *by this proxy*, and the
//!   *resolved IP* is what gets range-checked -- not the hostname string --
//!   so a DNS answer that resolves an otherwise-innocuous-looking name to an
//!   internal address is still denied.
//! - If the daemon fails to bind this proxy at startup, the `agy` provider
//!   must be treated as unavailable (fail-closed) -- see `main.rs`, which
//!   checks `AppState.agy_proxy_port` before ever building the sandboxed
//!   command. agy is never dispatched unproxied.
//!
//! ## Known residual limitation: host loopback is directly reachable
//! `sandbox.rs` does NOT `--unshare-net` (see its module docs for why: this
//! host has no `/dev/net/tun`, so a `slirp4netns`-attached isolated netns
//! that still reaches the real internet isn't possible here) -- agy runs in
//! the HOST network namespace. Confirmed live (dual-review finding,
//! independently reproduced): common HTTP clients (curl, and per Go's own
//! `httpproxy` documentation, `net/http`'s default transport too) do NOT
//! route loopback destinations through a configured `HTTPS_PROXY`/
//! `HTTP_PROXY` at all -- so `is_blocked`'s loopback checks do not actually
//! apply to agy's real traffic, and the sandboxed agy process CAN reach
//! `127.0.0.1:<any host port>` directly, bypassing this proxy entirely.
//! This is a real gap this host's hardware doesn't allow fully closing
//! (would need a network namespace with its own NIC, i.e. `slirp4netns` +
//! `/dev/net/tun`, or root for a veth pair -- neither available; see
//! `sandbox.rs` docs). Mitigations actually in place:
//!   - `review-daemon`'s own bearer token (`REVIEW_DAEMON_TOKEN`) is
//!     stripped from the sandboxed env by `sanitize::sanitized_env()`
//!     before it ever reaches agy, so a sandboxed agy reaching
//!     `127.0.0.1:<review-daemon port>` directly gets a 401, not a working
//!     dispatch credential.
//!   - `sandbox.rs` also sets `NO_PROXY`/`no_proxy` to an empty string, so
//!     an inherited `NO_PROXY` value can't be used to add MORE bypassed
//!     hosts on top of this one.
//! This does NOT protect other unauthenticated loopback-bound services that
//! may exist on the host running review-daemon -- operators deploying this
//! daemon should be aware agy can reach the host's own `127.0.0.1` port
//! range directly, and should not rely on "loopback-only" as a security
//! boundary for anything else running there.
//!
//! ## Known residual limitation: proxy enforcement is not kernel-level
//! (raised in dual review, second round) Because `sandbox.rs` shares the
//! host network namespace (the loopback-reachability tradeoff above), this
//! proxy is only reached because agy's own HTTP client *chooses* to consult
//! `HTTPS_PROXY`/`HTTP_PROXY`. That is real for agy's actual, observed
//! traffic (a Go binary using `net/http`'s default transport) -- but it is
//! NOT a kernel-enforced guarantee against every possible code path inside
//! agy's process tree. If agy's own tool-use surface ever runs a subprocess
//! or opens a raw socket that does not consult those env vars (e.g. a raw
//! `connect()` call, or a tool that explicitly disables proxying), that
//! traffic reaches the shared host network namespace directly -- including
//! RFC1918/LAN addresses, not just loopback -- with nothing at the OS level
//! to stop it. Classic seccomp filtering cannot close this: `connect()`'s
//! destination is passed by pointer, which cBPF cannot dereference, so
//! syscall-argument filtering on the destination address would require a
//! `SECCOMP_RET_USER_NOTIF` supervisor (reading the target's memory via
//! `process_vm_readv` to inspect the actual `sockaddr`) -- a materially
//! larger undertaking not implemented in this pass. The only complete fix
//! is real network-namespace isolation, which (as above) needs
//! `/dev/net/tun` or root, neither available on this host.
//!
//! What IS empirically true, as partial (not structural) mitigation: agy
//! was tested live against three separate prompt-injection framings asking
//! it to run exactly this kind of bypass (a direct instruction, a
//! "pre-approved CI health-check" framing, and -- closest to the real
//! threat model -- an instruction hidden inside a reviewed diff's comment
//! text). In all three, agy's own model-level judgment correctly identified
//! and refused the request as a prompt-injection/security concern, without
//! any OS-level enforcement doing the refusing. This is genuine evidence
//! agy is not trivially exploitable this way today, but it is model
//! behavior, not a security boundary this system can rely on -- a
//! sufficiently different or novel injection payload could behave
//! differently. Treat this proxy as closing the case of agy's OWN
//! legitimate/errant traffic, and treat full protection against an
//! actively-uncooperative agy internals as an accepted, hardware-imposed
//! gap on this host pending either root/tun becoming available or a
//! seccomp-notify supervisor being built as follow-up work.

use std::net::IpAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Max bytes read while looking for the CONNECT request line's terminating
/// `\r\n\r\n` -- bounds memory for a client that never sends one.
const MAX_HEADER_BYTES: usize = 8 * 1024;

/// Start the proxy bound to `127.0.0.1:0` (OS-assigned port). Returns the
/// assigned port and spawns the accept loop as a detached background task
/// that runs for the life of the daemon process. Returns `Err` if the bind
/// itself fails (caller must treat that as "agy sandbox unavailable").
pub async fn spawn() -> std::io::Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    tokio::spawn(accept_loop(listener));
    Ok(port)
}

async fn accept_loop(listener: TcpListener) {
    loop {
        match listener.accept().await {
            Ok((stream, _peer)) => {
                tokio::spawn(async move {
                    if let Err(e) = handle_conn(stream).await {
                        tracing::debug!("egress_proxy: connection handling error: {e}");
                    }
                });
            }
            Err(e) => {
                tracing::warn!("egress_proxy: accept error: {e}");
            }
        }
    }
}

async fn handle_conn(mut stream: TcpStream) -> std::io::Result<()> {
    let request_line = match read_request_line(&mut stream).await {
        Ok(Some(line)) => line,
        Ok(None) => return Ok(()), // connection closed before a full line arrived
        Err(e) => {
            let _ = write_status(&mut stream, "400 Bad Request").await;
            return Err(e);
        }
    };

    let Some((host, port)) = parse_connect_target(&request_line) else {
        let _ = write_status(&mut stream, "400 Bad Request").await;
        return Ok(());
    };

    let target = match tokio::net::lookup_host((host.as_str(), port)).await {
        Ok(mut addrs) => addrs.next(),
        Err(_) => None,
    };

    let Some(target_addr) = target else {
        let _ = write_status(&mut stream, "502 Bad Gateway").await;
        return Ok(());
    };

    if is_blocked(target_addr.ip()) {
        tracing::info!(host = %host, ip = %target_addr.ip(), "egress_proxy: DENY (internal/LAN address)");
        let _ = write_status(&mut stream, "403 Forbidden").await;
        return Ok(());
    }
    tracing::debug!(host = %host, ip = %target_addr.ip(), "egress_proxy: ALLOW");

    let mut upstream = match TcpStream::connect(target_addr).await {
        Ok(s) => s,
        Err(_) => {
            let _ = write_status(&mut stream, "502 Bad Gateway").await;
            return Ok(());
        }
    };

    write_status(&mut stream, "200 Connection Established").await?;
    tokio::io::copy_bidirectional(&mut stream, &mut upstream).await?;
    Ok(())
}

async fn write_status(stream: &mut TcpStream, status: &str) -> std::io::Result<()> {
    stream
        .write_all(format!("HTTP/1.1 {status}\r\n\r\n").as_bytes())
        .await
}

/// Read up to the first `\r\n\r\n` (or `MAX_HEADER_BYTES`, whichever comes
/// first) and return just the request line. We don't need any headers for a
/// CONNECT tunnel -- only the request line's target.
async fn read_request_line(stream: &mut TcpStream) -> std::io::Result<Option<String>> {
    let mut buf = Vec::with_capacity(512);
    let mut chunk = [0u8; 512];
    loop {
        if let Some(pos) = find_subslice(&buf, b"\r\n") {
            let line = String::from_utf8_lossy(&buf[..pos]).to_string();
            return Ok(Some(line));
        }
        if buf.len() > MAX_HEADER_BYTES {
            return Err(std::io::Error::other("request line too large"));
        }
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Parse `"CONNECT host:port HTTP/1.1"` -> `(host, port)`. Anything else
/// (other methods, malformed target) is rejected by returning `None`.
fn parse_connect_target(request_line: &str) -> Option<(String, u16)> {
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?;
    if !method.eq_ignore_ascii_case("CONNECT") {
        return None;
    }
    let target = parts.next()?;
    let (host, port_str) = target.rsplit_once(':')?;
    let port: u16 = port_str.parse().ok()?;
    // An IPv6-literal CONNECT target is bracketed per RFC 7230/3986, e.g.
    // "CONNECT [::1]:443" -- strip the brackets so `lookup_host` (and, for
    // is_blocked's purposes, a direct IpAddr parse) sees a bare address
    // rather than failing to resolve "[::1]" as a hostname. Without this,
    // any IPv6-literal target (not just internal ones) would 502 -- a
    // correctness gap flagged in dual review, harmless from a security
    // standpoint (fails closed) but real.
    let host = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host);
    if host.is_empty() {
        return None;
    }
    Some((host.to_string(), port))
}

/// Whether `ip` is an address this proxy must never tunnel to: RFC1918
/// private ranges, loopback, link-local, multicast/broadcast, or an
/// IPv6 unique-local/mapped equivalent of any of those. This is the entire
/// enforcement point of the sandbox's network policy -- it runs against the
/// *resolved* address, not the hostname string.
fn is_blocked(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
                || v4.is_multicast()
                || is_cgnat_v4(&v4)
        }
        IpAddr::V6(v6) => {
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return is_blocked(IpAddr::V4(mapped));
            }
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || is_unique_local_v6(&v6)
                || is_unicast_link_local_v6(&v6)
        }
    }
}

/// `100.64.0.0/10` (RFC 6598 Carrier-Grade NAT / shared address space) --
/// increasingly used as an internal range by cloud/ISP networks; not
/// covered by `Ipv4Addr::is_private` (which is only the classic RFC1918
/// ranges) and `is_shared()` is nightly-only, so this is checked by hand.
fn is_cgnat_v4(v4: &std::net::Ipv4Addr) -> bool {
    let o = v4.octets();
    o[0] == 100 && (o[1] & 0xc0) == 64
}

/// `fc00::/7` (unique local addresses) -- IPv6's RFC1918 equivalent.
fn is_unique_local_v6(v6: &std::net::Ipv6Addr) -> bool {
    (v6.octets()[0] & 0xfe) == 0xfc
}

/// `fe80::/10` (link-local unicast).
fn is_unicast_link_local_v6(v6: &std::net::Ipv6Addr) -> bool {
    v6.octets()[0] == 0xfe && (v6.octets()[1] & 0xc0) == 0x80
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn blocks_rfc1918_ranges() {
        assert!(is_blocked(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))));
        assert!(is_blocked(IpAddr::V4(Ipv4Addr::new(172, 16, 0, 1))));
        assert!(is_blocked(IpAddr::V4(Ipv4Addr::new(172, 31, 255, 254))));
        assert!(is_blocked(IpAddr::V4(Ipv4Addr::new(192, 168, 0, 223))));
    }

    #[test]
    fn blocks_cgnat_shared_address_space() {
        // 100.64.0.0/10 (RFC 6598) -- flagged in dual review as missing.
        assert!(is_blocked(IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1))));
        assert!(is_blocked(IpAddr::V4(Ipv4Addr::new(100, 100, 0, 1))));
        assert!(is_blocked(IpAddr::V4(Ipv4Addr::new(100, 127, 255, 254))));
        // Just outside the /10 on both sides must NOT be blocked by this rule.
        assert!(!is_blocked(IpAddr::V4(Ipv4Addr::new(100, 63, 255, 255))));
        assert!(!is_blocked(IpAddr::V4(Ipv4Addr::new(100, 128, 0, 0))));
    }

    #[test]
    fn blocks_loopback_and_link_local_and_unspecified() {
        assert!(is_blocked(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))));
        assert!(is_blocked(IpAddr::V4(Ipv4Addr::new(169, 254, 1, 1))));
        assert!(is_blocked(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0))));
        assert!(is_blocked(IpAddr::V6(Ipv6Addr::LOCALHOST)));
        assert!(is_blocked(IpAddr::V6(Ipv6Addr::UNSPECIFIED)));
    }

    #[test]
    fn blocks_ipv6_unique_local_and_link_local() {
        assert!(is_blocked(IpAddr::V6(Ipv6Addr::new(
            0xfd00, 0, 0, 0, 0, 0, 0, 1
        ))));
        assert!(is_blocked(IpAddr::V6(Ipv6Addr::new(
            0xfe80, 0, 0, 0, 0, 0, 0, 1
        ))));
    }

    #[test]
    fn blocks_ipv4_mapped_private_address() {
        // ::ffff:<internal-ip> -- an IPv6-mapped IPv4 private address must // pii-test-fixture
        // still be caught by the IPv4 rules, not slip through as "not IPv4".
        let mapped = Ipv6Addr::new(0, 0, 0, 0, 0, 0xffff, 0xc0a8, 0x0001);
        assert!(is_blocked(IpAddr::V6(mapped)));
    }

    #[test]
    fn allows_public_internet_addresses() {
        // 8.8.8.8 (Google public DNS) and a real public IPv6 address.
        assert!(!is_blocked(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))));
        assert!(!is_blocked(IpAddr::V6(Ipv6Addr::new(
            0x2606, 0x4700, 0, 0, 0, 0, 0x6812, 0x273
        ))));
    }

    #[test]
    fn parse_connect_target_accepts_well_formed_line() {
        let (host, port) = parse_connect_target("CONNECT example.com:443 HTTP/1.1").unwrap();
        assert_eq!(host, "example.com");
        assert_eq!(port, 443);
    }

    #[test]
    fn parse_connect_target_rejects_non_connect_methods() {
        assert!(parse_connect_target("GET http://example.com/ HTTP/1.1").is_none());
        assert!(parse_connect_target("POST /dispatch HTTP/1.1").is_none());
    }

    #[test]
    fn parse_connect_target_strips_ipv6_literal_brackets() {
        let (host, port) = parse_connect_target("CONNECT [::1]:443 HTTP/1.1").unwrap();
        assert_eq!(host, "::1");
        assert_eq!(port, 443);
    }

    #[test]
    fn parse_connect_target_rejects_malformed_target() {
        assert!(parse_connect_target("CONNECT not-a-host-port HTTP/1.1").is_none());
        assert!(parse_connect_target("CONNECT :443 HTTP/1.1").is_none());
        assert!(parse_connect_target("CONNECT example.com:notaport HTTP/1.1").is_none());
    }

    #[tokio::test]
    async fn spawn_binds_loopback_ephemeral_port_and_accepts_connections() {
        let port = spawn().await.expect("proxy should bind");
        assert_ne!(port, 0);
        // A raw TCP connect to the assigned port should succeed (the accept
        // loop is live), independent of what request we then send.
        let stream = TcpStream::connect(("127.0.0.1", port)).await;
        assert!(stream.is_ok());
    }

    #[tokio::test]
    async fn denies_connect_to_a_loopback_target() {
        let port = spawn().await.expect("proxy should bind");
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream
            .write_all(b"CONNECT 127.0.0.1:9999 HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let mut buf = [0u8; 64];
        let n = stream.read(&mut buf).await.unwrap();
        let resp = String::from_utf8_lossy(&buf[..n]);
        assert!(resp.contains("403"), "expected 403 Forbidden, got: {resp}");
    }
}
