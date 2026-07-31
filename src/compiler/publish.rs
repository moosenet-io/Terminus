//! BLD-05 — artifact publish (checksummed, build-once → publish → consume).
//!
//! After a successful build, the compiler publishes the binary into the shared
//! build dataset under a content-addressed layout:
//!
//!   ${BUILD_DATASET_ROOT}/artifacts/<module>/<channel>/<sha>/<target>/<bin>
//!   ${BUILD_DATASET_ROOT}/artifacts/<module>/<channel>/<sha>/<target>/<bin>.sha256
//!
//! where `<sha>` is the SHA-256 of the binary (also written to the `.sha256`
//! sidecar the constellation-updater verifies before an atomic-mv swap).
//!
//! This module does NOT flip a `current` pointer — channel promotion is BLD-07
//! (`compiler_release`). Publish only writes the immutable sha dir + sidecar.
//!
//! ## Interim relay (before BLD-01 mounts land)
//! If the dataset is not mounted RW on the build host, the artifact is relayed
//! over a single ssh/rsync hop to a host that has it RW (`render_relay_argv`) —
//! no primary-host mount required. When the dataset IS mounted locally, publish
//! is a plain in-process copy + sidecar write.
//!
//! All paths come from config (`BUILD_DATASET_ROOT`); no literals (S1).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::ToolError;

/// The default artifact channel a fresh build publishes into. Promotion to
/// `stable` is a separate pointer-flip (BLD-07), never a rebuild.
pub const DEFAULT_CHANNEL: &str = "experimental";

/// The build/un-promoted channel — the ONLY channel `compiler_build` may bless.
pub const BUILD_CHANNEL: &str = DEFAULT_CHANNEL;

/// The promote-only release channel — reachable ONLY via `compiler_release`
/// (promote-by-copy), never by a rebuild blessing it directly.
pub const STABLE_CHANNEL: &str = "stable";

/// Whether a channel may only be blessed by `compiler_release` (promote), never by
/// a build. Keeps release discipline: a rebuild can never write `stable/current`.
pub fn is_promote_only_channel(channel: &str) -> bool {
    channel == STABLE_CHANNEL
}

/// The relative artifact path (under the dataset root) for a built binary:
/// `artifacts/<module>/<channel>/<sha>/<target>/<bin>`.
pub fn artifact_rel_path(
    module: &str,
    channel: &str,
    sha: &str,
    target: &str,
    bin: &str,
) -> PathBuf {
    PathBuf::from("artifacts")
        .join(module)
        .join(channel)
        .join(sha)
        .join(target)
        .join(bin)
}

/// The absolute artifact path under `dataset_root`.
pub fn artifact_abs_path(
    dataset_root: &Path,
    module: &str,
    channel: &str,
    sha: &str,
    target: &str,
    bin: &str,
) -> PathBuf {
    dataset_root.join(artifact_rel_path(module, channel, sha, target, bin))
}

/// Lowercase-hex SHA-256 of a byte slice.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    let digest = h.finalize();
    to_hex(&digest)
}

/// Lowercase-hex SHA-256 of a file's contents (streamed).
pub async fn sha256_file(path: &Path) -> Result<String, ToolError> {
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|e| ToolError::Execution(format!("read {} for sha256: {e}", path.display())))?;
    Ok(sha256_hex(&bytes))
}

fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

/// The `.sha256` sidecar contents: `<sha>  <bin>\n` (the `sha256sum -c` format,
/// so a consumer can verify with the standard tool).
pub fn sidecar_contents(sha: &str, bin: &str) -> String {
    format!("{sha}  {bin}\n")
}

/// A published artifact's locations, returned to the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Published {
    pub sha256: String,
    pub artifact_path: PathBuf,
    pub sha256_path: PathBuf,
    pub relayed: bool,
}

/// Publish a built binary LOCALLY into a dataset that is mounted RW on this host.
/// Computes the sha, copies the binary into the content-addressed layout, and
/// writes the `.sha256` sidecar. Does NOT flip any `current` pointer.
pub async fn publish_local(
    dataset_root: &Path,
    module: &str,
    channel: &str,
    target: &str,
    bin_name: &str,
    built_bin: &Path,
) -> Result<Published, ToolError> {
    let sha = sha256_file(built_bin).await?;
    let dest = artifact_abs_path(dataset_root, module, channel, &sha, target, bin_name);
    let dest_dir = dest
        .parent()
        .ok_or_else(|| ToolError::Execution("artifact path has no parent".into()))?;
    tokio::fs::create_dir_all(dest_dir)
        .await
        .map_err(|e| ToolError::Execution(format!("mkdir {}: {e}", dest_dir.display())))?;
    tokio::fs::copy(built_bin, &dest)
        .await
        .map_err(|e| ToolError::Execution(format!("copy artifact → {}: {e}", dest.display())))?;

    let sidecar = dest.with_file_name(format!("{bin_name}.sha256"));
    tokio::fs::write(&sidecar, sidecar_contents(&sha, bin_name))
        .await
        .map_err(|e| ToolError::Execution(format!("write sidecar {}: {e}", sidecar.display())))?;

    Ok(Published {
        sha256: sha,
        artifact_path: dest,
        sha256_path: sidecar,
        relayed: false,
    })
}

/// Render the rsync argv for the INTERIM relay-publish hop: push the built
/// binary to `<relay_host>:<dataset_root>/<rel artifact path>`, creating the
/// remote dirs. Used when the dataset is not mounted RW on the build host.
///
/// Pure (returns the argv) so the relay command is testable offline; the
/// executor runs it. `-R --mkpath` semantics are emulated by pre-creating the
/// remote dir over ssh in the executor; here we render the plain file push.
pub fn render_relay_argv(
    relay_host: &str,
    remote_dataset_root: &str,
    module: &str,
    channel: &str,
    sha: &str,
    target: &str,
    bin_name: &str,
    local_bin: &Path,
) -> Vec<String> {
    let rel = artifact_rel_path(module, channel, sha, target, bin_name);
    let remote = format!(
        "{}:{}/{}",
        relay_host,
        remote_dataset_root.trim_end_matches('/'),
        rel.display()
    );
    vec![
        "rsync".to_string(),
        "-a".to_string(),
        "--mkpath".to_string(),
        // `-s`/--protect-args: the remote path (built from module/channel/target/
        // bin) is sent verbatim, never re-split by the remote shell.
        "-s".to_string(),
        local_bin.to_string_lossy().to_string(),
        remote,
    ]
}

/// A complete relay-publish plan for the no-dataset-mount path: the two rsync
/// commands that deliver BOTH the binary AND its `<bin>.sha256` sidecar into the
/// content-addressed layout on the relay host, plus the sidecar body to stage
/// locally first. Bundling them here guarantees the sidecar is never dropped — an
/// artifact without its `.sha256` is unverifiable by the constellation-updater,
/// so relay must mirror the local publish's binary+sidecar pair.
pub struct RelayPlan {
    pub binary_argv: Vec<String>,
    pub sidecar_argv: Vec<String>,
    /// The `<bin>.sha256` body to write to the local staging sidecar path (the
    /// same `sha256sum -c` format as the local publish) before the sidecar rsync.
    pub sidecar_body: String,
    /// Remote destination paths (for the returned [`Published`]).
    pub remote_binary: PathBuf,
    pub remote_sidecar: PathBuf,
}

/// Build the [`RelayPlan`] for `(module, channel, sha, target, bin)`: the binary
/// is relayed from `local_bin` and the sidecar from `local_sidecar` (the caller
/// writes `sidecar_body` there first), both into
/// `<remote_dataset_root>/artifacts/<module>/<channel>/<sha>/<target>/`.
#[allow(clippy::too_many_arguments)]
pub fn render_relay_plan(
    relay_host: &str,
    remote_dataset_root: &str,
    module: &str,
    channel: &str,
    sha: &str,
    target: &str,
    bin_name: &str,
    local_bin: &Path,
    local_sidecar: &Path,
) -> RelayPlan {
    let sidecar_name = format!("{bin_name}.sha256");
    let binary_argv = render_relay_argv(
        relay_host,
        remote_dataset_root,
        module,
        channel,
        sha,
        target,
        bin_name,
        local_bin,
    );
    let sidecar_argv = render_relay_argv(
        relay_host,
        remote_dataset_root,
        module,
        channel,
        sha,
        target,
        &sidecar_name,
        local_sidecar,
    );
    let root = remote_dataset_root.trim_end_matches('/');
    let remote_binary =
        PathBuf::from(root).join(artifact_rel_path(module, channel, sha, target, bin_name));
    let remote_sidecar = PathBuf::from(root).join(artifact_rel_path(
        module,
        channel,
        sha,
        target,
        &sidecar_name,
    ));
    RelayPlan {
        binary_argv,
        sidecar_argv,
        sidecar_body: sidecar_contents(sha, bin_name),
        remote_binary,
        remote_sidecar,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// BLD-07 — artifact store: the `current` channel pointer + `compiler_release`
// promote. Publish (above) writes the immutable, content-addressed sha dir; this
// section adds the mutable, atomically-flipped `current` pointer per (module,
// channel), a per-sha manifest, a rollback-capable history, and the promote that
// blesses an already-built sha into a channel with NO recompile (Rust-train
// model). Every path derives from the configured dataset root — no infra literals
// (S1) — and nothing here reads a secret env (S7).
// ─────────────────────────────────────────────────────────────────────────────

/// The pointer file inside a channel dir naming the blessed (current) sha the
/// constellation-updater fetches. Flipped atomically (temp + rename), never
/// partially written.
pub const CURRENT_POINTER: &str = "current";
/// Sidecar to `current` holding the PREVIOUS blessed sha — the one-step rollback
/// target. Updated atomically alongside `current`.
pub const PREVIOUS_POINTER: &str = "current.prev";
/// Append-only JSONL audit log of every bless / promote / rollback in a channel.
pub const HISTORY_LOG: &str = "history.jsonl";
/// The per-sha manifest filename (a small `dist-manifest.json`-style index).
pub const MANIFEST_NAME: &str = "dist-manifest.json";
/// Default number of sha dirs retained per channel by pruning. The store never
/// prunes below 2 (nor the current / previous sha) regardless of this value.
pub const DEFAULT_RETAIN_PER_CHANNEL: usize = 2;

/// The channel dir: `${dataset_root}/artifacts/<module>/<channel>`.
pub fn channel_dir(dataset_root: &Path, module: &str, channel: &str) -> PathBuf {
    dataset_root.join("artifacts").join(module).join(channel)
}

/// The immutable content-addressed sha dir: `<channel_dir>/<sha>`.
pub fn sha_dir(dataset_root: &Path, module: &str, channel: &str, sha: &str) -> PathBuf {
    channel_dir(dataset_root, module, channel).join(sha)
}

/// Path of the `current` pointer for a (module, channel).
pub fn current_pointer_path(dataset_root: &Path, module: &str, channel: &str) -> PathBuf {
    channel_dir(dataset_root, module, channel).join(CURRENT_POINTER)
}

/// Path of the `current.prev` (rollback target) pointer for a (module, channel).
pub fn previous_pointer_path(dataset_root: &Path, module: &str, channel: &str) -> PathBuf {
    channel_dir(dataset_root, module, channel).join(PREVIOUS_POINTER)
}

/// Path of the append-only history log for a (module, channel).
pub fn history_log_path(dataset_root: &Path, module: &str, channel: &str) -> PathBuf {
    channel_dir(dataset_root, module, channel).join(HISTORY_LOG)
}

/// Path of the per-sha manifest.
pub fn manifest_path(dataset_root: &Path, module: &str, channel: &str, sha: &str) -> PathBuf {
    sha_dir(dataset_root, module, channel, sha).join(MANIFEST_NAME)
}

/// A small per-sha manifest written next to the artifact — a stable, machine-
/// readable index of what a sha dir contains (module/channel/target/bin + the
/// content-address + relative paths).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub module: String,
    pub channel: String,
    pub sha256: String,
    pub target: String,
    pub bin: String,
    /// Artifact + sidecar paths relative to the dataset root (portable across
    /// hosts that mount the dataset at different mount points).
    pub artifact_rel: String,
    pub sha256_rel: String,
    /// RFC3339 UTC timestamp the manifest was written.
    pub created_at: String,
}

/// One audit entry in the channel history log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// RFC3339 UTC timestamp.
    pub at: String,
    /// `bless` (publish flipped experimental), `promote` (cross-channel release),
    /// or `rollback` (reverted to the previous blessed sha).
    pub action: String,
    /// The sha the pointer was moved TO.
    pub sha: String,
    /// The sha the pointer was moved FROM (rollback target after this entry).
    pub previous: Option<String>,
    /// For a promote: the source channel the sha was blessed from.
    pub from_channel: Option<String>,
}

/// Atomically write `contents` to `path`: write a uniquely-named temp file in the
/// SAME directory, then `rename` it over the target. `rename(2)` within one
/// filesystem is atomic, so a concurrent reader sees either the old pointer or the
/// new one — never a truncated/partial pointer. On any failure the temp is
/// removed. This is the single choke point for every pointer flip.
async fn atomic_write(path: &Path, contents: &str) -> Result<(), ToolError> {
    let dir = path
        .parent()
        .ok_or_else(|| ToolError::Execution("pointer path has no parent".into()))?;
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|e| ToolError::Execution(format!("mkdir {}: {e}", dir.display())))?;
    let fname = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "pointer".to_string());
    let tmp = dir.join(format!(".{fname}.tmp.{}", uuid::Uuid::new_v4()));
    if let Err(e) = tokio::fs::write(&tmp, contents).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(ToolError::Execution(format!(
            "write temp pointer {}: {e}",
            tmp.display()
        )));
    }
    if let Err(e) = tokio::fs::rename(&tmp, path).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(ToolError::Execution(format!(
            "atomic rename {} → {}: {e}",
            tmp.display(),
            path.display()
        )));
    }
    Ok(())
}

/// Read a pointer file (`current` / `current.prev`), returning `None` when it does
/// not exist yet (a channel with no blessed sha), and trimming trailing newline.
async fn read_pointer(path: &Path) -> Result<Option<String>, ToolError> {
    match tokio::fs::read_to_string(path).await {
        Ok(s) => {
            let v = s.trim().to_string();
            Ok(if v.is_empty() { None } else { Some(v) })
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(ToolError::Execution(format!(
            "read pointer {}: {e}",
            path.display()
        ))),
    }
}

/// Query the current blessed sha for a (module, channel), if any.
pub async fn read_current(
    dataset_root: &Path,
    module: &str,
    channel: &str,
) -> Result<Option<String>, ToolError> {
    read_pointer(&current_pointer_path(dataset_root, module, channel)).await
}

/// Query the previous blessed sha (the rollback target) for a (module, channel).
pub async fn read_previous(
    dataset_root: &Path,
    module: &str,
    channel: &str,
) -> Result<Option<String>, ToolError> {
    read_pointer(&previous_pointer_path(dataset_root, module, channel)).await
}

/// Read + parse the channel history log (oldest first). A missing log is an empty
/// history; an unparsable line is skipped rather than failing the whole read.
pub async fn read_history(
    dataset_root: &Path,
    module: &str,
    channel: &str,
) -> Result<Vec<HistoryEntry>, ToolError> {
    let path = history_log_path(dataset_root, module, channel);
    let body = match tokio::fs::read_to_string(&path).await {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(ToolError::Execution(format!(
                "read history {}: {e}",
                path.display()
            )))
        }
    };
    Ok(body
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<HistoryEntry>(l).ok())
        .collect())
}

/// Append one entry to the channel history log (create-or-append; never rewrites
/// prior lines, so the audit trail is immutable).
async fn append_history(
    dataset_root: &Path,
    module: &str,
    channel: &str,
    entry: &HistoryEntry,
) -> Result<(), ToolError> {
    use tokio::io::AsyncWriteExt;
    let path = history_log_path(dataset_root, module, channel);
    if let Some(dir) = path.parent() {
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|e| ToolError::Execution(format!("mkdir {}: {e}", dir.display())))?;
    }
    let mut line = serde_json::to_string(entry)
        .map_err(|e| ToolError::Execution(format!("serialize history entry: {e}")))?;
    line.push('\n');
    let mut f = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .await
        .map_err(|e| ToolError::Execution(format!("open history {}: {e}", path.display())))?;
    f.write_all(line.as_bytes())
        .await
        .map_err(|e| ToolError::Execution(format!("append history {}: {e}", path.display())))?;
    Ok(())
}

/// Write (or overwrite) the per-sha manifest for an already-published artifact.
pub async fn write_manifest(
    dataset_root: &Path,
    module: &str,
    channel: &str,
    sha: &str,
    target: &str,
    bin: &str,
) -> Result<Manifest, ToolError> {
    let artifact_rel = artifact_rel_path(module, channel, sha, target, bin)
        .to_string_lossy()
        .to_string();
    let sha256_rel = artifact_rel_path(module, channel, sha, target, &format!("{bin}.sha256"))
        .to_string_lossy()
        .to_string();
    let manifest = Manifest {
        module: module.to_string(),
        channel: channel.to_string(),
        sha256: sha.to_string(),
        target: target.to_string(),
        bin: bin.to_string(),
        artifact_rel,
        sha256_rel,
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    let path = manifest_path(dataset_root, module, channel, sha);
    if let Some(dir) = path.parent() {
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|e| ToolError::Execution(format!("mkdir {}: {e}", dir.display())))?;
    }
    let body = serde_json::to_vec_pretty(&manifest)
        .map_err(|e| ToolError::Execution(format!("serialize manifest: {e}")))?;
    tokio::fs::write(&path, &body)
        .await
        .map_err(|e| ToolError::Execution(format!("write manifest {}: {e}", path.display())))?;
    Ok(manifest)
}

/// Read + parse the per-sha manifest, if present.
pub async fn read_manifest(
    dataset_root: &Path,
    module: &str,
    channel: &str,
    sha: &str,
) -> Result<Option<Manifest>, ToolError> {
    let path = manifest_path(dataset_root, module, channel, sha);
    match tokio::fs::read(&path).await {
        Ok(bytes) => serde_json::from_slice::<Manifest>(&bytes)
            .map(Some)
            .map_err(|e| ToolError::Execution(format!("parse manifest {}: {e}", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(ToolError::Execution(format!(
            "read manifest {}: {e}",
            path.display()
        ))),
    }
}

/// VERIFY-BEFORE-BLESS: prove a sha dir holds a genuine, self-consistent artifact
/// before any pointer is moved onto it. Checks that BOTH the binary and its
/// `.sha256` sidecar exist AND that the content-address dir name equals both the
/// binary's actual SHA-256 and the sha recorded in the sidecar. Any mismatch or
/// missing file is a hard error (fail closed) — so `current` can never point at a
/// sha that was never built, was corrupted, or whose checksum disagrees.
pub async fn verify_sha_artifact(
    dataset_root: &Path,
    module: &str,
    channel: &str,
    sha: &str,
    target: &str,
    bin: &str,
) -> Result<(), ToolError> {
    let bin_path = artifact_abs_path(dataset_root, module, channel, sha, target, bin);
    let sidecar = bin_path.with_file_name(format!("{bin}.sha256"));
    if !tokio::fs::try_exists(&bin_path).await.unwrap_or(false) {
        return Err(ToolError::NotFound(format!(
            "artifact binary is missing at {}",
            bin_path.display()
        )));
    }
    if !tokio::fs::try_exists(&sidecar).await.unwrap_or(false) {
        return Err(ToolError::NotFound(format!(
            "artifact sha256 sidecar is missing at {}",
            sidecar.display()
        )));
    }
    let actual = sha256_file(&bin_path).await?;
    if actual != sha {
        return Err(ToolError::Conflict(format!(
            "artifact sha mismatch: binary hashes to {actual} but lives under content-address dir {sha}"
        )));
    }
    let recorded = tokio::fs::read_to_string(&sidecar)
        .await
        .map_err(|e| ToolError::Execution(format!("read sidecar {}: {e}", sidecar.display())))?;
    let recorded_sha = recorded.split_whitespace().next().unwrap_or("");
    if recorded_sha != sha {
        return Err(ToolError::Conflict(format!(
            "sidecar sha mismatch: {} records {recorded_sha} but content-address dir is {sha}",
            sidecar.display()
        )));
    }
    Ok(())
}

/// TERM #565 (round-2 review finding 1) — the module's DEFAULT target triple, the
/// ONLY target whose artifact a `current` pointer may be aimed at.
///
/// ## The hazard this type exists to make unrepresentable
///
/// An artifact lives at `<channel>/<sha>/<target>/<bin>` — target-ADDRESSED. The
/// `current` pointer ([`current_pointer_path`]) is keyed by `(module, channel)`
/// only — target-AGNOSTIC. Nothing in the pointer records which target's artifact
/// it means, and every consumer (`compiler_release` promote / rollback / current,
/// and the updater behind them) resolves the artifact under
/// `effective_triple(module)`. So blessing a sha that was built for some OTHER
/// target leaves the live pointer aimed at a sha whose artifact those consumers
/// cannot find — a module whose live release is unresolvable.
///
/// Allowlisting the triple does NOT address this: `x86_64-unknown-linux-gnu` and
/// `x86_64-unknown-linux-musl` are both perfectly supported targets, and a
/// gnu-target build of a musl-default module is exactly the broken case. The
/// hazard is the DISAGREEMENT with the module default, not the identity of the
/// triple.
///
/// ## The rule
///
/// A pointer flip is permitted ONLY when the artifact's target equals the
/// module's effective default. An off-default target may still be BUILT and
/// GATED (the TERM #564 use case: a one-off native-glibc build of a musl-default
/// module, which is why chord / terminus-primary need `TARGET_NATIVE=1`) — it
/// simply cannot silently become the live release.
///
/// The value is CONSTRUCTIBLE ONLY from a module name, via
/// [`DefaultTarget::for_module`], which resolves through `effective_triple` — the
/// single resolution point `compiler_release` also uses. No production caller can
/// pass its own target as its own "default" and defeat the guard; the only other
/// constructor is `#[cfg(test)]`.
///
/// ## default-target MIGRATION — now DETECTED, still not PREVENTED
///
/// This guard is a WRITE-TIME check on the flip. It stops a NEW off-default
/// artifact from being blessed. It does NOT cover the pointer going stale UNDER
/// an unchanged pointer: an operator edits `BUILD_MODULE_TARGET_<MODULE>` (or the
/// fleet `BUILD_TARGET_TRIPLE`) from target A to target B while `current` still
/// names a sha whose artifact was only ever built for A. No flip occurs, so
/// nothing re-runs this guard — but `compiler_release` now resolves
/// `effective_triple(module) = B` and the live pointer becomes unresolvable,
/// reached by the CONFIG door instead of the write door.
///
/// **What round-7 added (`resolve_pointer` / `resolve_current`):** the read side
/// now DETECTS this. `compiler_release op=current` resolves the pointer to a
/// concrete artifact path under the effective target, and `rollback` resolves
/// `current.prev` the same way; either failing to resolve returns a diagnostic
/// naming the sha, the resolved target, the exact path checked, and (when the sha
/// dir holds other target subdirs) the target it WAS built for — pointing
/// explicitly at a changed `BUILD_MODULE_TARGET_*` / `BUILD_TARGET_TRIPLE` as the
/// likely cause. Nothing is guessed, repaired, or silently served from another
/// target. So the hazard is no longer SILENT: it surfaces as an actionable error
/// at the first read instead of a bare not-found inside whichever consumer
/// happened to hit it.
///
/// TODO(term-564-followup — default-target migration, STILL OPEN): detection is
/// not prevention. What remains is automatic handling at CONFIG-CHANGE time,
/// which a build-time guard cannot own:
/// 1. **Validate at config-change time** — when a module's effective target
///    changes, check that every channel's `current` still resolves under the new
///    target and refuse / loudly alarm if it does not (the check itself is now
///    a one-line `resolve_current` call; what is missing is a hook that runs it
///    when the config changes).
/// 2. **Migrate the pointer** — rebuild the blessed sha for the new target and
///    re-point `current` at it (or roll `current` back to a sha that does
///    resolve) as part of the target change.
///
/// The underlying target-agnostic pointer is PRE-EXISTING (it predates TERM
/// #564/#565); this branch narrows the failure from silent-and-confusing to
/// loud-and-named, and does not claim to close it.
///
/// No Plane item exists for this yet (no reachable Plane tool at the time of
/// writing, and improvising a raw API call would violate the single-sanctioned-door
/// rule), so THIS COMMENT IS THE RECORD. File it when the tool is reachable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefaultTarget(String);

impl DefaultTarget {
    /// Resolve `module`'s default target through the single resolution point
    /// (`BUILD_MODULE_TARGET_<MODULE>` → `BUILD_TARGET_TRIPLE` → the fleet default).
    pub fn for_module(module: &str) -> Self {
        Self(super::effective_triple(module))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Test-only constructor, so the pointer-target invariant can be exercised
    /// without mutating process-wide env. Deliberately NOT available to
    /// production code — see the type doc.
    #[cfg(test)]
    pub fn from_raw_for_test(target: &str) -> Self {
        Self(target.to_string())
    }
}

/// The single guard behind every pointer flip: refuse to aim a target-agnostic
/// `current` pointer at an artifact built for a non-default target. See
/// [`DefaultTarget`] for the full rationale.
fn guard_pointer_target(
    module: &str,
    channel: &str,
    target: &str,
    default: &DefaultTarget,
) -> Result<(), ToolError> {
    if target == default.as_str() {
        return Ok(());
    }
    Err(ToolError::InvalidArgument(format!(
        "refusing to move {module}/{channel} current onto an artifact built for target \
         {target:?}: this module's default target is {default:?}, and the current pointer is \
         keyed by (module, channel) with NO target component — so the flip would leave the live \
         pointer on a sha whose artifact compiler_release (which resolves \
         effective_triple({module:?}) = {default:?}) cannot find. An off-default target may be \
         BUILT and GATED, but never blessed; change this module's BUILD_MODULE_TARGET_* config \
         if {target:?} is meant to be its release target.",
        default = default.as_str()
    )))
}

/// A `current` / `current.prev` pointer RESOLVED to a concrete artifact path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPointer {
    /// The sha the pointer names.
    pub sha: String,
    /// The target the pointer was resolved UNDER (the module's effective triple,
    /// unless the caller overrode it).
    pub target: String,
    /// The artifact path that exists for `(sha, target, bin)`.
    pub artifact_path: PathBuf,
}

/// List the target-triple subdirs actually present under a sha dir — i.e. the
/// targets this sha WAS built for. Best-effort: a missing/unreadable sha dir is
/// an empty list. Used ONLY to enrich a resolution diagnostic; it is NEVER a
/// fallback (see [`resolve_pointer`]).
async fn targets_present_for_sha(
    dataset_root: &Path,
    module: &str,
    channel: &str,
    sha: &str,
) -> Vec<String> {
    let dir = sha_dir(dataset_root, module, channel, sha);
    let mut found = Vec::new();
    if let Ok(mut rd) = tokio::fs::read_dir(&dir).await {
        while let Ok(Some(entry)) = rd.next_entry().await {
            if entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
                if let Some(name) = entry.file_name().to_str() {
                    found.push(name.to_string());
                }
            }
        }
    }
    found.sort();
    found
}

/// TERM #565 (round-7) — the READ-SIDE detector for the default-target MIGRATION
/// hazard documented on [`DefaultTarget`].
///
/// [`guard_pointer_target`] closes the WRITE door: no flip can aim a pointer at
/// an off-default artifact. It cannot close the CONFIG door — an operator edits
/// `BUILD_MODULE_TARGET_<MODULE>` (or the fleet `BUILD_TARGET_TRIPLE`) from A to
/// B while `current` still names a sha only ever built for A. No flip occurs, so
/// no write-time guard re-runs, and every consumer afterwards resolves under B
/// and finds nothing. That cannot be PREVENTED from the build path (it would mean
/// owning the config-change path), but it CAN be DETECTED here, at the one place
/// a pointer becomes an artifact path, and reported as a named cause instead of a
/// bare not-found from whichever consumer happened to hit it first.
///
/// Returns `Ok(None)` when the channel has no pointer of this kind yet (an
/// unblessed channel is not an error). Returns `Ok(Some(..))` when the pointer's
/// artifact exists under `target`. Otherwise returns a diagnostic naming the sha,
/// the resolved target, the exact path checked, and — when the sha dir holds
/// OTHER target subdirs — which targets it WAS built for.
///
/// It NEVER guesses, repairs, or falls back to another target: a wrong artifact
/// silently promoted is far worse than a loud error. The remedy is always an
/// operator action (rebuild the sha for the new target, or roll the channel back
/// to a sha that resolves).
pub async fn resolve_pointer(
    dataset_root: &Path,
    module: &str,
    channel: &str,
    pointer: &str,
    sha: Option<String>,
    target: &str,
    bin: &str,
) -> Result<Option<ResolvedPointer>, ToolError> {
    let Some(sha) = sha else {
        return Ok(None);
    };
    let artifact_path = artifact_abs_path(dataset_root, module, channel, &sha, target, bin);
    if tokio::fs::try_exists(&artifact_path).await.unwrap_or(false) {
        return Ok(Some(ResolvedPointer {
            sha,
            target: target.to_string(),
            artifact_path,
        }));
    }
    let built_for = targets_present_for_sha(dataset_root, module, channel, &sha).await;
    let built_for_note = if built_for.is_empty() {
        String::from(
            " No target dir exists for this sha at all, so it was either never built into this \
             channel or has been pruned.",
        )
    } else {
        format!(
            " This sha IS present for target(s) [{}] — i.e. it was built and blessed for a \
             DIFFERENT target than the one now configured.",
            built_for.join(", ")
        )
    };
    Err(ToolError::NotFound(format!(
        "{module}/{channel} {pointer} points at sha {sha}, but no artifact exists for target \
         {target:?}: checked {path}. This usually means the module's configured target changed \
         (BUILD_MODULE_TARGET_<MODULE> / BUILD_TARGET_TRIPLE) after this pointer was set.\
         {built_for_note} The pointer is NOT repaired automatically and no other target is \
         substituted: rebuild {sha} for {target:?} and re-bless it, or roll {module}/{channel} \
         back to a sha that does resolve under {target:?}.",
        path = artifact_path.display()
    )))
}

/// Resolve a channel's live `current` pointer to its artifact under `target`.
/// See [`resolve_pointer`] — this is the read-side entry point `compiler_release`
/// uses so a query of the live release pointer surfaces the migration hazard
/// rather than handing a consumer a sha nothing can find.
pub async fn resolve_current(
    dataset_root: &Path,
    module: &str,
    channel: &str,
    target: &str,
    bin: &str,
) -> Result<Option<ResolvedPointer>, ToolError> {
    let sha = read_current(dataset_root, module, channel).await?;
    resolve_pointer(
        dataset_root,
        module,
        channel,
        CURRENT_POINTER,
        sha,
        target,
        bin,
    )
    .await
}

/// The outcome of a `set_current` pointer flip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetCurrentOutcome {
    /// The sha `current` now points to.
    pub sha: String,
    /// The sha `current` pointed to BEFORE this flip (the new rollback target).
    pub previous: Option<String>,
    /// False when `current` already pointed at `sha` — an idempotent no-op that
    /// left the history untouched.
    pub changed: bool,
}

/// Atomically move a channel's `current` pointer onto `sha`, recording the prior
/// value as the rollback target and appending a history entry. Idempotent: if
/// `current` already equals `sha` this is a no-op (no history churn). Writes the
/// `current.prev` (rollback) pointer BEFORE `current`, so a crash between the two
/// leaves a consistent (prev=old, current=old) state, never a dangling rollback.
///
/// VERIFY-BEFORE-BLESS (defense-in-depth): this SAFE PRIMITIVE runs
/// [`verify_sha_artifact`] on `sha` FIRST — before the idempotent short-circuit
/// and before any pointer write — so NO caller (present or future) can bless a sha
/// whose binary/`.sha256` is missing, corrupt, or checksum-mismatched. The flip is
/// refused (fail closed) on any verification failure. Callers must therefore pass
/// the `target`/`bin` that address the artifact.
///
/// TERM #565: this is ALSO the single choke point for the POINTER-TARGET
/// invariant — `target` must equal the module's [`DefaultTarget`], because the
/// pointer written here has no target component. Every pointer flip in the store
/// (`bless_build`, `promote`, `rollback_current`) routes through this function,
/// so an off-default target can never leave `current` aimed at an artifact
/// `compiler_release` cannot resolve.
#[allow(clippy::too_many_arguments)]
pub async fn set_current(
    dataset_root: &Path,
    module: &str,
    channel: &str,
    sha: &str,
    target: &str,
    default_target: &DefaultTarget,
    bin: &str,
    action: &str,
    from_channel: Option<&str>,
) -> Result<SetCurrentOutcome, ToolError> {
    // The pointer is target-agnostic: refuse BEFORE any verification or write.
    guard_pointer_target(module, channel, target, default_target)?;
    // Never move `current` onto an unverified sha — the pointer flip itself is the
    // choke point, independent of what the caller already checked.
    verify_sha_artifact(dataset_root, module, channel, sha, target, bin).await?;

    let previous = read_current(dataset_root, module, channel).await?;
    if previous.as_deref() == Some(sha) {
        return Ok(SetCurrentOutcome {
            sha: sha.to_string(),
            previous,
            changed: false,
        });
    }
    // Record the rollback target first (old current), then flip current — both
    // atomic. Order makes an interrupted flip fail safe.
    if let Some(old) = &previous {
        atomic_write(&previous_pointer_path(dataset_root, module, channel), old).await?;
    }
    atomic_write(&current_pointer_path(dataset_root, module, channel), sha).await?;
    append_history(
        dataset_root,
        module,
        channel,
        &HistoryEntry {
            at: chrono::Utc::now().to_rfc3339(),
            action: action.to_string(),
            sha: sha.to_string(),
            previous: previous.clone(),
            from_channel: from_channel.map(str::to_string),
        },
    )
    .await?;
    Ok(SetCurrentOutcome {
        sha: sha.to_string(),
        previous,
        changed: true,
    })
}

/// Roll a channel's `current` back to the previous blessed sha (`current.prev`).
/// Records the rollback as its own history entry, and (because it goes through
/// `set_current`) sets the now-old current as the new rollback target — so a
/// rollback is itself reversible. Errors if there is no previous sha to revert to.
///
/// FAIL-CLOSED: the rollback TARGET is verified with the SAME
/// [`verify_sha_artifact`] (binary + `.sha256` exist AND dir-name == actual
/// sha256 == sidecar sha) BEFORE the pointer moves — using the caller's
/// `target`/`bin`. If the previous sha's artifact is missing or corrupt (e.g. it
/// was pruned), the rollback is REFUSED with a clear error and `current` is left
/// untouched — a broken sha is never blessed.
pub async fn rollback_current(
    dataset_root: &Path,
    module: &str,
    channel: &str,
    target: &str,
    default_target: &DefaultTarget,
    bin: &str,
) -> Result<SetCurrentOutcome, ToolError> {
    // TERM #565: refuse an off-default target up front, so a rollback cannot even
    // begin to aim the target-agnostic pointer at an unresolvable artifact.
    guard_pointer_target(module, channel, target, default_target)?;
    let prev = read_previous(dataset_root, module, channel)
        .await?
        .ok_or_else(|| {
            ToolError::NotFound(format!(
                "no previous blessed sha recorded for {module}/{channel}; nothing to roll back to"
            ))
        })?;
    // TERM #565 (round-7): resolve the rollback target under the EFFECTIVE target
    // BEFORE verifying it, so a pointer stranded by a default-target change
    // reports that named cause instead of a bare missing-binary. Purely a better
    // diagnostic on an already-failing path — the fail-closed verification below
    // is unchanged, and nothing is repaired or substituted.
    resolve_pointer(
        dataset_root,
        module,
        channel,
        PREVIOUS_POINTER,
        Some(prev.clone()),
        target,
        bin,
    )
    .await?;
    // Explicit, clearly-attributed verification of the rollback target (in
    // addition to the check inside `set_current`), so the refusal names rollback.
    verify_sha_artifact(dataset_root, module, channel, &prev, target, bin)
        .await
        .map_err(|e| {
            ToolError::NotFound(format!(
                "refusing to roll {module}/{channel} back to {prev}: the rollback target is not \
                 a verified artifact (missing/corrupt): {e}"
            ))
        })?;
    set_current(
        dataset_root,
        module,
        channel,
        &prev,
        target,
        default_target,
        bin,
        "rollback",
        None,
    )
    .await
}

/// Recursively copy a directory tree (files + subdirs; no symlinks in an artifact
/// dir). Iterative (explicit stack) so it needs no async recursion. Used by
/// `promote` to give the destination channel its OWN copy of an already-built sha
/// (Rust-train: `stable` holds a copy of the blessed `experimental` sha, so
/// pruning `experimental` never strands `stable`).
async fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), ToolError> {
    let mut stack = vec![(src.to_path_buf(), dst.to_path_buf())];
    while let Some((s, d)) = stack.pop() {
        tokio::fs::create_dir_all(&d)
            .await
            .map_err(|e| ToolError::Execution(format!("mkdir {}: {e}", d.display())))?;
        let mut rd = tokio::fs::read_dir(&s)
            .await
            .map_err(|e| ToolError::Execution(format!("read_dir {}: {e}", s.display())))?;
        while let Some(entry) = rd
            .next_entry()
            .await
            .map_err(|e| ToolError::Execution(format!("read_dir entry {}: {e}", s.display())))?
        {
            let ft = entry
                .file_type()
                .await
                .map_err(|e| ToolError::Execution(format!("file_type: {e}")))?;
            let sp = entry.path();
            let dp = d.join(entry.file_name());
            if ft.is_dir() {
                stack.push((sp, dp));
            } else {
                tokio::fs::copy(&sp, &dp).await.map_err(|e| {
                    ToolError::Execution(format!("copy {} → {}: {e}", sp.display(), dp.display()))
                })?;
            }
        }
    }
    Ok(())
}

/// Prune old sha dirs in a channel, RETAINING the newest `retain` (never fewer
/// than 2) PLUS the sha dirs the `current` and `current.prev` pointer files
/// actually reference. Age is judged by sha-dir mtime. Returns the pruned shas.
/// Only immediate SUBDIRECTORIES are considered (the pointer/history/manifest
/// files are never touched).
///
/// The current/previous targets are read from the POINTER FILES at prune time —
/// never inferred from a caller-passed value — so an older sha that
/// `current.prev` still points to (e.g. after an idempotent re-bless whose
/// "previous" is the current sha) is always protected.
pub async fn prune_channel(
    dataset_root: &Path,
    module: &str,
    channel: &str,
    retain: usize,
) -> Result<Vec<String>, ToolError> {
    let retain = retain.max(2);
    let dir = channel_dir(dataset_root, module, channel);
    // Read the REAL pointer targets now, so retention protects them regardless of
    // their age or of any caller-supplied hint.
    let current = read_current(dataset_root, module, channel).await?;
    let previous = read_previous(dataset_root, module, channel).await?;
    let mut rd = match tokio::fs::read_dir(&dir).await {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(ToolError::Execution(format!(
                "read_dir {}: {e}",
                dir.display()
            )))
        }
    };
    let mut shas: Vec<(String, std::time::SystemTime)> = Vec::new();
    while let Some(entry) = rd
        .next_entry()
        .await
        .map_err(|e| ToolError::Execution(format!("read_dir entry {}: {e}", dir.display())))?
    {
        let ft = entry
            .file_type()
            .await
            .map_err(|e| ToolError::Execution(format!("file_type: {e}")))?;
        if !ft.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let mtime = entry
            .metadata()
            .await
            .ok()
            .and_then(|m| m.modified().ok())
            .unwrap_or(std::time::UNIX_EPOCH);
        shas.push((name, mtime));
    }
    // Newest first, so `take(retain)` keeps the most recent shas.
    shas.sort_by(|a, b| b.1.cmp(&a.1));
    // Protected set = the ACTUAL current + current.prev pointer targets, plus the
    // newest `retain` shas by mtime.
    let mut keep_set: HashSet<String> = HashSet::new();
    if let Some(c) = &current {
        keep_set.insert(c.clone());
    }
    if let Some(p) = &previous {
        keep_set.insert(p.clone());
    }
    for (name, _) in shas.iter().take(retain) {
        keep_set.insert(name.clone());
    }
    let mut pruned = Vec::new();
    for (name, _) in &shas {
        if keep_set.contains(name) {
            continue;
        }
        let victim = dir.join(name);
        tokio::fs::remove_dir_all(&victim)
            .await
            .map_err(|e| ToolError::Execution(format!("prune {}: {e}", victim.display())))?;
        pruned.push(name.clone());
    }
    Ok(pruned)
}

/// The outcome of a `compiler_release` promote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromoteOutcome {
    pub module: String,
    pub sha256: String,
    pub from_channel: String,
    pub to_channel: String,
    /// The destination `current` before this promote (the new rollback target).
    pub previous_current: Option<String>,
    /// True when the sha's artifact tree was copied into the destination channel
    /// (false when it was already present + verified there).
    pub copied: bool,
    /// True when the destination was ALREADY blessed at this sha — an idempotent
    /// no-op (no copy, no pointer move, no history entry).
    pub already_current: bool,
    /// sha dirs pruned from the destination channel by retention.
    pub pruned: Vec<String>,
    pub current_path: PathBuf,
}

/// Promote an already-built sha into `to_channel` by pointer flip — NO recompile
/// (Rust-train model). Steps, all fail-closed:
///   1. Verify the sha is a genuine build in `from_channel` (refuse an unbuilt sha).
///   2. If already blessed at this sha in `to_channel` (and it verifies) → no-op.
///   3. Give `to_channel` its own verified copy of the sha's artifact tree (skip
///      when `from == to`, or when a verified copy already exists).
///   4. Atomically flip `to_channel/current` onto the sha (records rollback +
///      history).
///   5. Prune the destination channel to the retention policy (keeps ≥2, plus the
///      new current + prior current).
#[allow(clippy::too_many_arguments)]
pub async fn promote(
    dataset_root: &Path,
    module: &str,
    from_channel: &str,
    to_channel: &str,
    sha: &str,
    target: &str,
    default_target: &DefaultTarget,
    bin: &str,
    retain: usize,
) -> Result<PromoteOutcome, ToolError> {
    // 0. TERM #565: the pointer-target invariant, checked BEFORE any artifact
    //    copy — a promote for an off-default target is refused outright rather
    //    than copying a tree it may never bless.
    guard_pointer_target(module, to_channel, target, default_target)?;
    // 1. Idempotent no-op FIRST — checked against the DESTINATION only. If the
    //    destination is already blessed at this sha AND its own copy verifies,
    //    re-promoting is a no-op regardless of the source: `stable` is independent
    //    from `experimental` after the copy, so the source `experimental/<sha>` may
    //    have since been pruned. Requiring the source here would wrongly fail a
    //    repeat promote of an already-released sha.
    let dest_verified = verify_sha_artifact(dataset_root, module, to_channel, sha, target, bin)
        .await
        .is_ok();
    let current = read_current(dataset_root, module, to_channel).await?;
    if current.as_deref() == Some(sha) && dest_verified {
        return Ok(PromoteOutcome {
            module: module.to_string(),
            sha256: sha.to_string(),
            from_channel: from_channel.to_string(),
            to_channel: to_channel.to_string(),
            previous_current: current,
            copied: false,
            already_current: true,
            pruned: Vec::new(),
            current_path: current_pointer_path(dataset_root, module, to_channel),
        });
    }

    // 2. The real promote path is fail-closed on the SOURCE: the sha MUST be a
    //    verified build in `from_channel` before we copy/bless it.
    verify_sha_artifact(dataset_root, module, from_channel, sha, target, bin)
        .await
        .map_err(|e| {
            ToolError::NotFound(format!(
                "refusing to promote {module}@{sha}: not a verified build in channel \
                 {from_channel}: {e}"
            ))
        })?;

    // 3. Ensure the destination channel has its own verified copy of the sha.
    let copied = if !dest_verified && from_channel != to_channel {
        let src = sha_dir(dataset_root, module, from_channel, sha);
        let dst = sha_dir(dataset_root, module, to_channel, sha);
        copy_dir_recursive(&src, &dst).await?;
        // Re-stamp the copied manifest so its `channel` reflects the destination.
        write_manifest(dataset_root, module, to_channel, sha, target, bin).await?;
        // Prove the copy is intact before we bless it.
        verify_sha_artifact(dataset_root, module, to_channel, sha, target, bin).await?;
        true
    } else {
        false
    };

    // 4. Atomic pointer flip (records rollback target + history).
    let action = if from_channel == to_channel {
        "bless"
    } else {
        "promote"
    };
    let set = set_current(
        dataset_root,
        module,
        to_channel,
        sha,
        target,
        default_target,
        bin,
        action,
        Some(from_channel),
    )
    .await?;

    // 5. Retention: keep ≥2 plus the ACTUAL current + current.prev pointer targets
    //    (prune reads the pointer files itself, so an older rollback target is safe).
    let pruned = prune_channel(dataset_root, module, to_channel, retain).await?;

    Ok(PromoteOutcome {
        module: module.to_string(),
        sha256: sha.to_string(),
        from_channel: from_channel.to_string(),
        to_channel: to_channel.to_string(),
        previous_current: set.previous,
        copied,
        already_current: false,
        pruned,
        current_path: current_pointer_path(dataset_root, module, to_channel),
    })
}

/// The outcome of a build-time bless.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildBlessOutcome {
    pub channel: String,
    /// False when `current` already pointed at this sha (idempotent no-op).
    pub blessed: bool,
    pub pruned: Vec<String>,
}

/// Bless a JUST-BUILT sha as `<channel>/current` on a local publish — writing the
/// per-sha manifest, flipping the pointer (verify-before-bless via
/// [`set_current`]), and pruning to retention.
///
/// RELEASE DISCIPLINE: a build may bless ONLY the build/experimental channel. A
/// promote-only channel (`stable`) is REFUSED — a rebuild must never write
/// `stable/current`; the sole path to stable is `compiler_release`
/// (promote-by-copy, no recompile). This guard is the structural guarantee that
/// the CURRENT-pointer bless at build time is experimental-only, independent of
/// whatever channel a caller might route the build into.
#[allow(clippy::too_many_arguments)]
pub async fn bless_build(
    dataset_root: &Path,
    module: &str,
    channel: &str,
    sha: &str,
    target: &str,
    default_target: &DefaultTarget,
    bin: &str,
    retain: usize,
) -> Result<BuildBlessOutcome, ToolError> {
    // TERM #565: an off-default-target build never blesses. Checked here too (in
    // addition to `set_current`) so the refusal happens before the manifest write.
    guard_pointer_target(module, channel, target, default_target)?;
    if is_promote_only_channel(channel) {
        return Err(ToolError::InvalidArgument(format!(
            "compiler_build may not bless the promote-only channel {channel:?}; a build blesses \
             only {BUILD_CHANNEL} — use compiler_release to promote a built sha to {channel}"
        )));
    }
    write_manifest(dataset_root, module, channel, sha, target, bin).await?;
    // set_current verifies the just-published sha before flipping (fail-closed).
    let set = set_current(
        dataset_root,
        module,
        channel,
        sha,
        target,
        default_target,
        bin,
        "bless",
        None,
    )
    .await?;
    // Prune reads the real current + current.prev pointers itself.
    let pruned = prune_channel(dataset_root, module, channel, retain).await?;
    Ok(BuildBlessOutcome {
        channel: channel.to_string(),
        blessed: set.changed,
        pruned,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_of_known_input() {
        // The canonical SHA-256("abc").
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // Empty input.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[tokio::test]
    async fn sha256_file_matches_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("blob.bin");
        let data = b"terminus-artifact-payload-v1";
        tokio::fs::write(&p, data).await.unwrap();
        assert_eq!(sha256_file(&p).await.unwrap(), sha256_hex(data));
    }

    #[test]
    fn artifact_layout_is_content_addressed() {
        let root = Path::new("/data/build");
        let p = artifact_abs_path(
            root,
            "chord",
            "experimental",
            "deadbeef",
            "x86_64-unknown-linux-musl",
            "chord",
        );
        assert_eq!(
            p,
            PathBuf::from(
                "/data/build/artifacts/chord/experimental/deadbeef/x86_64-unknown-linux-musl/chord"
            )
        );
    }

    #[test]
    fn sidecar_is_sha256sum_format() {
        assert_eq!(sidecar_contents("abc123", "chord"), "abc123  chord\n");
    }

    #[tokio::test]
    async fn publish_local_writes_artifact_and_sidecar_with_matching_sha() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("dataset");
        tokio::fs::create_dir_all(&root).await.unwrap();

        // A fake built binary.
        let src_dir = dir.path().join("src");
        tokio::fs::create_dir_all(&src_dir).await.unwrap();
        let bin = src_dir.join("mymod");
        let payload = b"ELF...pretend-binary...";
        tokio::fs::write(&bin, payload).await.unwrap();

        let pub_ = publish_local(
            &root,
            "mymod",
            "experimental",
            "x86_64-unknown-linux-musl",
            "mymod",
            &bin,
        )
        .await
        .unwrap();

        // Sha matches the payload.
        assert_eq!(pub_.sha256, sha256_hex(payload));
        assert!(!pub_.relayed);

        // Artifact copied to the content-addressed path.
        let copied = tokio::fs::read(&pub_.artifact_path).await.unwrap();
        assert_eq!(copied, payload);
        assert!(pub_.artifact_path.to_string_lossy().contains(&pub_.sha256));

        // Sidecar has the sha256sum-format line and its sha matches the file.
        let sidecar = tokio::fs::read_to_string(&pub_.sha256_path).await.unwrap();
        assert_eq!(sidecar, format!("{}  mymod\n", pub_.sha256));
        assert_eq!(sha256_file(&pub_.artifact_path).await.unwrap(), pub_.sha256);
    }

    #[tokio::test]
    async fn publish_local_does_not_write_a_current_pointer() {
        // BLD-05 must NOT flip `current` (that is BLD-07). Assert no `current`
        // file appears anywhere under the channel dir after a publish.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("dataset");
        tokio::fs::create_dir_all(&root).await.unwrap();
        let bin = dir.path().join("bin");
        tokio::fs::write(&bin, b"x").await.unwrap();

        publish_local(&root, "m", "experimental", "t", "bin", &bin)
            .await
            .unwrap();

        let channel_dir = root.join("artifacts/m/experimental");
        let mut found_current = false;
        let mut stack = vec![channel_dir];
        while let Some(d) = stack.pop() {
            let mut rd = tokio::fs::read_dir(&d).await.unwrap();
            while let Some(e) = rd.next_entry().await.unwrap() {
                if e.file_name() == "current" {
                    found_current = true;
                }
                if e.file_type().await.unwrap().is_dir() {
                    stack.push(e.path());
                }
            }
        }
        assert!(
            !found_current,
            "publish must not create a `current` pointer"
        );
    }

    #[test]
    fn relay_argv_targets_remote_content_path() {
        let argv = render_relay_argv(
            "builduser@relay",
            "/data/build",
            "chord",
            "experimental",
            "abcd",
            "x86_64-unknown-linux-musl",
            "chord",
            Path::new("/tmp/out/chord"),
        );
        let j = argv.join(" ");
        assert!(argv[0] == "rsync");
        assert!(
            argv.contains(&"-s".to_string()),
            "relay rsync must protect remote path args"
        );
        assert!(j.contains("/tmp/out/chord"));
        assert!(j.contains(
            "builduser@relay:/data/build/artifacts/chord/experimental/abcd/x86_64-unknown-linux-musl/chord"
        ));
    }

    #[test]
    fn relay_plan_delivers_both_binary_and_sha256_sidecar() {
        let plan = render_relay_plan(
            "builduser@relay",
            "/data/build/",
            "chord",
            "experimental",
            "abcd",
            "x86_64-unknown-linux-musl",
            "chord",
            Path::new("/tmp/out/chord"),
            Path::new("/tmp/out/chord.sha256"),
        );
        let base = "builduser@relay:/data/build/artifacts/chord/experimental/abcd/x86_64-unknown-linux-musl";
        // The binary relay targets `<...>/chord`.
        let bj = plan.binary_argv.join(" ");
        assert!(bj.contains(&format!("{base}/chord")), "binary dest: {bj}");
        assert!(bj.contains("/tmp/out/chord"));
        // The sidecar relay targets `<...>/chord.sha256` — the required, previously
        // missing, verifiable companion.
        let sj = plan.sidecar_argv.join(" ");
        assert!(
            sj.contains(&format!("{base}/chord.sha256")),
            "sidecar dest: {sj}"
        );
        assert!(sj.contains("/tmp/out/chord.sha256"));
        // The sidecar body is the sha256sum-format line for the binary.
        assert_eq!(plan.sidecar_body, "abcd  chord\n");
        // Reported remote paths match the content-addressed layout, sidecar next
        // to the binary.
        assert_eq!(
            plan.remote_binary,
            PathBuf::from(
                "/data/build/artifacts/chord/experimental/abcd/x86_64-unknown-linux-musl/chord"
            )
        );
        assert_eq!(
            plan.remote_sidecar,
            PathBuf::from("/data/build/artifacts/chord/experimental/abcd/x86_64-unknown-linux-musl/chord.sha256")
        );
    }

    // ── BLD-07 store tests ──────────────────────────────────────────────────

    const T: &str = "x86_64-unknown-linux-musl";

    /// The module DEFAULT target for these tests — equal to `T`, so every
    /// pre-existing test builds and blesses the module's own default target and
    /// the TERM #565 pointer-target guard is a no-op for them. The mismatch case
    /// has its own dedicated test below.
    fn dt() -> DefaultTarget {
        DefaultTarget::from_raw_for_test(T)
    }

    /// Deterministically set a path's mtime (and atime) to `secs` since the epoch
    /// via `utimes(2)`, so retention's newest-first ordering is testable without a
    /// wall-clock race (no extra crate; `libc` is already a dep).
    fn set_mtime(path: &Path, secs: i64) {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        let tv = libc::timeval {
            tv_sec: secs as libc::time_t,
            tv_usec: 0,
        };
        let times = [tv, tv];
        // Safe: `c` outlives the call; `times` is a valid 2-element array.
        unsafe {
            libc::utimes(c.as_ptr(), times.as_ptr());
        }
    }

    /// Publish a fake binary with `payload` into `channel` and return its sha.
    async fn seed_artifact(
        root: &Path,
        module: &str,
        channel: &str,
        bin: &str,
        payload: &[u8],
    ) -> String {
        seed_artifact_for_target(root, module, channel, T, bin, payload).await
    }

    /// Same, for an explicit target triple (so the off-default-target case has a
    /// GENUINE, fully-verifiable artifact on disk — the guard must refuse it for
    /// being off-default, not for being missing or corrupt).
    async fn seed_artifact_for_target(
        root: &Path,
        module: &str,
        channel: &str,
        target: &str,
        bin: &str,
        payload: &[u8],
    ) -> String {
        let dir = tempfile::tempdir().unwrap();
        let built = dir.path().join(bin);
        tokio::fs::write(&built, payload).await.unwrap();
        let p = publish_local(root, module, channel, target, bin, &built)
            .await
            .unwrap();
        p.sha256
    }

    #[tokio::test]
    async fn set_current_is_atomic_via_temp_rename_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let sha = seed_artifact(root, "m", "experimental", "m", b"v1").await;

        let out = set_current(root, "m", "experimental", &sha, T, &dt(), "m", "bless", None)
            .await
            .unwrap();
        assert!(out.changed);
        assert_eq!(out.previous, None);
        // Pointer file holds exactly the sha.
        let cur = tokio::fs::read_to_string(current_pointer_path(root, "m", "experimental"))
            .await
            .unwrap();
        assert_eq!(cur, sha);
        assert_eq!(read_current(root, "m", "experimental").await.unwrap(), Some(sha));
        // No leftover temp file from the atomic write.
        let chan = channel_dir(root, "m", "experimental");
        let mut rd = tokio::fs::read_dir(&chan).await.unwrap();
        while let Some(e) = rd.next_entry().await.unwrap() {
            let n = e.file_name().to_string_lossy().to_string();
            assert!(!n.starts_with(".current"), "stray temp pointer left: {n}");
        }
    }

    #[tokio::test]
    async fn set_current_is_idempotent_no_history_churn() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let sha = seed_artifact(root, "m", "experimental", "m", b"v1").await;

        let a = set_current(root, "m", "experimental", &sha, T, &dt(), "m", "bless", None)
            .await
            .unwrap();
        assert!(a.changed);
        let b = set_current(root, "m", "experimental", &sha, T, &dt(), "m", "bless", None)
            .await
            .unwrap();
        assert!(!b.changed, "re-blessing the same sha must be a no-op");
        // Exactly ONE history entry despite two calls.
        let hist = read_history(root, "m", "experimental").await.unwrap();
        assert_eq!(hist.len(), 1);
        assert_eq!(hist[0].sha, sha);
    }

    #[tokio::test]
    async fn current_query_returns_blessed_sha_and_none_when_unset() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert_eq!(read_current(root, "m", "stable").await.unwrap(), None);
        let sha = seed_artifact(root, "m", "stable", "m", b"blob").await;
        set_current(root, "m", "stable", &sha, T, &dt(), "m", "bless", None)
            .await
            .unwrap();
        assert_eq!(read_current(root, "m", "stable").await.unwrap(), Some(sha));
    }

    #[tokio::test]
    async fn verify_before_bless_rejects_missing_mismatch_and_bad_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // Never-built sha → missing.
        assert!(verify_sha_artifact(root, "m", "experimental", "deadbeef", T, "m")
            .await
            .is_err());
        // A genuine build verifies.
        let sha = seed_artifact(root, "m", "experimental", "m", b"payload").await;
        verify_sha_artifact(root, "m", "experimental", &sha, T, "m")
            .await
            .unwrap();
        // Corrupt the binary under its content-address dir → sha mismatch.
        let binp = artifact_abs_path(root, "m", "experimental", &sha, T, "m");
        tokio::fs::write(&binp, b"tampered").await.unwrap();
        assert!(verify_sha_artifact(root, "m", "experimental", &sha, T, "m")
            .await
            .is_err());
    }

    #[tokio::test]
    async fn promote_refuses_unbuilt_sha_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let err = promote(root, "m", "experimental", "stable", "notbuilt", T, &dt(), "m", 2)
            .await
            .unwrap_err();
        assert!(format!("{err:?}").contains("refusing to promote"));
        // Nothing was blessed in stable.
        assert_eq!(read_current(root, "m", "stable").await.unwrap(), None);
    }

    #[tokio::test]
    async fn promote_flips_stable_by_copy_no_rebuild_and_verifies() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let sha = seed_artifact(root, "chord", "experimental", "chord", b"chord-bin-v7").await;

        let out = promote(root, "chord", "experimental", "stable", &sha, T, &dt(), "chord", 2)
            .await
            .unwrap();
        assert!(out.copied, "stable must get its own copy of the sha tree");
        assert!(!out.already_current);
        // stable/current now points at the promoted sha…
        assert_eq!(
            read_current(root, "chord", "stable").await.unwrap(),
            Some(sha.clone())
        );
        // …and the destination artifact verifies (copied, not rebuilt).
        verify_sha_artifact(root, "chord", "stable", &sha, T, "chord")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn promote_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let sha = seed_artifact(root, "m", "experimental", "m", b"v1").await;
        let first = promote(root, "m", "experimental", "stable", &sha, T, &dt(), "m", 2)
            .await
            .unwrap();
        assert!(first.copied && !first.already_current);
        let second = promote(root, "m", "experimental", "stable", &sha, T, &dt(), "m", 2)
            .await
            .unwrap();
        assert!(second.already_current, "second promote must be a no-op");
        assert!(!second.copied);
        // Only one promote in the history.
        let hist = read_history(root, "m", "stable").await.unwrap();
        assert_eq!(hist.iter().filter(|h| h.action == "promote").count(), 1);
    }

    #[tokio::test]
    async fn promote_is_idempotent_even_after_source_is_pruned() {
        // Once a sha is released, `stable` holds its own copy and is independent of
        // `experimental`. Re-promoting the already-blessed sha must be a no-op even
        // if the source `experimental/<sha>` tree has since been pruned — NOT an
        // error. (The dest-already-blessed check must precede the source check.)
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let sha = seed_artifact(root, "m", "experimental", "m", b"v1").await;
        let first = promote(root, "m", "experimental", "stable", &sha, T, &dt(), "m", 2)
            .await
            .unwrap();
        assert!(first.copied && !first.already_current);

        // Prune the SOURCE copy entirely (simulates experimental retention rolling
        // past this sha). stable/current still points at the verified dest copy.
        tokio::fs::remove_dir_all(sha_dir(root, "m", "experimental", &sha))
            .await
            .unwrap();
        assert!(
            !tokio::fs::try_exists(sha_dir(root, "m", "experimental", &sha))
                .await
                .unwrap()
        );

        let repeat = promote(root, "m", "experimental", "stable", &sha, T, &dt(), "m", 2)
            .await
            .expect("re-promote of an already-released sha must succeed, not error");
        assert!(repeat.already_current, "must be an idempotent no-op");
        assert!(!repeat.copied);
        // Still exactly one promote recorded (no churn).
        let hist = read_history(root, "m", "stable").await.unwrap();
        assert_eq!(hist.iter().filter(|h| h.action == "promote").count(), 1);
        // And the destination is still blessed + verifiable.
        assert_eq!(read_current(root, "m", "stable").await.unwrap(), Some(sha.clone()));
        verify_sha_artifact(root, "m", "stable", &sha, T, "m")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn bless_build_blesses_experimental_and_never_stable() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let sha = seed_artifact(root, "m", "experimental", "m", b"v1").await;

        let out = bless_build(root, "m", "experimental", &sha, T, &dt(), "m", 2)
            .await
            .unwrap();
        assert!(out.blessed);
        assert_eq!(out.channel, "experimental");
        // experimental/current is flipped …
        assert_eq!(
            read_current(root, "m", "experimental").await.unwrap(),
            Some(sha.clone())
        );
        // … and stable/current is NEVER touched by a build.
        assert_eq!(read_current(root, "m", "stable").await.unwrap(), None);
    }

    #[tokio::test]
    async fn bless_build_refuses_the_promote_only_stable_channel() {
        // Even if a build were routed at channel=stable, blessing stable/current
        // via a rebuild is refused (stable is compiler_release-only). The artifact
        // may exist under stable/<sha>, but the CURRENT pointer must not move.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let sha = seed_artifact(root, "m", "stable", "m", b"v1").await;

        let err = bless_build(root, "m", "stable", &sha, T, &dt(), "m", 2)
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidArgument(_)),
            "a build must not bless the stable channel: {err:?}"
        );
        assert!(
            format!("{err:?}").contains("compiler_release"),
            "the error should point at compiler_release: {err:?}"
        );
        // stable/current was NOT blessed by the build.
        assert_eq!(read_current(root, "m", "stable").await.unwrap(), None);
        assert!(is_promote_only_channel("stable"));
        assert!(!is_promote_only_channel("experimental"));
    }

    #[tokio::test]
    async fn rollback_reverts_to_previous_and_history_records_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let sha1 = seed_artifact(root, "m", "experimental", "m", b"one").await;
        let sha2 = seed_artifact(root, "m", "experimental", "m", b"two").await;
        assert_ne!(sha1, sha2);

        promote(root, "m", "experimental", "stable", &sha1, T, &dt(), "m", 2)
            .await
            .unwrap();
        promote(root, "m", "experimental", "stable", &sha2, T, &dt(), "m", 2)
            .await
            .unwrap();
        assert_eq!(read_current(root, "m", "stable").await.unwrap(), Some(sha2.clone()));
        assert_eq!(read_previous(root, "m", "stable").await.unwrap(), Some(sha1.clone()));

        let rb = rollback_current(root, "m", "stable", T, &dt(), "m").await.unwrap();
        assert!(rb.changed);
        assert_eq!(read_current(root, "m", "stable").await.unwrap(), Some(sha1.clone()));
        // Rollback is itself reversible: prev now points at sha2.
        assert_eq!(read_previous(root, "m", "stable").await.unwrap(), Some(sha2));
        let hist = read_history(root, "m", "stable").await.unwrap();
        assert_eq!(hist.last().unwrap().action, "rollback");
    }

    #[tokio::test]
    async fn rollback_with_no_previous_errors() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert!(rollback_current(root, "m", "stable", T, &dt(), "m").await.is_err());
    }

    #[tokio::test]
    async fn rollback_to_missing_or_corrupt_prev_is_refused_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let sha1 = seed_artifact(root, "m", "experimental", "m", b"one").await;
        let sha2 = seed_artifact(root, "m", "experimental", "m", b"two").await;
        promote(root, "m", "experimental", "stable", &sha1, T, &dt(), "m", 2)
            .await
            .unwrap();
        promote(root, "m", "experimental", "stable", &sha2, T, &dt(), "m", 2)
            .await
            .unwrap();
        // current=sha2, current.prev=sha1.
        assert_eq!(read_previous(root, "m", "stable").await.unwrap(), Some(sha1.clone()));

        // (a) Corrupt the rollback target's binary under stable → verify fails.
        let binp = artifact_abs_path(root, "m", "stable", &sha1, T, "m");
        tokio::fs::write(&binp, b"tampered").await.unwrap();
        let err = rollback_current(root, "m", "stable", T, &dt(), "m")
            .await
            .unwrap_err();
        assert!(
            format!("{err:?}").contains("refusing to roll"),
            "corrupt rollback target must be refused: {err:?}"
        );
        // current is UNTOUCHED — the broken sha was never blessed.
        assert_eq!(read_current(root, "m", "stable").await.unwrap(), Some(sha2.clone()));

        // (b) Delete the rollback target's tree entirely → still refused.
        tokio::fs::remove_dir_all(sha_dir(root, "m", "stable", &sha1))
            .await
            .unwrap();
        assert!(rollback_current(root, "m", "stable", T, &dt(), "m").await.is_err());
        assert_eq!(read_current(root, "m", "stable").await.unwrap(), Some(sha2));
    }

    #[tokio::test]
    async fn set_current_refuses_an_unverified_target() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // A sha whose artifact was never published → the pointer flip is refused.
        let err = set_current(root, "m", "experimental", "deadbeef", T, &dt(), "m", "bless", None)
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::NotFound(_)),
            "unverified target must be refused: {err:?}"
        );
        assert_eq!(read_current(root, "m", "experimental").await.unwrap(), None);
    }

    #[tokio::test]
    async fn idempotent_rebless_does_not_prune_the_real_current_prev_target() {
        // Regression: an idempotent set_current returns previous == the CURRENT sha,
        // NOT the current.prev file's target. Prune must read the pointer FILES, so
        // an OLDER sha that current.prev still references is protected.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // Three shas with strictly increasing mtimes: sha1 (oldest) .. sha3 (newest).
        let mut shas = Vec::new();
        for i in 0..3u8 {
            let sha = seed_artifact(root, "m", "experimental", "m", &[b'p', i]).await;
            set_mtime(&sha_dir(root, "m", "experimental", &sha), 5_000 + i as i64);
            shas.push(sha);
        }
        // Bless sha1 then sha3 → current=sha3, current.prev=sha1 (the OLDEST).
        set_current(root, "m", "experimental", &shas[0], T, &dt(), "m", "bless", None)
            .await
            .unwrap();
        set_current(root, "m", "experimental", &shas[2], T, &dt(), "m", "bless", None)
            .await
            .unwrap();
        assert_eq!(
            read_previous(root, "m", "experimental").await.unwrap(),
            Some(shas[0].clone())
        );
        // Idempotent re-bless of the current sha (its `previous` return is sha3).
        let set = set_current(root, "m", "experimental", &shas[2], T, &dt(), "m", "bless", None)
            .await
            .unwrap();
        assert!(!set.changed);
        // Prune retain=2: newest-2 = {sha3, sha2}. WITHOUT reading current.prev,
        // sha1 (oldest, and the rollback target) would be pruned — the bug.
        let pruned = prune_channel(root, "m", "experimental", 2).await.unwrap();
        assert!(
            !pruned.contains(&shas[0]),
            "current.prev target (sha1) must NOT be pruned; pruned={pruned:?}"
        );
        assert!(
            tokio::fs::try_exists(sha_dir(root, "m", "experimental", &shas[0]))
                .await
                .unwrap(),
            "the rollback target sha dir must survive"
        );
        // And a rollback to it still succeeds (proves it stayed verifiable).
        let rb = rollback_current(root, "m", "experimental", T, &dt(), "m")
            .await
            .unwrap();
        assert_eq!(rb.sha, shas[0]);
    }

    #[tokio::test]
    async fn retention_keeps_at_least_two_and_prunes_older() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // Three distinct shas, blessed in order so mtimes strictly increase.
        let mut shas = Vec::new();
        for i in 0..3u8 {
            let sha = seed_artifact(root, "m", "experimental", "m", &[b'a', i]).await;
            // Ensure a distinct, increasing mtime ordering for the sha dirs.
            let d = sha_dir(root, "m", "experimental", &sha);
            set_mtime(&d, 1_000 + i as i64);
            set_current(root, "m", "experimental", &sha, T, &dt(), "m", "bless", None)
                .await
                .unwrap();
            shas.push(sha);
        }
        // retain=2 → keep newest 2 (shas[1], shas[2]) + current(shas[2]) + prev(shas[1]).
        // The oldest (shas[0]) is pruned.
        let pruned = prune_channel(root, "m", "experimental", 2)
            .await
            .unwrap();
        assert_eq!(pruned, vec![shas[0].clone()]);
        assert!(!tokio::fs::try_exists(sha_dir(root, "m", "experimental", &shas[0]))
            .await
            .unwrap());
        assert!(tokio::fs::try_exists(sha_dir(root, "m", "experimental", &shas[1]))
            .await
            .unwrap());
        assert!(tokio::fs::try_exists(sha_dir(root, "m", "experimental", &shas[2]))
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn prune_never_drops_below_two_even_with_retain_one() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut shas = Vec::new();
        for i in 0..3u8 {
            let sha = seed_artifact(root, "m", "experimental", "m", &[b'z', i]).await;
            let d = sha_dir(root, "m", "experimental", &sha);
            set_mtime(&d, 2_000 + i as i64);
            shas.push(sha);
        }
        // retain=1 is clamped up to 2 → two survive.
        prune_channel(root, "m", "experimental", 1).await.unwrap();
        let mut surviving = 0;
        let mut rd = tokio::fs::read_dir(channel_dir(root, "m", "experimental"))
            .await
            .unwrap();
        while let Some(e) = rd.next_entry().await.unwrap() {
            if e.file_type().await.unwrap().is_dir() {
                surviving += 1;
            }
        }
        assert_eq!(surviving, 2, "retention floor of 2 must hold");
    }

    #[tokio::test]
    async fn manifest_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let sha = seed_artifact(root, "chord", "experimental", "chord", b"bin").await;
        let written = write_manifest(root, "chord", "experimental", &sha, T, "chord")
            .await
            .unwrap();
        let read = read_manifest(root, "chord", "experimental", &sha)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(written, read);
        assert_eq!(read.module, "chord");
        assert_eq!(read.channel, "experimental");
        assert_eq!(read.sha256, sha);
        assert!(read.artifact_rel.contains(&sha));
        assert!(read.sha256_rel.ends_with("chord.sha256"));
    }
    // ── TERM #565 (round-2 review finding 1): the POINTER-TARGET invariant ────
    //
    // The artifact is target-addressed (`<sha>/<target>/<bin>`); the `current`
    // pointer is NOT (`<channel>/current`, keyed by module+channel only). So the
    // ONLY target whose artifact `current` may be aimed at is the module's
    // effective default — the one `compiler_release` resolves. Anything else
    // leaves the live pointer on a sha whose artifact release cannot find.
    //
    // MUTATION CHECK for a future reader: delete the `guard_pointer_target` call
    // in `set_current` (and the two early ones in `promote` / `bless_build`) and
    // EVERY assertion below flips — the calls succeed and `current` is written.

    /// The off-default target here is a REAL, fully-verifiable artifact (seeded
    /// under its own target dir, correct sha + sidecar), so the refusal can only
    /// be the pointer-target invariant — not verify-before-bless doing the work.
    const OFF: &str = "x86_64-unknown-linux-gnu";

    #[tokio::test]
    async fn bless_build_refuses_an_off_default_target_and_leaves_current_unset() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        assert_ne!(OFF, T, "the fixture must actually be off-default");

        let sha = seed_artifact_for_target(root, "m", "experimental", OFF, "m", b"native").await;
        // Sanity: the artifact really is complete and verifiable at OFF.
        verify_sha_artifact(root, "m", "experimental", &sha, OFF, "m")
            .await
            .expect("the off-default artifact is genuine — only its TARGET is wrong");

        let err = bless_build(root, "m", "experimental", &sha, OFF, &dt(), "m", 2)
            .await
            .expect_err("an off-default-target build must never bless current");
        assert!(
            matches!(err, ToolError::InvalidArgument(_)),
            "expected InvalidArgument, got {err:?}"
        );

        // THE invariant: the live pointer was not created/moved.
        assert_eq!(
            read_current(root, "m", "experimental").await.unwrap(),
            None,
            "current must not exist after a refused off-default bless"
        );
        assert!(
            !tokio::fs::try_exists(&current_pointer_path(root, "m", "experimental"))
                .await
                .unwrap(),
            "no current pointer file may be written at all"
        );
    }

    #[tokio::test]
    async fn set_current_refuses_an_off_default_target_and_does_not_disturb_the_pointer() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // A legitimate default-target release is already live.
        let good = seed_artifact(root, "m", "experimental", "m", b"default").await;
        bless_build(root, "m", "experimental", &good, T, &dt(), "m", 2)
            .await
            .unwrap();
        assert_eq!(
            read_current(root, "m", "experimental").await.unwrap().as_deref(),
            Some(good.as_str())
        );

        // Now an off-default-target build tries to take the pointer.
        let off = seed_artifact_for_target(root, "m", "experimental", OFF, "m", b"native").await;
        assert_ne!(off, good);
        let err = set_current(root, "m", "experimental", &off, OFF, &dt(), "m", "bless", None)
            .await
            .expect_err("set_current is the choke point — it must refuse");
        assert!(matches!(err, ToolError::InvalidArgument(_)), "{err:?}");

        // The previously-good release is untouched — no hijack, no dangling prev.
        assert_eq!(
            read_current(root, "m", "experimental").await.unwrap().as_deref(),
            Some(good.as_str()),
            "the live pointer must still name the DEFAULT-target artifact"
        );
        assert_eq!(read_previous(root, "m", "experimental").await.unwrap(), None);
    }

    #[tokio::test]
    async fn promote_and_rollback_refuse_an_off_default_target() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        let off = seed_artifact_for_target(root, "m", "experimental", OFF, "m", b"native").await;
        let err = promote(root, "m", "experimental", "stable", &off, OFF, &dt(), "m", 2)
            .await
            .expect_err("promote must refuse an off-default target");
        assert!(matches!(err, ToolError::InvalidArgument(_)), "{err:?}");
        assert_eq!(read_current(root, "m", "stable").await.unwrap(), None);
        // Refused BEFORE any copy — stable never got a tree.
        assert!(
            !tokio::fs::try_exists(&sha_dir(root, "m", "stable", &off))
                .await
                .unwrap(),
            "a refused promote must not have copied the artifact tree"
        );

        let err = rollback_current(root, "m", "stable", OFF, &dt(), "m")
            .await
            .expect_err("rollback must refuse an off-default target");
        assert!(matches!(err, ToolError::InvalidArgument(_)), "{err:?}");
    }

    /// The default-target path is UNCHANGED — without this, a guard hardwired to
    /// refuse everything would pass the three tests above.
    #[tokio::test]
    async fn the_module_default_target_still_blesses_normally() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let sha = seed_artifact(root, "m", "experimental", "m", b"default").await;
        let out = bless_build(root, "m", "experimental", &sha, T, &dt(), "m", 2)
            .await
            .unwrap();
        assert!(out.blessed);
        assert_eq!(
            read_current(root, "m", "experimental").await.unwrap().as_deref(),
            Some(sha.as_str())
        );
    }

    /// `DefaultTarget::for_module` is the ONLY production constructor, and it
    /// resolves through the single resolution point (`effective_triple`) — so a
    /// caller cannot pass its own target as its own "default" and defeat the guard.
    #[test]
    fn default_target_for_module_matches_the_single_resolution_point() {
        let m = "a-module-with-no-configured-target-xyz";
        assert_eq!(
            DefaultTarget::for_module(m).as_str(),
            super::super::effective_triple(m)
        );
    }

    // ── TERM #565 round-7: read-side detection of the config-door hazard ──────
    //
    // The write door is guarded (above). These cover the CONFIG door: a pointer
    // blessed for target A while the module's effective target is later changed
    // to B out of band. Nothing re-runs the write guard, so the read side must
    // detect it and say WHY — without guessing or falling back to A.

    /// Pointer set for target A; effective target is then B; no artifact exists
    /// under B ⇒ resolving the live pointer returns a diagnostic that NAMES the
    /// sha, the resolved target, the path checked, the target it was actually
    /// built for, and the config knobs that are the likely cause.
    #[tokio::test]
    async fn resolve_current_diagnoses_a_pointer_stranded_by_a_default_target_change() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // Blessed legitimately under target A (== the module default at the time).
        let sha = seed_artifact(root, "m", "experimental", "m", b"built-for-A").await;
        bless_build(root, "m", "experimental", &sha, T, &dt(), "m", 2)
            .await
            .unwrap();

        // Config door: the module's effective target is now OFF (B). The pointer
        // is untouched, so no write-time guard ran.
        let err = resolve_current(root, "m", "experimental", OFF, "m")
            .await
            .expect_err("a pointer that resolves to nothing under B must be an error");
        let msg = format!("{err:?}");
        assert!(matches!(err, ToolError::NotFound(_)), "{msg}");
        let checked = artifact_abs_path(root, "m", "experimental", &sha, OFF, "m")
            .display()
            .to_string();
        assert!(
            msg.contains(&checked),
            "the diagnostic must name the path it checked: {msg}"
        );
        // The sha and the target must be named IN THEIR OWN RIGHT — the checked
        // path happens to contain both as components, so assert against the
        // message with that path elided, or a diagnostic that only echoed a path
        // would pass.
        let prose = msg.replace(&checked, "<CHECKED-PATH>");
        assert!(
            prose.contains(&sha),
            "the diagnostic must name the sha, not only embed it in the path: {msg}"
        );
        assert!(
            prose.contains(OFF),
            "the diagnostic must name the RESOLVED target: {msg}"
        );
        assert!(
            prose.contains(T),
            "the diagnostic must name the target the sha WAS built for: {msg}"
        );
        assert!(
            msg.contains("BUILD_MODULE_TARGET_") && msg.contains("BUILD_TARGET_TRIPLE"),
            "the diagnostic must name the likely cause (the target config): {msg}"
        );

        // Detection only — nothing repaired, nothing substituted.
        assert_eq!(
            read_current(root, "m", "experimental").await.unwrap().as_deref(),
            Some(sha.as_str()),
            "the detector must not touch the pointer"
        );
    }

    /// POSITIVE CONTROL — the normal case still resolves. Without this, a
    /// detector hardwired to always error would pass the test above.
    #[tokio::test]
    async fn resolve_current_resolves_normally_under_the_effective_target() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        let sha = seed_artifact(root, "m", "experimental", "m", b"built-for-A").await;
        bless_build(root, "m", "experimental", &sha, T, &dt(), "m", 2)
            .await
            .unwrap();

        let resolved = resolve_current(root, "m", "experimental", T, "m")
            .await
            .expect("the pointer resolves under the module's own default target")
            .expect("a blessed channel resolves to Some");
        assert_eq!(resolved.sha, sha);
        assert_eq!(resolved.target, T);
        assert_eq!(
            resolved.artifact_path,
            artifact_abs_path(root, "m", "experimental", &sha, T, "m")
        );
        assert!(tokio::fs::try_exists(&resolved.artifact_path).await.unwrap());

        // And an unblessed channel is `None`, not an error.
        assert!(resolve_current(root, "m", "stable", T, "m")
            .await
            .unwrap()
            .is_none());
    }

    /// The other read of a live release pointer: `rollback` resolves
    /// `current.prev`. A prev stranded by the same config change reports the same
    /// named cause instead of a bare missing-binary, and `current` is untouched.
    #[tokio::test]
    async fn rollback_diagnoses_a_prev_pointer_stranded_by_a_default_target_change() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // Two blesses under target A, so `current.prev` names the first.
        let one = seed_artifact(root, "m", "experimental", "m", b"one").await;
        bless_build(root, "m", "experimental", &one, T, &dt(), "m", 2)
            .await
            .unwrap();
        let two = seed_artifact(root, "m", "experimental", "m", b"two").await;
        bless_build(root, "m", "experimental", &two, T, &dt(), "m", 2)
            .await
            .unwrap();
        assert_eq!(
            read_previous(root, "m", "experimental").await.unwrap().as_deref(),
            Some(one.as_str())
        );

        // Config door: the module's effective default is now OFF, so the guard
        // is satisfied (target == default) but `current.prev` was built for A.
        let off_default = DefaultTarget::from_raw_for_test(OFF);
        let err = rollback_current(root, "m", "experimental", OFF, &off_default, "m")
            .await
            .expect_err("rolling back onto an unresolvable prev must be refused");
        let msg = format!("{err:?}");
        let checked = artifact_abs_path(root, "m", "experimental", &one, OFF, "m")
            .display()
            .to_string();
        let prose = msg.replace(&checked, "<CHECKED-PATH>");
        assert!(
            prose.contains(&one),
            "the diagnostic must name the prev sha in its own right: {msg}"
        );
        assert!(
            prose.contains(OFF),
            "the diagnostic must name the target: {msg}"
        );
        assert!(
            msg.contains("BUILD_MODULE_TARGET_") && msg.contains("BUILD_TARGET_TRIPLE"),
            "the diagnostic must name the likely cause: {msg}"
        );
        assert_eq!(
            read_current(root, "m", "experimental").await.unwrap().as_deref(),
            Some(two.as_str()),
            "a refused rollback must leave current untouched"
        );
    }
}
