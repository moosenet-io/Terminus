//! BLD-05 — build-host selection (primary vs heavy).
//!
//! Two-tier build model:
//!   - **Primary** — the dev box (ample appdata-backed ext4, moderate RAM,
//!     capped). The DEFAULT for small/medium builds.
//!   - **Heavy** — the big-RAM/GPU host, freed on demand by idle-mode. Used when
//!     a module's known peak exceeds the primary's budget, or `fast=true`.
//!
//! Selection is `auto` by default: primary UNLESS the module's known peak RAM
//! (config `BUILD_MODULE_PEAK_MB_<MODULE>`) exceeds the heavy threshold
//! (`BUILD_HEAVY_THRESHOLD_MB`), or the caller asked for `fast`. The caller may
//! also force a role explicitly (`host="primary"|"heavy"`).
//!
//! Every host address, cap value, and threshold comes from config env vars
//! (materialized from the vault where sensitive) — NO literals in source (S1).

use crate::compiler::scope::ScopeCaps;
use crate::error::ToolError;

/// A build host role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostRole {
    Primary,
    Heavy,
}

impl HostRole {
    pub fn as_str(self) -> &'static str {
        match self {
            HostRole::Primary => "primary",
            HostRole::Heavy => "heavy",
        }
    }

    /// The env var naming this role's host address.
    fn host_env(self) -> &'static str {
        match self {
            HostRole::Primary => "BUILD_HOST_PRIMARY",
            HostRole::Heavy => "BUILD_HOST_HEAVY",
        }
    }
}

/// The requested host from the tool argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostRequest {
    Auto,
    Primary,
    Heavy,
}

impl HostRequest {
    pub fn parse(s: &str) -> Result<Self, ToolError> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => Ok(HostRequest::Auto),
            "primary" => Ok(HostRequest::Primary),
            "heavy" => Ok(HostRequest::Heavy),
            other => Err(ToolError::InvalidArgument(format!(
                "host must be auto|primary|heavy, got {other:?}"
            ))),
        }
    }
}

/// Default heavy-selection threshold (MB of peak build RSS) when
/// `BUILD_HEAVY_THRESHOLD_MB` is unset. A release build of the big workspaces
/// peaks ~5GB capped; the heavy host is for genuinely large/uncapped/fast work,
/// so the default threshold sits above the capped-primary budget.
const DEFAULT_HEAVY_THRESHOLD_MB: u64 = 16_000;

/// Pure host-selection heuristic (the test entry point). Deterministic given its
/// inputs so the selection rule is unit-testable without touching the env.
///
/// - explicit `Primary`/`Heavy` request → honored as-is.
/// - `Auto` → `Heavy` iff `fast` OR the module's known peak exceeds `threshold_mb`;
///   otherwise `Primary`.
pub fn select_role(
    request: HostRequest,
    fast: bool,
    module_peak_mb: Option<u64>,
    threshold_mb: u64,
) -> HostRole {
    match request {
        HostRequest::Primary => HostRole::Primary,
        HostRequest::Heavy => HostRole::Heavy,
        HostRequest::Auto => {
            if fast {
                HostRole::Heavy
            } else if module_peak_mb.map(|p| p > threshold_mb).unwrap_or(false) {
                HostRole::Heavy
            } else {
                HostRole::Primary
            }
        }
    }
}

/// Read a trimmed non-empty env var.
fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn env_u64(key: &str, default: u64) -> u64 {
    env_nonempty(key)
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// The configured heavy threshold (`BUILD_HEAVY_THRESHOLD_MB`).
pub fn heavy_threshold_mb() -> u64 {
    env_u64("BUILD_HEAVY_THRESHOLD_MB", DEFAULT_HEAVY_THRESHOLD_MB)
}

/// A module's known peak build RSS in MB, from `BUILD_MODULE_PEAK_MB_<MODULE>`
/// (module upper-cased, non-alphanumerics → `_`). `None` when unset (⇒ the
/// heuristic treats it as "fits the primary" unless `fast`).
pub fn module_peak_mb(module: &str) -> Option<u64> {
    let key = format!("BUILD_MODULE_PEAK_MB_{}", env_key_fragment(module));
    env_nonempty(&key).and_then(|v| v.parse().ok())
}

fn env_key_fragment(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// A fully-resolved build host: its role, its address (for relay/ssh), and its
/// resource caps.
#[derive(Debug, Clone)]
pub struct ResolvedHost {
    pub role: HostRole,
    /// ssh/rsync destination (`user@host` form), from `BUILD_HOST_<ROLE>`.
    /// `None` when the compiler runs LOCALLY on this host (build-in-place — the
    /// primary's own ext4), so no relay hop is needed.
    pub address: Option<String>,
    pub caps: ScopeCaps,
}

impl ResolvedHost {
    /// Whether the build runs in place on this box (no ssh relay).
    pub fn is_local(&self) -> bool {
        self.address.is_none()
    }
}

/// The four config env-var names carrying a role's resource caps.
fn cap_keys(role: HostRole) -> (&'static str, &'static str, &'static str, &'static str) {
    match role {
        HostRole::Primary => (
            "BUILD_PRIMARY_MEMORY_MAX",
            "BUILD_PRIMARY_CPU_QUOTA",
            "BUILD_PRIMARY_IO_WEIGHT",
            "BUILD_PRIMARY_JOBS",
        ),
        HostRole::Heavy => (
            "BUILD_HEAVY_MEMORY_MAX",
            "BUILD_HEAVY_CPU_QUOTA",
            "BUILD_HEAVY_IO_WEIGHT",
            "BUILD_HEAVY_JOBS",
        ),
    }
}

/// Pure cap resolver (the test entry point): EVERY cap comes from config via
/// `lookup`; there are **no hardcoded literal defaults** for these
/// host-capacity / Plex-protection values (S1). A missing/blank var is a hard
/// [`ToolError::NotConfigured`] naming the exact var — the operator MUST size
/// the caps per host, because a wrong default could either starve the build or,
/// worse, under-protect Plex (the whole point of the swap-off cap). Config vars:
/// `BUILD_{PRIMARY,HEAVY}_{MEMORY_MAX,CPU_QUOTA,IO_WEIGHT,JOBS}`.
pub fn caps_from_lookup(
    role: HostRole,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<ScopeCaps, ToolError> {
    let (mem_key, cpu_key, io_key, jobs_key) = cap_keys(role);
    let require = |key: &str| -> Result<String, ToolError> {
        lookup(key)
            .filter(|v| !v.trim().is_empty())
            .ok_or_else(|| ToolError::NotConfigured(format!("{key} is not configured")))
    };
    let memory_max = require(mem_key)?;
    let cpu_quota = require(cpu_key)?;
    let io_weight = require(io_key)?;
    let jobs_raw = require(jobs_key)?;
    let jobs: u32 = jobs_raw.trim().parse().map_err(|_| {
        ToolError::InvalidArgument(format!(
            "{jobs_key} must be a positive integer, got {jobs_raw:?}"
        ))
    })?;
    if jobs == 0 {
        return Err(ToolError::InvalidArgument(format!(
            "{jobs_key} must be >= 1"
        )));
    }
    Ok(ScopeCaps {
        memory_max: memory_max.trim().to_string(),
        cpu_quota: cpu_quota.trim().to_string(),
        io_weight: io_weight.trim().to_string(),
        jobs,
    })
}

/// Env-backed caps for a role (delegates to [`caps_from_lookup`], reading each
/// cap from its `BUILD_{ROLE}_*` config var). Errors (`NotConfigured`) when any
/// cap var for the selected role is unset.
pub fn caps_for(role: HostRole) -> Result<ScopeCaps, ToolError> {
    caps_from_lookup(role, env_nonempty)
}

/// Resolve the full host for a build, reading config from the environment.
///
/// The primary is treated as LOCAL (build-in-place, no relay) unless
/// `BUILD_HOST_PRIMARY` is explicitly set (some topologies relay to it too). The
/// heavy host always resolves an address (`BUILD_HOST_HEAVY`) — it's a remote
/// hop — and it is an error for `Heavy` to be selected without that address.
pub fn resolve(request: HostRequest, module: &str, fast: bool) -> Result<ResolvedHost, ToolError> {
    let role = select_role(request, fast, module_peak_mb(module), heavy_threshold_mb());
    let address = env_nonempty(role.host_env());
    if role == HostRole::Heavy && address.is_none() {
        return Err(ToolError::NotConfigured(format!(
            "heavy build selected for module {module:?} but {} is not configured",
            HostRole::Heavy.host_env()
        )));
    }
    Ok(ResolvedHost {
        role,
        address,
        caps: caps_for(role)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_defaults_to_primary_for_small_module() {
        let r = select_role(HostRequest::Auto, false, None, DEFAULT_HEAVY_THRESHOLD_MB);
        assert_eq!(r, HostRole::Primary);
        let r = select_role(HostRequest::Auto, false, Some(4_000), 16_000);
        assert_eq!(r, HostRole::Primary);
    }

    #[test]
    fn auto_picks_heavy_when_peak_exceeds_threshold() {
        let r = select_role(HostRequest::Auto, false, Some(24_000), 16_000);
        assert_eq!(r, HostRole::Heavy);
    }

    #[test]
    fn auto_picks_heavy_when_fast_even_for_small_module() {
        let r = select_role(HostRequest::Auto, true, Some(1_000), 16_000);
        assert_eq!(r, HostRole::Heavy);
    }

    #[test]
    fn explicit_request_overrides_heuristic() {
        // Explicit primary even for a huge module.
        assert_eq!(
            select_role(HostRequest::Primary, true, Some(99_000), 16_000),
            HostRole::Primary
        );
        // Explicit heavy even for a tiny one.
        assert_eq!(
            select_role(HostRequest::Heavy, false, Some(10), 16_000),
            HostRole::Heavy
        );
    }

    #[test]
    fn host_request_parse() {
        assert_eq!(HostRequest::parse("auto").unwrap(), HostRequest::Auto);
        assert_eq!(HostRequest::parse("").unwrap(), HostRequest::Auto);
        assert_eq!(HostRequest::parse("PRIMARY").unwrap(), HostRequest::Primary);
        assert_eq!(HostRequest::parse("heavy").unwrap(), HostRequest::Heavy);
        assert!(HostRequest::parse("banana").is_err());
    }

    #[test]
    fn env_key_fragment_uppercases_and_replaces() {
        assert_eq!(env_key_fragment("lumina-core"), "LUMINA_CORE");
        assert_eq!(env_key_fragment("Chord"), "CHORD");
    }

    #[test]
    fn caps_require_every_config_var() {
        // S1/finding-3: no hardcoded literal defaults — an unset cap var is a
        // hard NotConfigured naming the exact missing var.
        let err = caps_from_lookup(HostRole::Primary, |_| None).unwrap_err();
        assert!(matches!(err, ToolError::NotConfigured(_)));
        // A partial config (jobs missing) still fails, naming the missing var.
        let partial = |k: &str| match k {
            "BUILD_PRIMARY_MEMORY_MAX" => Some("8G".to_string()),
            "BUILD_PRIMARY_CPU_QUOTA" => Some("200%".to_string()),
            "BUILD_PRIMARY_IO_WEIGHT" => Some("40".to_string()),
            _ => None,
        };
        match caps_from_lookup(HostRole::Primary, partial) {
            Err(ToolError::NotConfigured(m)) => assert!(m.contains("BUILD_PRIMARY_JOBS")),
            other => panic!("expected NotConfigured naming JOBS, got {other:?}"),
        }
    }

    #[test]
    fn caps_parse_from_config() {
        let cfg = |k: &str| match k {
            "BUILD_HEAVY_MEMORY_MAX" => Some("100G".to_string()),
            "BUILD_HEAVY_CPU_QUOTA" => Some("3200%".to_string()),
            "BUILD_HEAVY_IO_WEIGHT" => Some("100".to_string()),
            "BUILD_HEAVY_JOBS" => Some("32".to_string()),
            _ => None,
        };
        let caps = caps_from_lookup(HostRole::Heavy, cfg).unwrap();
        assert_eq!(caps.memory_max, "100G");
        assert_eq!(caps.cpu_quota, "3200%");
        assert_eq!(caps.io_weight, "100");
        assert_eq!(caps.jobs, 32);
    }

    #[test]
    fn caps_reject_bad_jobs() {
        let with_jobs = |jobs: &'static str| {
            move |k: &str| match k {
                "BUILD_PRIMARY_MEMORY_MAX" => Some("8G".to_string()),
                "BUILD_PRIMARY_CPU_QUOTA" => Some("200%".to_string()),
                "BUILD_PRIMARY_IO_WEIGHT" => Some("40".to_string()),
                "BUILD_PRIMARY_JOBS" => Some(jobs.to_string()),
                _ => None,
            }
        };
        assert!(caps_from_lookup(HostRole::Primary, with_jobs("0")).is_err());
        assert!(caps_from_lookup(HostRole::Primary, with_jobs("notanint")).is_err());
        assert_eq!(
            caps_from_lookup(HostRole::Primary, with_jobs("4"))
                .unwrap()
                .jobs,
            4
        );
    }
}
