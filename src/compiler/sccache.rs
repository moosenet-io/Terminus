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
    pub username: Option<String>,
    pub password: Option<String>,
    /// Logical DB index as a string (sccache wants it as text), if present.
    pub db: Option<String>,
}

/// Parse a `redis://` / `rediss://` URL into its endpoint + auth + db parts.
/// `None` when the scheme is not a redis scheme or the host is empty. Deliberately
/// dependency-free (no `url` crate) so parsing is trivially unit-testable and the
/// password never transits a logging-prone type.
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
        username,
        password,
        db,
    })
}

/// Default reachability-probe timeout (ms) for the resolved Redis endpoint,
/// overridable via `SCCACHE_REDIS_PROBE_MS`. Kept sub-second so resolving the
/// sccache backend never stalls a build; a dead endpoint fails open fast.
const DEFAULT_PROBE_MS: u64 = 300;

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
        |host, port| tcp_reachable(host, port, timeout),
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

/// Fast bounded TCP-connect reachability check. `true` iff a connection to any
/// resolved address of `host:port` succeeds within `timeout`. Non-fatal —
/// callers fall open when it is `false`.
fn tcp_reachable(host: &str, port: u16, timeout: std::time::Duration) -> bool {
    use std::net::ToSocketAddrs;
    match (host, port).to_socket_addrs() {
        Ok(addrs) => addrs
            .into_iter()
            .any(|addr| std::net::TcpStream::connect_timeout(&addr, timeout).is_ok()),
        Err(_) => false,
    }
}

/// Split a `RedisUrlParts.endpoint` (`redis://host[:port]`) into `(host, port)`,
/// defaulting the port to 6379 and stripping IPv6 brackets for the probe.
pub fn endpoint_host_port(parts: &RedisUrlParts) -> (String, u16) {
    let hostport = parts
        .endpoint
        .strip_prefix("redis://")
        .or_else(|| parts.endpoint.strip_prefix("rediss://"))
        .unwrap_or(&parts.endpoint);
    // IPv6 literal: `[::1]:6379` or `[::1]`.
    if let Some(rest) = hostport.strip_prefix('[') {
        if let Some((h, tail)) = rest.split_once(']') {
            let port = tail
                .strip_prefix(':')
                .and_then(|p| p.parse().ok())
                .unwrap_or(6379);
            return (h.to_string(), port);
        }
    }
    match hostport.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().unwrap_or(6379)),
        None => (hostport.to_string(), 6379),
    }
}

/// Pure builder (a test entry point): OPTIONAL secret URL + dataset root, with
/// no reachability check (assumes the endpoint is reachable). `None`/unparseable
/// ⇒ fail-open local dir. Retained for the split-env / fail-open-on-missing
/// tests; production goes through [`resolve`] (which probes).
pub fn from_secret(secret_url: Option<&str>, dataset_root: &str) -> SccacheEnv {
    from_secret_with_probe(secret_url, dataset_root, |_, _| true)
}

/// The full builder (the injectable test entry point): selects Redis mode ONLY
/// when the URL parses AND `probe(host, port)` returns `true`; otherwise fails
/// OPEN to the local disk cache. Injecting `probe` makes the unreachable-endpoint
/// decision offline-testable.
pub fn from_secret_with_probe(
    secret_url: Option<&str>,
    dataset_root: &str,
    probe: impl Fn(&str, u16) -> bool,
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

    // Reachability gate: a syntactically valid but dead endpoint falls open.
    let (host, port) = endpoint_host_port(&parts);
    if !probe(&host, port) {
        warn!(
            "sccache: Redis endpoint {}:{} unreachable — falling open to local cache dir",
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

    const DATASET: &str = "/data/build";

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
    fn split_env_preferred_over_bare_url() {
        // The whole point of BLD-05's sccache wiring: we emit the SPLIT env, not
        // a single SCCACHE_REDIS var (which fell back to local disk in testing).
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
    fn fails_open_to_local_dir_when_unconfigured() {
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
            |_, _| false, // injected: endpoint unreachable
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
            |h, p| {
                *seen.borrow_mut() = (h.to_string(), p);
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
    fn describe_never_contains_password() {
        let env = from_secret(Some("redis://default:sup3rsecret@h:6379/1"), DATASET);
        assert!(!env.describe().contains("sup3rsecret"));
    }
}
