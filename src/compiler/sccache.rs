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
                format!("sccache→local-dir {dir} (fail-open: Redis not configured)")
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

/// Build the sccache env for a build, reading the `SCCACHE_REDIS` secret from the
/// process environment (materialized from the vault). Fails OPEN to a local disk
/// cache under `dataset_root` when the secret is absent or unparseable.
///
/// `dataset_root` is `${BUILD_DATASET_ROOT}`; the local fallback lives at
/// `${BUILD_DATASET_ROOT}/cache/sccache` (per the BLD-05 spec edge case).
pub fn resolve(dataset_root: &str) -> SccacheEnv {
    from_secret(env_nonempty(SCCACHE_REDIS_SECRET).as_deref(), dataset_root)
}

/// Pure builder (the test entry point): given an OPTIONAL secret URL and the
/// dataset root, produce the sccache env + mode. `None`/unparseable ⇒ fail-open
/// local dir.
pub fn from_secret(secret_url: Option<&str>, dataset_root: &str) -> SccacheEnv {
    let mut vars = BTreeMap::new();
    // Always wrap rustc with sccache; the backend below decides where objects go.
    vars.insert("RUSTC_WRAPPER".to_string(), SccacheEnv::binary());

    match secret_url.and_then(parse_redis_url) {
        Some(parts) => {
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
        None => {
            // Fail OPEN: point sccache at a local disk directory so a Redis
            // outage (or an unconfigured endpoint) never blocks a build.
            let dir = local_cache_dir(dataset_root);
            vars.insert("SCCACHE_DIR".to_string(), dir);
            SccacheEnv {
                vars,
                mode: SccacheMode::LocalDir,
            }
        }
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
    fn describe_never_contains_password() {
        let env = from_secret(Some("redis://default:sup3rsecret@h:6379/1"), DATASET);
        assert!(!env.describe().contains("sup3rsecret"));
    }
}
