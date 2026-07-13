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

use crate::error::ToolError;
use crate::compiler::scope::ScopeCaps;

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
    env_nonempty(key).and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn env_u32(key: &str, default: u32) -> u32 {
    env_nonempty(key).and_then(|v| v.parse().ok()).unwrap_or(default)
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
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' })
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

/// Resolve the caps for a role from config, with role-appropriate defaults.
///
/// Primary defaults are conservative (fits a moderate-RAM dev box co-located
/// with user services); heavy defaults assume the big host freed by idle-mode.
pub fn caps_for(role: HostRole) -> ScopeCaps {
    let (mem_key, cpu_key, io_key, jobs_key, mem_def, cpu_def, io_def, jobs_def) = match role {
        HostRole::Primary => (
            "BUILD_PRIMARY_MEMORY_MAX",
            "BUILD_PRIMARY_CPU_QUOTA",
            "BUILD_PRIMARY_IO_WEIGHT",
            "BUILD_PRIMARY_JOBS",
            "12G",
            "400%",
            "50",
            4u32,
        ),
        HostRole::Heavy => (
            "BUILD_HEAVY_MEMORY_MAX",
            "BUILD_HEAVY_CPU_QUOTA",
            "BUILD_HEAVY_IO_WEIGHT",
            "BUILD_HEAVY_JOBS",
            "100G",
            "3200%",
            "100",
            32u32,
        ),
    };
    ScopeCaps {
        memory_max: env_nonempty(mem_key).unwrap_or_else(|| mem_def.to_string()),
        cpu_quota: env_nonempty(cpu_key).unwrap_or_else(|| cpu_def.to_string()),
        io_weight: env_nonempty(io_key).unwrap_or_else(|| io_def.to_string()),
        jobs: env_u32(jobs_key, jobs_def),
    }
}

/// Resolve the full host for a build, reading config from the environment.
///
/// The primary is treated as LOCAL (build-in-place, no relay) unless
/// `BUILD_HOST_PRIMARY` is explicitly set (some topologies relay to it too). The
/// heavy host always resolves an address (`BUILD_HOST_HEAVY`) — it's a remote
/// hop — and it is an error for `Heavy` to be selected without that address.
pub fn resolve(
    request: HostRequest,
    module: &str,
    fast: bool,
) -> Result<ResolvedHost, ToolError> {
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
        caps: caps_for(role),
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
    fn primary_caps_have_defaults() {
        // With no env set, defaults apply (this test does not mutate env).
        let caps = caps_for(HostRole::Primary);
        assert!(!caps.memory_max.is_empty());
        assert!(caps.jobs >= 1);
    }
}
