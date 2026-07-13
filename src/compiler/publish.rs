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

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::error::ToolError;

/// The default artifact channel a fresh build publishes into. Promotion to
/// `stable` is a separate pointer-flip (BLD-07), never a rebuild.
pub const DEFAULT_CHANNEL: &str = "experimental";

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
}
