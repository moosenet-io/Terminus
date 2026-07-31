//! BLD-05 — sccache environment wiring for the compiler tool.
//!
//! The compiler runs every `cargo` invocation with `RUSTC_WRAPPER=sccache` so
//! compile artifacts are shared across build hosts through the terminus-primary
//! Redis (BLD-20). Two hard requirements shape this module:
//!
//! 1. **Prefer the SPLIT env form.** sccache 0.10.0 accepts either a single
//!    `SCCACHE_REDIS` URL OR the split `SCCACHE_REDIS_ENDPOINT` /
//!    `SCCACHE_REDIS_USERNAME` / `SCCACHE_REDIS_PASSWORD` / `SCCACHE_REDIS_DB` /
//!    `SCCACHE_REDIS_KEY_PREFIX` variables. In testing a plain `SCCACHE_REDIS`
//!    URL silently fell back to the local disk cache (no Redis hits), so we parse
//!    the auth'd URL (`redis://<user>:<pass>@<host>:<port>/<db>`) OUT of the
//!    `SCCACHE_REDIS` secret and export the split form, which connects reliably.
//! 2. **Fail OPEN.** If the Redis endpoint secret is absent or unparseable, the
//!    build must NEVER fail on the cache — sccache is pointed at a local disk
//!    directory (`${BUILD_DATASET_ROOT}/cache/sccache`) instead. A cache outage
//!    degrades to a slower cold build, never a broken one.
//!
//! ## Secrets (S1/S7)
//! The endpoint+auth is read from the `SCCACHE_REDIS` env var, which is
//! materialized from the runtime secret store into the process environment at
//! boot (see `crate::secrets_bootstrap`). This module never contains a literal
//! endpoint, host, port, or password, and the parsed password is placed only in
//! the child process's env map — it is never logged (`describe()` redacts it).

use std::collections::BTreeMap;

use tracing::warn;

/// The env-var name (materialized from the vault) carrying the auth'd Redis URL
/// sccache should use — a full `redis://default:<pass>@<host>:<port>/<db>`.
const SCCACHE_REDIS_SECRET: &str = "SCCACHE_REDIS";

/// Overridable sccache binary name/path (`SCCACHE_BIN`); default `sccache`
/// (a bare binary assumed on the build host's PATH — not an infra literal).
const SCCACHE_BIN_ENV: &str = "SCCACHE_BIN";

/// Stable key prefix so every constellation build shares one logical keyspace
/// in the Redis `sccache:*` namespace (matches `crate::redis::Namespace::Sccache`).
const KEY_PREFIX: &str = "sccache";

/// Which backend sccache was wired to, for logging / `compiler_status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SccacheMode {
    /// Shared Redis backend (the fast path — split env parsed from the secret).
    Redis,
    /// Local disk fallback (fail-open: secret absent or unparseable).
    LocalDir,
}

impl SccacheMode {
    pub fn as_str(self) -> &'static str {
        match self {
            SccacheMode::Redis => "redis",
            SccacheMode::LocalDir => "local-dir",
        }
    }
}

/// The resolved sccache wiring: the env vars to layer onto the cargo child, plus
/// which backend was selected.
#[derive(Debug, Clone)]
pub struct SccacheEnv {
    /// Env vars to set on the cargo child process (`RUSTC_WRAPPER` + backend).
    pub vars: BTreeMap<String, String>,
    pub mode: SccacheMode,
}

impl SccacheEnv {
    /// The sccache binary the compiler should invoke for `--show-stats` etc.
    pub fn binary() -> String {
        env_nonempty(SCCACHE_BIN_ENV).unwrap_or_else(|| "sccache".to_string())
    }

    /// A single-line, secret-free summary for logs.
    pub fn describe(&self) -> String {
        match self.mode {
            SccacheMode::Redis => {
                let ep = self
                    .vars
                    .get("SCCACHE_REDIS_ENDPOINT")
                    .map(String::as_str)
                    .unwrap_or("?");
                format!("sccache→redis endpoint={ep} (password redacted)")
            }
            SccacheMode::LocalDir => {
                let dir = self
                    .vars
                    .get("SCCACHE_DIR")
                    .map(String::as_str)
                    .unwrap_or("?");
                format!("sccache→local-dir {dir} (fail-open: Redis unavailable)")
            }
        }
    }
}

/// Read a trimmed, non-empty env var; `None` when unset/empty.
fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// The parsed pieces of a `redis://[user[:pass]@]host[:port][/db]` URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedisUrlParts {
    /// Endpoint WITHOUT auth or db, e.g. `redis://host:6379` — the value
    /// sccache's `SCCACHE_REDIS_ENDPOINT` expects.
    pub endpoint: String,
    /// Host without brackets (for the reachability probe).
    pub host: String,
    /// Port, ALREADY validated (1..=65535) when present. `None` when the URL
    /// omitted the port (a caller defaults it to 6379 for the probe). A
    /// present-but-invalid port never reaches here — it fails the whole parse.
    pub port: Option<u16>,
    pub username: Option<String>,
    pub password: Option<String>,
    /// Logical DB index as a string (sccache wants it as text), if present.
    pub db: Option<String>,
}

/// Parse a port string as a valid TCP port (1..=65535). Empty / non-numeric /
/// zero / overflow ⇒ `None` (treated as invalid by the caller).
fn parse_port(s: &str) -> Option<u16> {
    match s.parse::<u16>() {
        Ok(p) if p >= 1 => Some(p),
        _ => None,
    }
}

/// Split `host[:port]` (or `[ipv6][:port]`) into `(host, Option<port>)`. Returns
/// `None` when a port is PRESENT but invalid (non-numeric / zero / out of range /
/// empty after `:`), so the caller treats the whole URL as unparseable. An ABSENT
/// port ⇒ `None` port (defaulted downstream). Strips IPv6 brackets from the host.
fn split_host_port(hostport: &str) -> Option<(String, Option<u16>)> {
    // IPv6 literal: `[::1]` or `[::1]:6379`.
    if let Some(rest) = hostport.strip_prefix('[') {
        let (host, tail) = rest.split_once(']')?;
        if host.is_empty() {
            return None;
        }
        return match tail {
            "" => Some((host.to_string(), None)),
            // Anything after `]` must be exactly `:port`.
            t => {
                let p = t.strip_prefix(':')?;
                Some((host.to_string(), Some(parse_port(p)?)))
            }
        };
    }
    match hostport.rsplit_once(':') {
        Some((h, p)) => {
            if h.is_empty() {
                return None;
            }
            Some((h.to_string(), Some(parse_port(p)?)))
        }
        None => Some((hostport.to_string(), None)),
    }
}

/// Parse a `redis://` / `rediss://` URL into its endpoint + auth + db parts.
/// `None` when the scheme is not a redis scheme, the host is empty, OR a port is
/// present but invalid (non-numeric / zero / out of 1..=65535) — a malformed port
/// makes the URL unparseable so the caller fails OPEN to the local cache dir
/// rather than exporting a bogus endpoint (S7). Deliberately dependency-free (no
/// `url` crate) so parsing is trivially unit-testable and the password never
/// transits a logging-prone type.
pub fn parse_redis_url(url: &str) -> Option<RedisUrlParts> {
    let url = url.trim();
    let (scheme, rest) = url.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "redis" && scheme != "rediss" {
        return None;
    }

    // Split optional `userinfo@` from `host:port/db`.
    let (userinfo, hostpart) = match rest.rsplit_once('@') {
        Some((u, h)) => (Some(u), h),
        None => (None, rest),
    };

    // Split optional `/db` (and drop any `?query`) off the host:port.
    let hostport_db = hostpart.split('?').next().unwrap_or(hostpart);
    let (hostport, db) = match hostport_db.split_once('/') {
        Some((hp, d)) if !d.is_empty() => (hp, Some(d.to_string())),
        Some((hp, _)) => (hp, None),
        None => (hostport_db, None),
    };
    if hostport.is_empty() {
        return None;
    }

    // Validate host + optional port; a present-but-invalid port fails the parse.
    let (host, port) = split_host_port(hostport)?;

    let (username, password) = match userinfo {
        Some(ui) => match ui.split_once(':') {
            Some((u, p)) => (
                (!u.is_empty()).then(|| u.to_string()),
                (!p.is_empty()).then(|| p.to_string()),
            ),
            None => ((!ui.is_empty()).then(|| ui.to_string()), None),
        },
        None => (None, None),
    };

    Some(RedisUrlParts {
        endpoint: format!("{scheme}://{hostport}"),
        host,
        port,
        username,
        password,
        db,
    })
}

/// Default reachability-probe timeout (ms) for the resolved Redis endpoint,
/// overridable via `SCCACHE_REDIS_PROBE_MS`. Kept sub-second so resolving the
/// sccache backend never stalls a build; a dead endpoint fails open fast.
const DEFAULT_PROBE_MS: u64 = 300;

/// The ambient `SCCACHE_REDIS` secret URL from the process environment (the full
/// `redis://user:pass@host:port/db` value), if configured. Exposed so callers can
/// add it to a redaction set — the child build inherits this ambient env var, so
/// a build script could echo the full URL even though the compiler wires the
/// split form. Returns the raw value (a secret) — do not log it.
pub fn ambient_secret_url() -> Option<String> {
    env_nonempty(SCCACHE_REDIS_SECRET)
}

/// Build the sccache env for a build, reading the `SCCACHE_REDIS` secret from the
/// process environment (materialized from the vault). Fails OPEN to a local disk
/// cache under `dataset_root` when the secret is absent, unparseable, **or the
/// endpoint is unreachable** — so a syntactically-valid-but-dead Redis never
/// makes a build depend on sccache runtime behavior.
///
/// `dataset_root` is `${BUILD_DATASET_ROOT}`; the local fallback lives at
/// `${BUILD_DATASET_ROOT}/cache/sccache` (per the BLD-05 spec edge case).
pub fn resolve(dataset_root: &str) -> SccacheEnv {
    let timeout = probe_timeout();
    from_secret_with_probe(
        env_nonempty(SCCACHE_REDIS_SECRET).as_deref(),
        dataset_root,
        |parts| redis_usable(parts, timeout),
    )
}

/// The configured probe timeout.
fn probe_timeout() -> std::time::Duration {
    let ms = env_nonempty("SCCACHE_REDIS_PROBE_MS")
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_PROBE_MS)
        .max(1);
    std::time::Duration::from_millis(ms)
}

/// Fast bounded TCP-connect reachability check. `Some(stream)` iff a connection
/// to any resolved address of `host:port` succeeds within `timeout`. Non-fatal —
/// callers fall open when it is `None`.
fn tcp_connect(host: &str, port: u16, timeout: std::time::Duration) -> Option<std::net::TcpStream> {
    use std::net::ToSocketAddrs;
    let addrs = (host, port).to_socket_addrs().ok()?;
    addrs
        .into_iter()
        .find_map(|addr| std::net::TcpStream::connect_timeout(&addr, timeout).ok())
}

/// Encode a RESP array command (`*N\r\n$len\r\n<arg>\r\n…`) — the wire form every
/// Redis server accepts, including during the pre-auth phase.
fn resp_command(args: &[&str]) -> Vec<u8> {
    let mut out = format!("*{}\r\n", args.len()).into_bytes();
    for a in args {
        out.extend_from_slice(format!("${}\r\n", a.len()).as_bytes());
        out.extend_from_slice(a.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out
}

/// TERM #564: a REAL usability probe for the sccache Redis backend — connect,
/// **AUTHENTICATE**, and `PING`. `true` only when the server answers `+PONG`.
///
/// This replaces a bare TCP-connect check, and the difference is the whole bug.
/// sccache's "fail open" only ever existed at *resolve* time here; sccache
/// itself fails **CLOSED at run time**. A Redis that is listening but rejects
/// our credentials passed the old TCP probe, so the compiler wired
/// `RUSTC_WRAPPER=sccache` + the Redis backend, and then EVERY cargo invocation
/// died about one second in with:
///
/// ```text
/// sccache: error: Server startup failed: cache storage failed to read: …
///     service: redis  path: .sccache_check
///     Source: NOAUTH: Authentication required.
/// ```
///
/// cargo exits 101 having compiled nothing and run zero tests — which the
/// `mode=test` gate then reported as a bare `FAIL (0 passed, 0 failed, 0
/// ignored)`. A cache outage MUST degrade to a slower cold build, never a dead
/// gate (module doc, requirement 2), so auth failure has to fall open exactly
/// like unreachability does.
///
/// Deliberately hand-rolled RESP over a plain `TcpStream` (no redis client
/// dependency, no async runtime) and strictly bounded by `timeout` on connect,
/// read, and write — resolving the sccache backend must never stall a build.
/// Any error, timeout, or non-`+PONG` reply ⇒ `false` ⇒ fail open.
///
/// ## `rediss://` (TLS) is treated as UNUSABLE, deliberately
/// This probe speaks PLAINTEXT RESP, so it cannot verify a TLS endpoint at all:
/// no AUTH, no PING, no answer it could honestly interpret. It therefore reports
/// `rediss://` as **unusable** and the caller falls open to the local cache dir.
///
/// This is a deliberate asymmetry, not an oversight. Returning "healthy" on the
/// strength of a bare TCP connect is EXACTLY the defect TERM #564 exists to
/// remove — a false "healthy" wires the Redis backend and then kills every cargo
/// invocation at sccache startup, producing a silent empty test gate; a false
/// "unhealthy" costs only a slower, locally-cached build. The cheap failure is
/// the correct one to take when we cannot actually verify. Do NOT "fix" this by
/// returning `true` for TLS; the honest fix, if a TLS endpoint is ever really
/// deployed, is to speak a real TLS handshake here.
fn redis_usable(parts: &RedisUrlParts, timeout: std::time::Duration) -> bool {
    use std::io::{Read, Write};

    let (host, port) = endpoint_host_port(parts);
    // TLS endpoint: unverifiable by this plaintext probe ⇒ report unusable so the
    // caller falls open to the local cache dir (see the doc comment above). Checked
    // BEFORE connecting — there is nothing a connect could tell us here.
    if parts.endpoint.starts_with("rediss://") {
        warn!(
            "sccache: Redis endpoint {}:{} is a TLS (rediss://) endpoint, which this plaintext \
             probe cannot verify — treating it as unusable and falling open to the local cache \
             dir. The build is UNAFFECTED apart from a cold cache (TERM #564)",
            host, port
        );
        return false;
    }
    let Some(mut stream) = tcp_connect(&host, port, timeout) else {
        return false;
    };
    if stream.set_read_timeout(Some(timeout)).is_err()
        || stream.set_write_timeout(Some(timeout)).is_err()
    {
        return false;
    }

    let mut req = Vec::new();
    // Whether an `AUTH` reply line precedes the `PING` reply in what we read back.
    let expect_auth = parts.password.is_some();
    if let Some(pass) = parts.password.as_deref() {
        // ACL form (`AUTH <user> <pass>`) when a username is present, legacy
        // single-arg form otherwise — matching what sccache itself will send.
        match parts.username.as_deref() {
            Some(user) => req.extend_from_slice(&resp_command(&["AUTH", user, pass])),
            None => req.extend_from_slice(&resp_command(&["AUTH", pass])),
        }
    }
    req.extend_from_slice(&resp_command(&["PING"]));
    if stream.write_all(&req).is_err() || stream.flush().is_err() {
        return false;
    }

    // Read whatever arrives within the budget. A successful exchange is short
    // (`+OK\r\n+PONG\r\n` when we authenticated, `+PONG\r\n` when we did not); an
    // auth failure is a single `-NOAUTH …` / `-WRONGPASS …` error line. Read until
    // [`evaluate_probe_reply`] can decide, we hit EOF/error/timeout, or we fill the
    // small buffer — every read bounded by `timeout`, so a silent or hung server
    // costs at most `timeout` and NEVER stalls the build.
    let mut seen: Vec<u8> = Vec::new();
    let mut buf = [0u8; 128];
    let mut verdict = ProbeVerdict::NeedMore;
    while seen.len() < 512 {
        match stream.read(&mut buf) {
            // EOF, a read error, or a read TIMEOUT — stop and judge what we have
            // (which, for a server that said nothing at all, is `NeedMore` ⇒ false).
            Ok(0) | Err(_) => break,
            Ok(n) => {
                seen.extend_from_slice(&buf[..n]);
                verdict = evaluate_probe_reply(&seen, expect_auth);
                if !matches!(verdict, ProbeVerdict::NeedMore) {
                    break;
                }
            }
        }
    }
    if matches!(verdict, ProbeVerdict::Usable) {
        // The parser is satisfied by the bytes we have — but "satisfied" only
        // covers what already arrived. TCP does not preserve message boundaries,
        // so `+OK\r\n+PONG\r\n` and a later `GARBAGE\r\n` can land in SEPARATE
        // segments; returning here would accept a chatty non-Redis server purely
        // because its extra bytes were slow. Spend one small bounded window
        // confirming nothing else is coming (round-3 review finding).
        if no_unsolicited_trailing_bytes(&mut stream) {
            return true;
        }
        warn!(
            "sccache: Redis endpoint {}:{} answered AUTH/PING correctly but then sent \
             UNSOLICITED trailing bytes — we spoke only AUTH/PING, so this is not a \
             well-behaved Redis. Treating it as unusable and falling open to the local cache \
             dir. The build is UNAFFECTED apart from a cold cache (TERM #564)",
            host, port
        );
        return false;
    }
    let reply = String::from_utf8_lossy(&seen);
    // The server's own error line (`-NOAUTH …`, `-WRONGPASS …`) is a status
    // string, never an echo of the credential we sent, so it is safe to log and
    // is the single most useful thing an operator can see here.
    let detail = reply.lines().next().unwrap_or("").trim();
    warn!(
        "sccache: Redis endpoint {}:{} did not answer PING (auth rejected, or not a Redis \
         server): {:?} — falling open to the local cache dir. The build is UNAFFECTED apart \
         from a cold cache; previously this misconfiguration made every cargo invocation \
         die at sccache startup with zero tests run (TERM #564)",
        host, port, detail
    );
    false
}

/// How long to watch for unsolicited trailing bytes AFTER the probe's reply is
/// already complete and affirmative (round-3 review finding).
///
/// The tradeoff, explicitly: this window is paid by every HEALTHY probe, because
/// a healthy Redis says nothing more after `+PONG` and the only way to learn that
/// is to wait. So it must be small — but a segmented `GARBAGE` line from a chatty
/// non-Redis server on loopback or a LAN arrives in well under a millisecond, so
/// even a short window catches it. 100 ms is far longer than any plausible
/// same-datacentre inter-segment gap, far shorter than the probe's own
/// `DEFAULT_PROBE_MS` (300 ms) budget, and utterly negligible against a build
/// that this probe runs ONCE for. Do NOT extend this to the full probe timeout:
/// that would slow every healthy build for no additional signal.
///
/// This value IS the bound of what the check can claim. Bytes arriving after it
/// are not detected, and no larger value removes that — a bounded probe cannot
/// prove a peer will never speak again, it can only move the boundary while
/// charging every healthy build for the move. See
/// [`no_unsolicited_trailing_bytes`] for the guaranteed/not-guaranteed split.
const POST_SUCCESS_DRAIN_MS: u64 = 100;

/// Bounded post-success check: `true` iff we can CONFIRM that nothing
/// unexpected arrives on `stream` within [`POST_SUCCESS_DRAIN_MS`] after a
/// complete, affirmative reply.
///
/// `evaluate_probe_reply` rejects trailing garbage that was already COALESCED
/// into the read buffer, but it can only judge bytes it has been given. This
/// closes the segmentation hole: trailing garbage is unhealthy whether or not it
/// happened to land in the first read.
///
/// ## The bound, stated honestly (round-6): what this DOES and does NOT prove
///
/// GUARANTEED — detected and reported UNHEALTHY: unsolicited bytes arriving
/// within [`POST_SUCCESS_DRAIN_MS`] of a complete affirmative reply; a clean EOF
/// that arrives with extra bytes; a connection reset; any other I/O error; and an
/// inability to bound the read at all.
///
/// NOT guaranteed, and NOT achievable: bytes arriving AFTER that window. A server
/// may answer `+PONG`, stay silent past the drain, and only then speak garbage,
/// and this probe will have already returned healthy. That is not a bug to be
/// fixed by waiting longer: **no bounded probe can prove a peer will never speak
/// again**, so extending the window only buys build latency for a boundary that
/// still exists. An earlier revision of this comment claimed the invariant was
/// "trailing garbage is ALWAYS unhealthy" — that absolute is not satisfiable by
/// any bounded check, and TERM #564 exists precisely because a documented-but-
/// unenforced invariant is worse than an honest, narrower one.
///
/// Why the residual risk is acceptable HERE specifically: this probe answers one
/// narrow question — "is this endpoint usable as an sccache backend right now?"
/// A real Redis sends nothing after `+PONG`, so the window is decisive for every
/// well-behaved and every chatty-on-loopback peer we can realistically meet. And
/// if an endpoint misbehaves LATER, the failure surfaces as a slow or failing
/// build — not as the silent empty gate that motivated this work.
///
/// **The rule, stated once, with no list of exceptions to remember: the ONLY
/// healthy outcomes of this check are a CLEAN EOF and BOUNDED SILENCE.
/// Everything else — unsolicited bytes, a connection reset, any other I/O
/// error, or an inability to bound the read at all — is UNHEALTHY.** An earlier
/// round of this code stated the rule as "…and any other error is also healthy,
/// because we already hold a good reply", and that exception is exactly what let
/// a check that never COMPLETED report itself as a check that PASSED.
///
/// Why "could not verify ⇒ unhealthy" is the right asymmetry: "unhealthy" here
/// is not a failure, it is a FALLBACK. The caller drops to the local sccache dir
/// and the build proceeds, just without a shared cache. So:
/// - wrongly unhealthy ⇒ a slower build. Recoverable, invisible, cheap.
/// - wrongly healthy ⇒ cargo launches with `RUSTC_WRAPPER=sccache` against a
///   Redis we could NOT verify, dies ~1 s in having compiled nothing, and the
///   gate reports `0 passed / 0 failed / no summary` — which every consumer
///   misreads as "no failures". That is the original TERM #564 bug.
///
/// Returning `true` because verification could not be COMPLETED is fail-closed
/// disguised as fail-open. Do not reintroduce it.
///
/// ⚠ READ THIS BEFORE "FIXING" THE TIMEOUT HANDLING — the timeout arm is the
/// inverse of every other timeout in this module. Everywhere else a read timeout
/// means "the server never answered" ⇒ UNUSABLE. HERE the server has already
/// answered correctly and we are only listening for bytes we hope never come, so
/// a timeout with nothing received is the EXPECTED HEALTHY outcome ⇒ `true`.
/// Treating it as a failure would declare every real Redis unusable and give the
/// whole fleet cold builds. That single inversion is deliberate; nothing else in
/// here is.
///
/// The decision itself lives in [`post_success_drain_is_healthy`], which is pure
/// and exhaustively unit-tested.
fn no_unsolicited_trailing_bytes(stream: &mut std::net::TcpStream) -> bool {
    use std::io::Read;

    let window = std::time::Duration::from_millis(POST_SUCCESS_DRAIN_MS);
    if stream.set_read_timeout(Some(window)).is_err() {
        // We cannot BOUND the read, so we cannot run the check at all: an
        // unbounded read could stall the build, and returning early would report
        // "verified" for a verification that never happened. Unhealthy ⇒ the
        // caller falls open to the local cache dir.
        return post_success_drain_is_healthy(None);
    }
    let mut buf = [0u8; 64];
    post_success_drain_is_healthy(Some(&stream.read(&mut buf)))
}

/// The WHOLE decision of the post-success drain, as a pure function over the
/// read's outcome — so every arm is directly unit-testable, including the
/// "could not bound the read" case, which cannot be provoked through a live
/// socket without unreasonable contortions.
///
/// `read` is `None` when the read could not even be ATTEMPTED under a bound
/// (i.e. `set_read_timeout` failed).
///
/// | outcome | verdict |
/// |---|---|
/// | `None` — the read could not be bounded, so nothing was verified | **UNHEALTHY** |
/// | `Ok(0)` — clean EOF, server closed saying nothing further | healthy |
/// | `Ok(n > 0)` — unsolicited trailing bytes | **UNHEALTHY** |
/// | `Err(WouldBlock)` / `Err(TimedOut)` — bounded silence | healthy |
/// | `Err(_)` — reset, or any other I/O error | **UNHEALTHY** |
///
/// Two healthy outcomes, everything else unhealthy. That rule is easier to keep
/// true than the list of exceptions it replaced.
///
/// Scope note (round-6): this table is exhaustive over the OUTCOMES OF ONE
/// BOUNDED READ, which is all this function is given. It is not a claim that a
/// healthy verdict proves the peer stays silent forever — see the
/// guaranteed/not-guaranteed split on [`no_unsolicited_trailing_bytes`].
fn post_success_drain_is_healthy(read: Option<&std::io::Result<usize>>) -> bool {
    match read {
        // Could not bound the read ⇒ could not verify ⇒ not healthy.
        None => false,
        // Clean EOF with nothing further — healthy. A close is not garbage.
        Some(Ok(0)) => true,
        // Unsolicited bytes after a complete reply — unhealthy, fall open.
        Some(Ok(_)) => false,
        // ⚠ A window that expired in SILENCE is the GOOD case here (the one
        // deliberate inversion — see the doc comment above). Every OTHER I/O
        // error (a reset, in particular) means the check did not complete, and
        // an incomplete check is never a pass.
        Some(Err(e)) => matches!(
            e.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ),
    }
}

/// The index of the first occurrence of `needle` in `hay`, if any.
fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > hay.len() {
        return needle.is_empty().then_some(0);
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// The decision state of the probe's bounded read loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeVerdict {
    /// The server produced a well-formed affirmative `+PONG` status line (after a
    /// `+OK` to our `AUTH`, when we sent one) ⇒ the backend is usable.
    Usable,
    /// The server answered, but not affirmatively — an error line (`-NOAUTH …`,
    /// `-WRONGPASS …`, `-ERR …`), a non-status reply type, or anything other than
    /// exactly the expected status lines ⇒ fail open.
    Unusable,
    /// Not enough COMPLETE lines yet to decide; keep reading (bounded).
    NeedMore,
}

/// TERM #564 (review finding 3): judge the probe's reply as WELL-FORMED RESP
/// status lines rather than by substring search.
///
/// The earlier implementation accepted `+PONG` found ANYWHERE in the buffer,
/// which would say "usable" for `-ERR unknown command '+PONG'`, for a bulk string
/// whose payload happened to contain those bytes, or for any non-Redis server
/// that echoed them — the same class of "declare healthy on flimsy evidence" bug
/// this whole change exists to remove. Now the reply must literally BE the
/// expected status lines:
///
/// - when we sent `AUTH`, the first complete line must be exactly `+OK`;
/// - the next complete line must be exactly `+PONG`.
///
/// Only COMPLETE (CRLF-terminated) lines are considered — a trailing partial line
/// yields [`ProbeVerdict::NeedMore`] so a reply split across TCP reads is not
/// misjudged. Anything malformed, unexpected, or non-affirmative ⇒
/// [`ProbeVerdict::Unusable`] ⇒ the caller falls open to the local cache dir.
///
/// TERM #565 (round-2 review finding 3): the reply must be the expected status
/// lines and NOTHING ELSE. The whole buffer is validated, not just its prefix — a
/// `+PONG` followed by ANY unexpected trailing bytes (a further line, or a partial
/// line we did not ask for) is [`ProbeVerdict::Unusable`], because we only spoke
/// `AUTH`/`PING` and a well-behaved Redis answers those with exactly one status
/// line each. Accepting `+PONG\r\nGARBAGE\r\n` would be the same "healthy on
/// flimsy evidence" bug one level down. This does NOT weaken the tri-state: a
/// partial reply arrives BEFORE the terminating `+PONG` line, so a split read
/// still yields `NeedMore` (verified by test) — only bytes AFTER a complete,
/// satisfied response are rejected.
fn evaluate_probe_reply(seen: &[u8], expect_auth: bool) -> ProbeVerdict {
    // Split into COMPLETE CRLF-terminated lines; ignore any trailing partial.
    let mut lines: Vec<&[u8]> = Vec::new();
    let mut rest = seen;
    while let Some(idx) = find_subslice(rest, b"\r\n") {
        lines.push(&rest[..idx]);
        rest = &rest[idx + 2..];
    }

    let mut next = 0usize;
    if expect_auth {
        match lines.get(next) {
            None => return ProbeVerdict::NeedMore,
            // Exactly `+OK` — not merely "starts with" or "contains".
            Some(l) if *l == b"+OK" => next += 1,
            // `-NOAUTH …`, `-WRONGPASS …`, `-ERR Client sent AUTH, but no
            // password is set`, or any other reply: sccache would hit the same
            // wall at startup, so fail open.
            Some(_) => return ProbeVerdict::Unusable,
        }
    }
    match lines.get(next) {
        None => ProbeVerdict::NeedMore,
        Some(l) if *l == b"+PONG" => {
            // The response is COMPLETE here. Anything still in the buffer —
            // another complete line, or a trailing partial one — is unsolicited
            // and makes this not the bounded reply we asked for.
            if lines.len() > next + 1 || !rest.is_empty() {
                return ProbeVerdict::Unusable;
            }
            ProbeVerdict::Usable
        }
        Some(_) => ProbeVerdict::Unusable,
    }
}

/// The `(host, port)` for the reachability probe. The port was already validated
/// during parsing (a malformed one would have failed the parse), so an absent
/// port is the ONLY reason to default — to the standard Redis port 6379.
pub fn endpoint_host_port(parts: &RedisUrlParts) -> (String, u16) {
    (parts.host.clone(), parts.port.unwrap_or(6379))
}

/// Pure builder (a test entry point): OPTIONAL secret URL + dataset root, with
/// no reachability check (assumes the endpoint is reachable). `None`/unparseable
/// ⇒ fail-open local dir. Retained for the split-env / fail-open-on-missing
/// tests; production goes through [`resolve`] (which probes).
pub fn from_secret(secret_url: Option<&str>, dataset_root: &str) -> SccacheEnv {
    from_secret_with_probe(secret_url, dataset_root, |_| true)
}

/// The full builder (the injectable test entry point): selects Redis mode ONLY
/// when the URL parses AND `probe(parts)` returns `true`; otherwise fails
/// OPEN to the local disk cache. Injecting `probe` makes the unusable-endpoint
/// decision offline-testable.
///
/// TERM #564: `probe` takes the WHOLE [`RedisUrlParts`] (not just host+port) so
/// production can check that the endpoint is not merely listening but actually
/// USABLE with our credentials — see [`redis_usable`]. A reachable-but-
/// unauthenticated Redis used to pass the old host/port-only probe and then kill
/// every build at sccache startup.
pub fn from_secret_with_probe(
    secret_url: Option<&str>,
    dataset_root: &str,
    probe: impl Fn(&RedisUrlParts) -> bool,
) -> SccacheEnv {
    let mut vars = BTreeMap::new();
    // Always wrap rustc with sccache; the backend below decides where objects go.
    vars.insert("RUSTC_WRAPPER".to_string(), SccacheEnv::binary());

    let fail_open = |mut vars: BTreeMap<String, String>| {
        // Fail OPEN: point sccache at a local disk directory so a Redis outage,
        // an unconfigured endpoint, or an unreachable one never blocks a build.
        vars.insert("SCCACHE_DIR".to_string(), local_cache_dir(dataset_root));
        SccacheEnv {
            vars,
            mode: SccacheMode::LocalDir,
        }
    };

    let parts = match secret_url.and_then(parse_redis_url) {
        Some(p) => p,
        None => return fail_open(vars),
    };

    // Usability gate: a syntactically valid but dead — or reachable-but-
    // unauthenticated (TERM #564) — endpoint falls open.
    if !probe(&parts) {
        let (host, port) = endpoint_host_port(&parts);
        warn!(
            "sccache: Redis endpoint {}:{} unusable — falling open to local cache dir",
            host, port
        );
        return fail_open(vars);
    }

    vars.insert("SCCACHE_REDIS_ENDPOINT".to_string(), parts.endpoint);
    if let Some(u) = parts.username {
        vars.insert("SCCACHE_REDIS_USERNAME".to_string(), u);
    }
    if let Some(p) = parts.password {
        vars.insert("SCCACHE_REDIS_PASSWORD".to_string(), p);
    }
    if let Some(db) = parts.db {
        vars.insert("SCCACHE_REDIS_DB".to_string(), db);
    }
    vars.insert(
        "SCCACHE_REDIS_KEY_PREFIX".to_string(),
        KEY_PREFIX.to_string(),
    );
    SccacheEnv {
        vars,
        mode: SccacheMode::Redis,
    }
}

/// The local disk fallback cache dir under the dataset root.
pub fn local_cache_dir(dataset_root: &str) -> String {
    let root = dataset_root.trim_end_matches('/');
    format!("{root}/cache/sccache")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    const DATASET: &str = "/data/build";

    /// RAII guard that removes `SCCACHE_BIN` for the duration of a test and
    /// restores whatever value (if any) was ambient beforehand on drop.
    /// `SccacheEnv::binary()` (used unconditionally by `from_secret*`, even
    /// when the caller passes an explicit `secret_url`) reads this var, so
    /// any test asserting `RUSTC_WRAPPER == "sccache"` is silently
    /// environment-dependent otherwise. This is not hypothetical: the
    /// compiler test-gate itself runs `cargo test` with `SCCACHE_BIN` set
    /// (so its OWN build is sccache-wrapped), which broke exactly these
    /// assertions on clean `main` — reproduced locally by exporting
    /// `SCCACHE_BIN=/opt/some/sccache-path` before `cargo test`.
    struct ScopedNoSccacheBin(Option<String>);

    impl ScopedNoSccacheBin {
        fn new() -> Self {
            let prior = std::env::var(SCCACHE_BIN_ENV).ok();
            std::env::remove_var(SCCACHE_BIN_ENV);
            Self(prior)
        }
    }

    impl Drop for ScopedNoSccacheBin {
        fn drop(&mut self) {
            match &self.0 {
                Some(v) => std::env::set_var(SCCACHE_BIN_ENV, v),
                None => std::env::remove_var(SCCACHE_BIN_ENV),
            }
        }
    }

    #[test]
    fn parses_full_authd_url() {
        let p = parse_redis_url("redis://default:<email>:6379/1").unwrap(); // pii-test-fixture (synthetic placeholder credential + RFC 2606 .invalid host, not an email)
        assert_eq!(p.endpoint, "redis://cache.invalid:6379");
        assert_eq!(p.username.as_deref(), Some("default"));
        assert_eq!(p.password.as_deref(), Some("placeholder-password"));
        assert_eq!(p.db.as_deref(), Some("1"));
    }

    #[test]
    fn parses_url_without_auth_or_db() {
        let p = parse_redis_url("redis://cache.invalid:6379").unwrap();
        assert_eq!(p.endpoint, "redis://cache.invalid:6379");
        assert_eq!(p.username, None);
        assert_eq!(p.password, None);
        assert_eq!(p.db, None);
    }

    #[test]
    fn parses_password_only_userinfo() {
        // `redis://:pass@host/2` — no username, password present.
        let p = parse_redis_url("redis://:<email>:6379/2").unwrap(); // pii-test-fixture (synthetic placeholder credential + RFC 2606 .invalid host, not an email)
        assert_eq!(p.username, None);
        assert_eq!(p.password.as_deref(), Some("placeholder-password"));
        assert_eq!(p.db.as_deref(), Some("2"));
    }

    #[test]
    fn rejects_non_redis_scheme() {
        assert!(parse_redis_url("http://host:6379/1").is_none());
        assert!(parse_redis_url("not a url").is_none());
        assert!(parse_redis_url("redis://").is_none());
    }

    #[test]
    #[serial]
    fn split_env_preferred_over_bare_url() {
        // The whole point of BLD-05's sccache wiring: we emit the SPLIT env, not
        // a single SCCACHE_REDIS var (which fell back to local disk in testing).
        // #[serial] + the guard below isolate SCCACHE_BIN, which
        // `SccacheEnv::binary()` reads ambiently — see `ScopedNoSccacheBin`.
        let _no_bin = ScopedNoSccacheBin::new();
        let env = from_secret(Some("redis://default:<email>:6379/1"), DATASET); // pii-test-fixture (synthetic placeholder credential + RFC 2606 .invalid host, not an email)
        assert_eq!(env.mode, SccacheMode::Redis);
        assert_eq!(
            env.vars.get("SCCACHE_REDIS_ENDPOINT").map(String::as_str),
            Some("redis://cache.invalid:6379")
        );
        assert_eq!(
            env.vars.get("SCCACHE_REDIS_PASSWORD").map(String::as_str),
            Some("placeholder-password")
        );
        assert_eq!(
            env.vars.get("SCCACHE_REDIS_DB").map(String::as_str),
            Some("1")
        );
        assert_eq!(
            env.vars.get("SCCACHE_REDIS_KEY_PREFIX").map(String::as_str),
            Some("sccache")
        );
        assert_eq!(
            env.vars.get("RUSTC_WRAPPER").map(String::as_str),
            Some("sccache")
        );
        // The bare single-var form must NOT be exported (it's the unreliable one).
        assert!(!env.vars.contains_key("SCCACHE_REDIS"));
    }

    #[test]
    #[serial]
    fn fails_open_to_local_dir_when_unconfigured() {
        // #[serial] + the guard below isolate SCCACHE_BIN, which
        // `SccacheEnv::binary()` reads ambiently — see `ScopedNoSccacheBin`.
        let _no_bin = ScopedNoSccacheBin::new();
        let env = from_secret(None, DATASET);
        assert_eq!(env.mode, SccacheMode::LocalDir);
        assert_eq!(
            env.vars.get("SCCACHE_DIR").map(String::as_str),
            Some("/data/build/cache/sccache")
        );
        // Still wraps rustc — the build proceeds, just with a local cache.
        assert_eq!(
            env.vars.get("RUSTC_WRAPPER").map(String::as_str),
            Some("sccache")
        );
        // No Redis vars leaked into the fail-open env.
        assert!(!env.vars.contains_key("SCCACHE_REDIS_ENDPOINT"));
        assert!(!env.vars.contains_key("SCCACHE_REDIS_PASSWORD"));
    }

    #[test]
    fn fails_open_when_secret_is_garbage() {
        // A present-but-unparseable secret must still degrade to local dir,
        // never error the build.
        let env = from_secret(Some("totally-not-a-redis-url"), DATASET);
        assert_eq!(env.mode, SccacheMode::LocalDir);
        assert!(env.vars.contains_key("SCCACHE_DIR"));
    }

    #[test]
    fn unreachable_endpoint_falls_open_to_local_dir() {
        // A syntactically valid but DEAD endpoint (probe returns false) must fall
        // open to the local dir — never select Redis mode.
        let env = from_secret_with_probe(
            Some("redis://default:<email>:6379/1"), // pii-test-fixture (synthetic placeholder credential + RFC 2606 .invalid host, not an email)
            DATASET,
            |_| false, // injected: endpoint unusable
        );
        assert_eq!(env.mode, SccacheMode::LocalDir);
        assert_eq!(
            env.vars.get("SCCACHE_DIR").map(String::as_str),
            Some("/data/build/cache/sccache")
        );
        // No Redis vars leaked (notably no password) when we fell open.
        assert!(!env.vars.contains_key("SCCACHE_REDIS_ENDPOINT"));
        assert!(!env.vars.contains_key("SCCACHE_REDIS_PASSWORD"));
    }

    #[test]
    fn reachable_endpoint_selects_redis_and_probes_right_hostport() {
        // The probe is called with the endpoint's host+port; when it passes,
        // Redis mode is selected with the split env.
        let seen = std::cell::RefCell::new((String::new(), 0u16));
        let env = from_secret_with_probe(
            Some("redis://default:<email>:6390/2"), // pii-test-fixture (synthetic placeholder credential + RFC 2606 .invalid host, not an email)
            DATASET,
            |parts| {
                let (h, p) = endpoint_host_port(parts);
                *seen.borrow_mut() = (h, p);
                true
            },
        );
        assert_eq!(env.mode, SccacheMode::Redis);
        assert_eq!(seen.borrow().0, "cache.invalid");
        assert_eq!(seen.borrow().1, 6390);
        assert_eq!(
            env.vars.get("SCCACHE_REDIS_ENDPOINT").map(String::as_str),
            Some("redis://cache.invalid:6390")
        );
    }

    #[test]
    fn endpoint_host_port_parses_default_and_ipv6() {
        let p = parse_redis_url("redis://cache.invalid:6379").unwrap();
        assert_eq!(endpoint_host_port(&p), ("cache.invalid".to_string(), 6379));
        // No explicit port ⇒ default 6379.
        let p2 = parse_redis_url("redis://no-port.invalid").unwrap();
        assert_eq!(endpoint_host_port(&p2), ("no-port.invalid".to_string(), 6379));
        // IPv6 literal with port — brackets stripped.
        let p3 = parse_redis_url("redis://[::1]:6380").unwrap();
        assert_eq!(endpoint_host_port(&p3), ("::1".to_string(), 6380));
    }

    #[test]
    fn malformed_port_makes_url_unparseable() {
        // A PRESENT-but-invalid port fails the whole parse (→ caller fails open),
        // never silently defaults to 6379.
        assert!(parse_redis_url("redis://cache.invalid:notaport/1").is_none());
        assert!(parse_redis_url("redis://cache.invalid:0/1").is_none()); // zero out of range
        assert!(parse_redis_url("redis://cache.invalid:99999/1").is_none()); // > 65535
        assert!(parse_redis_url("redis://cache.invalid:/1").is_none()); // empty after ':'
        assert!(parse_redis_url("redis://[::1]:notaport").is_none()); // ipv6 bad port
                                                                      // Absent port parses (defaulted to 6379 downstream); a valid port is kept.
        let absent = parse_redis_url("redis://cache.invalid/1").unwrap();
        assert_eq!(absent.port, None);
        assert_eq!(endpoint_host_port(&absent), ("cache.invalid".to_string(), 6379));
        let valid = parse_redis_url("redis://cache.invalid:6380/1").unwrap();
        assert_eq!(valid.port, Some(6380));
        assert_eq!(endpoint_host_port(&valid), ("cache.invalid".to_string(), 6380));
    }

    #[test]
    fn malformed_port_url_fails_open_to_local_dir() {
        // End-to-end: a valid-scheme URL with a bad port must degrade to the local
        // SCCACHE_DIR exactly like a missing/garbage URL — with a probe that would
        // otherwise say "reachable" (proving the port, not reachability, decided it).
        let env = from_secret_with_probe(
            Some("redis://default:<email>:notaport/1"), // pii-test-fixture (synthetic placeholder credential + RFC 2606 .invalid host, not an email)
            DATASET,
            |_| true, // even if host:6379 were reachable…
        );
        assert_eq!(env.mode, SccacheMode::LocalDir);
        assert!(env.vars.contains_key("SCCACHE_DIR"));
        assert!(!env.vars.contains_key("SCCACHE_REDIS_ENDPOINT"));
        assert!(!env.vars.contains_key("SCCACHE_REDIS_PASSWORD"));
    }

    #[test]
    fn describe_never_contains_password() {
        let env = from_secret(Some("redis://default:<email>:6379/1"), DATASET); // pii-test-fixture (synthetic placeholder credential + RFC 2606 .invalid host, not an email)
        assert!(!env.describe().contains("leak-canary-placeholder-password"));
    }

    // ── TERM #564: the probe must judge USABILITY, not mere reachability ─────

    #[test]
    fn resp_command_encodes_wire_form() {
        assert_eq!(resp_command(&["PING"]), b"*1\r\n$4\r\nPING\r\n".to_vec());
        assert_eq!(
            resp_command(&["AUTH", "default", "placeholder-password"]),
            b"*3\r\n$4\r\nAUTH\r\n$7\r\ndefault\r\n$20\r\nplaceholder-password\r\n".to_vec()
        );
    }

    /// Review finding 3: the reply must BE the expected RESP status lines, not
    /// merely contain `+PONG` somewhere.
    #[test]
    fn probe_reply_requires_wellformed_status_lines() {
        use ProbeVerdict::*;

        // The happy paths.
        assert_eq!(evaluate_probe_reply(b"+OK\r\n+PONG\r\n", true), Usable);
        assert_eq!(evaluate_probe_reply(b"+PONG\r\n", false), Usable);

        // Auth rejected — the regression this change exists for.
        assert_eq!(
            evaluate_probe_reply(b"-NOAUTH Authentication required.\r\n", true),
            Unusable
        );
        assert_eq!(
            evaluate_probe_reply(b"-WRONGPASS invalid username-password pair\r\n", true),
            Unusable
        );
        assert_eq!(
            evaluate_probe_reply(b"-ERR Client sent AUTH, but no password is set\r\n", true),
            Unusable
        );
        // A password-less probe against a password-protected server.
        assert_eq!(
            evaluate_probe_reply(b"-NOAUTH Authentication required.\r\n", false),
            Unusable
        );

        // `+OK\r\n` ALONE is not accepted (kept from the previous assertions):
        // AUTH succeeded but PING has not answered yet.
        assert_eq!(evaluate_probe_reply(b"+OK\r\n", true), NeedMore);
        assert_eq!(evaluate_probe_reply(b"+OK\r\n", false), Unusable);

        // A truncated `+P` is not accepted (kept from the previous assertions).
        assert_eq!(evaluate_probe_reply(b"+P", false), NeedMore);
        assert_eq!(evaluate_probe_reply(b"+OK\r\n+P", true), NeedMore);
        // …and never becomes a pass just because more bytes arrive that are not
        // an exact `+PONG` line.
        assert_eq!(evaluate_probe_reply(b"+OK\r\n+PONGZILLA\r\n", true), Unusable);

        // THE substring bug, precisely: `+PONG` present in the buffer but NOT as
        // the reply status line. The old `find(&seen, b"+PONG")` said "usable".
        assert_eq!(
            evaluate_probe_reply(b"-ERR unknown command '+PONG'\r\n", true),
            Unusable
        );
        assert_eq!(
            evaluate_probe_reply(b"$5\r\n+PONG\r\n", false),
            Unusable,
            "a bulk-string payload containing the bytes is not a +PONG status line"
        );

        // Leading whitespace / a non-status reply type is not affirmative.
        assert_eq!(evaluate_probe_reply(b" +PONG\r\n", false), Unusable);
        assert_eq!(evaluate_probe_reply(b":1\r\n", false), Unusable);
        // Nothing at all yet.
        assert_eq!(evaluate_probe_reply(b"", false), NeedMore);
    }

    /// TERM #565 (round-2 review finding 3): a COMPLETE, satisfied response
    /// followed by ANY unexpected trailing bytes is NOT "usable". The stated
    /// invariant is that a malformed result is ALWAYS unhealthy; accepting a
    /// prefix and ignoring the rest of the buffer contradicts it.
    ///
    /// Mutation-check for a future reader: delete the trailing-data rejection in
    /// `evaluate_probe_reply` and every assertion in this test that expects
    /// `Unusable` flips to `Usable`.
    #[test]
    fn probe_reply_rejects_trailing_data_after_a_complete_reply() {
        use ProbeVerdict::*;

        // The exact byte sequence called out by review.
        assert_eq!(
            evaluate_probe_reply(b"+PONG\r\nGARBAGE\r\n", false),
            Unusable,
            "a +PONG followed by unsolicited trailing data is not a clean reply"
        );
        assert_eq!(
            evaluate_probe_reply(b"+OK\r\n+PONG\r\nGARBAGE\r\n", true),
            Unusable
        );
        // Trailing data need not be a complete line to be unexpected.
        assert_eq!(evaluate_probe_reply(b"+PONG\r\nGAR", false), Unusable);
        // Even a SECOND well-formed status line is unexpected — we sent exactly
        // one PING (and at most one AUTH), so a second +PONG means we are not
        // talking to what we think we are talking to.
        assert_eq!(evaluate_probe_reply(b"+PONG\r\n+PONG\r\n", false), Unusable);
        // An AUTH reply we never asked for, ahead of the PONG, is likewise not
        // the expected shape.
        assert_eq!(evaluate_probe_reply(b"+OK\r\n+PONG\r\n", false), Unusable);

        // …and the tri-state is INTACT: a reply legitimately split across TCP
        // reads is still NeedMore, never a false negative. These are the exact
        // prefixes of the two happy-path replies.
        assert_eq!(evaluate_probe_reply(b"+PON", false), NeedMore);
        assert_eq!(evaluate_probe_reply(b"+PONG\r", false), NeedMore);
        assert_eq!(evaluate_probe_reply(b"+OK\r\n", true), NeedMore);
        assert_eq!(evaluate_probe_reply(b"+OK\r\n+PONG\r", true), NeedMore);
        // …and each completes to Usable once the final CRLF arrives.
        assert_eq!(evaluate_probe_reply(b"+PONG\r\n", false), Usable);
        assert_eq!(evaluate_probe_reply(b"+OK\r\n+PONG\r\n", true), Usable);
    }

    #[test]
    fn find_subslice_locates_or_reports_absent() {
        assert_eq!(find_subslice(b"+OK\r\n+PONG\r\n", b"\r\n"), Some(3));
        assert_eq!(find_subslice(b"+OK", b"\r\n"), None);
        // A needle longer than the haystack can never match.
        assert_eq!(find_subslice(b"+P", b"+PONG"), None);
    }

    // ── The REAL probe, against a REAL local listener (review finding 2) ──────
    //
    // These exercise `redis_usable` itself rather than an injected closure, so
    // they FAIL if the probe body ever regresses to a bare TCP connect (a bare
    // connect would return `true` for every rejecting/silent server below).
    // Hermetic: a `TcpListener` on 127.0.0.1:0, no real Redis, no network.

    /// Spawn a one-shot fake server on an ephemeral loopback port. `reply`
    /// `Some(bytes)` ⇒ drain the request and answer with exactly those bytes;
    /// `None` ⇒ accept and say NOTHING (the silent/hung-server case), holding the
    /// socket open so the probe must rely on its own bounded read timeout.
    /// Returns the bound address. The thread is detached and dies with the test
    /// binary.
    fn spawn_fake_redis(reply: Option<&'static [u8]>) -> std::net::SocketAddr {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                // Drain the probe's AUTH/PING so our write never races a reset.
                let _ = sock.set_read_timeout(Some(std::time::Duration::from_millis(500)));
                let mut scratch = [0u8; 256];
                let _ = sock.read(&mut scratch);
                match reply {
                    Some(bytes) => {
                        let _ = sock.write_all(bytes);
                        let _ = sock.flush();
                    }
                    // Hold the connection open, silent, well past the probe's
                    // timeout — the probe must time out, not hang.
                    None => std::thread::sleep(std::time::Duration::from_secs(2)),
                }
            }
        });
        addr
    }

    /// Like [`spawn_fake_redis`], but writes the reply in TWO segments with a
    /// deliberate gap, then holds the socket open. This is what makes the
    /// round-3 finding testable: `first` is flushed on its own (so it really is
    /// its own TCP segment), the probe's parser is satisfied by it, and only
    /// `second` — sent `gap` later — reveals whether the probe checked for
    /// trailing bytes or returned the moment it was satisfied.
    ///
    /// `second: None` ⇒ nothing more is ever sent, but the connection is HELD
    /// OPEN for `hold` (no EOF): the case that breaks if a post-success drain
    /// waits too long or reads a timeout as failure.
    fn spawn_fake_redis_segmented(
        first: &'static [u8],
        gap: std::time::Duration,
        second: Option<&'static [u8]>,
        hold: std::time::Duration,
    ) -> std::net::SocketAddr {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let _ = sock.set_read_timeout(Some(std::time::Duration::from_millis(500)));
                let mut scratch = [0u8; 256];
                let _ = sock.read(&mut scratch);
                let _ = sock.write_all(first);
                let _ = sock.flush();
                std::thread::sleep(gap);
                if let Some(bytes) = second {
                    let _ = sock.write_all(bytes);
                    let _ = sock.flush();
                }
                // Hold the socket open so the probe sees silence, not EOF.
                std::thread::sleep(hold);
            }
        });
        addr
    }

    fn probe_against(addr: std::net::SocketAddr, url_tmpl: &str) -> bool {
        let url = url_tmpl.replace("{port}", &addr.port().to_string());
        let parts = parse_redis_url(&url).expect("test URL parses");
        redis_usable(&parts, std::time::Duration::from_millis(400))
    }

    /// A server that is LISTENING and ACCEPTS, but rejects our credentials with a
    /// genuine `-NOAUTH` — the exact production failure. Must be UNUSABLE.
    /// A bare-TCP-connect probe would call this healthy and kill every build.
    #[test]
    fn real_probe_rejects_a_noauth_listener() {
        let addr = spawn_fake_redis(Some(b"-NOAUTH Authentication required.\r\n"));
        assert!(
            !probe_against(addr, "redis://default:placeholder-password@127.0.0.1:{port}/1"),
            "a NOAUTH server must be judged unusable (fail open)"
        );
    }

    /// Same, with a genuine `-WRONGPASS` reply.
    #[test]
    fn real_probe_rejects_a_wrongpass_listener() {
        let addr = spawn_fake_redis(Some(b"-WRONGPASS invalid username-password pair\r\n"));
        assert!(
            !probe_against(addr, "redis://default:placeholder-password@127.0.0.1:{port}/1"),
            "a WRONGPASS server must be judged unusable (fail open)"
        );
    }

    /// A server that accepts the connection and then says NOTHING. The bounded
    /// read must time out and report unusable — never hang the build.
    #[test]
    fn real_probe_times_out_on_a_silent_listener() {
        let addr = spawn_fake_redis(None);
        let started = std::time::Instant::now();
        assert!(
            !probe_against(addr, "redis://default:placeholder-password@127.0.0.1:{port}/1"),
            "a silent server must be judged unusable (fail open)"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "the probe must be bounded, not hang: took {:?}",
            started.elapsed()
        );
    }

    /// The positive control — without it, a probe hardwired to `false` would pass
    /// every test above. A server that authenticates and answers `+PONG` IS usable.
    ///
    /// This is also the CLEAN-CLOSE case for the round-3 post-success drain:
    /// `spawn_fake_redis` drops the socket right after writing, so the drain sees
    /// EOF with no extra bytes. A clean close is not garbage and must stay healthy.
    #[test]
    fn real_probe_accepts_a_healthy_listener() {
        let addr = spawn_fake_redis(Some(b"+OK\r\n+PONG\r\n"));
        assert!(
            probe_against(addr, "redis://default:placeholder-password@127.0.0.1:{port}/1"),
            "an authenticating server answering +PONG must be usable"
        );
        // …and with no credentials in the URL, a bare `+PONG` is enough.
        let addr2 = spawn_fake_redis(Some(b"+PONG\r\n"));
        assert!(probe_against(addr2, "redis://127.0.0.1:{port}"));
    }

    /// Finding 3 end-to-end through the REAL probe: `+PONG` present as a
    /// SUBSTRING but not as the reply status line must NOT be accepted.
    #[test]
    fn real_probe_rejects_pong_as_a_mere_substring() {
        let addr = spawn_fake_redis(Some(b"-ERR unknown command '+PONG'\r\n"));
        assert!(
            !probe_against(addr, "redis://default:placeholder-password@127.0.0.1:{port}/1"),
            "`+PONG` inside an error line is not an affirmative reply"
        );
    }

    /// Round-2 finding 3 end-to-end through the REAL probe: a `+PONG` followed by
    /// unsolicited trailing bytes is not a clean reply. Paired with
    /// `real_probe_accepts_a_healthy_listener` (same server, minus the garbage),
    /// so this cannot pass by the probe simply being broken.
    ///
    /// Round-3 note: this test writes the whole reply in ONE `write_all`, so
    /// WHICH layer rejects it (the parser's same-buffer check, or the bounded
    /// post-success drain) depends on TCP segmentation — it is not deterministic.
    /// `real_probe_rejects_trailing_garbage_in_a_later_segment` makes the
    /// segmented case explicit and deliberate. This test is still worth keeping:
    /// it is the COALESCED path, which is what a real chatty server on loopback
    /// most often produces, and together the two pin both layers.
    #[test]
    fn real_probe_rejects_trailing_garbage_after_pong() {
        let addr = spawn_fake_redis(Some(b"+PONG\r\nGARBAGE\r\n"));
        assert!(
            !probe_against(addr, "redis://127.0.0.1:{port}"),
            "trailing data after the reply must be judged unusable (fail open)"
        );
    }

    // ── Round-3 finding: SEGMENTED trailing garbage ──────────────────────────
    //
    // Timing margins, chosen deliberately for a loaded shared build host:
    //   * garbage gap        = 20 ms  — 5× under the 100 ms drain window
    //   * drain window       = 100 ms (`POST_SUCCESS_DRAIN_MS`)
    //   * hold-open          = 1500 ms — 15× over the drain window
    //   * probe timeout      = 400 ms (`probe_against`)
    // Every margin is an order of magnitude, so a scheduling hiccup of tens of
    // milliseconds cannot flip any of these tests.

    /// The round-3 finding, made explicit: the fake server flushes a COMPLETE,
    /// VALID reply, waits, and only THEN sends garbage. `evaluate_probe_reply`
    /// cannot see this — the garbage is not in the buffer when the parser is
    /// satisfied — so this fails unless `redis_usable` performs its bounded
    /// post-success check. Removing that check must turn this test RED
    /// (mutation-verified).
    #[test]
    fn real_probe_rejects_trailing_garbage_in_a_later_segment() {
        let addr = spawn_fake_redis_segmented(
            b"+OK\r\n+PONG\r\n",
            std::time::Duration::from_millis(20),
            Some(b"GARBAGE\r\n"),
            std::time::Duration::from_millis(1500),
        );
        assert!(
            !probe_against(addr, "redis://default:placeholder-password@127.0.0.1:{port}/1"),
            "trailing garbage arriving in a LATER TCP segment — but still WITHIN the drain \
             window — must be judged unusable: the bounded claim is that garbage inside the \
             window is unhealthy, not merely garbage that happened to land in the first read"
        );
    }

    /// The positive control for the test above, and the case that regresses if
    /// the post-success drain waits too long or misreads its timeout as failure:
    /// a healthy server that answers correctly and then holds the connection OPEN
    /// and SILENT (no EOF) must still be USABLE, and must not cost more than the
    /// bounded drain window.
    #[test]
    fn real_probe_accepts_a_healthy_listener_that_holds_the_socket_open() {
        let addr = spawn_fake_redis_segmented(
            b"+OK\r\n+PONG\r\n",
            std::time::Duration::from_millis(20),
            None,
            std::time::Duration::from_millis(1500),
        );
        let started = std::time::Instant::now();
        assert!(
            probe_against(addr, "redis://default:placeholder-password@127.0.0.1:{port}/1"),
            "silence after a correct reply is the EXPECTED healthy outcome — a read timeout in \
             the post-success window must not be read as a failure"
        );
        // The drain must be BOUNDED and small: it is paid by every healthy probe.
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "the post-success check must be bounded and short, not the full probe timeout or \
             the server's hold: took {:?}",
            started.elapsed()
        );
    }

    // ── Round-5 finding: an INCOMPLETE post-success check is not a PASS ──────
    //
    // Two paths used to return `true` (healthy) from `no_unsolicited_trailing_bytes`
    // without having verified anything: a non-timeout `Err(_)` from the drain read,
    // and a `set_read_timeout` failure. Both are now UNHEALTHY. "Unhealthy" is a
    // FALLBACK (local cache dir, slower build), not a failure; a false "healthy"
    // is the original TERM #564 dead-gate. The cheap mistake is the correct one.

    /// Like [`spawn_fake_redis`], but after writing a COMPLETE, VALID reply and
    /// waiting `gap`, it aborts the connection with `SO_LINGER = 0` so the peer
    /// observes an **ECONNRESET**, not a clean FIN. That is the only way to drive
    /// the drain's non-timeout `Err(_)` arm through a real socket.
    ///
    /// The `gap` matters: it gives the probe time to consume the reply first, so
    /// the reset lands while the probe is in its post-success drain with an empty
    /// receive buffer (a reset arriving with unread data would instead destroy
    /// the reply, and the test would pass for the wrong reason). The mutation
    /// check below is what proves the intended arm is the one being exercised.
    #[cfg(unix)]
    fn spawn_fake_redis_then_reset(
        reply: &'static [u8],
        gap: std::time::Duration,
    ) -> std::net::SocketAddr {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let _ = sock.set_read_timeout(Some(std::time::Duration::from_millis(500)));
                let mut scratch = [0u8; 256];
                let _ = sock.read(&mut scratch);
                let _ = sock.write_all(reply);
                let _ = sock.flush();
                std::thread::sleep(gap);
                // SO_LINGER = 0 ⇒ close() emits RST instead of FIN, which is
                // what makes the peer's next read fail with ECONNRESET.
                // `TcpStream::set_linger` is still unstable on the pinned
                // toolchain (rust #88494), so set the sockopt directly; `libc`
                // is already a direct dependency of this crate.
                use std::os::unix::io::AsRawFd;
                let linger = libc::linger {
                    l_onoff: 1,
                    l_linger: 0,
                };
                // SAFETY: `sock` is a live, owned socket fd for the duration of
                // this call, and `&linger`/its size describe a correctly-typed
                // `struct linger` for SOL_SOCKET/SO_LINGER.
                let rc = unsafe {
                    libc::setsockopt(
                        sock.as_raw_fd(),
                        libc::SOL_SOCKET,
                        libc::SO_LINGER,
                        &linger as *const libc::linger as *const libc::c_void,
                        std::mem::size_of::<libc::linger>() as libc::socklen_t,
                    )
                };
                debug_assert_eq!(rc, 0, "SO_LINGER=0 must be settable for the reset test");
                drop(sock);
            }
        });
        addr
    }

    /// A server that answers `+OK\r\n+PONG\r\n` correctly and then ABRUPTLY
    /// RESETS the connection. The reply looked good, but the post-success check
    /// could not be completed — so we did not verify the absence of trailing
    /// garbage, and an unverified probe must fall open.
    ///
    /// Mutation-check for a future reader: flip the `Some(Err(_))` arm of
    /// `post_success_drain_is_healthy` back to `true` and this test goes RED
    /// (verified when it was written).
    #[test]
    #[cfg(unix)]
    fn real_probe_rejects_a_connection_reset_after_pong() {
        let addr = spawn_fake_redis_then_reset(
            b"+OK\r\n+PONG\r\n",
            std::time::Duration::from_millis(40),
        );
        assert!(
            !probe_against(addr, "redis://default:placeholder-password@127.0.0.1:{port}/1"),
            "a peer that resets the connection after +PONG left the post-success check \
             INCOMPLETE — an incomplete verification must fall open, never report healthy"
        );
    }

    /// The pure decision function, every arm — including the `set_read_timeout`
    /// failure path (`None`), which is not reachable through a live socket
    /// without unreasonable contortions and is therefore covered here directly.
    ///
    /// Mutation-checks (each verified when written): flipping `None => false` to
    /// `true`, or `Some(Err(_)) => …` to unconditional `true`, turns the
    /// correspondingly-named assertion below RED.
    #[test]
    fn post_success_drain_only_clean_eof_and_bounded_silence_are_healthy() {
        use std::io::{Error, ErrorKind};

        // ── the ONLY two healthy outcomes ────────────────────────────────────
        assert!(
            post_success_drain_is_healthy(Some(&Ok(0))),
            "clean EOF is healthy: a close is not garbage"
        );
        assert!(
            post_success_drain_is_healthy(Some(&Err(Error::from(ErrorKind::WouldBlock)))),
            "bounded silence after +PONG is the EXPECTED healthy outcome"
        );
        assert!(
            post_success_drain_is_healthy(Some(&Err(Error::from(ErrorKind::TimedOut)))),
            "bounded silence (reported as TimedOut on some platforms) is healthy"
        );

        // ── everything else is unhealthy ─────────────────────────────────────
        assert!(
            !post_success_drain_is_healthy(None),
            "a read we could not BOUND verified nothing — reporting healthy there is \
             fail-closed disguised as fail-open (set_read_timeout failure path)"
        );
        assert!(
            !post_success_drain_is_healthy(Some(&Ok(1))),
            "unsolicited trailing bytes are unhealthy"
        );
        for kind in [
            ErrorKind::ConnectionReset,
            ErrorKind::ConnectionAborted,
            ErrorKind::BrokenPipe,
            ErrorKind::NotConnected,
            ErrorKind::Other,
        ] {
            assert!(
                !post_success_drain_is_healthy(Some(&Err(Error::from(kind)))),
                "{kind:?} left the post-success check incomplete — unhealthy"
            );
        }
    }

    /// Finding 1: a `rediss://` (TLS) endpoint is unverifiable by this plaintext
    /// probe, so it must be reported UNUSABLE — even when something is listening
    /// and would happily answer `+OK`/`+PONG` in plaintext. The old code returned
    /// `true` here on the strength of a bare TCP connect.
    #[test]
    fn real_probe_treats_tls_endpoint_as_unusable() {
        let addr = spawn_fake_redis(Some(b"+OK\r\n+PONG\r\n"));
        assert!(
            !probe_against(addr, "rediss://default:placeholder-password@127.0.0.1:{port}/1"),
            "rediss:// is unverifiable here and must fall open, never be assumed healthy"
        );
    }

    /// …and that unusable verdict must actually degrade the wiring to the local
    /// cache dir, with no Redis env (notably no password) handed to the build.
    #[test]
    fn tls_endpoint_falls_open_to_local_dir_end_to_end() {
        let addr = spawn_fake_redis(Some(b"+OK\r\n+PONG\r\n"));
        let url = format!("rediss://default:placeholder-password@127.0.0.1:{}/1", addr.port());
        let timeout = std::time::Duration::from_millis(400);
        let env = from_secret_with_probe(Some(&url), DATASET, |p| redis_usable(p, timeout));
        assert_eq!(env.mode, SccacheMode::LocalDir);
        assert_eq!(
            env.vars.get("SCCACHE_DIR").map(String::as_str),
            Some("/data/build/cache/sccache")
        );
        assert!(!env.vars.contains_key("SCCACHE_REDIS_ENDPOINT"));
        assert!(!env.vars.contains_key("SCCACHE_REDIS_PASSWORD"));
    }

    /// End-to-end through the REAL probe: a live-but-rejecting server must
    /// produce the local-dir fail-open wiring. This is the injected
    /// `reachable_but_unauthenticated_endpoint_falls_open` test's invariant,
    /// exercised for real.
    #[test]
    fn real_noauth_endpoint_falls_open_to_local_dir_end_to_end() {
        let addr = spawn_fake_redis(Some(b"-NOAUTH Authentication required.\r\n"));
        let url = format!("redis://default:placeholder-password@127.0.0.1:{}/1", addr.port());
        let timeout = std::time::Duration::from_millis(400);
        let env = from_secret_with_probe(Some(&url), DATASET, |p| redis_usable(p, timeout));
        assert_eq!(env.mode, SccacheMode::LocalDir);
        assert!(!env.vars.contains_key("SCCACHE_REDIS_ENDPOINT"));
        assert!(!env.vars.contains_key("SCCACHE_REDIS_PASSWORD"));
    }

    /// The regression this whole change exists for: an endpoint that is
    /// LISTENING but rejects our credentials must fall OPEN to the local dir.
    /// Before TERM #564 the probe only TCP-connected, so this selected Redis
    /// mode and every subsequent `cargo` invocation died ~1s in with
    /// `sccache: error: Server startup failed … NOAUTH: Authentication
    /// required.` — zero tests compiled, which the mode=test gate then reported
    /// as a bare `0 passed, 0 failed`.
    ///
    /// NOTE (review finding 2): this test INJECTS the probe result, so it covers
    /// the WIRING (an unusable verdict ⇒ local-dir fail-open, no leaked Redis
    /// env) but could not catch the probe itself regressing to a bare TCP
    /// connect. `real_probe_rejects_a_noauth_listener` /
    /// `real_noauth_endpoint_falls_open_to_local_dir_end_to_end` cover that
    /// invariant against a real loopback listener. Keep both.
    #[test]
    fn reachable_but_unauthenticated_endpoint_falls_open() {
        let env = from_secret_with_probe(
            Some("redis://default:<email>:6380/1"), // pii-test-fixture (synthetic placeholder credential + RFC 2606 .invalid host, not an email)
            DATASET,
            // Injected: TCP is fine, AUTH/PING is not — exactly what
            // `redis_usable` returns for a NOAUTH/WRONGPASS reply.
            |_| false,
        );
        assert_eq!(env.mode, SccacheMode::LocalDir);
        assert_eq!(
            env.vars.get("SCCACHE_DIR").map(String::as_str),
            Some("/data/build/cache/sccache")
        );
        // Crucially: no half-configured Redis env is handed to the build.
        assert!(!env.vars.contains_key("SCCACHE_REDIS_ENDPOINT"));
        assert!(!env.vars.contains_key("SCCACHE_REDIS_PASSWORD"));
        // …and sccache is still wired as the wrapper (the cache degrades, the
        // build does not break).
        assert!(env.vars.contains_key("RUSTC_WRAPPER"));
    }

    /// The probe receives the credentials, not just host+port — otherwise it
    /// could not have authenticated at all (this is the signature change).
    #[test]
    fn probe_receives_full_parts_including_credentials() {
        let seen: std::cell::RefCell<Option<RedisUrlParts>> = std::cell::RefCell::new(None);
        let env = from_secret_with_probe(
            Some("redis://alice:<email>:6390/2"), // pii-test-fixture (synthetic placeholder credential + RFC 2606 .invalid host, not an email)
            DATASET,
            |parts| {
                *seen.borrow_mut() = Some(parts.clone());
                true
            },
        );
        assert_eq!(env.mode, SccacheMode::Redis);
        let got = seen.borrow().clone().expect("probe was called");
        assert_eq!(got.username.as_deref(), Some("alice"));
        assert_eq!(got.password.as_deref(), Some("placeholder-password"));
        assert_eq!(got.db.as_deref(), Some("2"));
        assert_eq!(endpoint_host_port(&got), ("cache.invalid".to_string(), 6390));
    }

    /// A DEAD endpoint (no TCP at all) still falls open — `redis_usable` is a
    /// strict superset of the old reachability check, so the pre-existing
    /// behavior is preserved. Uses a port nothing listens on, exercising the
    /// REAL production probe rather than an injected one.
    #[test]
    fn real_probe_on_dead_endpoint_is_unusable() {
        // 127.0.0.1:1 — reserved, never listening.
        let parts = parse_redis_url("redis://default:placeholder-password@127.0.0.1:1").unwrap();
        assert!(!redis_usable(&parts, std::time::Duration::from_millis(200)));
    }
}
