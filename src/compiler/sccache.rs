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
/// Any error, timeout, or non-`+PONG` reply ⇒ `false` ⇒ fail open. A `rediss://`
/// (TLS) endpoint is NOT spoken here: we only do the plain-TCP reachability half
/// for it and accept it, since attempting a plaintext RESP handshake against a
/// TLS listener would produce a false negative and needlessly disable the cache.
fn redis_usable(parts: &RedisUrlParts, timeout: std::time::Duration) -> bool {
    use std::io::{Read, Write};

    let (host, port) = endpoint_host_port(parts);
    let Some(mut stream) = tcp_connect(&host, port, timeout) else {
        return false;
    };
    // TLS endpoint: we can't speak plaintext RESP to it. Reachability is all we
    // can honestly assert, so keep the pre-existing behavior for `rediss://`.
    if parts.endpoint.starts_with("rediss://") {
        return true;
    }
    if stream.set_read_timeout(Some(timeout)).is_err()
        || stream.set_write_timeout(Some(timeout)).is_err()
    {
        return false;
    }

    let mut req = Vec::new();
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
    // (`+OK\r\n+PONG\r\n`), so a small buffer and a single bounded read attempt
    // loop is enough; we stop as soon as we can decide.
    // A successful exchange is short (`+OK\r\n+PONG\r\n`); an auth failure is a
    // single `-NOAUTH …` / `-WRONGPASS …` error line. Read until we can decide,
    // hit EOF/error, or fill the small buffer — every read bounded by `timeout`.
    let mut seen: Vec<u8> = Vec::new();
    let mut buf = [0u8; 128];
    while seen.len() < 512 {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                seen.extend_from_slice(&buf[..n]);
                // `+PONG` ⇒ usable. A `-` error reply ⇒ unusable. Either way we
                // have our answer and must not block for more bytes.
                if find(&seen, b"+PONG") || find(&seen, b"-") {
                    break;
                }
            }
        }
    }
    if find(&seen, b"+PONG") {
        return true;
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

/// Whether `needle` occurs anywhere in `hay`. Tiny helper so the RESP reply
/// decision above reads plainly (and is unit-testable).
fn find(hay: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || needle.len() > hay.len() {
        return needle.is_empty();
    }
    hay.windows(needle.len()).any(|w| w == needle)
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
        let p = parse_redis_url("redis://default:s3cr3t@cache-host:6379/1").unwrap();
        assert_eq!(p.endpoint, "redis://cache-host:6379");
        assert_eq!(p.username.as_deref(), Some("default"));
        assert_eq!(p.password.as_deref(), Some("s3cr3t"));
        assert_eq!(p.db.as_deref(), Some("1"));
    }

    #[test]
    fn parses_url_without_auth_or_db() {
        let p = parse_redis_url("redis://cache-host:6379").unwrap();
        assert_eq!(p.endpoint, "redis://cache-host:6379");
        assert_eq!(p.username, None);
        assert_eq!(p.password, None);
        assert_eq!(p.db, None);
    }

    #[test]
    fn parses_password_only_userinfo() {
        // `redis://:pass@host/2` — no username, password present.
        let p = parse_redis_url("redis://:onlypass@h:6379/2").unwrap();
        assert_eq!(p.username, None);
        assert_eq!(p.password.as_deref(), Some("onlypass"));
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
        let env = from_secret(Some("redis://default:pw@h:6379/1"), DATASET);
        assert_eq!(env.mode, SccacheMode::Redis);
        assert_eq!(
            env.vars.get("SCCACHE_REDIS_ENDPOINT").map(String::as_str),
            Some("redis://h:6379")
        );
        assert_eq!(
            env.vars.get("SCCACHE_REDIS_PASSWORD").map(String::as_str),
            Some("pw")
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
            Some("redis://default:pw@dead-host:6379/1"),
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
            Some("redis://default:pw@cache-host:6390/2"),
            DATASET,
            |parts| {
                let (h, p) = endpoint_host_port(parts);
                *seen.borrow_mut() = (h, p);
                true
            },
        );
        assert_eq!(env.mode, SccacheMode::Redis);
        assert_eq!(seen.borrow().0, "cache-host");
        assert_eq!(seen.borrow().1, 6390);
        assert_eq!(
            env.vars.get("SCCACHE_REDIS_ENDPOINT").map(String::as_str),
            Some("redis://cache-host:6390")
        );
    }

    #[test]
    fn endpoint_host_port_parses_default_and_ipv6() {
        let p = parse_redis_url("redis://h:6379").unwrap();
        assert_eq!(endpoint_host_port(&p), ("h".to_string(), 6379));
        // No explicit port ⇒ default 6379.
        let p2 = parse_redis_url("redis://onlyhost").unwrap();
        assert_eq!(endpoint_host_port(&p2), ("onlyhost".to_string(), 6379));
        // IPv6 literal with port — brackets stripped.
        let p3 = parse_redis_url("redis://[::1]:6380").unwrap();
        assert_eq!(endpoint_host_port(&p3), ("::1".to_string(), 6380));
    }

    #[test]
    fn malformed_port_makes_url_unparseable() {
        // A PRESENT-but-invalid port fails the whole parse (→ caller fails open),
        // never silently defaults to 6379.
        assert!(parse_redis_url("redis://host:notaport/1").is_none());
        assert!(parse_redis_url("redis://host:0/1").is_none()); // zero out of range
        assert!(parse_redis_url("redis://host:99999/1").is_none()); // > 65535
        assert!(parse_redis_url("redis://host:/1").is_none()); // empty after ':'
        assert!(parse_redis_url("redis://[::1]:notaport").is_none()); // ipv6 bad port
                                                                      // Absent port parses (defaulted to 6379 downstream); a valid port is kept.
        let absent = parse_redis_url("redis://host/1").unwrap();
        assert_eq!(absent.port, None);
        assert_eq!(endpoint_host_port(&absent), ("host".to_string(), 6379));
        let valid = parse_redis_url("redis://host:6380/1").unwrap();
        assert_eq!(valid.port, Some(6380));
        assert_eq!(endpoint_host_port(&valid), ("host".to_string(), 6380));
    }

    #[test]
    fn malformed_port_url_fails_open_to_local_dir() {
        // End-to-end: a valid-scheme URL with a bad port must degrade to the local
        // SCCACHE_DIR exactly like a missing/garbage URL — with a probe that would
        // otherwise say "reachable" (proving the port, not reachability, decided it).
        let env = from_secret_with_probe(
            Some("redis://default:pw@host:notaport/1"),
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
        let env = from_secret(Some("redis://default:sup3rsecret@h:6379/1"), DATASET);
        assert!(!env.describe().contains("sup3rsecret"));
    }

    // ── TERM #564: the probe must judge USABILITY, not mere reachability ─────

    #[test]
    fn resp_command_encodes_wire_form() {
        assert_eq!(resp_command(&["PING"]), b"*1\r\n$4\r\nPING\r\n".to_vec());
        assert_eq!(
            resp_command(&["AUTH", "default", "pw"]),
            b"*3\r\n$4\r\nAUTH\r\n$7\r\ndefault\r\n$2\r\npw\r\n".to_vec()
        );
    }

    #[test]
    fn find_locates_resp_markers() {
        assert!(find(b"+OK\r\n+PONG\r\n", b"+PONG"));
        assert!(find(b"-NOAUTH Authentication required.\r\n", b"-"));
        assert!(!find(b"+OK\r\n", b"+PONG"));
        // A needle longer than the haystack can never match.
        assert!(!find(b"+P", b"+PONG"));
    }

    /// The regression this whole change exists for: an endpoint that is
    /// LISTENING but rejects our credentials must fall OPEN to the local dir.
    /// Before TERM #564 the probe only TCP-connected, so this selected Redis
    /// mode and every subsequent `cargo` invocation died ~1s in with
    /// `sccache: error: Server startup failed … NOAUTH: Authentication
    /// required.` — zero tests compiled, which the mode=test gate then reported
    /// as a bare `0 passed, 0 failed`.
    #[test]
    fn reachable_but_unauthenticated_endpoint_falls_open() {
        let env = from_secret_with_probe(
            Some("redis://default:wrongpw@cache-host:6380/1"),
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
            Some("redis://alice:s3cret@cache-host:6390/2"),
            DATASET,
            |parts| {
                *seen.borrow_mut() = Some(parts.clone());
                true
            },
        );
        assert_eq!(env.mode, SccacheMode::Redis);
        let got = seen.borrow().clone().expect("probe was called");
        assert_eq!(got.username.as_deref(), Some("alice"));
        assert_eq!(got.password.as_deref(), Some("s3cret"));
        assert_eq!(got.db.as_deref(), Some("2"));
        assert_eq!(endpoint_host_port(&got), ("cache-host".to_string(), 6390));
    }

    /// A DEAD endpoint (no TCP at all) still falls open — `redis_usable` is a
    /// strict superset of the old reachability check, so the pre-existing
    /// behavior is preserved. Uses a port nothing listens on, exercising the
    /// REAL production probe rather than an injected one.
    #[test]
    fn real_probe_on_dead_endpoint_is_unusable() {
        // 127.0.0.1:1 — reserved, never listening.
        let parts = parse_redis_url("redis://default:pw@127.0.0.1:1").unwrap();
        assert!(!redis_usable(&parts, std::time::Duration::from_millis(200)));
    }
}
