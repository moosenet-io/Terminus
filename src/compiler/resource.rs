//! PCON-09: resource-aware admission budget.
//!
//! With per-SHA stage/target isolation in place (PCON-01..04), the per-`(module,
//! sha)` correctness lock no longer needs to force different-SHA builds of one
//! module to serialize — it only guards against two builds of the *identical*
//! SHA. What still needs bounding is the HOST's finite RAM/CPU/disk: many
//! independent SHAs admitting at once could OOM the box (and Plex lives here).
//!
//! This module models that budget. Admission bounds concurrent builds by:
//!   - **RAM** — the sum of each building job's `module_peak_mb` (a conservative
//!     default when a module has no configured peak) must fit `max_ram_mb`.
//!     Enforced ATOMICALLY inside the queue's `claim` Lua, summing the peaks of
//!     the jobs currently in the host's in-flight set (so it is inherently
//!     self-healing: a job leaving the set — via `release` or the reconcile
//!     backstop — frees its budget with no extra bookkeeping).
//!   - **job count** — `max_jobs` per host (falls back to the per-role
//!     `BUILD_HOST_CAP_*`), enforced by the existing host-cap arg to `claim`.
//!   - **disk headroom** — a free-space floor on the build-scratch filesystem,
//!     checked in the scheduler before dispatch.
//!
//! The per-build systemd-scope cgroup caps (`MemoryMax` / `MemorySwapMax=0` /
//! `CPUQuota`, the Plex protection in [`super::scope`]) are UNCHANGED — this
//! controller decides HOW MANY builds admit; the cgroup still bounds each one.
//!
//! ## Safe rollout (degrade-open)
//! Every knob defaults to OFF (`0`), so with nothing configured the RAM/disk
//! gates are inert and behavior is exactly today's (the existing per-host cap +
//! `(module, sha)` lock). An operator opts in by setting the caps.

use std::path::Path;

use crate::compiler::host::module_peak_mb;

/// Conservative per-module peak-build RAM (MB) used when a module has no
/// `BUILD_MODULE_PEAK_MB_<MODULE>` configured, so RAM admission never
/// over-commits on an unknown module. Overridable via `BUILD_MODULE_PEAK_MB_DEFAULT`.
pub const DEFAULT_MODULE_PEAK_MB: u64 = 4096;

/// A host's resource-admission budget. `Copy` so it threads cheaply through the
/// queue/scheduler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceBudget {
    /// Max total concurrent build RAM (MB) admissible on a host. `0` ⇒ the RAM
    /// gate is DISABLED (today's behavior; also the degrade-open value).
    pub max_ram_mb: u64,
    /// Host concurrent-job cap. `0` ⇒ fall back to the per-role `BUILD_HOST_CAP_*`.
    pub max_jobs: u32,
    /// Free-disk floor (MB) required before admitting a new build. `0` ⇒ no gate.
    pub min_disk_mb: u64,
    /// Conservative per-module peak used when a module has no configured peak.
    pub default_peak_mb: u64,
}

impl Default for ResourceBudget {
    /// [`ResourceBudget::unbounded`] — the safe, behavior-preserving default.
    fn default() -> Self {
        Self::unbounded()
    }
}

impl ResourceBudget {
    /// The unbounded budget: RAM/disk gates OFF, job cap deferred to the role
    /// cap. The DEFAULT when nothing is configured — behavior is unchanged until
    /// an operator opts in.
    pub const fn unbounded() -> Self {
        Self {
            max_ram_mb: 0,
            max_jobs: 0,
            min_disk_mb: 0,
            default_peak_mb: DEFAULT_MODULE_PEAK_MB,
        }
    }

    /// Load the budget from config env (all optional, all degrade-open):
    ///   - `BUILD_CONCURRENCY_MAX_RAM_MB` (default `0` = unlimited)
    ///   - `BUILD_CONCURRENCY_MAX_JOBS` (default `0` = use the role cap)
    ///   - `BUILD_CONCURRENCY_MIN_DISK_MB` (default `0` = no disk gate)
    ///   - `BUILD_MODULE_PEAK_MB_DEFAULT` (default [`DEFAULT_MODULE_PEAK_MB`])
    pub fn from_env() -> Self {
        let u64env = |k: &str, d: u64| {
            std::env::var(k)
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
                .unwrap_or(d)
        };
        let u32env = |k: &str, d: u32| {
            std::env::var(k)
                .ok()
                .and_then(|v| v.trim().parse::<u32>().ok())
                .unwrap_or(d)
        };
        Self {
            max_ram_mb: u64env("BUILD_CONCURRENCY_MAX_RAM_MB", 0),
            max_jobs: u32env("BUILD_CONCURRENCY_MAX_JOBS", 0),
            min_disk_mb: u64env("BUILD_CONCURRENCY_MIN_DISK_MB", 0),
            default_peak_mb: u64env("BUILD_MODULE_PEAK_MB_DEFAULT", DEFAULT_MODULE_PEAK_MB).max(1),
        }
    }

    /// The admission peak (MB) for a module: its configured
    /// `BUILD_MODULE_PEAK_MB_<MODULE>`, else the conservative [`Self::default_peak_mb`].
    /// A malformed configured value is treated as absent (falls to the default)
    /// rather than crashing admission.
    pub fn peak_for(&self, module: &str) -> u64 {
        module_peak_mb(module)
            .ok()
            .flatten()
            .unwrap_or(self.default_peak_mb)
            .max(1)
    }
}

/// Whether a new build of `peak_mb` admits given the peaks already building on
/// the host and the RAM budget. `max_ram_mb == 0` ⇒ unlimited (always admits).
/// This is the exact rule the queue's `claim` Lua mirrors.
pub fn ram_admits(
    peak_mb: u64,
    building_peaks: impl IntoIterator<Item = u64>,
    max_ram_mb: u64,
) -> bool {
    if max_ram_mb == 0 {
        return true;
    }
    let total: u64 = peak_mb.saturating_add(building_peaks.into_iter().sum());
    total <= max_ram_mb
}

/// Free space (MB) on the filesystem holding `path`, via `statvfs`. `None` when
/// the stat fails (path missing / unsupported) — the caller then degrades OPEN.
pub fn disk_free_mb(path: &Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: statvfs fills a zeroed struct; we only read scalar fields from it.
    unsafe {
        let mut st: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c.as_ptr(), &mut st) != 0 {
            return None;
        }
        let bsize = if st.f_frsize != 0 {
            st.f_frsize as u64
        } else {
            st.f_bsize as u64
        };
        Some(bsize.saturating_mul(st.f_bavail as u64) / (1024 * 1024))
    }
}

/// Whether `path` resides on a tmpfs/ramfs filesystem (the small in-RAM mounts a
/// build target/TMPDIR must NEVER land on). Uses `statfs` `f_type` when `path`
/// exists; a stat failure returns `false` (unknown — the caller applies its own
/// lexical prefix guard on top). `TMPFS_MAGIC = 0x0102_1994`, `RAMFS_MAGIC =
/// 0x8584_58f6`.
pub fn is_tmpfs(path: &Path) -> bool {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: statfs fills a zeroed struct; we only read the scalar f_type.
    unsafe {
        let mut st: libc::statfs = std::mem::zeroed();
        if libc::statfs(c.as_ptr(), &mut st) != 0 {
            return false;
        }
        let t = st.f_type as i64;
        t == 0x0102_1994 || t == 0x8584_58f6u32 as i64
    }
}

/// Whether `path`'s filesystem has at least `min_mb` free. `min_mb == 0` disables
/// the gate (always `true`). A failed stat degrades OPEN (`true`) — a build is
/// never blocked on an unreadable path.
pub fn disk_headroom_ok(path: &Path, min_mb: u64) -> bool {
    if min_mb == 0 {
        return true;
    }
    match disk_free_mb(path) {
        Some(free) => free >= min_mb,
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ram_admits_unlimited_when_zero_budget() {
        assert!(ram_admits(999_999, [1000, 2000], 0));
    }

    #[test]
    fn ram_admits_when_fits_and_rejects_when_over() {
        // 4000 building + 4000 new = 8000 <= 8192 → admits.
        assert!(ram_admits(4000, [4000], 8192));
        // 4000 building + 5000 new = 9000 > 8192 → rejected.
        assert!(!ram_admits(5000, [4000], 8192));
    }

    #[test]
    fn ram_admits_two_independent_jobs_then_bounds_the_third() {
        // Two 4GB jobs fit an 8GB budget; a third does not.
        assert!(ram_admits(4096, [4096], 8192)); // 2nd admits
        assert!(!ram_admits(4096, [4096, 4096], 8192)); // 3rd blocked
    }

    #[test]
    fn unbounded_budget_disables_ram_and_disk() {
        let b = ResourceBudget::unbounded();
        assert_eq!(b.max_ram_mb, 0);
        assert_eq!(b.min_disk_mb, 0);
        assert!(disk_headroom_ok(Path::new("/definitely/missing/path"), 0));
    }

    #[test]
    fn disk_headroom_degrades_open_on_bad_path() {
        // A nonexistent path can't be statted → degrade open (never block).
        assert!(disk_headroom_ok(Path::new("/no/such/dir/xyz"), 1_000_000));
    }

    #[test]
    fn disk_headroom_ok_on_a_real_dir_with_tiny_floor() {
        assert!(disk_headroom_ok(Path::new("/"), 1));
    }
}
