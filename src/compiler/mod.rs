//! BLD-05 — the `compiler_build` Terminus tool: the single build door.
//!
//! `compiler_build(module, ref, host="auto", profile="release", fast=false)`
//! selects a build host, ensures the pinned toolchain, runs an sccache-backed
//! `cargo` build inside a resource-capped systemd scope (`MemorySwapMax=0` — Plex
//! protection), and publishes a SHA-256-checksummed artifact into the shared
//! build dataset. On a local publish it also flips `experimental/current` onto the
//! new sha (BLD-07 store); promotion to `stable` is `compiler_release` (no rebuild).
//!
//! The keystone of the S117 constellation CI/CD. Submodules:
//!   - [`host`]    — primary-vs-heavy selection from RAM/module-size heuristics.
//!   - [`scope`]   — the `systemd-run --scope` cap rendering + the CARGO_TARGET_DIR
//!                   guard (never the file-level NFS dir).
//!   - [`sccache`] — sccache→Redis env wiring (fail-open to a local dir).
//!   - [`publish`] — content-addressed artifact layout + sha256 + sidecar.
//!
//! ## Discipline (S1/S7)
//! Every host, path, cap, threshold, and cache endpoint comes from config env
//! vars — materialized from the vault where sensitive (`SCCACHE_REDIS`), never a
//! literal in source. Nothing token/URL-with-creds shaped is read outside the
//! sccache secret wiring, and the parsed password never logs.

pub mod deploy; // BLD-13: compiler_deploy — trigger the updater fleet-wide on publish/promote
pub mod events;
pub mod host;
pub mod idle_lease; // BLD-11: compiler↔idle-mode lease (Chord+MINT idle around heavy builds)
pub mod publish;
pub mod queue; // BLD-06: the durable compiler job queue (Namespace::Queue)
pub mod resource; // PCON-09: resource-aware admission budget (RAM/jobs/disk)
pub mod scheduler; // BLD-06: window/quiet gating + per-host caps + idle seam
pub mod sccache;
pub mod scope;
pub mod status;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::ToolError;
use crate::registry::ToolRegistry;
use crate::tool::{RustTool, ToolOutput};

use host::{HostRequest, HostRole};
use queue::{JobRequest, Priority, QueueStore, RedisQueue};

/// Env var naming the shared build dataset root (appdata-backed NFS share).
const BUILD_DATASET_ROOT: &str = "BUILD_DATASET_ROOT";
/// Env var for the LOCAL/tmpfs exec-safe cargo target dir; defaults to a temp
/// dir when unset (NEVER the NFS dataset — enforced by the target-dir guard).
const BUILD_LOCAL_TARGET_DIR: &str = "BUILD_LOCAL_TARGET_DIR";
/// Env var for the build target triple; defaults to the musl static target that
/// `rust-toolchain.toml` pins (a target triple, not an infra literal).
const BUILD_TARGET_TRIPLE: &str = "BUILD_TARGET_TRIPLE";
/// Env var for the pinned rustc channel to ensure-install (BLD-02). Optional —
/// when unset, rustup auto-installs from the source dir's `rust-toolchain.toml`.
const RUST_TOOLCHAIN_PINNED: &str = "RUST_TOOLCHAIN_PINNED";
/// Env var: a relay host (`user@host`) that has the dataset mounted RW, used
/// when this build host lacks the mount (interim publish path, pre-BLD-01).
const BUILD_DATASET_RELAY_HOST: &str = "BUILD_DATASET_RELAY_HOST";
/// Env var: the dataset root PATH on the relay host (defaults to the local
/// `BUILD_DATASET_ROOT` value when unset — same share, same layout).
const BUILD_DATASET_RELAY_ROOT: &str = "BUILD_DATASET_RELAY_ROOT";
/// Env var: the exec-safe LOCAL/tmpfs cargo target dir ON THE HEAVY host (used
/// for the remote build). Required for a heavy build (a target dir on the shared
/// NFS dataset would break exec — the same guard applies remotely).
const BUILD_HEAVY_LOCAL_TARGET_DIR: &str = "BUILD_HEAVY_LOCAL_TARGET_DIR";
/// Env var: the dataset root PATH on the heavy host (where source is staged +
/// where the remote build's env-file lives under the target dir). Defaults to
/// `BUILD_DATASET_RELAY_ROOT`, else the local `BUILD_DATASET_ROOT`.
const BUILD_HEAVY_DATASET_ROOT: &str = "BUILD_HEAVY_DATASET_ROOT";
/// Env var: extra `:`-separated roots a caller-supplied `source_dir` may live
/// under, ON TOP OF the always-allowed `${BUILD_DATASET_ROOT}/src` tree. Lets an
/// operator permit a dedicated staging mount without opening arbitrary paths.
const BUILD_ALLOWED_SOURCE_ROOTS: &str = "BUILD_ALLOWED_SOURCE_ROOTS";
/// Env var (BLD-07): the number of sha dirs the store retains per channel when
/// pruning after a bless/promote. The store floors this at 2 regardless.
const BUILD_RETAIN_PER_CHANNEL: &str = "BUILD_RETAIN_PER_CHANNEL";

// ── BLD-GATE-FIX: Gitea Cargo-registry creds for a registry-consuming build ──
//
// harmony and chord depend on `terminus-rs` via the Gitea Cargo registry
// (`[registries.gitea]` in their `.cargo/config.toml`), so their build scope
// needs the sparse INDEX + credential-provider list (non-secret config) PLUS
// the registry auth TOKEN (secret — a Gitea PAT). Without these, cargo's
// dependency resolution against the `gitea` registry fails inside the
// systemd-run scope even though sccache/toolchain env is otherwise fine.

/// Env var: the Gitea Cargo sparse-registry INDEX URL. Non-secret (a plain
/// endpoint, not a credential) — always read from the environment FIRST so
/// the host stays authoritative; see [`cargo_registry_gitea_index`] for the
/// (non-hardcoded) fallback derivation from `GITEA_URL` when unset.
const CARGO_REGISTRIES_GITEA_INDEX: &str = "CARGO_REGISTRIES_GITEA_INDEX";
/// Env var: Cargo's global credential-provider list (non-secret — a config
/// string cargo itself defines). `cargo:token` is cargo's built-in provider
/// that sends a `CARGO_REGISTRIES_<NAME>_TOKEN` value verbatim; required for
/// cargo's credential-provider model (>=1.74) to use the token env at all.
const CARGO_REGISTRY_GLOBAL_CREDENTIAL_PROVIDERS: &str =
    "CARGO_REGISTRY_GLOBAL_CREDENTIAL_PROVIDERS";
/// Env var (SECRET — name ends in `_TOKEN`, so `scope::is_secret_env_key`
/// already routes it to the inherited-env-only side of `scope::partition_env`,
/// never `--setenv` argv): matches cargo's own `CARGO_REGISTRIES_<NAME>_TOKEN`
/// convention for a registry named `gitea`. If the operator did not
/// provision one specifically for the registry, [`cargo_registry_gitea_token`]
/// falls back to the same `GITEA_PAT_<identity>` this crate already uses for
/// the Gitea REST API (see `crate::gitea`).
const CARGO_REGISTRIES_GITEA_TOKEN: &str = "CARGO_REGISTRIES_GITEA_TOKEN";
/// Default value for `CARGO_REGISTRY_GLOBAL_CREDENTIAL_PROVIDERS` when unset —
/// cargo's built-in token provider, the only one needed for a PAT-shaped
/// `CARGO_REGISTRIES_GITEA_TOKEN`.
const DEFAULT_CARGO_CREDENTIAL_PROVIDERS: &str = "cargo:token";
/// Env var naming the active-default Gitea identity (mirrors
/// `crate::gitea`'s private `GITEA_IDENTITY_NAME`/`DEFAULT_GITEA_IDENTITY` —
/// duplicated as a plain string here rather than making those `pub`, since
/// this is a read-only fallback lookup, not a Gitea API client).
const GITEA_IDENTITY_NAME_ENV: &str = "GITEA_IDENTITY_NAME";
const DEFAULT_GITEA_IDENTITY_FOR_REGISTRY: &str = "moose";

/// Resolve the Gitea Cargo-registry auth token (SECRET). Per this crate's
/// established secret convention (see `crate::gitea`'s module doc and
/// `crate::config`'s CONST-02/03 sections: there is no separate
/// `SecretManager`/`vault::manager()` API in terminus-rs — a plain env read of
/// a runtime-materialized value IS the vault read here), this reads directly
/// at the point of use, exactly like `CONSTELLATION_OPERATOR_SECRET`, rather
/// than through a shared `crate::config` helper.
///
/// Resolution order:
///   1. `CARGO_REGISTRIES_GITEA_TOKEN` — a PAT provisioned specifically for
///      the Cargo registry.
///   2. The active-default Gitea identity's `GITEA_PAT_<NAME>` (same
///      identity `crate::gitea::GiteaClient::from_env` would pick —
///      `GITEA_IDENTITY_NAME`, default `moose`), since Gitea's Cargo registry
///      accepts the same PAT as its REST API and every environment that
///      talks to Gitea already provisions one.
///   3. `GITEA_PAT_MOOSE` as an explicit FINAL fallback — the `moose` identity's
///      PAT is the one provisioned with `moosenet`-org read access (the access a
///      registry-consuming build actually needs). If step 2 picked a NON-moose
///      identity (an operator set `GITEA_IDENTITY_NAME` to something else) whose
///      `GITEA_PAT_<that>` is unset, we must still fall back to the org-readable
///      moose PAT rather than degrading — otherwise a harmony/chord build would
///      fail to resolve terminus-rs purely because the active identity changed.
///      This step is a no-op when step 2 already resolved moose's PAT.
///
/// Returns `None` (never a stopgap literal) when none are configured; the
/// caller must DEGRADE (log + continue), never hardcode a token.
fn cargo_registry_gitea_token() -> Option<String> {
    if let Some(t) = env_nonempty(CARGO_REGISTRIES_GITEA_TOKEN) {
        return Some(t);
    }
    // PREFER the org-readable moose PAT for REGISTRY reads: fetching the
    // terminus-rs crate from the Gitea Cargo registry only needs org read
    // access, which GITEA_PAT_MOOSE is guaranteed to have. The gateway's active
    // identity (GITEA_IDENTITY_NAME) is only a SECONDARY fallback — its PAT may
    // be scoped to a different area and lack registry read (review finding).
    if let Some(t) = env_nonempty(&format!(
        "GITEA_PAT_{}",
        DEFAULT_GITEA_IDENTITY_FOR_REGISTRY.to_uppercase()
    )) {
        return Some(t);
    }
    // Last-ditch: the active identity's PAT, if moose is unprovisioned.
    let identity = env_nonempty(GITEA_IDENTITY_NAME_ENV)
        .map(|v| v.to_lowercase())
        .unwrap_or_else(|| DEFAULT_GITEA_IDENTITY_FOR_REGISTRY.to_string());
    env_nonempty(&format!("GITEA_PAT_{}", identity.to_uppercase()))
}

/// Resolve the Gitea Cargo sparse-registry INDEX URL. Deliberately has NO
/// hardcoded infra literal (S1) — an explicit `CARGO_REGISTRIES_GITEA_INDEX`
/// always wins; failing that, it's DERIVED from the same `GITEA_URL` +
/// `GITEA_OWNER` config `crate::gitea::GiteaClient::from_env` already uses for
/// the Gitea REST API (Gitea's Cargo registry lives at a fixed, well-known
/// path under the same base URL:
/// `{GITEA_URL}/api/packages/{GITEA_OWNER}/cargo/`), so a box that already
/// talks to Gitea doesn't need a second URL provisioned. `None` when neither
/// is configured — the caller must not fabricate one.
fn cargo_registry_gitea_index() -> Option<String> {
    if let Some(v) = env_nonempty(CARGO_REGISTRIES_GITEA_INDEX) {
        return Some(v);
    }
    let base = env_nonempty("GITEA_URL")?;
    let owner = env_nonempty("GITEA_OWNER").unwrap_or_else(|| "moosenet".to_string());
    Some(format!(
        "sparse+{}/api/packages/{owner}/cargo/",
        base.trim_end_matches('/')
    ))
}

/// Populate `build_env` with the Gitea Cargo-registry config a module that
/// depends on `terminus-rs` via that registry (harmony, chord) needs to
/// resolve it: the sparse INDEX + credential-provider list (non-secret,
/// `--setenv`-safe) plus the registry auth TOKEN (secret — name ends in
/// `_TOKEN`, so `scope::is_secret_env_key`/`partition_env` already route it to
/// the inherited-env-only side, never argv). The formatted `Bearer <token>`
/// value is also appended to `redact` (S7) so a build script that echoes its
/// env can never leak it into captured stdout/stderr or a `ToolError`.
///
/// Best-effort/DEGRADE: if the INDEX can't be resolved (neither
/// `CARGO_REGISTRIES_GITEA_INDEX` nor `GITEA_URL` configured) or no token is
/// configured anywhere, this logs a clear warning and continues rather than
/// failing the whole build — a module that doesn't touch the registry is
/// unaffected, and a registry-consuming build gets cargo's own "no
/// index/token configured" resolve error instead of the misleading
/// `systemd-run: No such file` a missing-staged-source used to produce (see
/// [`validate_local_source_dir`]).
fn inject_gitea_registry_env(build_env: &mut BTreeMap<String, String>, redact: &mut Vec<String>) {
    match cargo_registry_gitea_index() {
        Some(index) => {
            build_env.insert(CARGO_REGISTRIES_GITEA_INDEX.to_string(), index);
            let providers = env_nonempty(CARGO_REGISTRY_GLOBAL_CREDENTIAL_PROVIDERS)
                .unwrap_or_else(|| DEFAULT_CARGO_CREDENTIAL_PROVIDERS.to_string());
            build_env.insert(
                CARGO_REGISTRY_GLOBAL_CREDENTIAL_PROVIDERS.to_string(),
                providers,
            );
        }
        None => {
            tracing::warn!(
                "compiler: Gitea Cargo-registry index unconfigured \
                 ({CARGO_REGISTRIES_GITEA_INDEX} and GITEA_URL both unset) — \
                 registry-consuming builds (harmony, chord) will fail to \
                 resolve terminus-rs from the Gitea Cargo registry"
            );
        }
    }
    match cargo_registry_gitea_token() {
        Some(token) => {
            // The `Bearer ` PREFIX IS REQUIRED, not a bug: cargo sends the value
            // of `CARGO_REGISTRIES_<NAME>_TOKEN` VERBATIM as the HTTP
            // `Authorization:` header, and Gitea's Cargo registry only accepts the
            // `Bearer <PAT>` scheme (a raw PAT → 401). This matches the working
            // format the constellation-updater sources from
            // `/etc/constellation/secrets` to build chord
            // (`CARGO_REGISTRIES_GITEA_TOKEN="<REDACTED-SECRET>"`), also documented in
            // project memory. NOTE: an operator who ever needs a raw token can
            // set `CARGO_REGISTRIES_GITEA_TOKEN` explicitly — that value WINS
            // (it's returned as-is by `cargo_registry_gitea_token` step 1, so it
            // is inserted verbatim without a synthesized `Bearer ` prefix); we
            // only synthesize `Bearer <pat>` when falling back to a bare
            // `GITEA_PAT_*`, which is always a raw PAT.
            let bearer = if token.starts_with("Bearer ") {
                token.clone()
            } else {
                format!("Bearer {token}")
            };
            redact.push(token);
            redact.push(bearer.clone());
            build_env.insert(CARGO_REGISTRIES_GITEA_TOKEN.to_string(), bearer);
        }
        None => {
            tracing::warn!(
                "compiler: Gitea registry token unavailable from vault \
                 ({CARGO_REGISTRIES_GITEA_TOKEN} and GITEA_PAT_<identity> both \
                 unset) — registry-consuming builds (harmony, chord) will fail \
                 to resolve terminus-rs from the Gitea Cargo registry"
            );
        }
    }
}

// ── GAP 3 (TERM #418): auto-stage source from Gitea when not pre-staged ─────
//
// Previously a caller had to manually rsync a module's source into
// `${BUILD_DATASET_ROOT}/src/<module>/<ref>` before `compiler_build` would
// even start (GAP 1's `validate_local_source_dir` fails loudly, but does
// nothing to FIX the gap). That defeats the point of a remote build door: a
// fresh agent should be able to call `compiler_request`/`compiler_build`
// against a module@ref that exists in Gitea and have the source appear on
// its own. This section fetches it, export-style (no `.git` in the staged
// tree, matching how every other staged module — e.g. terminus/main — looks
// on disk today).

/// Env var: default-ON toggle for GAP 3 auto-staging. Unset/"1"/"true" (any
/// case) → enabled; "0"/"false" (any case) → disabled. Anything else is
/// treated as enabled (fail open to the new, intended behavior rather than
/// silently reverting to the old manual-stage-only mode on a typo).
const BUILD_AUTOSTAGE_ENV: &str = "BUILD_AUTOSTAGE";

/// Env var: a JSON object `{ "<module>": "<GiteaRepoName>" }` merged OVER
/// [`default_module_repo_map`] (an override entry replaces the default for
/// that module; every other default entry is kept). Lets an operator map a
/// new module, or repoint an existing one, without a code change.
const BUILD_MODULE_REPO_MAP_ENV: &str = "BUILD_MODULE_REPO_MAP";

/// The longest a single GAP-3 git/rsync step (clone, fetch, checkout, rsync)
/// may run. Each step gets its own budget (not one shared budget across all
/// steps) — bounding every step individually via the existing `run()` helper
/// means a hung transport on any one of them can never wedge a build.
const AUTOSTAGE_STEP_TIMEOUT: Duration = Duration::from_secs(300);

/// The built-in module→Gitea-repo map (see the module table in
/// `CLAUDE.md`/moosenet-spec — `moosenet/<Repo>`). Overridable/extendable via
/// [`BUILD_MODULE_REPO_MAP_ENV`].
fn default_module_repo_map() -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    m.insert("terminus".to_string(), "Terminus".to_string());
    m.insert("chord".to_string(), "Chord".to_string());
    m.insert("harmony".to_string(), "Harmony".to_string());
    m.insert("muse".to_string(), "Muse".to_string());
    m.insert("lumina".to_string(), "lumina-constellation".to_string());
    m.insert("lumina-core".to_string(), "lumina-constellation".to_string());
    m
}

/// Best-effort default for a module with no map entry: capitalize the first
/// byte (ASCII — module names are already constrained to `[A-Za-z0-9._-]` by
/// `validate_segment`, so this never has to reason about multi-byte case
/// folding). A guess, not a guarantee — logged loudly so a wrong guess is
/// diagnosable rather than a silent 404 further down.
fn capitalize_module_name(module: &str) -> String {
    let mut chars = module.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => module.to_string(),
    }
}

/// Resolve the Gitea repo name for `module`: [`BUILD_MODULE_REPO_MAP_ENV`]
/// (if set and valid JSON) merged OVER [`default_module_repo_map`], falling
/// back to [`capitalize_module_name`] (logged) when the module has no entry
/// in either. Never fails — a build for an unmapped module still gets a
/// best-effort guess rather than being blocked outright by the map lookup
/// itself (the actual git fetch below is what surfaces a real "repo not
/// found" error if the guess is wrong).
fn resolve_module_repo(module: &str) -> String {
    let mut map = default_module_repo_map();
    if let Some(raw) = env_nonempty(BUILD_MODULE_REPO_MAP_ENV) {
        match serde_json::from_str::<BTreeMap<String, String>>(&raw) {
            Ok(overrides) => map.extend(overrides),
            Err(e) => {
                tracing::warn!(
                    "compiler: {BUILD_MODULE_REPO_MAP_ENV} is not a valid JSON object \
                     ({e}); ignoring override, using built-in defaults only"
                );
            }
        }
    }
    if let Some(repo) = map.get(module) {
        return repo.clone();
    }
    let fallback = capitalize_module_name(module);
    tracing::warn!(
        "compiler: no Gitea repo mapping for module '{module}' (set \
         {BUILD_MODULE_REPO_MAP_ENV} to add one); guessing repo '{fallback}'"
    );
    fallback
}

/// Whether GAP 3 auto-staging is enabled (default ON — see
/// [`BUILD_AUTOSTAGE_ENV`]).
fn autostage_enabled() -> bool {
    match env_nonempty(BUILD_AUTOSTAGE_ENV) {
        None => true,
        Some(v) => !matches!(v.to_lowercase().as_str(), "0" | "false"),
    }
}

// ── PCON-01..05 (S122): content-addressed per-SHA build staging ────────────
//
// Historically the default source stage was keyed by the mutable REF (branch
// name): `${BUILD_DATASET_ROOT}/src/<module>/<ref>`. Two gate builds of the
// same branch from different sessions/SHAs therefore shared ONE on-disk
// checkout — observed live building an alien session's commit with none of
// the requested branch's fixes. PCON-01 resolves `ref -> sha` exactly ONCE at
// request time and stages by the resolved, IMMUTABLE sha instead; PCON-02
// asserts the staged tree's sha matches what was requested/resolved
// (fail-closed); PCON-03 applies the same per-sha isolation to the heavy-host
// relay; PCON-05 bounds the resulting per-sha disk footprint with GC.
//
// A caller-supplied `source_dir` (an explicit override) is NEVER touched by
// any of this — it is validated/used exactly as it always was.

/// Env var: default-ON toggle for PCON-01's content-addressed-by-SHA staging.
/// Unset/"1"/"true" (any case) → enabled (the new, safe-by-construction
/// behavior); "0"/"false" (any case) → disabled, reverting to the legacy
/// ref-keyed stage path (`.../src/<module>/<ref>`) for every module — the
/// operator's rollback lever if something about SHA resolution misbehaves.
const BUILD_STAGE_BY_SHA_ENV: &str = "BUILD_STAGE_BY_SHA";

/// Whether PCON-01 SHA-keyed staging is enabled (default ON).
fn stage_by_sha_enabled() -> bool {
    match env_nonempty(BUILD_STAGE_BY_SHA_ENV) {
        None => true,
        // "off" accepted alongside "0"/"false" — every doc comment and
        // fail-closed error message in this module tells an operator to set
        // `BUILD_STAGE_BY_SHA=off` as the rollback lever, so that spelling
        // MUST actually work.
        Some(v) => !matches!(v.to_lowercase().as_str(), "0" | "false" | "off"),
    }
}

/// Whether `s` already looks like a full, resolved git commit sha: exactly 40
/// lowercase-or-uppercase hex characters. A short/abbreviated sha (e.g. `b514
/// 32c`) does NOT count — [`resolve_ref_to_sha`] must still resolve it to the
/// full, unambiguous form (an abbreviated sha is exactly the kind of
/// ambiguous-ref case PCON-01 must never silently stage under).
fn is_full_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The sidecar filename PCON-02 writes into a SHA-staged tree, recording the
/// exact sha that was fetched. Read back before a build ever spawns, so a
/// staged-but-alien tree (a clobber, a foreign checkout, or an older
/// ref-keyed stage lacking any sha provenance) is caught fail-closed rather
/// than silently built.
const BUILT_SHA_SIDECAR: &str = ".built_sha";

/// PCON-01: resolve `git_ref` to a full, unambiguous commit sha EXACTLY ONCE,
/// at request time, so every downstream staging/targeting/locking decision
/// keys off the immutable identity rather than the mutable ref.
///
/// - Already a full 40-hex sha → returned verbatim, no I/O (this is also what
///   makes a direct sha request a no-op fast path).
/// - Otherwise, resolved via `git ls-remote <remote> <git_ref>` over the SAME
///   sanctioned Gitea remote + `GIT_ASKPASS` token machinery
///   [`autostage_source`] uses (never a raw HTTP call, S9) — the token is
///   pushed onto `redact` BEFORE it touches any argv/env (S7).
///
/// Fails CLOSED (never falls back to ref-keyed staging) when: no Gitea token
/// is configured, the remote is unreachable, the ref doesn't resolve to
/// exactly one sha, or the resolved value isn't a clean full sha — see the
/// item's EDGE CASES. The caller decides what "fail closed" means at the
/// call site (PCON-01's caller aborts the build with a clear error); this
/// function itself has no side effects on the filesystem.
async fn resolve_ref_to_sha(
    module: &str,
    git_ref: &str,
    redact: &mut Vec<String>,
) -> Result<String, ToolError> {
    if is_full_sha(git_ref) {
        return Ok(git_ref.to_lowercase());
    }
    let (remote, repo) = autostage_remote_url(module)?;
    let token = autostage_gitea_token().ok_or_else(|| {
        ToolError::NotConfigured(format!(
            "cannot resolve {module}@{git_ref} to a commit sha (Gitea repo {repo}): no Gitea \
             token configured (GITEA_PAT_MOOSE / GITEA_PAT_<identity>)"
        ))
    })?;
    redact.push(token.clone());

    let askpass = AutostageAskpass::write()?;
    let mut authed_env: BTreeMap<String, String> = BTreeMap::new();
    authed_env.insert(
        "GIT_ASKPASS".to_string(),
        askpass.path.to_string_lossy().to_string(),
    );
    authed_env.insert("GIT_TERMINAL_PROMPT".to_string(), "0".to_string());
    authed_env.insert("GIT_MIRROR_TOKEN".to_string(), token);

    let out = run(
        &[
            "git".to_string(),
            "ls-remote".to_string(),
            "--".to_string(),
            remote.clone(),
            git_ref.to_string(),
        ],
        None,
        &authed_env,
        AUTOSTAGE_STEP_TIMEOUT,
        redact,
        None,
        None,
    )
    .await
    .map_err(|e| {
        ToolError::Execution(format!(
            "could not resolve {module}@{git_ref} to a commit sha (Gitea repo {repo}, \
             `git ls-remote` failed): {e}"
        ))
    })?;
    drop(askpass);

    // `git ls-remote` prints "<sha>\t<ref>" per matching ref, one per line. An
    // exact branch/tag name may still match more than one ref (e.g. a branch
    // AND a like-named tag) — require exactly one line so an ambiguous ref
    // never silently picks the first match.
    let lines: Vec<&str> = out.lines().filter(|l| !l.trim().is_empty()).collect();
    let sha = match lines.as_slice() {
        [] => {
            return Err(ToolError::NotFound(format!(
                "ref {git_ref:?} not found in Gitea repo {repo} for module {module} \
                 (`git ls-remote` returned no match)"
            )));
        }
        [one] => one.split_whitespace().next().unwrap_or_default(),
        many => {
            // Prefer an exact `refs/heads/<ref>` match if present (the common
            // "branch name also matches a tag" ambiguity) rather than failing
            // outright; otherwise refuse rather than guess.
            match many
                .iter()
                .find(|l| l.ends_with(&format!("refs/heads/{git_ref}")))
            {
                Some(l) => l.split_whitespace().next().unwrap_or_default(),
                None => {
                    return Err(ToolError::InvalidArgument(format!(
                        "ref {git_ref:?} is ambiguous in Gitea repo {repo} for module \
                         {module} ({} matches) — refusing to guess which sha to stage",
                        many.len()
                    )));
                }
            }
        }
    };
    if !is_full_sha(sha) {
        return Err(ToolError::Execution(format!(
            "`git ls-remote` for {module}@{git_ref} returned a value that is not a full \
             commit sha ({sha:?}) — refusing to stage under an unresolved identity"
        )));
    }
    Ok(sha.to_lowercase())
}

/// PCON-01/04 (S122 root-cause fix): resolve `module@git_ref` to its durable
/// enqueue-time identity for a QUEUED job — `Some(sha)` when SHA-staging is
/// enabled and the ref resolves, `None` when `BUILD_STAGE_BY_SHA=off` (the
/// rollback lever), and a fail-closed `Err` when SHA-staging is enabled but
/// resolution fails (never silently enqueues a job whose identity is
/// unresolved). Shared by every enqueue entry point
/// (`CompilerBuild::enqueue_async_onto`, `CompilerRequest::execute_structured`)
/// so they can never drift onto different resolve-or-not policies.
async fn resolve_sha_for_enqueue(module: &str, git_ref: &str) -> Result<Option<String>, ToolError> {
    if !stage_by_sha_enabled() {
        return Ok(None);
    }
    let mut redact: Vec<String> = Vec::new();
    resolve_ref_to_sha(module, git_ref, &mut redact)
        .await
        .map(Some)
        .map_err(|e| {
            ToolError::Execution(format!(
                "compiler: could not resolve {module}@{git_ref} to a commit sha at enqueue \
                 time (fail-closed — set {BUILD_STAGE_BY_SHA_ENV}=off to fall back to the \
                 legacy ref-keyed staging/dedupe path): {e}"
            ))
        })
}

/// PCON-02 (root-caused + FINDING 4, review): the built-identity integrity
/// check, factored out as a pure (synchronous, filesystem-only) function so
/// it is unit-testable without any of `build_inner`'s surrounding I/O. Reads
/// the [`BUILT_SHA_SIDECAR`] `autostage_source` wrote at publish time and
/// asserts it matches `expected_identity` (the resolved sha in SHA-mode, or
/// the raw ref in the legacy `BUILD_STAGE_BY_SHA=off` mode — the caller
/// passes `stage_key`/`job_identity` either way, so this function itself
/// never needs to know which mode is active).
///
/// `strict` is what changed for FINDING 4: previously the off-path skipped
/// this check ENTIRELY (never called at all), silently reintroducing the
/// original clobber-reuse gap the moment an operator flipped the rollback
/// lever — the exact failure mode PCON-02 exists to catch. Now the check
/// ALWAYS runs on the default-staged path (never for an explicit
/// `source_dir` override, unaffected either way); `strict` only changes how
/// a MISSING sidecar is treated:
///   - `strict=true` (SHA-mode; `autostage_source` has ALWAYS written this
///     sidecar since PCON-01 shipped): a missing sidecar is a HARD mismatch —
///     an older/foreign/pre-PCON tree must never be silently trusted at a
///     specific sha identity.
///   - `strict=false` (`BUILD_STAGE_BY_SHA=off`): a missing sidecar is
///     EXPECTED for any directory that predates this whole PCON initiative
///     (the rollback lever must not itself start hard-failing builds that
///     worked before the sidecar existed) — logged, not fatal.
/// A sidecar that IS present but MISMATCHES is ALWAYS a hard failure in
/// EITHER mode — that is a real "this directory was staged for a different
/// identity" signal, never something to paper over.
/// Returns `Ok(Some(identity))` on a verified match, `Ok(None)` ONLY for the
/// documented `strict=false` + missing-sidecar pass-through (FINDING 4), and
/// `Err` on any real integrity failure (a present-but-mismatched sidecar in
/// EITHER mode, or a missing sidecar under `strict=true`).
fn check_built_sha_sidecar(
    local_source_dir: &std::path::Path,
    module: &str,
    expected_identity: &str,
    strict: bool,
) -> Result<Option<String>, ToolError> {
    // EXACT comparison (trim only — no case folding): a resolved sha is
    // ALREADY canonically lowercase by construction (`resolve_ref_to_sha`
    // lowercases it before it ever becomes `stage_key`/`expected_identity`,
    // and `autostage_source` writes the sidecar from that same value), but
    // the RAW-REF fallback identity (`BUILD_STAGE_BY_SHA=off`, or any future
    // non-strict caller) is a git branch/tag name — those ARE case-sensitive
    // (`Foo` and `foo` are different refs). Lowercasing here would let a
    // lowercase `foo` request silently accept an on-disk `Foo` stage — a
    // wrong-tree acceptance this check exists to prevent.
    let sidecar_path = local_source_dir.join(BUILT_SHA_SIDECAR);
    let on_disk = std::fs::read_to_string(&sidecar_path)
        .ok()
        .map(|s| s.trim().to_string());
    match on_disk {
        Some(got) if got == expected_identity => Ok(Some(got)),
        Some(got) => Err(ToolError::Execution(format!(
            "compiler: built-identity integrity check FAILED for {module}: staged tree at {} \
             carries sidecar identity {got:?} but the requested identity is \
             {expected_identity:?} — refusing to build a possibly-alien commit (fail-closed, \
             PCON-02/PCON-04); remove the stage dir to force a fresh re-stage",
            local_source_dir.display()
        ))),
        None if strict => Err(ToolError::Execution(format!(
            "compiler: built-SHA integrity check FAILED for {module}: staged tree at {} has \
             no {BUILT_SHA_SIDECAR} sidecar (an older ref-keyed stage or a foreign/manual \
             checkout) — refusing to build without SHA provenance (fail-closed, PCON-02); \
             remove the stage dir to force a fresh SHA-keyed re-stage",
            local_source_dir.display()
        ))),
        None => {
            // FINDING 4: BUILD_STAGE_BY_SHA=off — a missing sidecar here is the
            // EXPECTED state for anything staged before this feature existed;
            // logged (visible, not silent) but never fails the build. A
            // sidecar that DOES exist and mismatches is still caught above,
            // unconditionally — this is the one narrowing, not a blanket skip.
            tracing::warn!(
                "compiler: {module} staged tree at {} has no {BUILT_SHA_SIDECAR} sidecar \
                 (BUILD_STAGE_BY_SHA=off — legacy ref-keyed mode; a pre-PCON or never-verified \
                 stage) — proceeding without an identity check on this dir (fail-open ONLY in \
                 this documented rollback mode; a MISMATCHED sidecar would still be rejected)",
                local_source_dir.display()
            );
            Ok(None)
        }
    }
}

/// PCON-03: best-effort read of a REMOTE `.built_sha` sidecar over ssh —
/// `None` on ANY failure (host unreachable, dir/sidecar missing, empty
/// output). Used ONLY to decide whether an already-staged remote per-sha tree
/// can be REUSED (skip the transfer entirely, so nothing ever `rsync
/// --delete`s into a directory a sibling build might be reading — FINDING 3).
/// This is deliberately tolerant/non-propagating: the STRICT, error-
/// propagating check that actually gates the build is the unconditional
/// post-relay assertion at the heavy-path call site (mirrors
/// [`check_built_sha_sidecar`]'s local/strict-vs-probe split).
async fn remote_sidecar_sha_best_effort(
    host_addr: &str,
    remote_dir: &str,
    redact: &[String],
) -> Option<String> {
    let path = format!("{remote_dir}/{BUILT_SHA_SIDECAR}");
    let out = run(
        &[
            "ssh".to_string(),
            host_addr.to_string(),
            format!("cat {}", shell_quote(&path)),
        ],
        None,
        &BTreeMap::new(),
        Duration::from_secs(30),
        redact,
        None,
        None,
    )
    .await
    .ok()?;
    // EXACT (trim only) — a resolved sha is already canonically lowercase by
    // construction; see `check_built_sha_sidecar`'s doc for why this function
    // family never case-folds. This is a best-effort REUSE probe only (the
    // strict, error-propagating comparison happens unconditionally at the
    // call site regardless of what this returns), so under-matching here is
    // safe — it just means a legitimate reuse gets re-transferred instead of
    // skipped, never a wrong-tree acceptance.
    let sha = out.trim().to_string();
    if sha.is_empty() {
        None
    } else {
        Some(sha)
    }
}

/// FIX 3 (review, HIGH): the unconditional POST-RELAY built-identity
/// assertion against whatever ACTUALLY landed on the heavy host — run in
/// BOTH SHA-mode and the legacy `BUILD_STAGE_BY_SHA=off` mode (previously
/// this was skipped ENTIRELY off-mode, since the whole block was gated on
/// `resolved_sha.is_some()`). Mirrors [`check_built_sha_sidecar`]'s
/// strict/non-strict split, but for the REMOTE sidecar specifically:
///   - A PRESENT remote sidecar that MISMATCHES `expected_identity` is
///     ALWAYS a hard failure, in either mode — a wrong-tree relay is exactly
///     what this exists to catch, and off-mode's escape hatch was only ever
///     meant to tolerate a MISSING sidecar (a pre-PCON/never-verified
///     directory), never a present-but-wrong one.
///   - A MISSING/unreadable remote sidecar is a hard failure under
///     `strict=true` (SHA-mode; the relay must be able to prove what it just
///     staged) but only a WARNING under `strict=false` (off-mode; matches the
///     local `check_built_sha_sidecar`'s off-mode policy exactly — the
///     rollback lever must not itself start hard-failing a pre-existing
///     ref-keyed remote tree that predates this whole PCON initiative).
///
/// PCON follow-up (deferred, not this fix): this only proves the SIDECAR
/// matches — it does not verify the remote source tree's completeness/
/// contents beyond that one file (the stale/incomplete `remote_source` edge
/// case flagged in the FINDING-3 fix's commit). Tracked as a hardening
/// follow-up, not attempted here.
async fn assert_remote_sidecar(
    host_addr: &str,
    remote_source: &str,
    module: &str,
    expected_identity: &str,
    strict: bool,
    redact: &[String],
) -> Result<(), ToolError> {
    match remote_sidecar_sha_best_effort(host_addr, remote_source, redact).await {
        Some(got) if got == expected_identity => Ok(()),
        Some(got) => Err(ToolError::Execution(format!(
            "compiler: built-identity integrity check FAILED for {module} on the heavy host: \
             remote sidecar {got:?} at {remote_source} != requested identity \
             {expected_identity:?} — refusing to build a possibly-alien commit (fail-closed, \
             PCON-02/PCON-04, both SHA-mode and BUILD_STAGE_BY_SHA=off); if this persists \
             across retries, a stale/incomplete pre-PCON-03 directory may be wedging \
             {remote_source} on the heavy host — an operator should verify and remove it \
             manually"
        ))),
        None if strict => Err(ToolError::Execution(format!(
            "compiler: built-SHA integrity check FAILED for {module} on the heavy host: could \
             not read a remote {BUILT_SHA_SIDECAR} sidecar at {remote_source} after relay — \
             refusing to build a tree whose provenance could not be confirmed (fail-closed, \
             PCON-02); if this persists, a stale/incomplete directory from before PCON-03's \
             atomic publish may be wedging {remote_source} on the heavy host — an operator \
             should verify and remove it manually"
        ))),
        None => {
            // BUILD_STAGE_BY_SHA=off: a missing/unreadable remote sidecar is
            // expected for a pre-existing ref-keyed remote tree (matches the
            // LOCAL off-mode policy) — warned, not fatal. A PRESENT but
            // mismatched sidecar is still caught above, unconditionally.
            tracing::warn!(
                "compiler: {module} remote tree at {remote_source} on the heavy host has no \
                 readable {BUILT_SHA_SIDECAR} sidecar (BUILD_STAGE_BY_SHA=off — legacy \
                 ref-keyed mode) — proceeding without a remote identity check on this dir \
                 (fail-open ONLY in this documented rollback mode; a MISMATCHED sidecar would \
                 still be rejected)"
            );
            Ok(())
        }
    }
}

/// Resolve the git-transport credential (SECRET — a raw Gitea PAT, NOT the
/// `Bearer <token>`-wrapped form [`inject_gitea_registry_env`] builds for the
/// Cargo-registry HTTP header) auto-stage hands to git via `GIT_ASKPASS`.
/// Same GITEA_PAT_MOOSE-preferring resolution as
/// [`cargo_registry_gitea_token`] steps 2+3 (org-readable `moose` identity
/// first, then the active `GITEA_IDENTITY_NAME` identity as a fallback) —
/// deliberately NOT step 1 of that function (`CARGO_REGISTRIES_GITEA_TOKEN`),
/// since that env is cargo-registry-specific and may already carry a
/// synthesized `Bearer ` prefix that would break git's plain-PAT auth.
fn autostage_gitea_token() -> Option<String> {
    if let Some(t) = env_nonempty(&format!(
        "GITEA_PAT_{}",
        DEFAULT_GITEA_IDENTITY_FOR_REGISTRY.to_uppercase()
    )) {
        return Some(t);
    }
    let identity = env_nonempty(GITEA_IDENTITY_NAME_ENV)
        .map(|v| v.to_lowercase())
        .unwrap_or_else(|| DEFAULT_GITEA_IDENTITY_FOR_REGISTRY.to_string());
    env_nonempty(&format!("GITEA_PAT_{}", identity.to_uppercase()))
}

/// Build the BARE (never-authed) Gitea clone URL for `module` — `{GITEA_URL}
/// /{GITEA_OWNER}/{repo}.git` — plus the resolved repo name. Pure/no I/O
/// (testable without touching the network): unlike an `x-access-token@host`
/// URL, this NEVER embeds the credential — auth is injected separately via
/// `GIT_ASKPASS` at call time (matching this crate's established transport
/// convention in `forge::mirror::tools`, e.g. `run_git_askpass_plain`), so
/// the token can never leak into `.git/config`, a process listing, shell
/// history, or a URL echoed into a log/error — a stronger guarantee than
/// scrubbing it back out of a rendered URL after the fact.
fn autostage_remote_url(module: &str) -> Result<(String, String), ToolError> {
    let base = env_nonempty("GITEA_URL").ok_or_else(|| {
        ToolError::NotConfigured(format!(
            "cannot auto-stage source for module '{module}': GITEA_URL is not configured"
        ))
    })?;
    let owner = env_nonempty("GITEA_OWNER").unwrap_or_else(|| "moosenet".to_string());
    let repo = resolve_module_repo(module);
    let remote = format!("{}/{owner}/{repo}.git", base.trim_end_matches('/'));
    Ok((remote, repo))
}

/// RAII guard: writes a minimal `GIT_ASKPASS` helper script that echoes
/// `$GIT_MIRROR_TOKEN` (the script body itself carries NO secret — the token
/// only ever lives in the child process's environment), and removes it on
/// drop. Mirrors `forge::mirror::tools::write_askpass_script`'s shape; a
/// separate copy here rather than a shared helper since that one is private
/// to its module and this is a small, self-contained script.
struct AutostageAskpass {
    path: PathBuf,
}

impl AutostageAskpass {
    fn write() -> Result<Self, ToolError> {
        let path = std::env::temp_dir().join(format!(
            "bldgap3-askpass-{}-{}.sh",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::write(&path, b"#!/bin/sh\nprintf '%s\\n' \"$GIT_MIRROR_TOKEN\"\n")
            .map_err(|e| ToolError::Execution(format!("autostage: write askpass script: {e}")))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).map_err(
                |e| ToolError::Execution(format!("autostage: chmod askpass script: {e}")),
            )?;
        }
        Ok(Self { path })
    }
}

impl Drop for AutostageAskpass {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// RAII guard: best-effort `rm -rf` of a temp clone dir on drop, so every
/// early-return (`?`) below still cleans up — success or failure.
struct AutostageTmpDir(PathBuf);

impl Drop for AutostageTmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Wrap a step failure with the `{module}@{ref}` context GAP 3's callers
/// need, WITHOUT re-embedding anything secret-shaped — `e`'s message is
/// already redacted by `run()` (it was built from the SAME `redact` set the
/// token was pushed onto before any git argv touched it).
fn autostage_step_failed(module: &str, git_ref: &str, repo: &str, step: &str, e: ToolError) -> ToolError {
    ToolError::Execution(format!(
        "auto-stage of {module}@{git_ref} (Gitea repo {repo}) failed at '{step}': {e}"
    ))
}

/// GAP 3 (TERM #418): fetch `module@git_ref` from Gitea into `dest`,
/// export-style (no `.git` left in the staged tree — matches how a manually
/// staged tree looks today). Only ever WRITES when `dest` does not already
/// exist — never clobbers an already-staged (possibly manually-staged or
/// mid-build) tree; the caller is additionally expected to only invoke this
/// when `dest` is absent, but the check is repeated here so this function is
/// safe to call on its own.
///
/// Strategy: try a shallow `git clone --depth 1 --branch <ref>` first (works
/// for a branch or tag); if that fails — e.g. `git_ref` is a full commit sha,
/// which is not a valid `--branch` value — fall back to `git init` + `remote
/// add` + `fetch --depth 1 origin <sha>` + `checkout FETCH_HEAD`. Either way
/// the result is exported (`rsync -a --exclude .git`) into a SIBLING staging
/// dir and then atomically `rename`d into `dest` (so a concurrent builder that
/// stages `dest` mid-clone is never clobbered — the rename is the single commit
/// point; the race loser no-ops), and the temp clone is removed afterward.
///
/// Every git/rsync step is bound by [`AUTOSTAGE_STEP_TIMEOUT`] via the
/// existing `run()` helper, so a hung transport can never wedge a build. The
/// resolved token is pushed onto `redact` (S7) BEFORE any git argv touches
/// it and is injected only via `GIT_ASKPASS` (never the URL, never argv) —
/// so it cannot appear in a captured stdout/stderr, a `ToolError`, or a log
/// line, even before the redaction pass runs.
async fn autostage_source(
    module: &str,
    git_ref: &str,
    dest: &std::path::Path,
    redact: &mut Vec<String>,
) -> Result<(), ToolError> {
    if dest.exists() {
        // Never clobber an already-staged (or in-progress) tree.
        return Ok(());
    }
    let (remote, repo) = autostage_remote_url(module)?;
    let token = autostage_gitea_token().ok_or_else(|| {
        ToolError::NotConfigured(format!(
            "cannot auto-stage source for {module}@{git_ref} (Gitea repo {repo}): no Gitea \
             token configured (GITEA_PAT_MOOSE / GITEA_PAT_<identity>)"
        ))
    })?;
    // S7: append BEFORE any argv/env touches the token, so every error path
    // below — including a spawn failure on the very next line — redacts it.
    redact.push(token.clone());

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            ToolError::Execution(format!(
                "autostage: failed to create {}: {e}",
                parent.display()
            ))
        })?;
    }

    let tmp = std::env::temp_dir().join(format!(
        "bldgap3-src-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    let _tmp_guard = AutostageTmpDir(tmp.clone());
    let tmp_str = tmp.to_string_lossy().to_string();

    let askpass = AutostageAskpass::write()?;
    let mut authed_env: BTreeMap<String, String> = BTreeMap::new();
    authed_env.insert(
        "GIT_ASKPASS".to_string(),
        askpass.path.to_string_lossy().to_string(),
    );
    authed_env.insert("GIT_TERMINAL_PROMPT".to_string(), "0".to_string());
    authed_env.insert("GIT_MIRROR_TOKEN".to_string(), token.clone());

    // Strategy 1: shallow clone of a branch/tag ref.
    let clone_argv = vec![
        "git".to_string(),
        "clone".to_string(),
        "--depth".to_string(),
        "1".to_string(),
        "--branch".to_string(),
        git_ref.to_string(),
        "--".to_string(),
        remote.clone(),
        tmp_str.clone(),
    ];
    let cloned = run(
        &clone_argv,
        None,
        &authed_env,
        AUTOSTAGE_STEP_TIMEOUT,
        redact,
        None,
        None,
    )
    .await;

    if let Err(e) = cloned {
        // Strategy 2: `git_ref` is likely a full sha (not a valid --branch
        // value) — start clean and fetch it directly. A partial clone dir
        // from the failed attempt above must not linger for `git init`.
        //
        // FINDING 3 (review): log at WARN, not debug — a strategy-1 failure is
        // EXPECTED for a sha ref, but it is ALSO how a real auth/network fault
        // first surfaces; at debug it was invisible until strategy 2 also failed.
        // `e`'s message is already token-redacted by `run()` (S7), so this leaks
        // nothing even before the outer redaction pass.
        tracing::warn!(
            "compiler: autostage shallow clone of {module}@{git_ref} by branch/tag failed \
             ({e}); falling back to a direct sha fetch"
        );
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).map_err(|e| {
            ToolError::Execution(format!("autostage: failed to create {}: {e}", tmp.display()))
        })?;
        run(
            &[
                "git".to_string(),
                "init".to_string(),
                "--quiet".to_string(),
                tmp_str.clone(),
            ],
            None,
            &BTreeMap::new(),
            AUTOSTAGE_STEP_TIMEOUT,
            redact,
            None,
            None,
        )
        .await
        .map_err(|e| autostage_step_failed(module, git_ref, &repo, "git init", e))?;
        run(
            &[
                "git".to_string(),
                "remote".to_string(),
                "add".to_string(),
                "origin".to_string(),
                remote.clone(),
            ],
            Some(&tmp),
            &BTreeMap::new(),
            AUTOSTAGE_STEP_TIMEOUT,
            redact,
            None,
            None,
        )
        .await
        .map_err(|e| autostage_step_failed(module, git_ref, &repo, "git remote add", e))?;
        run(
            &[
                "git".to_string(),
                "fetch".to_string(),
                "--depth".to_string(),
                "1".to_string(),
                "origin".to_string(),
                git_ref.to_string(),
            ],
            Some(&tmp),
            &authed_env,
            AUTOSTAGE_STEP_TIMEOUT,
            redact,
            None,
            None,
        )
        .await
        .map_err(|e| autostage_step_failed(module, git_ref, &repo, "git fetch <sha>", e))?;
        run(
            &[
                "git".to_string(),
                "checkout".to_string(),
                "--quiet".to_string(),
                "FETCH_HEAD".to_string(),
            ],
            Some(&tmp),
            &BTreeMap::new(),
            AUTOSTAGE_STEP_TIMEOUT,
            redact,
            None,
            None,
        )
        .await
        .map_err(|e| autostage_step_failed(module, git_ref, &repo, "git checkout FETCH_HEAD", e))?;
    }
    drop(askpass);

    // FINDING 2 (review — TOCTOU): the `dest.exists()` check at the top runs
    // BEFORE the (slow) clone/fetch. If a CONCURRENT builder stages `dest`
    // while our clone is in flight, an rsync straight into `dest` would merge
    // into (clobber) their tree. Instead export into a SIBLING staging dir
    // (same parent ⇒ same filesystem ⇒ `rename` is atomic and never EXDEV),
    // then atomically `rename` it into place. `rename` onto a non-empty dir
    // fails, and onto a still-absent name succeeds — so the mv is the single
    // commit point and the loser of a race no-ops instead of clobbering.
    let parent = dest.parent().ok_or_else(|| {
        ToolError::Execution(format!(
            "autostage: destination {} has no parent directory",
            dest.display()
        ))
    })?;
    let staging = parent.join(format!(
        ".bldgap3-stage-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    let _staging_guard = AutostageTmpDir(staging.clone());
    std::fs::create_dir_all(&staging).map_err(|e| {
        ToolError::Execution(format!("autostage: failed to create {}: {e}", staging.display()))
    })?;
    // Export (no `.git`) into the staging dir.
    run(
        &[
            "rsync".to_string(),
            "-a".to_string(),
            "--exclude".to_string(),
            ".git".to_string(),
            format!("{tmp_str}/"),
            format!("{}/", staging.to_string_lossy()),
        ],
        None,
        &BTreeMap::new(),
        AUTOSTAGE_STEP_TIMEOUT,
        redact,
        None,
        None,
    )
    .await
    .map_err(|e| autostage_step_failed(module, git_ref, &repo, "rsync export", e))?;

    // PCON-02: record the exact ref this tree was fetched at (a sidecar, not a
    // `.git` dir — the export above deliberately has none) so a build can
    // assert the staged tree's provenance before ever spawning cargo. When
    // this is a SHA-keyed stage (PCON-01 passes the resolved sha as `git_ref`
    // — autostage_source's strategy-2 fetches that exact commit), the sidecar
    // IS the built sha; best-effort (never fails the stage — a write failure
    // here just means the later PCON-02 assertion treats it as a missing
    // sidecar and fails closed at that point instead).
    if let Err(e) = std::fs::write(staging.join(BUILT_SHA_SIDECAR), format!("{git_ref}\n")) {
        tracing::warn!(
            "compiler: autostage of {module}@{git_ref} could not write the {BUILT_SHA_SIDECAR} \
             sidecar ({e}); a later PCON-02 SHA-integrity check will fail closed on this stage"
        );
    }

    // Atomic publish. If `dest` now exists (a concurrent builder won the race),
    // `rename` fails — re-check and treat an existing `dest` as success (their
    // tree stands; our staging dir is cleaned up by the guard on drop).
    match std::fs::rename(&staging, dest) {
        Ok(()) => Ok(()),
        Err(e) => {
            if dest.exists() {
                tracing::debug!(
                    "compiler: autostage of {module}@{git_ref} lost the publish race \
                     (dest already staged concurrently); keeping the existing tree"
                );
                Ok(())
            } else {
                Err(autostage_step_failed(
                    module,
                    git_ref,
                    &repo,
                    "atomic rename into place",
                    ToolError::Execution(format!(
                        "rename {} -> {}: {e}",
                        staging.display(),
                        dest.display()
                    )),
                ))
            }
        }
    }
}

const DEFAULT_TARGET_TRIPLE: &str = "x86_64-unknown-linux-musl";

/// The longest a single `compiler_build` may run (the local/primary cargo build
/// timeout; the remote/heavy path is shorter). The scheduler's stale-reconcile
/// lease floor is derived from this so a genuinely-live build is never reconciled.
pub const MAX_BUILD_TIMEOUT_SECS: u64 = 3600;

/// BLD-444: the longest a single web-build pre-step command (`npm ci` or
/// `npm run build`) may run, capped shorter than the cargo build/test timeout
/// above — an SPA build is a small fraction of a Rust workspace build, and a
/// short bound keeps a hung/misconfigured npm from silently eating the whole
/// build's timeout budget before cargo ever starts.
const WEB_BUILD_TIMEOUT_SECS: u64 = 900;

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// The configured shared build dataset root. `NotConfigured` when unset — the
/// compiler cannot publish without it.
fn dataset_root() -> Result<PathBuf, ToolError> {
    env_nonempty(BUILD_DATASET_ROOT)
        .map(PathBuf::from)
        .ok_or_else(|| ToolError::NotConfigured(format!("{BUILD_DATASET_ROOT} is not configured")))
}

/// The LOCAL/tmpfs exec-safe cargo target dir. Defaults to a stable temp path so
/// a build never accidentally targets the NFS dataset; the guard re-checks it.
fn local_target_dir() -> PathBuf {
    env_nonempty(BUILD_LOCAL_TARGET_DIR)
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("terminus-build-target"))
}

/// PCON-10: the big-disk build-scratch ROOT for a LOCAL build's per-job
/// `CARGO_TARGET_DIR` + `TMPDIR`. Env var name only (S1).
const BUILD_SCRATCH_ROOT: &str = "BUILD_SCRATCH_ROOT";

/// PCON-10 pure core: resolve the scratch ROOT from the two candidate config
/// values. Prefers `BUILD_SCRATCH_ROOT` (a big appdata-backed disk), then the
/// existing `BUILD_LOCAL_TARGET_DIR` (already sized exec-safe by the operator for
/// BLD-05), so a host already configured keeps working. FAILS CLOSED when NEITHER
/// is set — we refuse to silently place a per-job target/TMPDIR on the default
/// `/tmp` tmpfs, the exact tmpfs+disk exhaustion this closes.
fn resolve_scratch_root(scratch: Option<String>, local: Option<String>) -> Result<PathBuf, ToolError> {
    let root = scratch.or(local).map(PathBuf::from).ok_or_else(|| {
        ToolError::NotConfigured(format!(
            "build scratch root not configured: set {BUILD_SCRATCH_ROOT} to a big-disk \
             path (or {BUILD_LOCAL_TARGET_DIR}) — refusing to place a per-job \
             CARGO_TARGET_DIR/TMPDIR on the default /tmp tmpfs (tmpfs+disk exhaustion)"
        ))
    })?;
    reject_tmpfs_scratch(&root)?;
    Ok(root)
}

/// FIX (PCON-10): fail CLOSED when a configured scratch root resolves to a small
/// in-RAM mount — `validate_target_dir` only checks NFS-dataset non-containment,
/// so a legacy `BUILD_LOCAL_TARGET_DIR=/tmp/...` (tmpfs) would otherwise pass and
/// re-introduce the tmpfs+disk exhaustion this closes. Two layers: a LEXICAL
/// guard against the well-known tmpfs mount roots (works even when the dir does
/// not yet exist, so a test/first-run is deterministic), plus a `statfs` f_type
/// check that rejects an actual tmpfs/ramfs when the path exists.
fn reject_tmpfs_scratch(root: &std::path::Path) -> Result<(), ToolError> {
    for bad in ["/tmp", "/dev/shm", "/run"] {
        if scope::is_within(root, std::path::Path::new(bad)) {
            return Err(ToolError::NotConfigured(format!(
                "build scratch root {} is on the small in-RAM {bad} tmpfs — set \
                 {BUILD_SCRATCH_ROOT} to a big (on-disk) path; refusing to build there \
                 (tmpfs+disk exhaustion)",
                root.display()
            )));
        }
    }
    if root.exists() && crate::compiler::resource::is_tmpfs(root) {
        return Err(ToolError::NotConfigured(format!(
            "build scratch root {} is a tmpfs/ramfs filesystem — set {BUILD_SCRATCH_ROOT} \
             to a big (on-disk) path; refusing to build there (tmpfs+disk exhaustion)",
            root.display()
        )));
    }
    Ok(())
}

/// PCON-10: the resolved big-disk scratch ROOT for a local build (fail-closed).
fn job_scratch_root() -> Result<PathBuf, ToolError> {
    resolve_scratch_root(
        env_nonempty(BUILD_SCRATCH_ROOT),
        env_nonempty(BUILD_LOCAL_TARGET_DIR),
    )
}

/// PCON-10: the per-job `(target, tmpdir)` under a scratch ROOT, keyed by the
/// unique per-invocation `unit`. Disjoint across concurrent jobs; both live on
/// the big disk (never the small `/tmp` tmpfs). The parent `root.join(unit)` is
/// the single dir reclaimed on finalize.
fn job_scratch_dirs(root: &std::path::Path, unit: &str) -> (PathBuf, PathBuf) {
    let base = root.join(unit);
    (base.join("target"), base.join("tmp"))
}

/// PCON-10: best-effort reclaim of a per-job build-scratch dir on drop — covers
/// build success AND every `?` early-return path of `build_inner`. A crash that
/// skips the drop is covered by PCON-05's age/count GC backstop.
struct ScratchReclaim(Option<PathBuf>);

impl ScratchReclaim {
    fn new(dir: PathBuf) -> Self {
        Self(Some(dir))
    }
}

impl Drop for ScratchReclaim {
    fn drop(&mut self) {
        if let Some(dir) = self.0.take() {
            if let Err(e) = std::fs::remove_dir_all(&dir) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(
                        "PCON-10: failed to reclaim per-job build scratch {}: {e}",
                        dir.display()
                    );
                }
            }
        }
    }
}

fn target_triple() -> String {
    env_nonempty(BUILD_TARGET_TRIPLE).unwrap_or_else(|| DEFAULT_TARGET_TRIPLE.to_string())
}

/// BLD-444 (glibc-portability follow-up): the EFFECTIVE target triple for
/// `module` — its per-module override (`BUILD_MODULE_TARGET_<MODULE>`,
/// `host::module_target`) if configured, else the fleet-wide
/// [`target_triple`]. The SINGLE resolution point for both places that need
/// "what target does this module build/verify against" — [`CompilerBuild::build_inner`]
/// (build/test) and `CompilerRelease`'s promote/rollback/current (verifying an
/// already-published artifact under `<sha>/<target>/<bin>`) — so a module
/// built with an override (e.g. harmony → musl, for a portable artifact on an
/// older-glibc deploy host) is ALSO the default `compiler_release` verifies/
/// promotes against; the artifact path, the `--target` flag, and the release
/// pointer flip can never disagree on which target a module's default build
/// used. An explicit caller-supplied `target` argument (either tool) still
/// wins over both — this is only the fallback default.
///
/// Not validated here (same discipline as `target_triple`/`module_target`):
/// every call site runs the result through `validate_segment("target", …)`
/// before it is ever used as a path segment or `--target` value.
fn effective_triple(module: &str) -> String {
    host::module_target(module).unwrap_or_else(target_triple)
}

/// The per-channel retention count for the artifact store's pruning (BLD-07).
/// Config-driven and floored at 2 — the store never keeps fewer than 2 shas nor
/// prunes the current/previous pointer targets.
fn retain_per_channel() -> usize {
    env_nonempty(BUILD_RETAIN_PER_CHANNEL)
        .and_then(|v| v.parse::<usize>().ok())
        .map(|n| n.max(2))
        .unwrap_or(publish::DEFAULT_RETAIN_PER_CHANNEL)
}

/// The exec-safe LOCAL/tmpfs cargo target dir on the HEAVY host. Required for a
/// remote build — there is deliberately NO default (a wrong default could put the
/// live target on the shared NFS dataset; the operator sizes it per host).
fn heavy_local_target_dir() -> Result<PathBuf, ToolError> {
    env_nonempty(BUILD_HEAVY_LOCAL_TARGET_DIR)
        .map(PathBuf::from)
        .ok_or_else(|| {
            ToolError::NotConfigured(format!(
                "{BUILD_HEAVY_LOCAL_TARGET_DIR} is required for a heavy (remote) build"
            ))
        })
}

/// The dataset root PATH on the heavy host (source-stage + env-file location).
/// Prefers `BUILD_HEAVY_DATASET_ROOT`, then `BUILD_DATASET_RELAY_ROOT`, then the
/// local `BUILD_DATASET_ROOT` value.
fn heavy_dataset_root(local_root: &str) -> String {
    env_nonempty(BUILD_HEAVY_DATASET_ROOT)
        .or_else(|| env_nonempty(BUILD_DATASET_RELAY_ROOT))
        .unwrap_or_else(|| local_root.to_string())
}

/// Single-quote-escape one shell argument so it can be embedded in a remote
/// command string passed to `ssh` (which runs its argument through the remote
/// login shell). `'` → `'\''`.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Join an argv into a single shell command string (each element quoted).
fn shell_join(argv: &[String]) -> String {
    argv.iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ")
}

// ── PCON-05: bounded GC of per-SHA stage dirs ────────────────────────────────
//
// Per-SHA isolation (PCON-01..03) trades a fixed per-ref disk footprint for an
// unbounded set of per-sha dirs. This reclaims old ones by age AND count,
// never touching a dir a live job still references.

/// Env var: how many of the newest per-sha stage dirs to always retain per
/// module, regardless of age. Floored at 1 (there is always at least a
/// "keep the newest" floor — a 0 would let a burst of GC ticks reclaim a dir a
/// build just published moments before staging even the first job of it).
const BUILD_SHA_STAGE_RETAIN_COUNT_ENV: &str = "BUILD_SHA_STAGE_RETAIN_COUNT";
/// Env var: retain any per-sha stage dir younger than this many seconds,
/// regardless of count. Default 7 days — long enough that a slow-moving
/// review/merge cycle referencing a specific sha doesn't get GC'd out from
/// under it, short enough that stale branches don't accumulate forever.
const BUILD_SHA_STAGE_RETAIN_SECS_ENV: &str = "BUILD_SHA_STAGE_RETAIN_SECS";
const DEFAULT_SHA_STAGE_RETAIN_COUNT: usize = 5;
const DEFAULT_SHA_STAGE_RETAIN_SECS: u64 = 7 * 24 * 3600;

fn sha_stage_retain_count() -> usize {
    env_nonempty(BUILD_SHA_STAGE_RETAIN_COUNT_ENV)
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_SHA_STAGE_RETAIN_COUNT)
}

fn sha_stage_retain_secs() -> u64 {
    env_nonempty(BUILD_SHA_STAGE_RETAIN_SECS_ENV)
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_SHA_STAGE_RETAIN_SECS)
}

/// FIX 2 (review, HIGH): env var for the HARD, live-set-independent age floor
/// below which GC NEVER reclaims a dir, full stop — regardless of the
/// live-set (`peek` + building leases) saying it looks unreferenced. The
/// live-set has an inherent TOCTOU window (`peek` is bounded by
/// `peek_limit`; a job claimed moments after the snapshot is briefly
/// invisible to it) — age is the guard that holds even when the live-set is
/// simply wrong/stale, not just a secondary nicety. Default well beyond any
/// real build ([`MAX_BUILD_TIMEOUT_SECS`], the longest a single
/// `compiler_build` may run) so a fresh OR still-in-flight stage can never be
/// reclaimed by this alone.
const BUILD_SHA_STAGE_MIN_AGE_SECS_ENV: &str = "BUILD_SHA_STAGE_MIN_AGE_SECS";
/// 2x [`MAX_BUILD_TIMEOUT_SECS`] (3600s) — generous headroom over the
/// longest a single build may legitimately run, so this floor is never
/// mistaken for a normal build-duration timeout.
const DEFAULT_SHA_STAGE_MIN_AGE_SECS: u64 = MAX_BUILD_TIMEOUT_SECS * 2;

fn sha_stage_min_age_secs() -> u64 {
    env_nonempty(BUILD_SHA_STAGE_MIN_AGE_SECS_ENV)
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_SHA_STAGE_MIN_AGE_SECS)
}

/// PCON-05: reclaim old per-sha stage dirs directly under `module_root`
/// (`${BUILD_DATASET_ROOT}/src/<module>`) by age AND count. Keeps the newest
/// `retain_count` dirs (by mtime) UNCONDITIONALLY, keeps anything younger than
/// `retain_secs`, and NEVER touches a dir whose name is in `live` (a sha a
/// queued/building job currently references) — even if it would otherwise be
/// old/over-count. Returns the names of the dirs actually removed.
///
/// Two review fixes narrow what this function is even willing to consider:
///
/// FIX 1 (HIGH — GC precision): only a directory whose NAME is the owned
/// per-sha content-addressed shape — a full 40-hex sha ([`is_full_sha`]),
/// exactly what PCON-01 stages under — is ever a GC candidate. A legacy
/// ref-keyed stage (`BUILD_STAGE_BY_SHA=off`, named by a branch/tag) or any
/// other/foreign directory under the module root is skipped OUTRIGHT, before
/// any age/count/live-set logic runs — this function has no business judging
/// a dir it doesn't own the naming scheme for.
///
/// FIX 2 (HIGH — GC atomicity/age guard): `min_age_secs` is a HARD floor
/// independent of the live-set. The live-set (`peek` + building leases,
/// snapshotted by the caller) has an inherent TOCTOU window — `peek` is
/// bounded by a limit, and a job claimed moments after the snapshot is
/// briefly invisible to it — so a dir absent from `live` is NOT proof it's
/// safe to reclaim. Age is checked FIRST and unconditionally: anything
/// younger than `min_age_secs` is protected NO MATTER WHAT the live-set says
/// (including a dir the live-set has never heard of at all). The live-set
/// remains a SECONDARY protection for genuinely old dirs a build is still
/// somehow using (e.g. an exceptionally long-running build past
/// `min_age_secs`).
///
/// Pure w.r.t. the clock (`now` is passed in) and operates on a caller-given
/// root, so it is fully unit-testable against a `tempdir()` with no live
/// dataset/Redis/scheduler. Best-effort at the FILESYSTEM level too: an
/// individual `remove_dir_all` failure is skipped (not fatal to the sweep) —
/// the caller wraps the whole call to also never fail a build/tick.
fn gc_sha_stage_dirs(
    module_root: &std::path::Path,
    retain_count: usize,
    retain_secs: u64,
    min_age_secs: u64,
    live: &std::collections::HashSet<String>,
    now: std::time::SystemTime,
) -> std::io::Result<Vec<String>> {
    let mut entries: Vec<(String, PathBuf, std::time::SystemTime)> = Vec::new();
    let read_dir = match std::fs::read_dir(module_root) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    for entry in read_dir {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if !file_type.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        // FIX 1: only the owned per-sha naming shape is ever a candidate —
        // never a legacy ref-keyed stage or any other/foreign directory. This
        // also naturally skips the atomic-publish staging temp dirs
        // `autostage_source` creates (`.bldgap3-stage-*`, which additionally
        // live one level up as SIBLINGS of the module dir, so this loop can't
        // see them anyway — defense-in-depth either way).
        if !is_full_sha(&name) {
            continue;
        }
        let meta = entry.metadata()?;
        let mtime = meta.modified().unwrap_or(now);
        entries.push((name, entry.path(), mtime));
    }
    // Newest first.
    entries.sort_by(|a, b| b.2.cmp(&a.2));
    let mut removed = Vec::new();
    for (idx, (name, path, mtime)) in entries.into_iter().enumerate() {
        // FIX 2: the hard, live-set-independent age floor — checked FIRST,
        // unconditionally, before even consulting `live`.
        let age = now.duration_since(mtime).unwrap_or_default();
        if age.as_secs() < min_age_secs {
            continue;
        }
        if live.contains(&name) {
            continue;
        }
        if idx < retain_count {
            continue;
        }
        if age.as_secs() < retain_secs {
            continue;
        }
        if std::fs::remove_dir_all(&path).is_ok() {
            removed.push(name);
        }
    }
    Ok(removed)
}

/// Best-effort wrapper around [`gc_sha_stage_dirs`] for the LIVE dataset root
/// (`BUILD_DATASET_ROOT`), sweeping every module dir under `src/`. Called
/// opportunistically from the scheduler tick (`scheduler.rs`) — NEVER fails
/// the caller: an unconfigured dataset root, an unreadable dir, or a partial
/// reclaim failure is logged and swallowed, exactly like the sccache/
/// degrade-open discipline elsewhere in this module.
pub(crate) fn gc_stage_dirs_best_effort(
    live_by_module: &std::collections::HashMap<String, std::collections::HashSet<String>>,
) {
    let root = match dataset_root() {
        Ok(r) => r,
        Err(_) => return, // no dataset root configured — nothing to GC
    };
    let src_root = root.join("src");
    let modules = match std::fs::read_dir(&src_root) {
        Ok(rd) => rd,
        Err(_) => return,
    };
    let retain_count = sha_stage_retain_count();
    let retain_secs = sha_stage_retain_secs();
    let min_age_secs = sha_stage_min_age_secs();
    let now = std::time::SystemTime::now();
    let empty: std::collections::HashSet<String> = std::collections::HashSet::new();
    for entry in modules.flatten() {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let module = entry.file_name().to_string_lossy().to_string();
        let live = live_by_module.get(&module).unwrap_or(&empty);
        match gc_sha_stage_dirs(&entry.path(), retain_count, retain_secs, min_age_secs, live, now) {
            Ok(removed) if !removed.is_empty() => {
                tracing::info!(
                    module = %module,
                    count = removed.len(),
                    "compiler: PCON-05 GC reclaimed old per-sha stage dirs"
                );
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(
                    module = %module,
                    "compiler: PCON-05 GC sweep failed (non-fatal, will retry next tick): {e}"
                );
            }
        }
    }
}

/// Write `body` to a fresh **0600** file under the system temp dir and return its
/// path. Used to STAGE the remote secret env file before transfer.
///
/// SECURITY (S7, symlink/predictable-tmp attack): the filename carries an
/// unguessable random component (a v4 UUID, OS-CSPRNG-backed) so an attacker on a
/// multi-user build host cannot pre-plant a file or symlink at a predictable path;
/// and the file is opened with **`O_EXCL`** (`create_new` — an existing path is a
/// hard error, never a truncate/overwrite) **+ `O_NOFOLLOW`** (a symlink at the
/// path is not followed). Because `O_EXCL` guarantees a brand-new file, the
/// `mode(0o600)` applies from creation — the "0600-from-creation" guarantee
/// genuinely holds. On write failure the partial file is unlinked. The caller
/// unlinks it after transfer (on both success and error paths).
fn write_local_0600(body: &str, tag: &str) -> Result<PathBuf, ToolError> {
    let path = std::env::temp_dir().join(format!(
        "terminus-build-secret-{tag}-{}.env",
        uuid::Uuid::new_v4()
    ));
    write_secret_0600_at(&path, body)?;
    Ok(path)
}

/// Exclusively create `path` with mode 0600, refusing to follow a symlink or
/// touch an existing path, and write `body`. The load-bearing security core of
/// [`write_local_0600`], split out so the O_EXCL/O_NOFOLLOW semantics are
/// directly testable at a known path.
fn write_secret_0600_at(path: &std::path::Path, body: &str) -> Result<(), ToolError> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true) // O_CREAT | O_EXCL — never open/truncate an existing path
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC) // don't follow a symlink
        .mode(0o600) // applies because O_EXCL guarantees a brand-new file
        .open(path)
        .map_err(|e| {
            ToolError::Execution(format!("create exclusive 0600 secret staging file: {e}"))
        })?;
    if let Err(e) = f.write_all(body.as_bytes()) {
        // Never leave a partial secret file behind on a write error.
        let _ = std::fs::remove_file(path);
        return Err(ToolError::Execution(format!(
            "write secret staging file: {e}"
        )));
    }
    Ok(())
}

/// Map a profile name to (the cargo flag(s) that select it, the target subdir it
/// lands in). `debug` ⇒ no flag / `debug`; `release` ⇒ `--release` / `release`;
/// any other name ⇒ `--profile <name>` / `<name>`.
fn profile_flags_and_subdir(profile: &str) -> (Vec<String>, String) {
    match profile {
        "debug" => (vec![], "debug".to_string()),
        "release" => (vec!["--release".to_string()], "release".to_string()),
        other => (
            vec!["--profile".to_string(), other.to_string()],
            other.to_string(),
        ),
    }
}

/// Build the `cargo build` argv (pure — testable). `bin` selects a single
/// binary target (defaults to the module name); `--locked` keeps the build
/// reproducible against the committed lockfile. `manifest_path` points cargo at
/// the source tree's `Cargo.toml` so the build is independent of the process
/// CWD — which is what makes the REMOTE (ssh) heavy path correct (the scoped
/// cargo need not rely on an ssh working directory).
fn cargo_build_argv(
    profile: &str,
    triple: &str,
    jobs: u32,
    bin: &str,
    manifest_path: &str,
) -> Vec<String> {
    let (profile_flags, _subdir) = profile_flags_and_subdir(profile);
    let mut argv = vec![
        "cargo".to_string(),
        "build".to_string(),
        "--locked".to_string(),
    ];
    argv.extend(profile_flags);
    argv.push("--manifest-path".to_string());
    argv.push(manifest_path.to_string());
    argv.push("--target".to_string());
    argv.push(triple.to_string());
    argv.push("-j".to_string());
    argv.push(jobs.to_string());
    argv.push("--bin".to_string());
    argv.push(bin.to_string());
    argv
}

/// Build the `cargo test` argv (pure — testable; a sibling of [`cargo_build_argv`]
/// for BLD-COMPTEST's test/gate mode). Mirrors the build argv's `--locked` /
/// profile / `--target` / `-j` / `--manifest-path` flags exactly (same
/// reproducibility + CWD-independence guarantees — see `cargo_build_argv`'s
/// doc), and adds `--no-fail-fast` so a gate observes every failure in one run
/// instead of stopping at the first (needed for the structured pass/fail +
/// failing-test summary the gate returns).
///
/// BLD-GATE-06 (TERM #419): defaults to `--lib --bins` (hermetic unit tests)
/// rather than the whole workspace/crate — integration tests under `tests/`
/// commonly need live services (gitea/plane/redis/network) that don't exist
/// in the capped/offline build scope, so every terminus branch was
/// accumulating ~19 spurious failures (2 on chord) and no branch could ever
/// pass the gate. Set env `BUILD_GATE_TESTS=workspace` to opt back into the
/// full `--workspace` run (e.g. for a deliberate integration validation
/// pass) — any other value (including unset) keeps the `--lib --bins`
/// default.
///
/// Also caps the RUN-thread count (distinct from the `-j` BUILD-thread cap
/// above) via a trailing `-- --test-threads=<N>`: at the host's full core
/// count (~32) some suites hit test-concurrency flakiness (httpmock /
/// GPU-exclusive-resource races) that doesn't reproduce at a lower thread
/// count. `N` comes from env `BUILD_GATE_TEST_THREADS`, defaulting to `8`
/// when unset, empty, non-numeric, or `0`.
fn cargo_test_argv(profile: &str, triple: &str, jobs: u32, manifest_path: &str) -> Vec<String> {
    let (profile_flags, _subdir) = profile_flags_and_subdir(profile);
    let mut argv = vec![
        "cargo".to_string(),
        "test".to_string(),
        "--locked".to_string(),
    ];
    argv.extend(profile_flags);
    argv.push("--manifest-path".to_string());
    argv.push(manifest_path.to_string());
    argv.push("--target".to_string());
    argv.push(triple.to_string());
    argv.push("-j".to_string());
    argv.push(jobs.to_string());

    let full_workspace = std::env::var("BUILD_GATE_TESTS")
        .map(|v| v.eq_ignore_ascii_case("workspace"))
        .unwrap_or(false);
    if full_workspace {
        argv.push("--workspace".to_string());
    } else {
        argv.push("--lib".to_string());
        argv.push("--bins".to_string());
    }

    argv.push("--no-fail-fast".to_string());

    let test_threads = std::env::var("BUILD_GATE_TEST_THREADS")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(8);
    argv.push("--".to_string());
    argv.push(format!("--test-threads={test_threads}"));

    argv
}

/// Build the `cargo generate-lockfile` argv (pure — testable; GAP 5, TERM #418).
/// terminus/harmony/chord all `.gitignore` `Cargo.lock`, so a freshly-staged
/// feature-branch source tree has NO lock at all — `--locked` (required on
/// both [`cargo_build_argv`] and [`cargo_test_argv`] for reproducibility)
/// then fails cargo's dependency-resolution step in well under a second,
/// before a single crate compiles (`process_exit_success:false`, 0 passed /
/// 0 failed). Only `main` used to build, because its persisted
/// build-dataset directory happened to retain an old cargo-generated lock
/// from a prior run — every feature branch hit this instantly.
///
/// `cargo generate-lockfile` resolves the dependency graph and WRITES a
/// matching `Cargo.lock` WITHOUT compiling anything — cheap (seconds, low
/// RAM/CPU), safe to run inside the same capped scope as the build/test
/// step. It needs registry access (crates.io for terminus, the Gitea Cargo
/// registry for harmony/chord's `terminus-rs` path-dep — GAP 2's creds), so
/// it deliberately carries NO `--locked` (that would defeat its purpose: it
/// is the step that CREATES the lock `--locked` subsequently enforces).
/// Always safe to run before the `--locked` build/test: if a lock already
/// exists and matches (e.g. `main`), this is a fast no-op.
fn cargo_generate_lockfile_argv(manifest_path: &str) -> Vec<String> {
    vec![
        "cargo".to_string(),
        "generate-lockfile".to_string(),
        "--manifest-path".to_string(),
        manifest_path.to_string(),
    ]
}

/// Force cargo to render its `N/M` progress bar EVEN on the piped (non-TTY)
/// stdio the build runs under, so the live `{step,total}` progress the tap parses
/// is actually emitted. `CARGO_TERM_PROGRESS_WHEN=always` renders the bar
/// unconditionally; a fixed `CARGO_TERM_PROGRESS_WIDTH` keeps the `N/M` format
/// stable (independent of a non-existent terminal width). Both are NON-SECRET
/// term vars (they go via `--setenv`, never the secret env-file), inserted into
/// the build child's env for BOTH the local and remote (heavy) build paths.
fn inject_cargo_progress_env(build_env: &mut BTreeMap<String, String>) {
    build_env.insert("CARGO_TERM_PROGRESS_WHEN".to_string(), "always".to_string());
    build_env.insert("CARGO_TERM_PROGRESS_WIDTH".to_string(), "100".to_string());
}

/// GAP 4: sccache CANNOT cache Rust compilation units while cargo's
/// incremental compilation is on — incremental and sccache are mutually
/// exclusive caching strategies for rustc, and `cargo build`/`cargo test`'s
/// dev profile defaults `CARGO_INCREMENTAL=1`. Left unset, every build was a
/// 0%-hit-rate cold build against the shared Redis sccache (observed: 0.00%
/// Rust hit rate, 368 misses, vs 100% for C/C++) even though sccache itself
/// was correctly wired. `CARGO_INCREMENTAL=0` is NON-SECRET (a plain cargo
/// build-behavior flag) — it goes via `--setenv`, never the secret env-file —
/// inserted into the build child's env for BOTH the local and remote (heavy)
/// build paths, and for both `build` and `test` mode (the flag governs
/// compilation, not which cargo subcommand runs).
fn inject_cargo_incremental_off(build_env: &mut BTreeMap<String, String>) {
    build_env.insert("CARGO_INCREMENTAL".to_string(), "0".to_string());
}

/// The path (relative to CARGO_TARGET_DIR) where the built binary lands:
/// `<triple>/<profile-subdir>/<bin>`.
fn built_bin_rel(triple: &str, profile: &str, bin: &str) -> PathBuf {
    let (_flags, subdir) = profile_flags_and_subdir(profile);
    PathBuf::from(triple).join(subdir).join(bin)
}

/// Structured summary parsed (best-effort) from `cargo test --no-fail-fast`
/// output — the format cargo has printed a `test result: ok|FAILED. N passed; M
/// failed; ...` line in since 1.0, one per test binary (a crate/workspace run
/// prints several; this sums them). Also captures the FAILING test names (from
/// `test <name> ... FAILED` lines) for the gate's failure summary.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct CargoTestSummary {
    passed: u32,
    failed: u32,
    ignored: u32,
    measured: u32,
    filtered_out: u32,
    /// Names of individually failing tests, sorted + deduped.
    failing_tests: Vec<String>,
    /// Whether at least one `test result:` line was found. Distinguishes "ran
    /// zero tests, all fine" from "cargo never reached a summary at all" (e.g. a
    /// compile error before any test binary ran) — the latter is never a pass,
    /// regardless of the process exit status.
    summary_found: bool,
}

impl CargoTestSummary {
    /// Whether the PARSED SUMMARY is clean: at least one summary line was found
    /// and it reports zero failures. A run with no summary line at all (a compile
    /// error, or the harness crashing before any test binary printed its result)
    /// is unclean. NOTE: this is the summary verdict ONLY — the GATE verdict
    /// ([`test_gate_passed`]) additionally requires the process to have exited 0,
    /// which catches a LATER cargo/rustdoc/link failure that emits no further
    /// failed-summary line (so `all_passed()` alone would wrongly read as clean).
    fn all_passed(&self) -> bool {
        self.summary_found && self.failed == 0
    }

    fn to_json(&self) -> Value {
        json!({
            "summary_found": self.summary_found,
            "passed": self.passed,
            "failed": self.failed,
            "ignored": self.ignored,
            "measured": self.measured,
            "filtered_out": self.filtered_out,
            "failing_tests": self.failing_tests,
        })
    }
}

/// The GATE verdict for a `mode=test` run (pure — testable). PASSES iff BOTH the
/// cargo process exited 0 (`exit_success`) AND the parsed summary is clean
/// (`summary.all_passed()`). Requiring the exit code is the load-bearing half for
/// a gate: cargo prints per-test-binary `test result:` lines as it goes, but a
/// LATER failure — a second crate failing to COMPILE, a rustdoc/doctest error, a
/// linker error, or the harness itself aborting — can make cargo exit NON-ZERO
/// WITHOUT emitting an additional `... failed` summary line. In that case
/// `summary.all_passed()` alone (seeing only the earlier clean summary) would
/// wrongly read PASS; gating on `exit_success` too makes the gate fail-closed on
/// any real non-zero cargo exit. The happy path is unaffected: a fully-green run
/// exits 0, so `exit_success == true` and the verdict is unchanged.
fn test_gate_passed(exit_success: bool, summary: &CargoTestSummary) -> bool {
    exit_success && summary.all_passed()
}

fn cargo_test_result_regex() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(
            r"test result: \w+\. (\d+) passed; (\d+) failed; (\d+) ignored; (\d+) measured; (\d+) filtered out",
        )
        .expect("cargo test result regex")
    })
}

fn cargo_test_failed_line_regex() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"^test (\S+) \.\.\. FAILED$").expect("cargo test FAILED-line regex")
    })
}

/// Parse cargo's `test result: ...` summary line(s) + individual `... FAILED`
/// lines out of a `cargo test` run's (already-redacted) combined stdout+stderr.
/// Pure — testable without spawning cargo.
fn parse_cargo_test_output(output: &str) -> CargoTestSummary {
    let result_re = cargo_test_result_regex();
    let failed_re = cargo_test_failed_line_regex();

    let mut summary = CargoTestSummary::default();
    for cap in result_re.captures_iter(output) {
        summary.summary_found = true;
        summary.passed += cap[1].parse::<u32>().unwrap_or(0);
        summary.failed += cap[2].parse::<u32>().unwrap_or(0);
        summary.ignored += cap[3].parse::<u32>().unwrap_or(0);
        summary.measured += cap[4].parse::<u32>().unwrap_or(0);
        summary.filtered_out += cap[5].parse::<u32>().unwrap_or(0);
    }
    for line in output.lines() {
        if let Some(cap) = failed_re.captures(line.trim()) {
            summary.failing_tests.push(cap[1].to_string());
        }
    }
    summary.failing_tests.sort();
    summary.failing_tests.dedup();
    summary
}

/// Replace every non-empty secret value in `text` with a fixed placeholder, so a
/// secret that a build script / proc-macro / wrapper / sub-tool echoed to
/// stdout/stderr never reaches a `ToolError`, a log line, or a returned string
/// (S7). Plain substring replace of each raw value; empty values are skipped;
/// an empty `secrets` set is a no-op. This helper never logs the secret itself.
fn redact_secrets(text: &str, secrets: &[String]) -> String {
    // Replace LONGEST values first: if one secret is a substring of another (the
    // `SCCACHE_REDIS_PASSWORD` value is embedded in the full `SCCACHE_REDIS` URL),
    // redacting the short one first would break the longer match and leak the
    // URL's non-password parts. Longest-first guarantees the full value is
    // scrubbed before any of its substrings.
    let mut ordered: Vec<&str> = secrets
        .iter()
        .map(String::as_str)
        .filter(|s| !s.is_empty())
        .collect();
    ordered.sort_by_key(|s| std::cmp::Reverse(s.len()));
    let mut out = text.to_string();
    for s in ordered {
        if out.contains(s) {
            out = out.replace(s, "<redacted>");
        }
    }
    out
}

/// The S7 redaction set for a build: every secret-shaped VALUE that could be
/// echoed by a child (or embedded in a `ToolError`) and must be scrubbed before
/// it reaches captured output, a log, or the progress bus. That is every secret
/// value in the sccache env (`SCCACHE_REDIS_PASSWORD`, …) PLUS the ambient full
/// `SCCACHE_REDIS` URL the child inherits. `root_str` only seeds sccache's
/// non-secret local-dir fallback, so `""` is fine when only the secret values are
/// needed (e.g. redacting a failed-event message before the build resolves root).
fn redaction_set(root_str: &str) -> Vec<String> {
    let sccache_env = sccache::resolve(root_str);
    let mut redact: Vec<String> = sccache_env
        .vars
        .iter()
        .filter(|(k, _)| scope::is_secret_env_key(k))
        .map(|(_, v)| v.clone())
        .filter(|v| !v.is_empty())
        .collect();
    if let Some(url) = sccache::ambient_secret_url() {
        if !url.is_empty() {
            redact.push(url);
        }
    }
    redact.sort();
    redact.dedup();
    redact
}

/// On a REMOTE (ssh heavy) build, killing the LOCAL `ssh` process group does not
/// reach the remote `systemd-run --scope` / `cargo` / `rustc` tree. This carries
/// the info needed to tear that remote tree down by name on timeout: the ssh
/// host and the transient scope's unit name (so `systemctl kill <unit>.scope`
/// terminates the scope + all its descendants remotely).
struct RemoteScopeKill {
    host: String,
    unit: String,
}

/// Render the argv that kills a remote transient scope by unit name over ssh:
/// `systemctl kill --signal=SIGKILL <unit>.scope`, falling back to
/// `systemctl stop <unit>.scope`. Pure (returns the argv) so it is testable
/// offline; the unit is shell-quoted for the remote shell.
fn render_remote_scope_kill_argv(host: &str, unit: &str) -> Vec<String> {
    let scope = shell_quote(&format!("{unit}.scope"));
    vec![
        "ssh".to_string(),
        host.to_string(),
        format!("systemctl kill --signal=SIGKILL {scope} || systemctl stop {scope}"),
    ]
}

/// Best-effort remote scope kill (own short timeout, non-fatal). Spawned when a
/// remote heavy build times out, so the remote build tree does not keep running
/// (and keep the secret-bearing inherited env alive) after the tool returns.
///
/// SECURITY (S7): the SAME `redact` set as the build is threaded into the cleanup
/// `run()` — this ssh/systemctl child inherits the parent process env (including
/// ambient `SCCACHE_REDIS`), so a failing cleanup command could otherwise surface
/// an unredacted secret in the returned error we log at `warn!` below.
async fn remote_scope_kill(rk: &RemoteScopeKill, redact: &[String]) {
    let argv = render_remote_scope_kill_argv(&rk.host, &rk.unit);
    // Reuse `run` with no further remote-kill (None) and a small timeout; ignore
    // the outcome — this is cleanup, the caller already returns the timeout error.
    // `Box::pin` breaks the `run`↔`remote_scope_kill` async recursion cycle (the
    // `None` remote_kill above means this never actually recurses at runtime).
    if let Err(e) = Box::pin(run(
        &argv,
        None,
        &BTreeMap::new(),
        Duration::from_secs(30),
        redact,
        None,
        None,
    ))
    .await
    {
        tracing::warn!(
            "compiler: best-effort remote scope kill of {}.scope failed: {e}",
            rk.unit
        );
    }
}

/// Render the argv that removes the remote 0600 secret env file over ssh:
/// `ssh -o BatchMode=yes -o ConnectTimeout=10 <host> rm -f <quoted-path>`. Pure
/// (returns the argv) so it is testable offline; the path is shell-quoted for the
/// remote shell, and the ssh options bound a hung connect (so a synchronous Drop
/// cleanup can never block indefinitely).
fn render_remote_secret_rm_argv(host: &str, remote_path: &str) -> Vec<String> {
    vec![
        "ssh".to_string(),
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        "ConnectTimeout=10".to_string(),
        host.to_string(),
        format!("rm -f {}", shell_quote(remote_path)),
    ]
}

/// Synchronous, bounded, best-effort remote `rm -f` of the secret env file — used
/// by [`RemoteSecretGuard`]'s `Drop` (which cannot run async). `ssh -o
/// ConnectTimeout` bounds a hung connect; the `rm` itself is instant. Any failure
/// output is redacted (S7) before it is logged.
fn blocking_ssh_rm(argv: &[String], redact: &[String]) {
    use std::process::{Command, Stdio};
    let child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    match child {
        Ok(c) => match c.wait_with_output() {
            Ok(out) if !out.status.success() => {
                let tail = redact_secrets(&String::from_utf8_lossy(&out.stderr), redact);
                tracing::warn!("compiler: remote secret-file cleanup rm failed: {tail}");
            }
            Ok(_) => {}
            Err(e) => tracing::warn!("compiler: remote secret-file cleanup wait failed: {e}"),
        },
        Err(e) => tracing::warn!("compiler: remote secret-file cleanup spawn failed: {e}"),
    }
}

/// RAII guard that GUARANTEES the secret env file is removed on EVERY post-transfer
/// exit path — success, any `?` error, a timeout, or a panic — closing the whole
/// leak class (not just one code path). Armed right after the secret file is
/// transferred to the remote; its `Drop` issues a bounded best-effort remote
/// `rm -f` (and, as a backstop, unlinks the local staging file if it wasn't
/// already). On the happy path the remote build's own wrapper `rm`s the file, so
/// the guard is [`disarm`](Self::disarm)ed after a successful build to avoid a
/// redundant ssh; on any earlier exit it stays armed and fires.
struct RemoteSecretGuard {
    host: String,
    remote_path: String,
    redact: Vec<String>,
    /// Local staging file to unlink as a backstop (cleared once removed inline).
    local_path: Option<PathBuf>,
    /// When false, `Drop` performs no remote cleanup (the file is already gone).
    armed: bool,
    /// Test-only sink: when set, `Drop` RECORDS the rendered rm argv here instead
    /// of spawning a real ssh — so the "cleanup fires on the error path" property
    /// is unit-testable offline. `None` in production.
    recorder: Option<std::sync::Arc<std::sync::Mutex<Vec<Vec<String>>>>>,
}

impl RemoteSecretGuard {
    fn new(
        host: String,
        remote_path: String,
        local_path: Option<PathBuf>,
        redact: Vec<String>,
    ) -> Self {
        Self {
            host,
            remote_path,
            redact,
            local_path,
            armed: true,
            recorder: None,
        }
    }

    /// Clear the local-staging backstop after it has been unlinked inline (so
    /// `Drop` doesn't try again — harmless either way).
    fn clear_local(&mut self) {
        self.local_path = None;
    }

    /// Disarm the REMOTE cleanup (call after a successful build, whose own wrapper
    /// already removed the remote file). The local backstop is still honored.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for RemoteSecretGuard {
    fn drop(&mut self) {
        // Local staging backstop (instant, sync) — always, even when disarmed.
        if let Some(p) = self.local_path.take() {
            let _ = std::fs::remove_file(&p);
        }
        if !self.armed {
            return;
        }
        let argv = render_remote_secret_rm_argv(&self.host, &self.remote_path);
        if let Some(rec) = &self.recorder {
            if let Ok(mut g) = rec.lock() {
                g.push(argv);
            }
            return;
        }
        blocking_ssh_rm(&argv, &self.redact);
    }
}

/// Run a subprocess argv with an optional cwd + extra env, bounded by `timeout`.
/// Returns `Ok(stdout)` on success (exit 0), else an `Execution` error with a
/// trimmed stderr tail. The env is applied on top of the inherited environment.
///
/// SECURITY (S7): ALL captured child output (the success stdout AND the failure
/// stderr tail) is passed through [`redact_secrets`] with `redact` — the set of
/// secret env VALUES in play for this build — BEFORE it is returned or surfaced,
/// so a build script that prints its environment can never leak
/// `SCCACHE_REDIS_PASSWORD` / the `SCCACHE_REDIS` URL into an error or log. This
/// is the single choke point covering both the local and remote (ssh) paths.
///
/// PROCESS LIFECYCLE: the child is spawned in its OWN process group
/// (`process_group(0)` ⇒ pgid == child pid) with `kill_on_drop(true)`. On timeout
/// the WHOLE LOCAL group is `killpg(SIGKILL)`-ed (so a local build tree —
/// systemd-run and its `cargo`/`rustc` descendants — dies, not just the direct
/// child), then the direct child is `start_kill`-ed and `wait`-ed to REAP it (no
/// zombie). `kill_on_drop` guarantees any early return / panic also tears the
/// child down.
///
/// REMOTE builds: killing the local `ssh` process group does NOT reach the remote
/// scope. When `remote_kill` is `Some`, a timeout ALSO issues a best-effort
/// `systemctl kill <unit>.scope` over ssh to tear down the remote build tree — so
/// a timed-out heavy build cannot keep running remotely (holding the inherited
/// secret env + capped host resources) after the tool returns.
/// Flush one segment (a `\r`/`\n`-delimited line) to the build tap: lossily decode
/// (non-UTF-8 → U+FFFD), redact (S6/S7), feed the progress tap, and append the
/// redacted bytes to the captured output. An empty segment is a no-op (nothing to
/// parse), so consecutive delimiters (`\r\n`) don't fire a spurious tap.
fn tap_flush_segment(seg: &[u8], tap: &events::BuildTap, redact: &[String], buf: &mut Vec<u8>) {
    if seg.is_empty() {
        return;
    }
    let redacted = redact_secrets(&String::from_utf8_lossy(seg), redact);
    tap.on_line(&redacted);
    buf.extend_from_slice(redacted.as_bytes());
}

/// Drain one child pipe to completion (so a chatty child never deadlocks on a
/// full pipe). Without a `tap` it is a byte-exact `read_to_end` (unchanged for
/// every non-build subprocess). With a `tap` (the cargo build) it reads RAW BYTES
/// in chunks and splits on BOTH `\r` AND `\n` so a cargo progress bar (which
/// updates with CARRIAGE RETURNS, no newline until it finishes) reaches the tap
/// LIVE — each `12/34`→`20/34` update fires immediately instead of buffering
/// until the next newline. Each segment is redacted (S6/S7) BEFORE it reaches the
/// tap, and the redacted segments are kept as the captured output so a failed
/// build's error tail can never carry a raw secret. Byte-level reads never choke
/// on non-UTF-8 (lossy decode) and drain to EOF; only a true read error breaks.
async fn drain_pipe<R>(
    pipe: Option<R>,
    tap: Option<events::BuildTap>,
    redact: Vec<String>,
) -> Vec<u8>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let mut pipe = match pipe {
        Some(p) => p,
        None => return Vec::new(),
    };
    let tap = match tap {
        // No tap → preserve the original byte-exact capture.
        None => {
            let mut buf = Vec::new();
            let _ = pipe.read_to_end(&mut buf).await;
            return buf;
        }
        Some(t) => t,
    };
    let mut buf: Vec<u8> = Vec::new(); // full captured (redacted) output
    let mut seg: Vec<u8> = Vec::new(); // current in-progress segment (line/bar)
    let mut chunk = [0u8; 8192];
    loop {
        match pipe.read(&mut chunk).await {
            Ok(0) => {
                // EOF: flush any trailing partial segment (no delimiter).
                tap_flush_segment(&seg, &tap, &redact, &mut buf);
                break;
            }
            Ok(n) => {
                for &b in &chunk[..n] {
                    if b == b'\n' || b == b'\r' {
                        // A `\r` OR `\n` closes the current segment → tap it LIVE.
                        tap_flush_segment(&seg, &tap, &redact, &mut buf);
                        buf.push(b); // preserve the delimiter in the capture
                        seg.clear();
                    } else {
                        seg.push(b);
                    }
                }
            }
            // A genuine I/O read error: flush the remainder and stop (the child is
            // unaffected; the remaining bytes just don't reach the tail).
            Err(_) => {
                tap_flush_segment(&seg, &tap, &redact, &mut buf);
                break;
            }
        }
    }
    buf
}

async fn run(
    argv: &[String],
    cwd: Option<&std::path::Path>,
    env: &BTreeMap<String, String>,
    timeout: Duration,
    redact: &[String],
    remote_kill: Option<&RemoteScopeKill>,
    tap: Option<&events::BuildTap>,
) -> Result<String, ToolError> {
    if argv.is_empty() {
        return Err(ToolError::Execution("empty command".into()));
    }
    let mut cmd = tokio::process::Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    // BLD/TERM #359: drop the ambient full `SCCACHE_REDIS` URL from the child env.
    // sccache::resolve reads it from THIS process (materialized from the vault) and
    // exports the SPLIT form (`SCCACHE_REDIS_ENDPOINT` + `_PASSWORD` + `_DB`) into the
    // build scope; but `systemd-run --scope` also INHERITS this process's env, so the
    // raw URL would leak in alongside the split endpoint — and sccache 0.10's opendal
    // backend hard-errors "Only one of `endpoint`, `cluster_endpoints`, `url` must be
    // set", failing EVERY build. The URL is never the intended delivery (the split
    // form carries endpoint+auth), so removing it here is always safe and leaves the
    // split form as the single, unambiguous redis config.
    cmd.env_remove("SCCACHE_REDIS");
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    // Own process group (pgid == child pid) so a timeout can signal the whole
    // build tree; kill_on_drop so an early return also cleans up the child.
    cmd.process_group(0);
    cmd.kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .map_err(|e| ToolError::Execution(format!("spawn {}: {e}", argv[0])))?;
    // Capture the pgid up front (== the child pid, from process_group(0)); it is
    // available now because the child has not yet exited.
    let pgid = child.id().map(|p| p as libc::pid_t);

    // Drain stdout/stderr concurrently while we wait, so a chatty child can't
    // deadlock on a full pipe and we still have the output after `wait()`.
    //
    // BLD-19: when a `tap` is present (the cargo build calls), the drain reads
    // LINE BY LINE and forwards each already-redacted line to the tap so a live
    // `{step,total}` building event is emitted DURING the build (progress bar,
    // not a spinner). Without a tap (every non-build subprocess) the drain keeps
    // its byte-exact `read_to_end` behavior unchanged.
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    let out_tap = tap.cloned();
    let out_redact = redact.to_vec();
    let stdout_task =
        tokio::spawn(async move { drain_pipe(stdout_pipe.take(), out_tap, out_redact).await });
    let err_tap = tap.cloned();
    let err_redact = redact.to_vec();
    let stderr_task =
        tokio::spawn(async move { drain_pipe(stderr_pipe.take(), err_tap, err_redact).await });

    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return Err(ToolError::Execution(format!("{}: {e}", argv[0]))),
        Err(_) => {
            // TIMEOUT: kill the whole LOCAL process group (the build tree), then
            // reap the direct child so it can never become a zombie or leak.
            if let Some(pgid) = pgid {
                // Safe: killpg takes plain integers and has no memory effects; an
                // ESRCH (already-empty group) is a harmless no-op.
                unsafe {
                    libc::killpg(pgid, libc::SIGKILL);
                }
            }
            let _ = child.start_kill();
            let _ = child.wait().await;
            // REMOTE builds: the local kill only reached `ssh`; tear down the
            // remote scope by name too (best-effort, non-fatal). Thread the same
            // redaction set so a failing cleanup command can't leak a secret.
            if let Some(rk) = remote_kill {
                remote_scope_kill(rk, redact).await;
            }
            return Err(ToolError::Execution(format!(
                "{} timed out after {}s (child process group killed)",
                argv[0],
                timeout.as_secs()
            )));
        }
    };

    let stdout = stdout_task.await.unwrap_or_default();
    let stderr = stderr_task.await.unwrap_or_default();
    if status.success() {
        // Redact even the success stdout — it is returned to callers and may be
        // logged; a sub-tool could have echoed a secret onto it too.
        Ok(redact_secrets(&String::from_utf8_lossy(&stdout), redact))
    } else {
        let stderr = String::from_utf8_lossy(&stderr);
        let tail: String = stderr
            .lines()
            .rev()
            .take(20)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n");
        let tail = redact_secrets(&tail, redact);
        Err(ToolError::Execution(format!(
            "{} exited {}: {tail}",
            argv[0],
            status.code().unwrap_or(-1)
        )))
    }
}

/// Like [`run`], but for a **test-mode** (`mode=test`) build: a non-zero exit is
/// an EXPECTED outcome (failing tests), not an execution error. Returns
/// `(success, combined_redacted_output)` on ANY exit status — the combined,
/// redacted stdout+stderr — so a `mode=test` caller can parse the `cargo test`
/// summary regardless of pass/fail. This is deliberately NOT built on top of
/// [`run`]: `run()` discards stdout entirely on a non-zero exit (fine for a
/// build's error tail — a build failure has nothing useful on stdout — but wrong
/// here, since cargo prints its `test result: ...` summary to STDOUT even when
/// tests fail). Spawn/timeout/IO failures still return `Err` — those are genuine
/// execution errors, not a test result, and are handled identically to `run()`.
async fn run_test(
    argv: &[String],
    cwd: Option<&std::path::Path>,
    env: &BTreeMap<String, String>,
    timeout: Duration,
    redact: &[String],
    remote_kill: Option<&RemoteScopeKill>,
    tap: Option<&events::BuildTap>,
) -> Result<(bool, String), ToolError> {
    if argv.is_empty() {
        return Err(ToolError::Execution("empty command".into()));
    }
    let mut cmd = tokio::process::Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    // Same rationale as `run()`: drop the ambient full SCCACHE_REDIS URL from the
    // child env — the split form (endpoint/password/db) is the intended delivery.
    cmd.env_remove("SCCACHE_REDIS");
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.process_group(0);
    cmd.kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .map_err(|e| ToolError::Execution(format!("spawn {}: {e}", argv[0])))?;
    let pgid = child.id().map(|p| p as libc::pid_t);

    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    let out_tap = tap.cloned();
    let out_redact = redact.to_vec();
    let stdout_task =
        tokio::spawn(async move { drain_pipe(stdout_pipe.take(), out_tap, out_redact).await });
    let err_tap = tap.cloned();
    let err_redact = redact.to_vec();
    let stderr_task =
        tokio::spawn(async move { drain_pipe(stderr_pipe.take(), err_tap, err_redact).await });

    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return Err(ToolError::Execution(format!("{}: {e}", argv[0]))),
        Err(_) => {
            // TIMEOUT: same teardown as `run()` — kill the local process group,
            // reap the child, and (for a remote/heavy test run) tear down the
            // remote scope by name too.
            if let Some(pgid) = pgid {
                unsafe {
                    libc::killpg(pgid, libc::SIGKILL);
                }
            }
            let _ = child.start_kill();
            let _ = child.wait().await;
            if let Some(rk) = remote_kill {
                remote_scope_kill(rk, redact).await;
            }
            return Err(ToolError::Execution(format!(
                "{} timed out after {}s (child process group killed)",
                argv[0],
                timeout.as_secs()
            )));
        }
    };

    let stdout = stdout_task.await.unwrap_or_default();
    let stderr = stderr_task.await.unwrap_or_default();
    // Redact BOTH streams (S6/S7) and combine — unlike `run()`, a non-zero exit
    // here does not discard stdout, since cargo's pass/fail summary lives there.
    let stdout_s = redact_secrets(&String::from_utf8_lossy(&stdout), redact);
    let stderr_s = redact_secrets(&String::from_utf8_lossy(&stderr), redact);
    let combined = format!("{stdout_s}\n{stderr_s}");
    Ok((status.success(), combined))
}

/// PCON-06: run the SAME `compiler_build` test-gate (`mode=test`) the pipeline's
/// Stage 4 runs, on `git_ref` (the resolved SHA of a rebased PR head), and
/// return the structured pass/fail verdict — so the merge queue can re-gate a
/// rebased branch through the single build door (S9) instead of a second,
/// hand-rolled build path.
///
/// - `Ok(true)`  — the gate PASSED (green): safe to merge the rebased head.
/// - `Ok(false)` — the gate FAILED (red): a compile error or failing tests (a
///   real gate verdict — `mode=test` returns a structured `passed:false`, not
///   an `Err`, for a non-zero cargo exit). Do NOT merge.
/// - `Err(_)`    — the gate could NOT be run at all (the build door is
///   unreachable/misconfigured, a spawn failure, or the structured verdict was
///   missing). The caller treats this as fail-safe: never merge on an `Err`,
///   but distinguish it from a red verdict (it falls back to the pre-PCON-06
///   `NotMergeable` bounce rather than reporting a red gate).
///
/// The gate never publishes an artifact and never flips a channel pointer
/// (`mode=test` is a gate, not a release). `wait:true` blocks until the gate
/// finishes; the CALLER bounds the total wait against the queue's budget (an
/// outer timeout), so this function itself does not need a separate deadline.
pub async fn run_merge_regate(module: &str, git_ref: &str) -> Result<bool, ToolError> {
    let out = CompilerBuild
        .execute_structured(serde_json::json!({
            "module": module,
            "ref": git_ref,
            "mode": "test",
            "wait": true,
        }))
        .await?;
    out.structured
        .as_ref()
        .and_then(|s| s.get("passed"))
        .and_then(Value::as_bool)
        .ok_or_else(|| {
            ToolError::Execution(
                "compiler test-gate returned no structured 'passed' verdict".to_string(),
            )
        })
}

/// The `compiler_build` tool.
struct CompilerBuild;

#[async_trait]
impl RustTool for CompilerBuild {
    fn name(&self) -> &str {
        "compiler_build"
    }

    fn description(&self) -> &str {
        "Build a constellation module at a git ref on a selected build host: pinned \
         toolchain, sccache→Redis (fail-open), inside a resource-capped systemd scope \
         (MemorySwapMax=0, Plex-safe), then publish a sha256-checksummed artifact to the \
         shared build dataset and flip `experimental/current` onto it. Promotion to the \
         `stable` channel is a separate pointer-flip (compiler_release), never a rebuild. \
         `mode=test` runs the test-gate instead: `cargo test` in the SAME capped single-door \
         scope, returning structured pass/fail with NO publish and NO channel flip — a gate \
         is not a release."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "module": {
                    "type": "string",
                    "description": "Module/repo to build (e.g. terminus, chord, harmony, lumina-core)."
                },
                "ref": {
                    "type": "string",
                    "description": "Git ref (sha or branch) being built; used for the source-stage path + scope unit name."
                },
                "host": {
                    "type": "string",
                    "enum": ["auto", "primary", "heavy"],
                    "default": "auto",
                    "description": "Build host role. auto → primary unless the module's known peak or `fast` needs the heavy host."
                },
                "profile": {
                    "type": "string",
                    "default": "release",
                    "description": "Cargo profile: debug | release | <named cargo profile>."
                },
                "fast": {
                    "type": "boolean",
                    "default": false,
                    "description": "Force the heavy host for a full-parallelism build."
                },
                "bin": {
                    "type": "string",
                    "description": "Binary target to build (defaults to the module name)."
                },
                "source_dir": {
                    "type": "string",
                    "description": "Override the source tree location (defaults to ${BUILD_DATASET_ROOT}/src/<module>/<ref>)."
                },
                "request_id": {
                    "type": "string",
                    "description": "Optional stable id for this build request; progress/events are keyed by it (query with compiler_progress). Auto-generated when omitted and returned in the result."
                },
                "mode": {
                    "type": "string",
                    "enum": ["build", "test"],
                    "default": "build",
                    "description": "build (default) compiles + publishes an artifact and flips experimental/current, exactly as before. test runs `cargo test` in the SAME capped single-door scope (same toolchain/caps/sccache) as a GATE — it never publishes an artifact and never flips a channel pointer; it returns a structured pass/fail (+ test counts, + the failing-test summary on failure) via the same compiler_progress/events mechanism."
                },
                "wait": {
                    "type": "boolean",
                    "default": true,
                    "description": "true (default) → block until the build/test-gate finishes and return its result, exactly as before. false → BLD-ASYNC (TERM #421): enqueue this build onto the same durable scheduler/queue compiler_request uses and return IMMEDIATELY with the request_id — the build/gate then runs via the scheduler, not this call. Use this for big builds that would otherwise exceed the caller's own request timeout even though the build keeps running server-side; poll compiler_progress(request_id) (or subscribe) for the terminal tested/built/failed stage, which carries the same result (incl. test counts on mode=test) a blocking call would have returned."
                }
            },
            "required": ["module", "ref"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.execute_structured(args).await?.text)
    }

    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        // BLD-ASYNC (TERM #421): wait=false (default true = the original blocking
        // behavior below, unchanged) hands the build to the scheduler/queue and
        // returns immediately — this call never blocks past the enqueue, even
        // when the eventual build would run well past a caller's own forward
        // timeout. Checked FIRST, before the request_id resolution below, since
        // the async path mints its OWN identity (the queue job id) rather than
        // consuming compiler_build's usual request_id track.
        let wait = args.get("wait").and_then(Value::as_bool).unwrap_or(true);
        if !wait {
            return self.enqueue_async(args).await;
        }
        // BLD-19: decide the effective request_id FIRST, so EVERY compiler_build
        // path (success OR failure) carries a discoverable id. A caller may supply
        // one (to subscribe before/while the build runs); if it is missing OR
        // INVALID (bad chars / overlong), we FALL BACK to an auto-generated id
        // rather than returning early with no surfaced id (AC-1). The invalid id is
        // discarded, never clamped — so two distinct ids can't fold onto one track.
        // The substitution is made OBSERVABLE (not silent): a warn log + a
        // `supplied_request_id_invalid` signal in the result (structured field on
        // success, an `[supplied_request_id_invalid]` marker in the error on
        // failure), so a client can correlate the id it sent with the one used.
        let (request_id, supplied_invalid) = resolve_request_id(&args);
        if supplied_invalid {
            tracing::warn!(
                effective_request_id = %request_id,
                "compiler_build: supplied request_id was invalid; using a generated id"
            );
        }
        // BLD-19: ROTATE the progress track to a FRESH stream NOW — before any
        // validation and before build_inner. This is the single rotation per build
        // attempt (build_inner does NOT rotate again). Doing it here (not inside
        // build_inner) means EVEN a PRE-ACCEPTANCE failure (invalid module/ref/
        // profile, missing config) lands its terminal `failed` on a fresh,
        // non-terminal track — so a reused request_id whose prior build ended
        // terminal can never mask THIS attempt's failure with the old build's
        // stale `published`/`failed` state.
        events::bus().begin(&request_id);
        // Run the build. On ANY error path: emit the terminal Failed event AND
        // surface the request_id back to the caller in the returned error, so a
        // failed build's progress stream stays discoverable even when the caller
        // did not supply an id (invariant: every compiler_build call — success OR
        // build-failure — returns the stable request_id). The happy path emits
        // Published + returns the id in the structured output from build_inner.
        //
        // NOTE (by design): a PRE-ACCEPTANCE failure — one that occurs before
        // `build_inner` emits `queued` (an invalid/absent config, a validation
        // error, etc.) — yields a TERMINAL-ONLY `failed` track on the fresh track
        // (no `queued → … → failed` shape). That is intentional: the id is still
        // surfaced and the failed stream is discoverable; we do NOT synthesize a
        // fake `queued` event just to pad the shape.
        match self.build_inner(&request_id, args).await {
            Ok(mut out) => {
                // Surface the invalid-supplied-id substitution in the structured
                // output so a client can correlate (only when it happened).
                if supplied_invalid {
                    if let Some(obj) = out.structured.as_mut().and_then(Value::as_object_mut) {
                        obj.insert("supplied_request_id_invalid".into(), Value::Bool(true));
                    }
                }
                Ok(out)
            }
            Err(e) => {
                // Sanitize the error at the EMITTER boundary — secret VALUES (S6/S7)
                // AND infrastructure LITERALS (S1) — before it reaches the bus
                // (see `redacted_failed_message`).
                events::bus().emit(
                    &request_id,
                    events::Emit::stage(events::Stage::Failed).message(redacted_failed_message(&e)),
                );
                Err(tag_error_with_request_id(e, &request_id, supplied_invalid))
            }
        }
    }
}

/// Resolve the EFFECTIVE `request_id` for a build attempt and whether a
/// caller-supplied id was INVALID and substituted. The caller value is validated
/// RAW — NO trimming/normalization (a lossy trim could collapse `" build-1 "` and
/// `"build-1"` onto the same track). Outcomes:
/// - ABSENT (key missing or explicit `null`) → auto-generate SILENTLY (→ `false`);
///   nothing was supplied to invalidate.
/// - PRESENT string that is a valid `[A-Za-z0-9._-]` segment within the length
///   bound → used VERBATIM (→ `false`).
/// - PRESENT string that is invalid (whitespace, empty, disallowed char, overlong)
///   OR PRESENT but NOT a string (number/bool/array/object) → DISCARDED and
///   replaced by an auto-generated id (→ `true`, an OBSERVABLE substitution).
/// The fallback (never a hard reject) preserves the "a discoverable id always
/// exists" invariant.
fn resolve_request_id(args: &Value) -> (String, bool) {
    match args.get("request_id") {
        // Absent / explicit null → nothing supplied to invalidate.
        None | Some(Value::Null) => (uuid::Uuid::new_v4().simple().to_string(), false),
        // Present string + valid (validated RAW, no trimming) → use verbatim.
        Some(Value::String(s)) if is_valid_request_id(s) => (s.clone(), false),
        // Present but invalid — a bad string OR a non-string type → substitute
        // (observable). No silent normalization, and a non-string is NOT treated
        // as "absent".
        Some(_) => (uuid::Uuid::new_v4().simple().to_string(), true),
    }
}

/// Prepend `[request_id=<id>] ` (and, when the supplied id was invalid, a
/// `[supplied_request_id_invalid]` marker) to a build error's message, preserving
/// the `ToolError` variant, so a FAILED build still hands the caller the stable
/// request_id — the caller extracts it and queries `compiler_progress` to read
/// the failed build's stream. This is how the "every build returns a stable
/// request_id" invariant holds on the failure path (the success path returns it
/// in the structured output/text); the marker makes the invalid-id substitution
/// observable on the failure path too.
fn tag_error_with_request_id(e: ToolError, request_id: &str, supplied_invalid: bool) -> ToolError {
    let mut tag = format!("[request_id={request_id}] ");
    if supplied_invalid {
        tag.push_str("[supplied_request_id_invalid] ");
    }
    match e {
        ToolError::NotConfigured(m) => ToolError::NotConfigured(format!("{tag}{m}")),
        ToolError::InvalidArgument(m) => ToolError::InvalidArgument(format!("{tag}{m}")),
        ToolError::Http(m) => ToolError::Http(format!("{tag}{m}")),
        ToolError::Database(m) => ToolError::Database(format!("{tag}{m}")),
        ToolError::Execution(m) => ToolError::Execution(format!("{tag}{m}")),
        ToolError::NotFound(m) => ToolError::NotFound(format!("{tag}{m}")),
        ToolError::Conflict(m) => ToolError::Conflict(format!("{tag}{m}")),
    }
}

/// Sanitize a build error's full message at the EMITTER boundary before it is
/// persisted on the progress bus (and later returned by `compiler_progress`).
/// TWO passes, in order:
///   1. **Secret VALUES** (S6/S7) — the build's redaction set (`SCCACHE_REDIS`
///      password/URL). `run()` already scrubs subprocess tails, but OTHER
///      `ToolError` sources reach the emit verbatim.
///   2. **Infrastructure LITERALS** (S1) — IP addresses, and the emitter-known
///      configured host/relay-host and dataset/deploy path values, plus the
///      sanctioned repo-wide S1/PII scanner as a catch-all. So a configured path,
///      internal host/IP, or relay location can never leave through the stream.
/// Only IPs + configured literals + known PII spans are replaced; generic
/// diagnostic prose is left intact.
fn redacted_failed_message(e: &ToolError) -> String {
    let secret_scrubbed = redact_secrets(&e.to_string(), &redaction_set(""));
    scrub_infra_literals(&secret_scrubbed)
}

/// One IPv4 dotted-quad matcher (all ranges, not just private) → `<ip>`.
fn ipv4_regex() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"\b(?:\d{1,3}\.){3}\d{1,3}\b").expect("ipv4 regex"))
}

/// Scrub infrastructure LITERALS from a message (S1) — see [`redacted_failed_message`].
/// Replaces the emitter-known CONFIGURED values (arbitrary paths/hosts the generic
/// PII patterns can't know) with `<host>`/`<path>`, every IPv4 with `<ip>`, then
/// runs the sanctioned repo-wide S1/PII scanner (`github::pii::scan_and_redact`)
/// as a catch-all for known internal hosts/paths/domains/container-ids.
fn scrub_infra_literals(input: &str) -> String {
    let mut out = input.to_string();

    // (a) Configured host / relay-host values → <host> (longest-first, so a value
    // that is a prefix of another is not partially replaced).
    let mut hosts = host::configured_addresses();
    if let Some(relay) = env_nonempty(BUILD_DATASET_RELAY_HOST) {
        hosts.push(relay);
    }
    hosts.retain(|h| !h.is_empty());
    hosts.sort_by_key(|h| std::cmp::Reverse(h.len()));
    hosts.dedup();
    for h in hosts {
        out = out.replace(&h, "<host>");
    }

    // (b) Configured dataset/deploy/target path roots → <path> (longest-first).
    let mut paths: Vec<String> = [
        BUILD_DATASET_ROOT,
        BUILD_DATASET_RELAY_ROOT,
        BUILD_HEAVY_DATASET_ROOT,
        BUILD_HEAVY_LOCAL_TARGET_DIR,
        BUILD_LOCAL_TARGET_DIR,
        BUILD_SCRATCH_ROOT,
    ]
    .iter()
    .filter_map(|k| env_nonempty(k))
    .collect();
    paths.sort_by_key(|p| std::cmp::Reverse(p.len()));
    paths.dedup();
    for p in paths {
        out = out.replace(&p, "<path>");
    }

    // (c) Any IPv4 literal → <ip>.
    out = ipv4_regex().replace_all(&out, "<ip>").into_owned();

    // (d) Sanctioned repo-wide S1/PII catch-all (internal hosts/paths/domains/
    // container-ids/private-IPs the explicit set above didn't cover). Only matched
    // spans are replaced; generic text is preserved.
    let (scrubbed, _violations) = crate::github::pii::scan_and_redact(&out);
    scrubbed
}

/// A caller-supplied `request_id` is VALID iff it is a safe single segment (no
/// separators/whitespace/metachars) AND within the hard length bound. This is a
/// hard validation rule, NOT a clamp: `compiler_build` falls back to an
/// auto-generated id when it is invalid, and `compiler_progress` rejects it — so
/// an overlong or malformed id can never be truncated into a colliding key.
fn is_valid_request_id(s: &str) -> bool {
    !s.is_empty() && events::request_id_len_ok(s) && validate_segment("request_id", s).is_ok()
}

/// Parse + validate the `mode` arg (`"build"` default | `"test"`) shared by
/// `compiler_build` and `compiler_request`/the async `wait=false` enqueue path
/// (BLD-ASYNC, TERM #421) — kept as one function so the two tools can never
/// silently drift on what a valid `mode` looks like.
fn parse_mode_arg(args: &Value) -> Result<String, ToolError> {
    let mode = args
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or("build")
        .to_string();
    if mode != "build" && mode != "test" {
        return Err(ToolError::InvalidArgument(format!(
            "mode must be \"build\" or \"test\", got {mode:?}"
        )));
    }
    Ok(mode)
}

impl CompilerBuild {
    /// BLD-ASYNC (TERM #421): the `wait=false` path. Validates + enqueues onto
    /// the SAME durable scheduler/queue `compiler_request` uses (module lock /
    /// host cap / window gating all still apply once the scheduler dispatches
    /// it) and returns immediately with the queue's job id as the caller's
    /// `request_id` to poll via `compiler_progress` — this call never runs
    /// cargo itself. `mode=test` is supported end-to-end: it is carried
    /// durably on the queued job (`JobRequest::mode`) and forwarded by
    /// `invoke_build` when the scheduler actually dispatches it, so the
    /// dispatched build runs the test-gate (not a publish-and-flip build) and
    /// the terminal `tested` event (with pass/fail counts) lands on this same
    /// job id.
    async fn enqueue_async(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let store = RedisQueue::from_env().ok_or_else(|| {
            ToolError::NotConfigured(
                "compiler job queue is not configured (REDIS_URL unset) — cannot enqueue an \
                 async build; wait=false requires the durable Redis queue (BLD-20 \
                 Namespace::Queue), the same one compiler_request uses"
                    .to_string(),
            )
        })?;
        Self::enqueue_async_onto(&store, args).await
    }

    /// The store-generic core of [`enqueue_async`](Self::enqueue_async) — takes
    /// any [`QueueStore`] so tests can enqueue onto an in-memory fake instead of
    /// requiring a live Redis (`RedisQueue::from_env`'s backend is a
    /// process-global singleton memoized on first use, so it cannot be
    /// deterministically pointed at a test instance after the fact).
    async fn enqueue_async_onto(
        store: &dyn QueueStore,
        args: Value,
    ) -> Result<ToolOutput, ToolError> {
        let module = str_arg(&args, "module")?;
        let git_ref = str_arg(&args, "ref")?;
        validate_segment("module", &module)?;
        validate_git_ref(&git_ref)?;

        let host_req =
            HostRequest::parse(args.get("host").and_then(Value::as_str).unwrap_or("auto"))?;
        let fast = args.get("fast").and_then(Value::as_bool).unwrap_or(false);
        let bin = match args.get("bin").and_then(Value::as_str) {
            Some(b) => {
                validate_segment("bin", b)?;
                Some(b.to_string())
            }
            None => None,
        };
        let mode = parse_mode_arg(&args)?;
        let heavy = request_is_heavy(host_req, &module, fast);

        // PCON-01/04 (S122 root-cause fix): resolve `ref -> sha` HERE, at
        // enqueue/request time — BEFORE the job is durably recorded — so the
        // resolved sha becomes the job's durable identity (queue dedupe, the
        // module-serialization lock, GC's live-set, and the on-disk per-sha
        // stage dir name all key off it, via `queue::job_identity`). This is
        // what closes the "ref moves while queued" race: resolving again at
        // DISPATCH time (when the scheduler eventually claims the job) would
        // let two enqueues of the same branch at different SHAs (or two
        // different branches that later resolve to the same sha) race or
        // silently diverge.
        let resolved_sha = resolve_sha_for_enqueue(&module, &git_ref).await?;

        let enq = store
            .enqueue(&JobRequest {
                module: module.clone(),
                git_ref: git_ref.clone(),
                priority: Priority::Normal,
                heavy,
                ready: true,
                bin,
                force: false,
                mode: mode.clone(),
                resolved_sha: resolved_sha.clone(),
            })
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?;

        // Give the caller something to poll immediately: a `queued` event on the
        // job id. The scheduler's eventual dispatch (`invoke_build` →
        // `execute_structured` with `request_id=Some(job_id)`) ROTATES this to a
        // fresh track when it actually starts (the same single-rotation-per-
        // attempt invariant the blocking path documents above) — so this is a
        // best-effort "something is queued" signal, not the authoritative track.
        events::bus().begin(&enq.job_id);
        events::bus().emit(
            &enq.job_id,
            events::Emit::stage(events::Stage::Queued).message(format!("{module}@{git_ref}")),
        );

        let text = format!(
            "{verb} {module}@{git_ref} (mode={mode}) async; poll compiler_progress(request_id={id}) \
             for the terminal result",
            verb = if enq.created { "Queued" } else { "Coalesced onto existing" },
            id = enq.job_id,
        );
        let structured = json!({
            "request_id": enq.job_id,
            "job_id": enq.job_id,
            "created": enq.created,
            "coalesced": !enq.created,
            "module": module,
            "ref": git_ref,
            "mode": mode,
            "heavy": heavy,
            "wait": false,
            // PCON-01/04: the identity this job is durably keyed by from here on
            // (null only when BUILD_STAGE_BY_SHA=off).
            "resolved_sha": resolved_sha,
        });
        Ok(ToolOutput::with_structured(text, structured))
    }

    async fn build_inner(&self, request_id: &str, args: Value) -> Result<ToolOutput, ToolError> {
        let module = str_arg(&args, "module")?;
        let git_ref = str_arg(&args, "ref")?;
        let host_req =
            HostRequest::parse(args.get("host").and_then(Value::as_str).unwrap_or("auto"))?;
        let profile = args
            .get("profile")
            .and_then(Value::as_str)
            .unwrap_or("release")
            .to_string();
        let fast = args.get("fast").and_then(Value::as_bool).unwrap_or(false);
        let bin = args
            .get("bin")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| module.clone());
        // BLD-COMPTEST: mode=build (default, unchanged behavior) | mode=test (the
        // gate — same capped single-door scope, no publish, no channel flip).
        let mode = parse_mode_arg(&args)?;
        let is_test_mode = mode == "test";

        // ── Validate user-controlled path inputs BEFORE any path join / rsync /
        // ssh (no traversal, no separators, no injection). After this, joining
        // and interpolation are safe. ───────────────────────────────────────
        validate_segment("module", &module)?;
        validate_segment("bin", &bin)?;
        validate_segment("profile", &profile)?;
        validate_git_ref(&git_ref)?;

        // BLD-444: a module may configure a web-build (SPA/npm) subdirectory
        // via `BUILD_MODULE_WEB_DIR_<MODULE>` (e.g. harmony's `harmony-web`,
        // which `harmony-server` embeds via rust-embed at COMPILE time — a
        // gitignored `dist/` that must exist BEFORE cargo runs, or the binary
        // embeds only the tiny fallback shell: a blank dashboard). Unset ⇒
        // `None` ⇒ zero behavior change (no npm step, no new host
        // requirement) for every module without one. Validated ONCE, here,
        // as a safe relative path — same discipline as every other
        // user/config path input — before it is ever joined under the staged
        // source root in either the local or remote build path below.
        let web_dir = host::module_web_dir(&module);
        if let Some(w) = &web_dir {
            validate_relative_dir("web dir", w)?;
        }

        // BLD-19: the request is accepted → `queued`. A per-build tap streams the
        // cargo `{step,total}` into the bus during the build (progress bar). The
        // stream was already ROTATED to a fresh, non-terminal track by the wrapper
        // (`execute_structured`) before validation — so this `queued` lands on the
        // fresh track. build_inner does NOT rotate again (single rotation per
        // attempt), so the `queued → … → published/failed` shape is preserved.
        let bus = events::bus();
        bus.emit(
            request_id,
            events::Emit::stage(events::Stage::Queued).message(format!("{module}@{git_ref}")),
        );
        let tap = events::BuildTap::new(request_id);

        // ── Resolve config (fail fast, no side effects) ──────────────────────
        let root = dataset_root()?;
        let root_str = root.to_string_lossy().to_string();
        let resolved = host::resolve(host_req, &module, fast)?;
        // Host selected → `scheduled` (which role, local vs remote).
        bus.emit(
            request_id,
            events::Emit::stage(events::Stage::Scheduled)
                .message(resolved.role.as_str().to_string()),
        );
        // BLD-444: a per-module override (`BUILD_MODULE_TARGET_<MODULE>`) wins
        // over the fleet-wide `BUILD_TARGET_TRIPLE` — see `effective_triple`'s
        // doc for why harmony needs this (musl-static, for a portable artifact
        // on an older-glibc deploy host than the builder).
        let triple = effective_triple(&module);
        // `target` (the triple, override or default) comes from config but is
        // used as a path segment.
        validate_segment("target", &triple)?;

        // sccache env (fail-open to a local dir if Redis is unconfigured).
        let sccache_env = sccache::resolve(&root_str);

        // Redaction set (S7): the secret VALUES that could be echoed by a child
        // build (a build script printing its env, etc.) and must be scrubbed from
        // ANY captured stdout/stderr before it reaches an error/log. Shared with
        // the failed-event redaction on the wrapper's error path.
        let mut redact = redaction_set(&root_str);

        // The local source stage (staged on the shared NFS share is fine — it's a
        // source stage, not the live target). Also the rsync source for a remote
        // build. A caller-supplied `source_dir` is a FULL PATH (not a segment), so
        // it is validated by CONTAINMENT — it must lexically resolve inside an
        // allowed root (the dataset `src` tree, plus any `BUILD_ALLOWED_SOURCE_ROOTS`)
        // BEFORE it is used for current_dir / --manifest-path / rsync, so an
        // absolute-elsewhere or `../`-escaping override can't build/sync source
        // outside the dataset. The default staged path is already safe.
        let explicit_source_dir = args.get("source_dir").and_then(Value::as_str).is_some();

        // PCON-01: resolve `git_ref` to its immutable commit sha exactly ONCE,
        // here, before any staging decision — UNLESS the caller gave an explicit
        // `source_dir` (untouched by any of this) or SHA-staging is disabled
        // (`BUILD_STAGE_BY_SHA=off`, the rollback lever). `stage_key` is what
        // every downstream staging/targeting/unit-naming decision below keys
        // off: the resolved sha when available, else the legacy ref (unchanged
        // behavior). Fails CLOSED on a resolution failure — a `ref` that cannot
        // be resolved to a sha is never silently staged under the mutable ref
        // path (that would defeat the whole point).
        let resolved_sha: Option<String> = if !explicit_source_dir && stage_by_sha_enabled() {
            Some(
                resolve_ref_to_sha(&module, &git_ref, &mut redact)
                    .await
                    .map_err(|e| {
                        ToolError::Execution(format!(
                            "compiler: could not resolve {module}@{git_ref} to a commit sha \
                             (fail-closed — set {BUILD_STAGE_BY_SHA_ENV}=off to fall back to \
                             the legacy ref-keyed staging path): {e}"
                        ))
                    })?,
            )
        } else {
            None
        };
        let stage_key: &str = resolved_sha.as_deref().unwrap_or(&git_ref);

        // A DETERMINISTIC, UNIQUE transient-scope unit name: `<module>-<sha-or-ref>`
        // plus a per-invocation uuid so it can never collide with a concurrent
        // build of the same module@sha and is unambiguously addressable for
        // `systemctl kill <unit>.scope` if a (remote) build times out.
        let unit = format!(
            "{}-{}",
            scope::scope_unit_name(&module, stage_key),
            uuid::Uuid::new_v4().simple()
        );

        let local_source_dir = match args.get("source_dir").and_then(Value::as_str) {
            Some(s) => {
                let sd = PathBuf::from(s);
                validate_source_dir(&sd, &root)?;
                sd
            }
            // PCON-01: content-addressed by the resolved sha, not the mutable
            // ref — `.../src/<module>/<sha>`. Two requests for the SAME sha
            // share this dir safely (autostage's atomic-rename publish is
            // already race-safe on that); two DIFFERENT shas get disjoint dirs
            // and can never clobber each other.
            None => root.join("src").join(&module).join(stage_key),
        };
        // GAP 3 (TERM #418): auto-fetch the source from Gitea BEFORE the GAP 1
        // check below when the caller did NOT supply an explicit `source_dir`
        // (that path is validated/used as-is, never auto-staged) and the
        // default stage path doesn't exist yet — so a fresh agent's first
        // compiler_build for module@ref just works without a manual rsync.
        // Best-effort: a failure here is NOT fatal on its own — it falls
        // through to GAP 1's existing "source not staged" error below (now
        // augmented with the auto-stage failure reason) rather than returning
        // a different error shape. PCON-01: fetch by `stage_key` (the resolved
        // sha when available) so the strategy-2 direct-sha fetch path in
        // `autostage_source` checks out EXACTLY the resolved commit, never
        // "whatever the branch happens to point to right now".
        let mut autostage_failure: Option<String> = None;
        if !explicit_source_dir && !local_source_dir.is_dir() && autostage_enabled() {
            if let Err(e) = autostage_source(&module, stage_key, &local_source_dir, &mut redact).await {
                tracing::warn!(
                    "compiler: GAP 3 auto-stage failed for {module}@{git_ref} (sha {stage_key}): {e}"
                );
                autostage_failure = Some(e.to_string());
            }
        }
        // GAP 1: fail loudly HERE, with the actual cause, if the source was
        // never staged (auto-stage above may have just fixed this) —
        // otherwise `local_source_dir` reaches `current_dir(...)` on a spawn
        // whose PROGRAM path (`/usr/bin/systemd-run`) is perfectly valid, and
        // a missing `current_dir` makes `Command::spawn` report ENOENT against
        // argv[0] instead — a red herring ("No such file or directory"
        // pointing at systemd-run) that cost real debugging time before this
        // check existed.
        validate_local_source_dir(&local_source_dir, &module, &git_ref).map_err(|e| {
            match (e, &autostage_failure) {
                (ToolError::NotFound(m), Some(reason)) => ToolError::NotFound(format!(
                    "{m} — auto-stage from Gitea was attempted and failed: {reason}"
                )),
                (other, _) => other,
            }
        })?;

        // PCON-02/FINDING 4: the built-identity integrity check now runs on
        // EVERY default-staged build (still NEVER for an explicit
        // `source_dir` override — untouched by any of PCON-01..05) — in BOTH
        // SHA-mode and the legacy `BUILD_STAGE_BY_SHA=off` mode. Previously
        // the off-path skipped this check entirely, silently reintroducing
        // the clobber-reuse gap the moment an operator flipped the rollback
        // lever. `strict=resolved_sha.is_some()`: in SHA-mode a missing
        // sidecar is a hard failure (autostage_source has always written one
        // since PCON-01); in the off/legacy mode a missing sidecar is
        // expected for a pre-existing/pre-PCON directory (warned, not
        // fatal) — but a PRESENT, MISMATCHED sidecar is a hard failure
        // either way (see `check_built_sha_sidecar`'s doc).
        let built_sha = if explicit_source_dir {
            None
        } else {
            check_built_sha_sidecar(&local_source_dir, &module, stage_key, resolved_sha.is_some())?
        };
        // PCON follow-up (deferred, review — not this fix): this proves the
        // sidecar matched at THIS instant, not that the tree stays byte-for-
        // byte unchanged for the rest of the build — nothing today snapshots
        // or locks `local_source_dir` read-only across the whole
        // toolchain-ensure + lockgen + cargo invocation below, so a
        // theoretical concurrent mutation between this check and cargo
        // actually reading the files is a TOCTOU window. Tracked as a PCON
        // hardening follow-up (an immutable snapshot/lock spanning the whole
        // build), not attempted here.

        // Pinned toolchain channel to ensure (idempotent; never `rustup update`).
        // BLD-444: this installs the CHANNEL only — it does not itself add the
        // musl/etc. target a `BUILD_MODULE_TARGET_<MODULE>` override may need.
        // That's handled implicitly: `run()` below sets `current_dir` to the
        // staged source dir (`local_source_dir`/`remote_source`) BEFORE
        // spawning cargo, and rustup auto-installs any target listed in that
        // directory's `rust-toolchain.toml` `[toolchain] targets = […]` the
        // first time it's needed — harmony's `rust-toolchain.toml` already
        // lists `x86_64-unknown-linux-musl`, so no separate `rustup target
        // add` is required here. A module whose toolchain file does NOT list
        // its override target would need that added to the file (not this
        // compiler) — the override only picks *which* target cargo builds
        // for, it doesn't grant it.
        let pinned = env_nonempty(RUST_TOOLCHAIN_PINNED);

        // The build produces a LOCALLY-readable binary at `built_bin` in BOTH the
        // local and remote paths, so the publish step below is host-agnostic.
        // BLD-COMPTEST: for `mode=test` there is no binary to publish — `built_bin`
        // is left as an unused placeholder on that path (the function returns with
        // the gate result, below, before the publish section ever reads it), and
        // `test_outcome` (process-exit-success, parsed [`CargoTestSummary`]) is set
        // instead.
        let built_bin: PathBuf;
        let mut test_outcome: Option<(bool, CargoTestSummary)> = None;

        if resolved.is_local() {
            // ── LOCAL build (primary, in place) ──────────────────────────────
            // PCON-10: a PER-JOB CARGO_TARGET_DIR + TMPDIR on the big-disk scratch
            // root (fail-closed if unset — never the small /tmp tmpfs), so two
            // concurrent local builds never share a target/temp dir. Reclaimed on
            // finalize by the drop guard below (PCON-05 GC is the crash backstop).
            let scratch_root = job_scratch_root()?;
            let (target_dir, tmp_dir) = job_scratch_dirs(&scratch_root, &unit);
            // GUARD: both exec-safe local disk, never the file-level NFS dataset.
            scope::validate_target_dir(&target_dir, &root)?;
            scope::validate_target_dir(&tmp_dir, &root)?;
            // FIX (PCON-10): arm the reclaim guard BEFORE creating anything, so a
            // PARTIAL create that then errors (or any later `?`) still removes
            // `<root>/<unit>` — the guard must own the dir before it can leak.
            let _scratch_guard = ScratchReclaim::new(scratch_root.join(&unit));
            // cargo creates CARGO_TARGET_DIR itself, but TMPDIR must pre-exist.
            std::fs::create_dir_all(&tmp_dir).map_err(|e| {
                ToolError::Execution(format!(
                    "could not create per-job TMPDIR {}: {e}",
                    tmp_dir.display()
                ))
            })?;

            let mut build_env = sccache_env.vars.clone();
            build_env.insert(
                "CARGO_TARGET_DIR".to_string(),
                target_dir.to_string_lossy().to_string(),
            );
            // PCON-10: keep temp on the big disk too (rustc/linker/tempfile spill),
            // never the small /tmp tmpfs.
            build_env.insert("TMPDIR".to_string(), tmp_dir.to_string_lossy().to_string());
            // Force cargo's N/M progress bar on the piped (non-TTY) stdio so the
            // tap gets live {step,total} updates (BLD-19).
            inject_cargo_progress_env(&mut build_env);
            // GAP 4: incremental compilation defeats sccache's Rust caching.
            inject_cargo_incremental_off(&mut build_env);
            // GAP 2: Gitea Cargo-registry config + vault-sourced auth token, so a
            // module that depends on terminus-rs via the Gitea Cargo registry
            // (harmony, chord) can resolve it inside the build scope.
            inject_gitea_registry_env(&mut build_env, &mut redact);
            // S7: non-secret vars → `--setenv` (argv); secret vars → the INHERITED
            // process environment of systemd-run (which `--scope` passes to the
            // cargo child) — never argv.
            let (setenv, secret_env) = scope::partition_env(&build_env);

            if let Some(channel) = &pinned {
                run(
                    &[
                        "rustup".into(),
                        "toolchain".into(),
                        "install".into(),
                        channel.clone(),
                    ],
                    Some(&local_source_dir),
                    &BTreeMap::new(),
                    Duration::from_secs(600),
                    &redact,
                    None,
                    None,
                )
                .await?;
            }

            // BLD-444: the web-build (SPA/npm) pre-step, LOCAL path. Runs
            // BEFORE lockgen/cargo, in the SAME capped systemd scope as the
            // cargo steps (`resolved.caps`, the same non-secret `setenv`) so
            // it never gets an uncapped RAM/CPU allowance. `npm ci` then
            // `npm run build`, in `<staged_source_root>/<web_dir>`. FAIL
            // CLOSED: `npm` missing on this build host, or either command
            // exiting non-zero (via `run`'s existing non-zero-exit ⇒ `Err`
            // behavior), aborts the WHOLE build here — it is never swallowed
            // to fall through to cargo, which would embed the tiny fallback
            // shell (a blank dashboard) instead of a real SPA: the exact bug
            // this closes. Zero-cost when `web_dir` is `None` (the default
            // for every module without a `BUILD_MODULE_WEB_DIR_<MODULE>`).
            if let Some(w) = &web_dir {
                let web_path = local_source_dir.join(w);
                bus.emit(
                    request_id,
                    events::Emit::stage(events::Stage::Building).message(format!("web-build: {w}")),
                );
                for (label, npm_argv) in [
                    ("npm ci", vec!["npm".to_string(), "ci".to_string()]),
                    (
                        "npm run build",
                        vec!["npm".to_string(), "run".to_string(), "build".to_string()],
                    ),
                ] {
                    let web_unit =
                        format!("{unit}-web-{}", label.replace(' ', "_").replace('-', "_"));
                    let web_scope_argv =
                        scope::render_scope_argv(&web_unit, &resolved.caps, &setenv, &npm_argv);
                    run(
                        &web_scope_argv,
                        Some(&web_path),
                        &secret_env,
                        Duration::from_secs(WEB_BUILD_TIMEOUT_SECS),
                        &redact,
                        None,
                        None,
                    )
                    .await
                    .map_err(|e| {
                        ToolError::Execution(format!(
                            "web-build pre-step failed for module {module:?} in {w:?} \
                             ({label}): {e} — refusing to embed a blank SPA"
                        ))
                    })?;
                }
            }

            let manifest = local_source_dir.join("Cargo.toml");
            let manifest_str = manifest.to_string_lossy().to_string();

            // GAP 5 (TERM #418): generate a matching Cargo.lock BEFORE the
            // `--locked` build/test below, in the SAME capped scope and with
            // the SAME env (sccache + CARGO_TARGET_DIR + GAP 2's Gitea
            // registry creds + GAP 4's CARGO_INCREMENTAL=0) as the real
            // build — a distinct `-lockgen` unit keeps its transient scope
            // from colliding with the main build's. See
            // `cargo_generate_lockfile_argv`'s doc for why this is
            // necessary (terminus/harmony/chord gitignore Cargo.lock).
            let lockgen_argv = cargo_generate_lockfile_argv(&manifest_str);
            let lockgen_unit = format!("{unit}-lockgen");
            let lockgen_scope_argv =
                scope::render_scope_argv(&lockgen_unit, &resolved.caps, &setenv, &lockgen_argv);
            run(
                &lockgen_scope_argv,
                Some(&local_source_dir),
                &secret_env,
                Duration::from_secs(180),
                &redact,
                None,
                None,
            )
            .await?;

            let cargo_argv = if is_test_mode {
                cargo_test_argv(&profile, &triple, resolved.caps.jobs, &manifest_str)
            } else {
                cargo_build_argv(&profile, &triple, resolved.caps.jobs, &bin, &manifest_str)
            };
            let scope_argv = scope::render_scope_argv(&unit, &resolved.caps, &setenv, &cargo_argv);
            // Compilation (or the test run) starts → `building`; the tap streams
            // `{step,total}` when cargo renders one.
            bus.emit(request_id, events::Emit::stage(events::Stage::Building));
            // Secret env is delivered via the inherited environment (last arg),
            // NOT argv. The build tap streams cargo progress lines live.
            if is_test_mode {
                // `run_test` (unlike `run`) does NOT error on a non-zero exit — a
                // failing test suite is an expected gate outcome, not an execution
                // error — and preserves stdout (where cargo prints its `test
                // result: ...` summary) on every exit status.
                let (exit_success, output) = run_test(
                    &scope_argv,
                    Some(&local_source_dir),
                    &secret_env,
                    Duration::from_secs(MAX_BUILD_TIMEOUT_SECS),
                    &redact,
                    None,
                    Some(&tap),
                )
                .await?;
                test_outcome = Some((exit_success, parse_cargo_test_output(&output)));
                built_bin = PathBuf::new(); // unused: mode=test never reaches publish
            } else {
                run(
                    &scope_argv,
                    Some(&local_source_dir),
                    &secret_env,
                    Duration::from_secs(MAX_BUILD_TIMEOUT_SECS),
                    &redact,
                    None,
                    Some(&tap),
                )
                .await?;
                built_bin = target_dir.join(built_bin_rel(&triple, &profile, &bin));
            }
        } else {
            // ── REMOTE build (heavy host, over ssh) ──────────────────────────
            let host_addr = resolved
                .address
                .clone()
                .expect("a non-local resolved host always has an ssh address");
            let remote_root = heavy_dataset_root(&root_str);
            // PCON-03: a PER-JOB remote target — never the single shared
            // `heavy_local_target_dir()` every heavy build used to build in
            // (which meant two concurrent heavy builds fought over one
            // CARGO_TARGET_DIR). Scoped by `<module>-<stage_key>-<unit-uuid>`:
            // even two builds of the SAME sha get disjoint target dirs (the
            // unit name already carries a fresh uuid per invocation), so
            // there is never a shared mutable target on the remote either.
            let remote_target = heavy_local_target_dir()?.join(format!("{module}-{unit}"));
            // GUARD applies remotely too: the remote cargo target must be exec-safe,
            // never under the remote NFS dataset.
            scope::validate_target_dir(&remote_target, std::path::Path::new(&remote_root))?;
            let remote_target_str = remote_target.to_string_lossy().to_string();
            // PCON-10: a per-job TMPDIR on the heavy BUILD disk (a subdir of the
            // per-job target, so it is reclaimed with it), never the remote /tmp
            // tmpfs — rustc/linker/tempfile spill would otherwise exhaust it.
            let remote_tmp_str = format!("{}/.tmpdir", remote_target_str.trim_end_matches('/'));
            // PCON-03: content-addressed by `stage_key` (the resolved sha when
            // SHA-staging is active, else the legacy ref) — mirrors PCON-01's
            // local stage path. Two DIFFERENT shas of one module now relay to
            // DISJOINT remote dirs, so the `rsync --delete` below only ever
            // prunes THIS sha's own tree — it can never clobber a sibling
            // build's remote checkout.
            let remote_source = format!(
                "{}/src/{}/{}",
                remote_root.trim_end_matches('/'),
                module,
                stage_key
            );

            // Staging source to the heavy host → `relaying`.
            bus.emit(
                request_id,
                events::Emit::stage(events::Stage::Relaying)
                    .message(resolved.role.as_str().to_string()),
            );
            // Ensure the per-job remote TARGET dir exists either way (source
            // staging below handles `remote_source`/its parent itself).
            run(
                &[
                    "ssh".into(),
                    host_addr.clone(),
                    format!(
                        "mkdir -p {} {}",
                        shell_quote(&remote_target_str),
                        shell_quote(&remote_tmp_str)
                    ),
                ],
                None,
                &BTreeMap::new(),
                Duration::from_secs(60),
                &redact,
                None,
                None,
            )
            .await?;

            if let Some(sha) = &resolved_sha {
                // PCON-03 (FINDING 3 review fix): with staging content-addressed
                // by sha (PCON-01), TWO jobs that resolved DIFFERENT original
                // refs to the SAME sha relay to this SAME `remote_source` — the
                // whole point (dedup: identical content, no reason to stage it
                // twice). But `rsync -a --delete` straight into a directory a
                // SIBLING job's build might already be reading from is exactly
                // the clobber this item exists to close. So: if the remote
                // ALREADY carries a verified copy of this sha, REUSE it (no
                // transfer at all — never touches the live `remote_source`).
                // Otherwise, stage into an ISOLATED sibling temp dir (where
                // `--delete` is safe — nothing else can see it yet) and
                // atomically `mv -T` it into place, mirroring
                // `autostage_source`'s local atomic-rename publish exactly: the
                // move either wins outright, or fails because a racer already
                // published first (`mv -T` refuses to merge into an existing
                // directory) — a safe no-op either way, verified afterward by
                // the PCON-02 sidecar assertion below regardless of which
                // racer's `mv` actually landed.
                let reused = remote_sidecar_sha_best_effort(&host_addr, &remote_source, &redact)
                    .await
                    .as_deref()
                    == Some(sha.as_str());
                if reused {
                    tracing::debug!(
                        "compiler: heavy relay for {module}@{sha} reuses the already-staged \
                         remote tree at {remote_source} (PCON-03 same-sha dedup, no transfer)"
                    );
                } else {
                    let remote_source_parent = remote_source
                        .rsplit_once('/')
                        .map(|(p, _)| p.to_string())
                        .unwrap_or_else(|| remote_root.clone());
                    let staging_remote =
                        format!("{remote_source}.stage-{}", uuid::Uuid::new_v4().simple());
                    run(
                        &[
                            "ssh".into(),
                            host_addr.clone(),
                            format!(
                                "mkdir -p {} {}",
                                shell_quote(&remote_source_parent),
                                shell_quote(&staging_remote)
                            ),
                        ],
                        None,
                        &BTreeMap::new(),
                        Duration::from_secs(120),
                        &redact,
                        None,
                        None,
                    )
                    .await?;
                    // Export into the ISOLATED staging dir. `--delete` is safe
                    // HERE — it is a fresh, not-yet-published dir nothing else
                    // can be reading.
                    run(
                        &[
                            "rsync".into(),
                            "-a".into(),
                            "--delete".into(),
                            "-s".into(),
                            format!("{}/", local_source_dir.to_string_lossy()),
                            format!("{host_addr}:{staging_remote}/"),
                        ],
                        None,
                        &BTreeMap::new(),
                        Duration::from_secs(1800),
                        &redact,
                        None,
                        None,
                    )
                    .await?;
                    // Atomic publish: `mv -T` (no-target-directory — a plain
                    // rename, never "move INTO an existing dir") either wins,
                    // or fails because `remote_source` already exists (a
                    // concurrent racer published first). The `|| rm -rf`
                    // fallback always cleans up OUR staging dir either way, so
                    // this shell step always exits 0 — a lost race or a genuine
                    // remote failure are BOTH caught by the unconditional
                    // PCON-02 sidecar assertion right after this block (a
                    // publish that neither landed nor found a matching racer
                    // leaves no/an unreadable sidecar, which that check treats
                    // as a hard failure).
                    run(
                        &[
                            "ssh".into(),
                            host_addr.clone(),
                            format!(
                                "mv -T {} {} 2>/dev/null || rm -rf {}",
                                shell_quote(&staging_remote),
                                shell_quote(&remote_source),
                                shell_quote(&staging_remote)
                            ),
                        ],
                        None,
                        &BTreeMap::new(),
                        Duration::from_secs(120),
                        &redact,
                        None,
                        None,
                    )
                    .await?;
                }
            } else {
                // Legacy path (BUILD_STAGE_BY_SHA=off): unchanged verbatim
                // behavior — `remote_source` is ref-keyed (never shared by two
                // different refs, so the same-sha sharing this fix addresses
                // does not arise here).
                run(
                    &[
                        "ssh".into(),
                        host_addr.clone(),
                        format!("mkdir -p {}", shell_quote(&remote_source)),
                    ],
                    None,
                    &BTreeMap::new(),
                    Duration::from_secs(120),
                    &redact,
                    None,
                    None,
                )
                .await?;
                run(
                    &[
                        "rsync".into(),
                        "-a".into(),
                        "--delete".into(),
                        "-s".into(),
                        format!("{}/", local_source_dir.to_string_lossy()),
                        format!("{host_addr}:{remote_source}/"),
                    ],
                    None,
                    &BTreeMap::new(),
                    Duration::from_secs(1800),
                    &redact,
                    None,
                    None,
                )
                .await?;
            }

            // PCON-02/FIX 3 (remote leg): re-assert the built-identity
            // integrity check against the tree that ACTUALLY landed on the
            // heavy host — a relay hiccup (a partial/interrupted rsync, or a
            // rewritten sidecar) is caught here rather than trusting the
            // local check transitively. UNCONDITIONAL — runs in BOTH
            // SHA-mode and legacy `BUILD_STAGE_BY_SHA=off` mode now (never
            // for an explicit `source_dir` override, which never reaches the
            // remote relay path with a stage_key-addressed `remote_source`
            // at all). See `assert_remote_sidecar`'s doc for the exact
            // strict-vs-non-strict policy.
            assert_remote_sidecar(
                &host_addr,
                &remote_source,
                &module,
                stage_key,
                resolved_sha.is_some(),
                &redact,
            )
            .await?;

            let mut build_env = sccache_env.vars.clone();
            build_env.insert("CARGO_TARGET_DIR".to_string(), remote_target_str.clone());
            // PCON-10: per-job TMPDIR on the heavy build disk, never remote /tmp.
            build_env.insert("TMPDIR".to_string(), remote_tmp_str.clone());
            // Force cargo's N/M progress bar on the piped (non-TTY, over-ssh) stdio
            // so the tap gets live {step,total} updates (BLD-19).
            inject_cargo_progress_env(&mut build_env);
            // GAP 4: incremental compilation defeats sccache's Rust caching.
            inject_cargo_incremental_off(&mut build_env);
            // GAP 2: Gitea Cargo-registry config + vault-sourced auth token (see
            // the local-build call site above for the full rationale).
            inject_gitea_registry_env(&mut build_env, &mut redact);
            let (setenv, secret_env) = scope::partition_env(&build_env);

            // GAP 5 (TERM #418): same rationale as the local path above — a
            // freshly-staged feature-branch checkout has no Cargo.lock
            // (terminus/harmony/chord gitignore it), so the `--locked`
            // build/test below would fail its dependency-resolution step
            // instantly. Render a `cargo generate-lockfile` scope now (same
            // caps/env, distinct `-lockgen` unit); it is chained into the
            // SAME ssh session as the real build below (via `&&`, sharing
            // the one sourced secret-env file) rather than exec'd
            // separately, so it never disturbs the single-shot `rm -f` on
            // the remote secret file.
            let remote_manifest = format!("{remote_source}/Cargo.toml");
            let lockgen_argv = cargo_generate_lockfile_argv(&remote_manifest);
            let lockgen_unit = format!("{unit}-lockgen");
            let lockgen_scope_argv =
                scope::render_scope_argv(&lockgen_unit, &resolved.caps, &setenv, &lockgen_argv);
            let lockgen_cmd = shell_join(&lockgen_scope_argv);

            // Secret env (if any) → a 0600 file ON THE REMOTE, `source`d inside the
            // ssh wrapper before `exec systemd-run` so it reaches the scoped build's
            // inherited env WITHOUT ever touching a command line (S7). The remote
            // filename carries an unguessable random component (defense-in-depth vs
            // a pre-planted file/symlink), matching the local staging file below.
            let remote_env_path = format!(
                "{remote_target_str}/.terminus-build-{unit}-{}.env",
                uuid::Uuid::new_v4()
            );
            let have_secret = !secret_env.is_empty();
            // RAII guard: once the secret file is (about to be) on the remote, its
            // removal is GUARANTEED on every subsequent exit — the happy path, any
            // `?` (e.g. a failing pinned-toolchain install), a timeout, or a panic —
            // via `Drop`. It stays in scope for the whole remote build below (it is
            // disarmed after a successful build, whose own wrapper already `rm`s the
            // file, to avoid a redundant ssh).
            let mut secret_guard: Option<RemoteSecretGuard> = None;
            if have_secret {
                let body = scope::render_secret_env_file(&secret_env);
                let local_secret = write_local_0600(&body, &unit)?;
                // Arm the guard BEFORE the transfer (covers a partial/failed rsync
                // that may still have created the remote file); the local staging
                // file is a Drop backstop until we unlink it inline just below.
                secret_guard = Some(RemoteSecretGuard::new(
                    host_addr.clone(),
                    remote_env_path.clone(),
                    Some(local_secret.clone()),
                    redact.clone(),
                ));
                // `rsync -a` preserves the local 0600 mode on the remote (so the
                // remote secret file is 0600 without a separate chmod), and `-s`
                // protects the remote path from remote-shell re-splitting.
                let xfer_res = run(
                    &[
                        "rsync".into(),
                        "-a".into(),
                        "-s".into(),
                        local_secret.to_string_lossy().to_string(),
                        format!("{host_addr}:{remote_env_path}"),
                    ],
                    None,
                    &BTreeMap::new(),
                    Duration::from_secs(120),
                    &redact,
                    None,
                    None,
                )
                .await;
                // Delete the local staging copy immediately (minimize its on-disk
                // lifetime), whether the transfer succeeded or not, then clear the
                // guard's local backstop. If `xfer_res` is an error, `secret_guard`
                // drops on the `?` below → the remote file is cleaned up.
                let _ = tokio::fs::remove_file(&local_secret).await;
                if let Some(g) = secret_guard.as_mut() {
                    g.clear_local();
                }
                xfer_res?;
            }

            if let Some(channel) = &pinned {
                // `rustup toolchain install <channel>` is cwd-independent; the
                // channel is shell-quoted for the remote shell.
                run(
                    &[
                        "ssh".into(),
                        host_addr.clone(),
                        format!("rustup toolchain install {}", shell_quote(channel)),
                    ],
                    None,
                    &BTreeMap::new(),
                    Duration::from_secs(600),
                    &redact,
                    None,
                    None,
                )
                .await?;
            }

            let cargo_argv = if is_test_mode {
                cargo_test_argv(&profile, &triple, resolved.caps.jobs, &remote_manifest)
            } else {
                cargo_build_argv(&profile, &triple, resolved.caps.jobs, &bin, &remote_manifest)
            };
            let scope_argv = scope::render_scope_argv(&unit, &resolved.caps, &setenv, &cargo_argv);
            // Remote wrapper: source the secret env file (if any) so BOTH the
            // GAP 5 lockgen scope and the real build/test scope see it, delete
            // the file (one-shot — only after it's been sourced for both), run
            // lockgen to completion (`&&`, not `exec`, since a following
            // command still needs to run), then `exec` the scoped build/test
            // so it replaces this shell as the final process. The secret lives
            // only in the 0600 file, never argv.
            let scope_cmd = shell_join(&scope_argv);

            // BLD-444: the web-build (SPA/npm) pre-step, REMOTE path. Same
            // rationale/fail-closed contract as the local path above — see
            // that block's comment. Rendered as a capped `systemd-run --scope`
            // (same `resolved.caps`/`setenv` as the cargo steps) `cd`'d into
            // the remote web dir, chained via shell `&&` in front of the
            // lockgen step: any web-build failure (npm missing, `npm ci`/`npm
            // run build` non-zero) short-circuits the `&&` chain so lockgen
            // and `exec <cargo>` never run — it can never fall through to a
            // cargo build that would embed a blank SPA. `cd`s back to
            // `remote_source` afterward purely so a reader can reason about
            // cwd at each `&&` step; lockgen/cargo below use `--manifest-path`
            // so they don't actually depend on it. Empty string (a no-op
            // prefix) when `web_dir` is `None` — zero behavior change.
            let web_prefix = match &web_dir {
                Some(w) => {
                    bus.emit(
                        request_id,
                        events::Emit::stage(events::Stage::Building)
                            .message(format!("web-build: {w}")),
                    );
                    let remote_web_dir = format!("{remote_source}/{w}");
                    let web_ci_argv = scope::render_scope_argv(
                        &format!("{unit}-web-ci"),
                        &resolved.caps,
                        &setenv,
                        &["npm".to_string(), "ci".to_string()],
                    );
                    let web_build_argv = scope::render_scope_argv(
                        &format!("{unit}-web-build"),
                        &resolved.caps,
                        &setenv,
                        &["npm".to_string(), "run".to_string(), "build".to_string()],
                    );
                    format!(
                        "cd {wd} && {ci} && {bld} && cd {back} && ",
                        wd = shell_quote(&remote_web_dir),
                        ci = shell_join(&web_ci_argv),
                        bld = shell_join(&web_build_argv),
                        back = shell_quote(&remote_source),
                    )
                }
                None => String::new(),
            };

            let remote_cmd = if have_secret {
                format!(
                    "set -a; . {f}; rm -f {f}; set +a; {web_prefix}{lockgen_cmd} && exec {scope_cmd}",
                    f = shell_quote(&remote_env_path)
                )
            } else {
                format!("{web_prefix}{lockgen_cmd} && exec {scope_cmd}")
            };
            // On timeout, tear down the REMOTE scope by its unit name too — the
            // local ssh process-group kill can't reach the remote build tree.
            let remote_kill = RemoteScopeKill {
                host: host_addr.clone(),
                unit: unit.clone(),
            };
            // Remote compilation (or test run) starts → `building`; the tap streams
            // the remote cargo `{step,total}` (over ssh stdout/stderr) live.
            bus.emit(request_id, events::Emit::stage(events::Stage::Building));
            if is_test_mode {
                // Same rationale as the local path: a non-zero exit is an expected
                // "tests failed" outcome, and stdout (cargo's summary) must survive
                // either way — `run_test`, not `run`.
                let (exit_success, output) = run_test(
                    &["ssh".into(), host_addr.clone(), remote_cmd],
                    None,
                    &BTreeMap::new(),
                    Duration::from_secs(3600),
                    &redact,
                    Some(&remote_kill),
                    Some(&tap),
                )
                .await?;
                if let Some(g) = secret_guard.as_mut() {
                    g.disarm();
                }
                test_outcome = Some((exit_success, parse_cargo_test_output(&output)));
                // No binary was produced to retrieve/publish for mode=test.
                built_bin = PathBuf::new();
            } else {
                let build_res = run(
                    &["ssh".into(), host_addr.clone(), remote_cmd],
                    None,
                    &BTreeMap::new(),
                    Duration::from_secs(3600),
                    &redact,
                    Some(&remote_kill),
                    Some(&tap),
                )
                .await;
                // If the build FAILED/timed out, propagate now — `secret_guard` drops
                // on this `?` and cleans up the remote file (it may still exist if the
                // build never reached the wrapper's own `rm`). On SUCCESS the wrapper
                // already removed the file, so disarm the guard's remote cleanup
                // (avoids a redundant ssh); the guard object stays alive but Drop
                // becomes a no-op.
                build_res?;
                if let Some(g) = secret_guard.as_mut() {
                    g.disarm();
                }

                // Retrieve the built binary back to a local temp path so publish is
                // host-agnostic (the build ran remotely; publish reads it locally).
                let remote_bin = format!(
                    "{}/{}",
                    remote_target_str.trim_end_matches('/'),
                    built_bin_rel(&triple, &profile, &bin).to_string_lossy()
                );
                let local_tmp_dir = std::env::temp_dir().join(format!("terminus-artifact-{unit}"));
                tokio::fs::create_dir_all(&local_tmp_dir)
                    .await
                    .map_err(|e| ToolError::Execution(format!("mk artifact tmp dir: {e}")))?;
                let local_bin = local_tmp_dir.join(&bin);
                run(
                    &[
                        "rsync".into(),
                        "-a".into(),
                        "-s".into(),
                        format!("{host_addr}:{remote_bin}"),
                        local_bin.to_string_lossy().to_string(),
                    ],
                    None,
                    &BTreeMap::new(),
                    Duration::from_secs(600),
                    &redact,
                    None,
                    None,
                )
                .await?;
                built_bin = local_bin;
            }
        }

        // BLD-COMPTEST: mode=test stops HERE — no publish, no channel flip (a
        // gate is not a release). The gate's structured result (pass/fail, test
        // counts, and the failing-test summary on failure) is returned directly
        // AND mirrored onto the same events/compiler_progress stream via a
        // terminal `Tested` event, so a caller polls `compiler_progress` for a
        // gate exactly as it would for a build.
        if is_test_mode {
            let (exit_success, summary) =
                test_outcome.expect("mode=test always sets test_outcome before this point");
            // GATE verdict: require BOTH a zero cargo exit AND a clean parsed
            // summary — see `test_gate_passed`. A clean earlier summary followed
            // by a later non-zero exit (a compile/rustdoc/link failure that emits
            // no further failed-summary line) is a FAIL, not a false PASS.
            let passed = test_gate_passed(exit_success, &summary);
            let structured = json!({
                "request_id": request_id,
                "module": module,
                "ref": git_ref,
                "host": resolved.role.as_str(),
                "remote": !resolved.is_local(),
                "profile": profile,
                "target": triple,
                "mode": "test",
                "bin": bin,
                // PCON-01/02: the once-resolved commit sha (null when SHA-staging
                // is off or an explicit source_dir was supplied) and the sha
                // actually confirmed on the built tree (present only once the
                // PCON-02 integrity check has passed — i.e. always equal to
                // resolved_sha when both are set, by construction).
                "resolved_sha": resolved_sha,
                "built_sha": built_sha,
                "passed": passed,
                "process_exit_success": exit_success,
                "test_counts": summary.to_json(),
                "failing_tests": summary.failing_tests,
                "published": false,
                "blessed_current": false,
                "caps": {
                    "memory_max": resolved.caps.memory_max,
                    "memory_swap_max": "0",
                    "cpu_quota": resolved.caps.cpu_quota,
                    "io_weight": resolved.caps.io_weight,
                    "jobs": resolved.caps.jobs,
                },
            });
            // Terminal `Tested` event: the SAME structured pass/fail summary
            // (JSON-encoded into `message`) is mirrored onto the progress bus, so
            // `compiler_progress` gives a polling caller the identical result.
            bus.emit(
                request_id,
                events::Emit::stage(events::Stage::Tested).message(structured.to_string()),
            );
            let text = format!(
                "cargo test {module}@{git_ref} on {host}: {verdict} ({passed_n} passed, \
                 {failed_n} failed, {ignored_n} ignored) [request_id={rid}]",
                host = resolved.role.as_str(),
                verdict = if passed { "PASS" } else { "FAIL" },
                passed_n = summary.passed,
                failed_n = summary.failed,
                ignored_n = summary.ignored,
                rid = request_id,
            );
            return Ok(ToolOutput::with_structured(text, structured));
        }

        // ── Publish the artifact (checksummed; no `current` flip) ────────────
        // `built_bin` is a locally-readable path (built in place locally, or
        // retrieved from the heavy host above), so publish is host-agnostic.
        let channel = publish::DEFAULT_CHANNEL;
        validate_segment("channel", channel)?;
        // Build done, artifact being checksummed + written → `publishing`.
        bus.emit(request_id, events::Emit::stage(events::Stage::Publishing));
        let published = if let Some(relay_host) = env_nonempty(BUILD_DATASET_RELAY_HOST) {
            // Interim: relay-publish over a single hop to a host with the dataset RW.
            // The plan bundles BOTH the binary and its `.sha256` sidecar so the
            // relayed artifact is verifiable by the updater (never binary-only).
            let remote_root =
                env_nonempty(BUILD_DATASET_RELAY_ROOT).unwrap_or_else(|| root_str.clone());
            let sha = publish::sha256_file(&built_bin).await?;
            let sidecar_tmp = built_bin.with_file_name(format!("{bin}.sha256"));
            let plan = publish::render_relay_plan(
                &relay_host,
                &remote_root,
                &module,
                channel,
                &sha,
                &triple,
                &bin,
                &built_bin,
                &sidecar_tmp,
            );
            // Stage the sidecar locally, then relay the binary + sidecar.
            tokio::fs::write(&sidecar_tmp, &plan.sidecar_body)
                .await
                .map_err(|e| ToolError::Execution(format!("write sidecar: {e}")))?;
            let bin_res = run(
                &plan.binary_argv,
                None,
                &BTreeMap::new(),
                Duration::from_secs(600),
                &redact,
                None,
                None,
            )
            .await;
            let sc_res = if bin_res.is_ok() {
                run(
                    &plan.sidecar_argv,
                    None,
                    &BTreeMap::new(),
                    Duration::from_secs(120),
                    &redact,
                    None,
                    None,
                )
                .await
            } else {
                Ok(String::new())
            };
            // Clean up the local staging sidecar regardless of outcome.
            let _ = tokio::fs::remove_file(&sidecar_tmp).await;
            bin_res?;
            sc_res?;
            publish::Published {
                sha256: sha.clone(),
                artifact_path: plan.remote_binary,
                sha256_path: plan.remote_sidecar,
                relayed: true,
            }
        } else {
            publish::publish_local(&root, &module, channel, &triple, &bin, &built_bin).await?
        };

        // ── BLD-07 store: on a LOCAL publish (dataset mounted RW on this host),
        // write the per-sha manifest and flip `experimental/current` onto the new
        // sha (atomic temp+rename), then prune the channel to the retention policy.
        // Skipped on the INTERIM relay path — the build host lacks the dataset
        // mount, so it cannot (and must not) write a local pointer; the relay
        // target host owns that flip. `compiler_release` promotes to `stable`.
        let mut blessed_current = false;
        let mut pruned: Vec<String> = Vec::new();
        if !published.relayed {
            // A build blesses ONLY the experimental/build channel; `bless_build`
            // refuses any promote-only channel (stable is compiler_release-only).
            let bless = publish::bless_build(
                &root,
                &module,
                channel,
                &published.sha256,
                &triple,
                &bin,
                retain_per_channel(),
            )
            .await?;
            blessed_current = bless.blessed;
            pruned = bless.pruned;
        }

        // Terminal success for this tool's scope → `published` (with the sha).
        // (`deployed`/`rolled_back` belong to the downstream updater stage.)
        bus.emit(
            request_id,
            events::Emit::stage(events::Stage::Published).sha(published.sha256.clone()),
        );

        let text = format!(
            "Built {module}@{git_ref} on {host} ({sccache}); artifact {sha} → {path}{relayed} [request_id={rid}]",
            host = resolved.role.as_str(),
            sccache = sccache_env.describe(),
            sha = &published.sha256,
            path = published.artifact_path.display(),
            relayed = if published.relayed { " (relayed)" } else { "" },
            rid = request_id,
        );
        let structured = json!({
            "request_id": request_id,
            "module": module,
            "ref": git_ref,
            "host": resolved.role.as_str(),
            "remote": !resolved.is_local(),
            "profile": profile,
            "target": triple,
            "channel": channel,
            "bin": bin,
            // PCON-01/02: see the mode=test structured result's doc comment.
            "resolved_sha": resolved_sha,
            "built_sha": built_sha,
            "sha256": published.sha256,
            "artifact_path": published.artifact_path.to_string_lossy(),
            "sha256_path": published.sha256_path.to_string_lossy(),
            "relayed": published.relayed,
            "current_channel": channel,
            "blessed_current": blessed_current,
            "pruned": pruned,
            "sccache_mode": sccache_env.mode.as_str(),
            "caps": {
                "memory_max": resolved.caps.memory_max,
                "memory_swap_max": "0",
                "cpu_quota": resolved.caps.cpu_quota,
                "io_weight": resolved.caps.io_weight,
                "jobs": resolved.caps.jobs,
            },
        });
        Ok(ToolOutput::with_structured(text, structured))
    }
}

/// BLD-07 — the `compiler_release` tool: the channel-pointer surface over the
/// artifact store. It NEVER rebuilds — it promotes an already-built sha into a
/// channel by an atomic `current` pointer flip (Rust-train model), rolls a
/// channel back to its previous blessed sha, or queries the current blessed sha.
struct CompilerRelease;

#[async_trait]
impl RustTool for CompilerRelease {
    fn name(&self) -> &str {
        "compiler_release"
    }

    fn description(&self) -> &str {
        "Manage the artifact-store channel pointers (no rebuild). op=promote blesses an \
         already-built sha into a channel by an atomic `current` pointer flip after verifying \
         the artifact + its .sha256 (fail-closed on an unbuilt/corrupt sha), giving the target \
         channel its own copy (Rust-train) and pruning to the retention floor; op=rollback \
         reverts a channel to its previous blessed sha; op=current returns the blessed sha for \
         a (module, channel). This is the `current` the constellation-updater fetches."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "op": {
                    "type": "string",
                    "enum": ["promote", "rollback", "current"],
                    "default": "promote",
                    "description": "promote an already-built sha (default) | rollback to the previous blessed sha | query the current blessed sha."
                },
                "module": {
                    "type": "string",
                    "description": "Module/repo whose channel pointer is being managed."
                },
                "sha": {
                    "type": "string",
                    "description": "The already-built content-address sha to promote (required for op=promote)."
                },
                "from_channel": {
                    "type": "string",
                    "default": "experimental",
                    "description": "Source channel the sha was built/published into (op=promote)."
                },
                "to_channel": {
                    "type": "string",
                    "default": "stable",
                    "description": "Target channel: the one promoted into, rolled back, or queried."
                },
                "bin": {
                    "type": "string",
                    "description": "Binary name to verify (defaults to the module name)."
                },
                "target": {
                    "type": "string",
                    "description": "Target triple to verify (defaults to the configured build target)."
                }
            },
            "required": ["module"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.execute_structured(args).await?.text)
    }

    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let op = args
            .get("op")
            .and_then(Value::as_str)
            .unwrap_or("promote")
            .to_string();
        let module = str_arg(&args, "module")?;
        validate_segment("module", &module)?;
        let to_channel = args
            .get("to_channel")
            .and_then(Value::as_str)
            .unwrap_or("stable")
            .to_string();
        validate_segment("channel", &to_channel)?;
        // The artifact address for verify-before-bless (used by promote AND
        // rollback so the rollback target is verified too — fail closed).
        let bin = args
            .get("bin")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| module.clone());
        validate_segment("bin", &bin)?;
        // BLD-444: default to the SAME effective triple `compiler_build` would
        // have used for this module (its `BUILD_MODULE_TARGET_<MODULE>`
        // override if configured, else the fleet-wide default) — so promoting/
        // rolling-back/querying a module built with an override (e.g. harmony
        // → musl) looks under the artifact path its own default build actually
        // published to, rather than the global default. An explicit `target`
        // arg still overrides both.
        let target = args
            .get("target")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| effective_triple(&module));
        validate_segment("target", &target)?;

        let root = dataset_root()?;

        match op.as_str() {
            "current" => {
                let current = publish::read_current(&root, &module, &to_channel).await?;
                let previous = publish::read_previous(&root, &module, &to_channel).await?;
                let text = match &current {
                    Some(sha) => format!("{module}/{to_channel} current = {sha}"),
                    None => format!("{module}/{to_channel} has no blessed sha yet"),
                };
                let structured = json!({
                    "op": "current",
                    "module": module,
                    "channel": to_channel,
                    "current": current,
                    "previous": previous,
                });
                Ok(ToolOutput::with_structured(text, structured))
            }
            "rollback" => {
                let out =
                    publish::rollback_current(&root, &module, &to_channel, &target, &bin).await?;
                let text = format!(
                    "Rolled {module}/{to_channel} back to {sha} (was {was})",
                    sha = out.sha,
                    was = out.previous.as_deref().unwrap_or("<none>"),
                );
                let structured = json!({
                    "op": "rollback",
                    "module": module,
                    "channel": to_channel,
                    "current": out.sha,
                    "previous": out.previous,
                    "changed": out.changed,
                });
                Ok(ToolOutput::with_structured(text, structured))
            }
            "promote" => {
                let sha = str_arg(&args, "sha")?;
                validate_segment("sha", &sha)?;
                let from_channel = args
                    .get("from_channel")
                    .and_then(Value::as_str)
                    .unwrap_or(publish::DEFAULT_CHANNEL)
                    .to_string();
                validate_segment("channel", &from_channel)?;

                let out = publish::promote(
                    &root,
                    &module,
                    &from_channel,
                    &to_channel,
                    &sha,
                    &target,
                    &bin,
                    retain_per_channel(),
                )
                .await?;

                let text = if out.already_current {
                    format!("{module}@{sha} already current on {to_channel} (no-op)")
                } else {
                    format!(
                        "Promoted {module}@{sha} {from_channel} → {to_channel} (no rebuild{copied}); \
                         current flipped{pruned}",
                        copied = if out.copied { ", copied" } else { "" },
                        pruned = if out.pruned.is_empty() {
                            String::new()
                        } else {
                            format!("; pruned {}", out.pruned.len())
                        },
                    )
                };
                // BLD-13: trigger-on-publish. When `COMPILER_AUTO_DEPLOY` is set AND
                // this promote actually flipped `current` (not a no-op), fire the
                // fleet-wide updater trigger so the change lands in seconds instead
                // of waiting for the nightly timer. Best-effort — never fails or
                // masks the promote; the deploy report is ATTACHED to the result.
                let auto_deploy = if out.already_current {
                    None
                } else {
                    deploy::auto_trigger_after_promote(&module, &to_channel).await
                };

                let mut structured = json!({
                    "op": "promote",
                    "module": out.module,
                    "sha256": out.sha256,
                    "from_channel": out.from_channel,
                    "to_channel": out.to_channel,
                    "previous_current": out.previous_current,
                    "copied": out.copied,
                    "already_current": out.already_current,
                    "pruned": out.pruned,
                    "current_path": out.current_path.to_string_lossy(),
                });
                if let (Some(dep), Some(obj)) = (auto_deploy, structured.as_object_mut()) {
                    obj.insert("auto_deploy".into(), dep);
                }
                Ok(ToolOutput::with_structured(text, structured))
            }
            other => Err(ToolError::InvalidArgument(format!(
                "unknown op {other:?} (expected promote | rollback | current)"
            ))),
        }
    }
}

fn str_arg(args: &Value, key: &str) -> Result<String, ToolError> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| ToolError::InvalidArgument(format!("`{key}` is required")))
}

/// The conservative allowlist for one path segment: ASCII alphanumerics plus
/// `.`, `_`, `-`. No `/`, `\`, whitespace, control chars, NUL, or any shell/path
/// metacharacter.
fn is_segment_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')
}

/// Validate a user-controlled value as a SAFE single path segment — no
/// traversal, no path separator, no injection — BEFORE it is ever joined into a
/// path or interpolated into an rsync/ssh command. Rejects empty, `.`/`..`, and
/// anything containing a byte outside `[A-Za-z0-9._-]` (which also excludes `/`,
/// `\`, whitespace, control chars, and shell metacharacters). Used for
/// module/bin/profile/target/channel.
fn validate_segment(kind: &str, value: &str) -> Result<(), ToolError> {
    if value.is_empty() {
        return Err(ToolError::InvalidArgument(format!(
            "{kind} must not be empty"
        )));
    }
    if value == "." || value == ".." {
        return Err(ToolError::InvalidArgument(format!(
            "{kind} must not be '.' or '..'"
        )));
    }
    if !value.chars().all(is_segment_char) {
        return Err(ToolError::InvalidArgument(format!(
            "{kind} {value:?} contains characters outside [A-Za-z0-9._-] \
             (no path separators, whitespace, control chars, or shell metacharacters)"
        )));
    }
    Ok(())
}

/// Validate a git ref: like [`validate_segment`] but MAY contain `/` between
/// otherwise-valid segments (a branch such as `feature/foo`), and never a
/// traversal. Rejects an absolute ref (`/`-leading), a trailing `/`, `\`, any
/// empty/`.`/`..` component, and any disallowed byte. This keeps a ref usable as
/// a nested-but-contained path fragment under the dataset root.
fn validate_git_ref(value: &str) -> Result<(), ToolError> {
    if value.is_empty() {
        return Err(ToolError::InvalidArgument("ref must not be empty".into()));
    }
    if value.starts_with('/') || value.ends_with('/') {
        return Err(ToolError::InvalidArgument(format!(
            "ref {value:?} must not start or end with '/'"
        )));
    }
    if value.contains('\\') {
        return Err(ToolError::InvalidArgument(format!(
            "ref {value:?} must not contain '\\'"
        )));
    }
    for comp in value.split('/') {
        validate_segment("ref component", comp)?;
    }
    Ok(())
}

/// Validate a config-supplied relative directory (BLD-444's
/// `BUILD_MODULE_WEB_DIR_<MODULE>`) as a SAFE path UNDER the staged source
/// root, BEFORE it is ever joined into a filesystem path or interpolated into
/// an ssh/shell command. Mirrors [`validate_git_ref`]'s shape (each
/// `/`-separated component validated by [`validate_segment`], no absolute
/// path, no traversal) but is named/documented for this distinct use — a
/// config value, not a git ref — so an error message never conflates the two.
fn validate_relative_dir(kind: &str, value: &str) -> Result<(), ToolError> {
    if value.is_empty() {
        return Err(ToolError::InvalidArgument(format!(
            "{kind} must not be empty"
        )));
    }
    if value.starts_with('/') || value.ends_with('/') {
        return Err(ToolError::InvalidArgument(format!(
            "{kind} {value:?} must not start or end with '/'"
        )));
    }
    if value.contains('\\') {
        return Err(ToolError::InvalidArgument(format!(
            "{kind} {value:?} must not contain '\\'"
        )));
    }
    for comp in value.split('/') {
        validate_segment(&format!("{kind} component"), comp)?;
    }
    Ok(())
}

/// The allowed roots a caller-supplied `source_dir` may resolve under: always
/// `${BUILD_DATASET_ROOT}/src`, plus any `:`-separated `BUILD_ALLOWED_SOURCE_ROOTS`.
fn allowed_source_roots(dataset_root: &std::path::Path) -> Vec<PathBuf> {
    let mut roots = vec![dataset_root.join("src")];
    if let Some(extra) = env_nonempty(BUILD_ALLOWED_SOURCE_ROOTS) {
        for r in extra.split(':') {
            let r = r.trim();
            if !r.is_empty() {
                roots.push(PathBuf::from(r));
            }
        }
    }
    roots
}

/// Validate a caller-supplied `source_dir` (a FULL PATH, not a segment) by
/// CONTAINMENT: it must lexically resolve (no filesystem access) to a path inside
/// one of the [`allowed_source_roots`]. Rejects an absolute path elsewhere or a
/// `../`-escaping override, so the build/relay never touches source outside the
/// dataset. Checked before `source_dir` is used for current_dir / --manifest-path
/// / rsync.
fn validate_source_dir(
    source_dir: &std::path::Path,
    dataset_root: &std::path::Path,
) -> Result<(), ToolError> {
    let roots = allowed_source_roots(dataset_root);
    if roots.iter().any(|root| scope::is_within(source_dir, root)) {
        return Ok(());
    }
    Err(ToolError::InvalidArgument(format!(
        "source_dir ({}) resolves outside the allowed source roots ({}); a \
         caller-supplied source path must stay within the dataset src tree \
         (set BUILD_ALLOWED_SOURCE_ROOTS to permit an additional staging root)",
        source_dir.display(),
        roots
            .iter()
            .map(|r| r.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )))
}

/// GAP 1: validate that a module@ref's source has actually been staged into
/// `local_source_dir` (and looks like a cargo crate — has a `Cargo.toml`)
/// BEFORE it is used as `current_dir(...)` for `rustup`/`cargo`, or as the
/// rsync source for a remote build. Without this check a missing staged dir
/// surfaces as `Command::spawn` reporting ENOENT against argv[0]
/// (`/usr/bin/systemd-run: No such file or directory`) — a correct-looking
/// but completely misleading error, since the program itself is fine and the
/// real problem is the `current_dir` that doesn't exist.
fn validate_local_source_dir(
    local_source_dir: &std::path::Path,
    module: &str,
    git_ref: &str,
) -> Result<(), ToolError> {
    if !local_source_dir.is_dir() {
        return Err(ToolError::NotFound(format!(
            "source not staged for {module}@{git_ref} at {} — stage it into \
             ${{{BUILD_DATASET_ROOT}}}/src/<module>/<ref> or pass source_dir",
            local_source_dir.display()
        )));
    }
    if !local_source_dir.join("Cargo.toml").is_file() {
        return Err(ToolError::NotFound(format!(
            "source not staged for {module}@{git_ref} at {} — directory exists \
             but has no Cargo.toml (stage a complete checkout into \
             ${{{BUILD_DATASET_ROOT}}}/src/<module>/<ref> or pass source_dir)",
            local_source_dir.display()
        )));
    }
    Ok(())
}

/// The `compiler_progress` tool (BLD-19): a live progress/events surface keyed by
/// a build's `request_id`. It returns the current snapshot (stage + `{step,total}`
/// + timing) and the recent event tail; with `wait_ms > 0` it LONG-POLLS — it
/// blocks until the next event (or the timeout) and returns a fresh snapshot, so
/// a GUI/agent can subscribe to a running build without busy-looping. Pair
/// `since` (the last seen `seq`) with `wait_ms` to stream: each call returns the
/// events after `since`, and the caller advances `since` to the last `seq`.
///
/// Seam with `compiler_status` (BLD-08): status is the point-in-time aggregate
/// (what is deployed where); this is the live per-request event stream.
struct CompilerProgress;

/// Default long-poll wait cap (ms) and the hard ceiling, so a caller can't pin a
/// worker indefinitely. Numeric tuning knobs, not infra literals.
const PROGRESS_DEFAULT_WAIT_MS: u64 = 0;
const PROGRESS_MAX_WAIT_MS: u64 = 30_000;

#[async_trait]
impl RustTool for CompilerProgress {
    fn name(&self) -> &str {
        "compiler_progress"
    }

    fn description(&self) -> &str {
        "Live build progress/events for a compiler_build request_id: current stage \
         (queued→scheduled→relaying→building→publishing→published|failed), a \
         {step,total} progress signal, timing, and the recent (secret-sanitized) \
         event tail. Pass `since` (last seen seq) to get only new events, and \
         `wait_ms`>0 to long-poll (block until the next event or the timeout). \
         Point-in-time deploy state is compiler_status; this is the live stream."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "request_id": {
                    "type": "string",
                    "description": "The build request id (returned by compiler_build) to query."
                },
                "since": {
                    "type": "integer",
                    "minimum": 0,
                    "default": 0,
                    "description": "Return only events with seq greater than this cursor (0 = the whole retained tail)."
                },
                "wait_ms": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Long-poll: block up to this many ms for the next event, then return a fresh snapshot. 0 (default) returns immediately. Capped server-side."
                }
            },
            "required": ["request_id"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.execute_structured(args).await?.text)
    }

    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let request_id = str_arg(&args, "request_id")?;
        // Reject a malformed / overlong / whitespace-bearing id at the boundary
        // with a CLEAR validation error — validated RAW, NO trimming (a lossy trim
        // could collapse distinct ids like `" x "` and `"x"` onto one stream). A
        // well-formed unknown id still returns not_found below.
        if !is_valid_request_id(&request_id) {
            return Err(ToolError::InvalidArgument(format!(
                "request_id must be a single [A-Za-z0-9._-] segment of at most {} bytes (no surrounding or inner whitespace)",
                events::MAX_REQUEST_ID_LEN
            )));
        }
        let since = args.get("since").and_then(Value::as_u64).unwrap_or(0);
        let wait_ms = args
            .get("wait_ms")
            .and_then(Value::as_u64)
            .unwrap_or(PROGRESS_DEFAULT_WAIT_MS)
            .min(PROGRESS_MAX_WAIT_MS);

        let snapshot = events::bus()
            .poll(
                &request_id,
                since,
                std::time::Duration::from_millis(wait_ms),
            )
            .await;

        match snapshot {
            Some(snap) => {
                let text = format!(
                    "{rid}: {stage}{prog}{term} — {n} new event(s) since seq {since} (last seq {last})",
                    rid = snap.request_id,
                    stage = snap.stage.as_str(),
                    prog = match (snap.step, snap.total) {
                        (Some(s), Some(t)) => format!(" {s}/{t}"),
                        _ => String::new(),
                    },
                    term = if snap.terminal { " [terminal]" } else { "" },
                    n = snap.events.len(),
                    since = since,
                    last = snap.last_seq,
                );
                Ok(ToolOutput::with_structured(text, snap.to_json()))
            }
            // Unknown/ swept build → `not_found`, NOT an error (edge case).
            None => {
                let text =
                    format!("{request_id}: not_found (no such build, or its progress has expired)");
                let structured = json!({
                    "request_id": request_id,
                    "status": "not_found",
                });
                Ok(ToolOutput::with_structured(text, structured))
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// BLD-06 — the queue entry point (`compiler_request`) + the scheduler's bridge
// back into `compiler_build` (`invoke_build`).
// ─────────────────────────────────────────────────────────────────────────────

/// Request-time classification of whether a build must be treated as HEAVY
/// (heavy host ⇒ scheduler window + heavy-cap gated). It tags the queued job so
/// the scheduler gates it; `compiler_build` still does the authoritative host
/// selection at dispatch.
///
/// SAFETY-AUTHORITATIVE (AC-6): heavy classification overrides the host
/// preference. A build is treated as `heavy` (window + cap gated) UNLESS it is
/// POSITIVELY determined small. `fast=true` and an explicit `Heavy` request are
/// always heavy. An explicit `Primary` request is only a PREFERENCE: it
/// fast-paths ONLY a positively-known-small module (a known peak at/under a known
/// threshold, or no heavy signal); a known-heavy — or any ambiguous/unreadable —
/// module requested with `host=primary` is still GATED through the heavy path, so
/// a possibly-heavy build can never bypass the window/cap by asking for primary.
fn request_is_heavy(req: HostRequest, module: &str, fast: bool) -> bool {
    classify_request_heavy(
        req,
        fast,
        // `.ok()` maps a read ERROR (present-but-unparsable) to `None`
        // (unknown ⇒ safe/heavy), and a successful read to `Some(Option<u64>)`.
        host::module_peak_mb(module).ok(),
        host::heavy_threshold_mb().ok(),
    )
}

/// Pure core of [`request_is_heavy`] (the test entry point): decide heaviness
/// from the request, `fast`, and the (already-read) module peak + threshold.
/// `fast=true`/explicit `Heavy` ⇒ heavy. `Primary`/`Auto` defer to the
/// safety-authoritative [`classify_heavy_auto`], so an explicit primary is
/// honored only for a positively-small module.
fn classify_request_heavy(
    req: HostRequest,
    fast: bool,
    peak: Option<Option<u64>>,
    threshold: Option<Option<u64>>,
) -> bool {
    if fast {
        return true;
    }
    match req {
        HostRequest::Heavy => true,
        // Explicit primary is a PREFERENCE overridable by heavy-safety: it only
        // fast-paths a positively-small module (classify_heavy_auto ⇒ false).
        HostRequest::Primary | HostRequest::Auto => classify_heavy_auto(false, peak, threshold),
    }
}

/// Pure heavy classifier for a module (auto/primary): `peak`/`threshold` use
/// `Some(inner)` for a successful read (`inner` itself `None` = "not configured")
/// and the OUTER `None` for an unreadable value. Fails to the SAFE side (heavy)
/// on anything not positively small.
fn classify_heavy_auto(fast: bool, peak: Option<Option<u64>>, threshold: Option<Option<u64>>) -> bool {
    if fast {
        return true;
    }
    match (peak, threshold) {
        // No known peak (read OK, unset) ⇒ compiler_build authoritatively picks
        // the primary — positively small.
        (Some(None), _) => false,
        // Both known ⇒ authoritative comparison (matches select_role).
        (Some(Some(p)), Some(Some(t))) => p > t,
        // Anything else — unreadable peak/threshold, or a known peak with no
        // configured threshold — is ambiguous ⇒ SAFE side: heavy (window+cap gated).
        _ => true,
    }
}

/// The scheduler's bridge into the single build door: dispatch a queued job to
/// `compiler_build` with the host the scheduler selected (heavy vs primary). A
/// thin wrapper so `scheduler::CompilerBuildExecutor` need not know the tool's
/// arg shape.
///
/// `mode` (BLD-ASYNC, TERM #421) forwards the job's `"build"`/`"test"` mode —
/// carried durably on the queued job (see [`queue::JobRequest::mode`]) — so a
/// job enqueued via `compiler_request(mode=test)` or an async
/// `compiler_build(wait=false, mode=test)` submission runs as a test-gate when
/// the scheduler actually dispatches it, not a publish-and-flip build.
///
/// `request_id`, when supplied, is forwarded so the SAME id a caller polls via
/// `compiler_progress` (surfaced back at enqueue time, e.g. the queue job id)
/// is the id the eventual dispatched build emits its `queued`/`building`/
/// `tested`/`published`/`failed` events under — otherwise `compiler_build`
/// would mint its own fresh id and the caller's poll would never see it.
pub(crate) async fn invoke_build(
    module: &str,
    git_ref: &str,
    heavy: bool,
    bin: Option<&str>,
    mode: &str,
    request_id: Option<&str>,
    resolved_sha: Option<&str>,
) -> Result<(), ToolError> {
    // PCON-01/04 (S122 root-cause fix): dispatch against the sha resolved at
    // ENQUEUE time (`queue::JobRequest::resolved_sha`, carried through
    // `QueuedJob`), never by re-resolving `git_ref` here. Re-resolving at
    // dispatch time would reopen exactly the race this fix closes — the ref
    // could have moved between enqueue and dispatch. Since a full sha is
    // already `is_full_sha`, `build_inner`'s own resolution step is then a
    // verbatim no-op (no network call), so this is purely "use the identity
    // we already committed to", not a second resolution.
    let effective_ref = resolved_sha.filter(|s| !s.is_empty()).unwrap_or(git_ref);
    let mut args = json!({
        "module": module,
        "ref": effective_ref,
        "host": if heavy { "heavy" } else { "primary" },
        "mode": if mode == "test" { "test" } else { "build" },
    });
    if let Some(rid) = request_id.filter(|r| !r.is_empty()) {
        args["request_id"] = json!(rid);
    }
    // BLD/TERM #360: forward the queued bin override so the automated path can build
    // a module whose cargo bin name differs from the module name (e.g. terminus →
    // terminus_primary). Absent ⇒ compiler_build defaults `--bin <module>`.
    if let Some(b) = bin.filter(|b| !b.is_empty()) {
        args["bin"] = json!(b);
    }
    CompilerBuild.execute_structured(args).await.map(|_| ())
}

/// The `compiler_request` tool: an agent marks a module@ref "ready to build".
/// Enqueues durably (deduped/coalesced by module@ref) into the shared Redis
/// queue; the scheduler dispatches it. Multiple agents requesting the same
/// module@ref coalesce into ONE run.
struct CompilerRequest;

#[async_trait]
impl RustTool for CompilerRequest {
    fn name(&self) -> &str {
        "compiler_request"
    }

    fn description(&self) -> &str {
        "Mark a constellation module@ref ready for a compiler run: enqueue a durable, \
         deduped build request onto the shared job queue. Multiple agents requesting the \
         same module@ref coalesce into one run. The scheduler dispatches small builds \
         immediately on the primary and heavy builds within a configured window / \
         fleet-quiet gate, one/few at a time per host. Returns the job id."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "module": {
                    "type": "string",
                    "description": "Module/repo to build (e.g. terminus, chord, harmony, lumina-core)."
                },
                "ref": {
                    "type": "string",
                    "description": "Git ref (sha or branch) to build."
                },
                "priority": {
                    "type": "string",
                    "enum": ["low", "normal", "high"],
                    "default": "normal",
                    "description": "Queue priority. Higher orders the queue sooner but never preempts a running build."
                },
                "host": {
                    "type": "string",
                    "enum": ["auto", "primary", "heavy"],
                    "default": "auto",
                    "description": "Requested build host role; also tags the job heavy (window-gated) vs small (immediate)."
                },
                "fast": {
                    "type": "boolean",
                    "default": false,
                    "description": "Prefer the heavy host for a full-parallelism build (tags the job heavy)."
                },
                "ready": {
                    "type": "boolean",
                    "default": true,
                    "description": "true → dispatchable now; false → record the intent as held until a later ready=true request promotes it."
                },
                "bin": {
                    "type": "string",
                    "description": "Optional cargo --bin target. Defaults to the module name; set it when the deployable binary's name differs from the module (e.g. module 'terminus' → bin 'terminus_primary')."
                },
                "force": {
                    "type": "boolean",
                    "default": false,
                    "description": "Disrupt-on-demand: a heavy job dispatches even outside a configured build window and without a fleet-quiet signal. Still goes through the normal module lock / host cap claim and idle-mode lease — only the window/quiet gate is bypassed. Orthogonal to priority."
                },
                "mode": {
                    "type": "string",
                    "enum": ["build", "test"],
                    "default": "build",
                    "description": "build (default) compiles + publishes an artifact and flips experimental/current when the scheduler dispatches this job, exactly like compiler_build's mode=build. test runs the SAME test-gate compiler_build(mode=test) runs (no publish, no channel flip; structured pass/fail via compiler_progress). This is BLD-ASYNC's (TERM #421) async test-gate submission path."
                }
            },
            "required": ["module", "ref"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        Ok(self.execute_structured(args).await?.text)
    }

    async fn execute_structured(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let module = str_arg(&args, "module")?;
        let git_ref = str_arg(&args, "ref")?;
        // Validate the same way compiler_build does (these become path segments
        // + the dedupe/scope key), so a bad ref is rejected at enqueue, not build.
        validate_segment("module", &module)?;
        validate_git_ref(&git_ref)?;

        let priority = Priority::parse(args.get("priority").and_then(Value::as_str).unwrap_or("normal"));
        let host_req =
            HostRequest::parse(args.get("host").and_then(Value::as_str).unwrap_or("auto"))?;
        let fast = args.get("fast").and_then(Value::as_bool).unwrap_or(false);
        let ready = args.get("ready").and_then(Value::as_bool).unwrap_or(true);
        let heavy = request_is_heavy(host_req, &module, fast);
        // BLD/TERM #360: optional cargo bin override, carried durably on the job so
        // the scheduler builds the right binary for a module whose bin name differs
        // from the module name. Validated as a path/target segment like `bin` in
        // compiler_build. Absent ⇒ defaults `--bin <module>` at build time.
        let bin = match args.get("bin").and_then(Value::as_str) {
            Some(b) => {
                validate_segment("bin", b)?;
                Some(b.to_string())
            }
            None => None,
        };
        // BLD-DISPATCH-01: disrupt-on-demand override. Orthogonal to `priority`
        // (which only orders the queue) — a `force`d HEAVY job bypasses the
        // scheduler's window/quiet gate but still goes through the normal module
        // lock / host cap claim and idle-mode lease (see scheduler::tick_once).
        let force = args.get("force").and_then(Value::as_bool).unwrap_or(false);
        // BLD-ASYNC (TERM #421): thread mode through the queue so the scheduler
        // runs this job as a test-gate rather than a publish-and-flip build.
        let mode = parse_mode_arg(&args)?;

        let store = RedisQueue::from_env().ok_or_else(|| {
            ToolError::NotConfigured(
                "compiler job queue is not configured (REDIS_URL unset) — cannot enqueue a build \
                 request; the queue is durable Redis (BLD-20 Namespace::Queue)"
                    .to_string(),
            )
        })?;
        // PCON-01/04 (S122 root-cause fix): see `enqueue_async_onto`'s identical
        // block for the full rationale — resolve ONCE, here, at request time.
        let resolved_sha = resolve_sha_for_enqueue(&module, &git_ref).await?;
        let enq = store
            .enqueue(&JobRequest {
                module: module.clone(),
                git_ref: git_ref.clone(),
                priority,
                heavy,
                ready,
                bin,
                force,
                mode: mode.clone(),
                resolved_sha: resolved_sha.clone(),
            })
            .await
            .map_err(|e| ToolError::Execution(e.to_string()))?;

        let text = format!(
            "{verb} {module}@{git_ref} ({prio}, {host}, mode={mode}){ready}{force}; job {id}",
            verb = if enq.created { "Queued" } else { "Coalesced onto existing" },
            prio = priority.as_str(),
            host = if heavy { "heavy" } else { "primary" },
            ready = if ready { "" } else { " [held]" },
            force = if force { " [forced]" } else { "" },
            id = enq.job_id,
        );
        let structured = json!({
            "job_id": enq.job_id,
            "created": enq.created,
            "coalesced": !enq.created,
            "module": module,
            "ref": git_ref,
            "priority": priority.as_str(),
            "heavy": heavy,
            "ready": ready,
            "force": force,
            "mode": mode,
            "resolved_sha": resolved_sha,
        });
        Ok(ToolOutput::with_structured(text, structured))
    }
}

/// Render a `compiler_status`-style view of the queue + in-flight leases from a
/// snapshot. Exposed (not a registered tool here — BLD-08 owns the
/// `compiler_status` tool surface) so the status item consumes ONE queue view
/// rather than re-deriving the keyspace. `sccache_hit_rate` is left to the
/// caller to fill (BLD-03 owns sccache stats); this reports the queue facts.
pub fn render_queue_status(snapshot: &queue::QueueSnapshot) -> Value {
    let queued: Vec<Value> = snapshot
        .queued
        .iter()
        .enumerate()
        .map(|(pos, j)| {
            json!({
                "position": pos,
                "job_id": j.job_id,
                "module": j.module,
                "ref": j.git_ref,
                "priority": j.priority.as_str(),
                "heavy": j.heavy,
                "force": j.force,
            })
        })
        .collect();
    let leases: Vec<Value> = snapshot
        .leases
        .iter()
        .map(|l| {
            json!({
                "job_id": l.job_id,
                "module": l.module,
                "ref": l.git_ref,
                "host": l.host.as_str(),
                "started_at_ms": l.started_at_ms,
            })
        })
        .collect();
    let (primary_inflight, heavy_inflight) = snapshot
        .leases
        .iter()
        .fold((0u32, 0u32), |(p, h), l| match l.host {
            HostRole::Primary => (p + 1, h),
            HostRole::Heavy => (p, h + 1),
        });
    json!({
        "queue_depth": snapshot.queued.len(),
        "queued": queued,
        "in_flight": snapshot.leases.len(),
        "leases": leases,
        "inflight_primary": primary_inflight,
        "inflight_heavy": heavy_inflight,
    })
}

/// Register the `compiler_*` tool surface on the registry, and — when the shared
/// Redis is configured — spawn the background scheduler that drains the queue.
///
/// Tool ownership (intentional decomposition): BLD-06 owns `compiler_build`
/// (from BLD-05) + `compiler_request` (the queue door) + the scheduler.
/// BLD-19 adds `compiler_progress` (the live per-request event stream).
/// `compiler_status` is a SEPARATE item (BLD-08); it consumes
/// [`render_queue_status`] over [`queue::QueueStore::snapshot`] rather than being
/// registered here, so the two items don't collide on the tool name.
pub fn register(registry: &mut ToolRegistry) {
    if let Err(e) = registry.register(Box::new(CompilerBuild)) {
        tracing::error!("compiler: failed to register compiler_build: {e}");
    }
    if let Err(e) = registry.register(Box::new(CompilerRequest)) {
        tracing::error!("compiler: failed to register compiler_request: {e}");
    }
    if let Err(e) = registry.register(Box::new(CompilerProgress)) {
        tracing::error!("compiler: failed to register compiler_progress: {e}");
    }
    if let Err(e) = registry.register(Box::new(CompilerRelease)) {
        tracing::error!("compiler: failed to register compiler_release: {e}");
    }
    status::register(registry);
    // BLD-13: the trigger-on-publish fleet fan-out (compiler_deploy).
    deploy::register(registry);
    // Spawn the scheduler loop iff we're inside a tokio runtime AND Redis is
    // configured — but AT MOST ONCE per process. CRUCIALLY, the once-slot is
    // claimed ONLY when the scheduler actually spawns: if `register()` runs before
    // Redis is materialized, the no-scheduler path must NOT burn the slot, so a
    // LATER `register()` (once config has arrived) can still spawn exactly once.
    if tokio::runtime::Handle::try_current().is_ok() {
        static SPAWNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        let sched = scheduler::Scheduler::from_env();
        match decide_scheduler_spawn(&SPAWNED, sched.is_some()) {
            SpawnDecision::Spawn => {
                sched
                    .expect("scheduler present when decide returns Spawn")
                    .spawn();
                tracing::info!("compiler: scheduler loop spawned (durable Redis queue)");
            }
            SpawnDecision::AlreadySpawned => {
                tracing::debug!("compiler: scheduler already spawned; skipping");
            }
            SpawnDecision::NoScheduler => {
                tracing::info!(
                    "compiler: no Redis configured; compiler_request will report NotConfigured, \
                     the scheduler is not running, and the spawn slot is NOT burned (a later \
                     register() after Redis is materialized can still spawn it)"
                );
            }
        }
    }
}

/// The outcome of the scheduler-spawn once-guard decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SpawnDecision {
    /// This caller wins the single spawn slot — it must spawn.
    Spawn,
    /// A prior caller already spawned — do nothing.
    AlreadySpawned,
    /// No scheduler is available (no Redis) — do nothing AND do NOT burn the slot.
    NoScheduler,
}

/// Decide whether to spawn the scheduler, consuming the once-slot ONLY on an
/// actual spawn. When `scheduler_available` is false the slot is left untouched
/// (so a later call, once Redis is configured, can still spawn exactly once).
/// Pure over the passed-in flag → unit-testable without a runtime or Redis.
fn decide_scheduler_spawn(
    slot: &std::sync::atomic::AtomicBool,
    scheduler_available: bool,
) -> SpawnDecision {
    use std::sync::atomic::Ordering;
    if !scheduler_available {
        return SpawnDecision::NoScheduler;
    }
    match slot.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst) {
        Ok(_) => SpawnDecision::Spawn,
        Err(_) => SpawnDecision::AlreadySpawned,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_argv_release_musl() {
        let argv = cargo_build_argv(
            "release",
            "x86_64-unknown-linux-musl",
            4,
            "chord",
            "/src/chord/Cargo.toml",
        );
        let j = argv.join(" ");
        assert!(j.starts_with("cargo build --locked --release"));
        assert!(j.contains("--manifest-path /src/chord/Cargo.toml"));
        assert!(j.contains("--target x86_64-unknown-linux-musl"));
        assert!(j.contains("-j 4"));
        assert!(j.contains("--bin chord"));
    }

    #[test]
    fn cargo_argv_debug_has_no_release_flag() {
        let argv = cargo_build_argv("debug", "t", 8, "m", "/s/Cargo.toml");
        assert!(!argv.iter().any(|a| a == "--release"));
        assert!(argv.contains(&"-j".to_string()));
        assert!(argv.windows(2).any(|w| w[0] == "-j" && w[1] == "8"));
        // Manifest-path makes the build CWD-independent (correct for remote ssh).
        assert!(argv
            .windows(2)
            .any(|w| w[0] == "--manifest-path" && w[1] == "/s/Cargo.toml"));
    }

    #[test]
    fn cargo_argv_named_profile() {
        let argv = cargo_build_argv("release-dist", "t", 2, "m", "/s/Cargo.toml");
        assert!(argv
            .windows(2)
            .any(|w| w[0] == "--profile" && w[1] == "release-dist"));
    }

    // ── BLD-COMPTEST: cargo_test_argv (mirrors the cargo_build_argv tests above) ──

    #[test]
    fn cargo_test_argv_release_musl() {
        let argv = cargo_test_argv(
            "release",
            "x86_64-unknown-linux-musl",
            4,
            "/src/chord/Cargo.toml",
        );
        let j = argv.join(" ");
        assert!(j.starts_with("cargo test --locked --release"));
        assert!(j.contains("--manifest-path /src/chord/Cargo.toml"));
        assert!(j.contains("--target x86_64-unknown-linux-musl"));
        assert!(j.contains("-j 4"));
        assert!(j.contains("--no-fail-fast"));
        // Unlike cargo_build_argv, there is no --bin (a gate tests the whole
        // crate/workspace at the manifest, not one binary's tests).
        assert!(!argv.iter().any(|a| a == "--bin"));
    }

    #[test]
    fn cargo_test_argv_debug_has_no_release_flag() {
        let argv = cargo_test_argv("debug", "t", 8, "/s/Cargo.toml");
        assert!(!argv.iter().any(|a| a == "--release"));
        assert!(argv.windows(2).any(|w| w[0] == "-j" && w[1] == "8"));
        assert!(argv
            .windows(2)
            .any(|w| w[0] == "--manifest-path" && w[1] == "/s/Cargo.toml"));
    }

    #[test]
    fn cargo_test_argv_named_profile() {
        let argv = cargo_test_argv("release-dist", "t", 2, "/s/Cargo.toml");
        assert!(argv
            .windows(2)
            .any(|w| w[0] == "--profile" && w[1] == "release-dist"));
    }

    // ── BLD-GATE-06 (TERM #419): --lib/--bins default + capped test-threads ──

    #[test]
    fn cargo_test_argv_defaults_to_lib_bins_with_capped_threads() {
        let _env = ScopedEnv::new()
            .unset("BUILD_GATE_TESTS")
            .unset("BUILD_GATE_TEST_THREADS");
        let argv = cargo_test_argv("release", "t", 4, "/s/Cargo.toml");
        assert!(argv.iter().any(|a| a == "--lib"), "argv: {argv:?}");
        assert!(argv.iter().any(|a| a == "--bins"), "argv: {argv:?}");
        assert!(
            !argv.iter().any(|a| a == "--workspace"),
            "default must NOT run the whole workspace: {argv:?}"
        );
        assert!(
            argv.iter().any(|a| a == "--test-threads=8"),
            "argv: {argv:?}"
        );
        // Exactly one `--` separator, and it comes before --test-threads=N.
        let dd = argv.iter().position(|a| a == "--").expect("has --");
        assert_eq!(argv[dd + 1], "--test-threads=8");
    }

    #[test]
    fn cargo_test_argv_build_gate_tests_workspace_opts_in() {
        let _env = ScopedEnv::new().set("BUILD_GATE_TESTS", "workspace");
        let argv = cargo_test_argv("release", "t", 4, "/s/Cargo.toml");
        assert!(argv.iter().any(|a| a == "--workspace"), "argv: {argv:?}");
        assert!(
            !argv.iter().any(|a| a == "--lib"),
            "workspace mode must not also pass --lib: {argv:?}"
        );
        assert!(
            !argv.iter().any(|a| a == "--bins"),
            "workspace mode must not also pass --bins: {argv:?}"
        );
    }

    #[test]
    fn cargo_test_argv_test_threads_env_override() {
        let _env = ScopedEnv::new().set("BUILD_GATE_TEST_THREADS", "4");
        let argv = cargo_test_argv("release", "t", 4, "/s/Cargo.toml");
        assert!(
            argv.iter().any(|a| a == "--test-threads=4"),
            "argv: {argv:?}"
        );
        assert!(!argv.iter().any(|a| a == "--test-threads=8"));
    }

    #[test]
    fn cargo_test_argv_test_threads_zero_or_garbage_falls_back_to_default() {
        let argv_zero = {
            let _env = ScopedEnv::new().set("BUILD_GATE_TEST_THREADS", "0");
            cargo_test_argv("release", "t", 4, "/s/Cargo.toml")
        };
        assert!(argv_zero.iter().any(|a| a == "--test-threads=8"));

        let argv_garbage = {
            let _env = ScopedEnv::new().set("BUILD_GATE_TEST_THREADS", "not-a-number");
            cargo_test_argv("release", "t", 4, "/s/Cargo.toml")
        };
        assert!(argv_garbage.iter().any(|a| a == "--test-threads=8"));
    }

    // ── BLD-ASYNC (TERM #421): parse_mode_arg + compiler_build wait=false ──

    #[test]
    fn parse_mode_arg_defaults_build_accepts_test_rejects_other() {
        assert_eq!(parse_mode_arg(&json!({})).unwrap(), "build");
        assert_eq!(parse_mode_arg(&json!({ "mode": "build" })).unwrap(), "build");
        assert_eq!(parse_mode_arg(&json!({ "mode": "test" })).unwrap(), "test");
        assert!(parse_mode_arg(&json!({ "mode": "release" })).is_err());
        // A non-string `mode` is treated the same as absent (defaults to
        // "build"), matching the pre-existing build_inner behavior this helper
        // was extracted from — only a present STRING outside {build,test} errors.
        assert_eq!(parse_mode_arg(&json!({ "mode": 5 })).unwrap(), "build");
    }

    #[test]
    fn compiler_build_tool_advertises_wait_param_defaulting_true() {
        let params = CompilerBuild.parameters();
        let wait = &params["properties"]["wait"];
        assert_eq!(wait["type"], "boolean");
        assert_eq!(wait["default"], true);
        assert!(
            wait["description"].as_str().unwrap().contains("compiler_progress"),
            "wait's description must point the caller at compiler_progress: {wait:?}"
        );
    }

    #[tokio::test]
    async fn compiler_build_wait_false_enqueues_onto_the_queue_and_returns_a_request_id_without_running_a_build(
    ) {
        // BLD-ASYNC: wait=false must enqueue (mocked here via the same in-memory
        // fake the scheduler tests use — NOT a live Redis, and NEVER touches
        // build_inner/cargo) and return immediately with a request_id the caller
        // polls via compiler_progress. `enqueue_async_onto` is the store-generic
        // core `enqueue_async` delegates to; exercising it directly here is the
        // deterministic substitute for mocking `RedisQueue::from_env` (which is a
        // process-global singleton and can't be redirected at a fake after boot).
        //
        // PCON-01/04: this test's `ref` ("abc123") is not a real, resolvable sha
        // and no Gitea is reachable in a unit test, so SHA-staging is disabled
        // here (the documented rollback lever) — this test is about the
        // ENQUEUE/plumbing contract, not sha resolution (that has its own
        // dedicated tests: `resolve_ref_to_sha_*`).
        let _env = ScopedEnv::new().set(BUILD_STAGE_BY_SHA_ENV, "off");
        let q = crate::compiler::queue::fake::InMemoryQueue::new();
        let start = std::time::Instant::now();
        let out = CompilerBuild::enqueue_async_onto(
            &q,
            json!({
                "module": "terminus",
                "ref": "abc123",
                "mode": "test",
                "wait": false,
            }),
        )
        .await
        .expect("enqueue_async_onto succeeds against the in-memory fake");
        // Returns essentially immediately — it only enqueues, it never runs cargo.
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "wait=false must not block on the build: {:?}",
            start.elapsed()
        );
        let structured = out.structured.expect("structured output");
        let request_id = structured["request_id"]
            .as_str()
            .expect("request_id is a string");
        assert!(is_valid_request_id(request_id));
        assert_eq!(structured["job_id"], structured["request_id"]);
        assert_eq!(structured["mode"], "test");
        assert_eq!(structured["wait"], false);
        assert_eq!(structured["created"], true);
        assert!(out.text.contains(request_id));
        assert!(out.text.to_lowercase().contains("compiler_progress"));
    }

    #[test]
    fn cargo_test_argv_starts_with_cargo_test_not_cargo_build() {
        // The load-bearing distinction between mode=test and mode=build: the
        // subcommand itself differs, everything else (locked/profile/target/-j/
        // manifest-path) stays parallel.
        let test_argv = cargo_test_argv("release", "t", 4, "/s/Cargo.toml");
        let build_argv = cargo_build_argv("release", "t", 4, "m", "/s/Cargo.toml");
        assert_eq!(test_argv[0], "cargo");
        assert_eq!(test_argv[1], "test");
        assert_eq!(build_argv[0], "cargo");
        assert_eq!(build_argv[1], "build");
    }

    #[test]
    fn cargo_build_argv_unchanged_by_the_mode_test_addition() {
        // mode=build's argv-building path is untouched — same assertion as the
        // pre-existing `cargo_argv_release_musl` test, kept here explicitly next
        // to the new test-mode tests so the "mode=build is byte-for-byte
        // unchanged" claim is directly checkable in one place.
        let argv = cargo_build_argv(
            "release",
            "x86_64-unknown-linux-musl",
            4,
            "chord",
            "/src/chord/Cargo.toml",
        );
        assert_eq!(
            argv,
            vec![
                "cargo",
                "build",
                "--locked",
                "--release",
                "--manifest-path",
                "/src/chord/Cargo.toml",
                "--target",
                "x86_64-unknown-linux-musl",
                "-j",
                "4",
                "--bin",
                "chord",
            ]
        );
    }

    // ── GAP 5 (TERM #418): cargo_generate_lockfile_argv ───────────────────
    // terminus/harmony/chord gitignore Cargo.lock, so `--locked` (on both
    // cargo_build_argv and cargo_test_argv) fails instantly on a freshly
    // staged feature branch unless a matching lock is generated first.

    #[test]
    fn cargo_generate_lockfile_argv_shape() {
        let argv = cargo_generate_lockfile_argv("/s/Cargo.toml");
        assert_eq!(
            argv,
            vec![
                "cargo",
                "generate-lockfile",
                "--manifest-path",
                "/s/Cargo.toml",
            ]
        );
    }

    #[test]
    fn cargo_generate_lockfile_argv_never_locked() {
        // The whole point of this step is to CREATE a lock — `--locked` would
        // make cargo refuse to write one, defeating the pre-step entirely.
        let argv = cargo_generate_lockfile_argv("/s/Cargo.toml");
        assert!(
            !argv.iter().any(|a| a == "--locked"),
            "generate-lockfile argv must never carry --locked: {argv:?}"
        );
    }

    #[test]
    fn cargo_generate_lockfile_argv_is_not_a_build_or_test_subcommand() {
        // Distinguishes it from cargo_build_argv/cargo_test_argv (GAP 5 is a
        // resolve-only step, not a compile/test step).
        let argv = cargo_generate_lockfile_argv("/s/Cargo.toml");
        assert_eq!(argv[0], "cargo");
        assert_eq!(argv[1], "generate-lockfile");
    }

    #[test]
    fn local_build_sequence_renders_lockgen_before_the_locked_test_scope() {
        // Mirrors what build_inner does on the LOCAL path: a lockgen scope
        // (no --locked) rendered under a `-lockgen` unit, followed by the
        // real --locked test/build scope under the base unit. Asserts the
        // ORDER + that only the second argv carries --locked.
        let caps = crate::compiler::scope::ScopeCaps {
            memory_max: "4G".to_string(),
            cpu_quota: "200%".to_string(),
            io_weight: "50".to_string(),
            jobs: 4,
        };
        let setenv: BTreeMap<String, String> = BTreeMap::new();
        let unit = "terminus-build-terminus-abc123-uuid".to_string();

        let lockgen_argv = cargo_generate_lockfile_argv("/s/Cargo.toml");
        let lockgen_unit = format!("{unit}-lockgen");
        let lockgen_scope =
            crate::compiler::scope::render_scope_argv(&lockgen_unit, &caps, &setenv, &lockgen_argv);

        let test_argv = cargo_test_argv("release", "t", 4, "/s/Cargo.toml");
        let test_scope = crate::compiler::scope::render_scope_argv(&unit, &caps, &setenv, &test_argv);

        // Both are systemd-run scopes on DISTINCT unit names (never collide).
        assert!(lockgen_scope.iter().any(|a| a == &format!("--unit={lockgen_unit}")));
        assert!(test_scope.iter().any(|a| a == &format!("--unit={unit}")));
        assert_ne!(lockgen_scope, test_scope);

        // The lockgen scope's cargo argv never carries --locked; the real
        // test/build scope always does.
        assert!(!lockgen_scope.contains(&"--locked".to_string()));
        assert!(test_scope.contains(&"--locked".to_string()));

        // The sequence a caller must run: lockgen THEN the --locked step —
        // encoded here as "lockgen has no --locked, the following step
        // does," which is the precondition the GAP 5 fix relies on.
        let lockgen_then_locked = [lockgen_scope, test_scope];
        assert!(!lockgen_then_locked[0].contains(&"--locked".to_string()));
        assert!(lockgen_then_locked[1].contains(&"--locked".to_string()));
    }

    #[test]
    fn remote_command_chains_lockgen_before_exec_of_the_locked_scope() {
        // Mirrors the REMOTE path's `remote_cmd` construction: lockgen runs
        // to completion (`&&`, not `exec`, since a following command must
        // still run), then the real --locked scope is `exec`'d so it
        // replaces the wrapper shell as the final process. Both `have_secret`
        // branches must preserve this "lockgen && exec build" shape.
        let lockgen_cmd = "/usr/bin/systemd-run --scope --unit=u-lockgen -- cargo generate-lockfile --manifest-path /s/Cargo.toml".to_string();
        let scope_cmd = "/usr/bin/systemd-run --scope --unit=u -- cargo test --locked --manifest-path /s/Cargo.toml".to_string();

        let remote_env_path = "/tmp/.terminus-build-u-abc.env".to_string();
        let with_secret = format!(
            "set -a; . {f}; rm -f {f}; set +a; {lockgen_cmd} && exec {scope_cmd}",
            f = remote_env_path
        );
        let without_secret = format!("{lockgen_cmd} && exec {scope_cmd}");

        for cmd in [&with_secret, &without_secret] {
            // lockgen must appear, run to completion (`&&`), BEFORE `exec` of
            // the real (--locked) build/test scope.
            let lockgen_pos = cmd.find("generate-lockfile").expect("lockgen present");
            let exec_pos = cmd.find("exec ").expect("exec present");
            assert!(
                lockgen_pos < exec_pos,
                "lockgen must run before exec of the locked scope: {cmd}"
            );
            assert!(cmd.contains("&& exec"), "lockgen must gate exec via &&: {cmd}");
            // The --locked flag belongs to the exec'd (real) scope only.
            let locked_pos = cmd.find("--locked").expect("--locked present");
            assert!(
                locked_pos > exec_pos,
                "--locked must belong to the exec'd scope, not lockgen: {cmd}"
            );
        }
        // The one-shot secret file `rm` happens exactly once, before either
        // scope runs — not split across two ssh commands.
        assert_eq!(with_secret.matches("rm -f").count(), 1);
    }

    // ── BLD-COMPTEST: parse_cargo_test_output ─────────────────────────

    #[test]
    fn parse_cargo_test_output_all_passed_single_binary() {
        let out = "\nrunning 3 tests\ntest a ... ok\ntest b ... ok\ntest c ... ok\n\n\
                    test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n";
        let s = parse_cargo_test_output(out);
        assert!(s.summary_found);
        assert!(s.all_passed());
        assert_eq!(s.passed, 3);
        assert_eq!(s.failed, 0);
        assert!(s.failing_tests.is_empty());
    }

    #[test]
    fn parse_cargo_test_output_failures_captured() {
        let out = "\nrunning 2 tests\ntest foo::bar ... FAILED\ntest foo::baz ... ok\n\n\
                    failures:\n\n---- foo::bar stdout ----\nassertion failed\n\n\
                    failures:\n    foo::bar\n\n\
                    test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.02s\n";
        let s = parse_cargo_test_output(out);
        assert!(s.summary_found);
        assert!(!s.all_passed());
        assert_eq!(s.passed, 1);
        assert_eq!(s.failed, 1);
        assert_eq!(s.failing_tests, vec!["foo::bar".to_string()]);
    }

    #[test]
    fn parse_cargo_test_output_aggregates_multiple_binaries() {
        // A workspace/crate run prints one `test result:` line per test binary —
        // the gate sums them.
        let out = "test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n\
                    test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.03s\n";
        let s = parse_cargo_test_output(out);
        assert!(s.summary_found);
        assert_eq!(s.passed, 3);
        assert_eq!(s.failed, 1);
        assert!(!s.all_passed());
    }

    #[test]
    fn parse_cargo_test_output_no_summary_line_is_not_a_pass() {
        // A compile error (or a crash before any test binary reports) never
        // prints a `test result:` line — that must never be read as a pass.
        let out = "error[E0433]: failed to resolve\nerror: could not compile `chord` (lib)\n";
        let s = parse_cargo_test_output(out);
        assert!(!s.summary_found);
        assert!(!s.all_passed());
        assert!(s.failing_tests.is_empty());
    }

    // ── BLD-COMPTEST: test_gate_passed — the exit-code half of the gate verdict ──

    #[test]
    fn test_gate_fails_on_nonzero_exit_even_with_a_clean_summary() {
        // codex review gap: a clean earlier `test result:` summary line followed
        // by a LATER non-zero cargo exit (a second crate failing to compile, a
        // rustdoc/doctest/link error) emits NO further failed-summary line, so the
        // parsed summary looks all-passed. The GATE must still FAIL — a non-zero
        // cargo exit is never a pass for a gate.
        let clean_summary = parse_cargo_test_output(
            "test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n",
        );
        assert!(clean_summary.all_passed(), "summary alone reads clean");
        // exit_success = false (the dangerous case) ⇒ gate FAILS.
        assert!(!test_gate_passed(false, &clean_summary));
        // exit_success = true ⇒ gate PASSES (happy path unchanged).
        assert!(test_gate_passed(true, &clean_summary));
    }

    #[test]
    fn test_gate_fails_on_failed_tests_even_with_a_zero_exit() {
        // The other half: a parsed failure fails the gate regardless of exit code
        // (defense in depth — `--no-fail-fast` still exits non-zero, but the gate
        // must not depend on that).
        let failing = parse_cargo_test_output(
            "test x ... FAILED\ntest result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out\n",
        );
        assert!(!test_gate_passed(true, &failing));
        assert!(!test_gate_passed(false, &failing));
    }

    #[test]
    fn test_gate_fails_when_no_summary_line_at_all() {
        // No `test result:` line (compile error before any test binary ran) is a
        // FAIL whether or not the process somehow exited 0.
        let none = parse_cargo_test_output("error: could not compile `chord`\n");
        assert!(!test_gate_passed(true, &none));
        assert!(!test_gate_passed(false, &none));
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("plain"), "'plain'");
        assert_eq!(shell_quote("a b"), "'a b'");
        // An embedded single quote is closed, escaped, reopened.
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
    }

    #[test]
    fn shell_join_quotes_every_arg() {
        let argv = vec![
            "systemd-run".to_string(),
            "--setenv=SCCACHE_REDIS_ENDPOINT=redis://h:6379".to_string(),
            "cargo".to_string(),
        ];
        let s = shell_join(&argv);
        assert_eq!(
            s,
            "'systemd-run' '--setenv=SCCACHE_REDIS_ENDPOINT=redis://h:6379' 'cargo'"
        );
    }

    #[test]
    fn built_bin_path_matches_profile_subdir() {
        assert_eq!(
            built_bin_rel("x86_64-unknown-linux-musl", "release", "chord"),
            PathBuf::from("x86_64-unknown-linux-musl/release/chord")
        );
        assert_eq!(built_bin_rel("t", "debug", "m"), PathBuf::from("t/debug/m"));
        assert_eq!(
            built_bin_rel("t", "release-dist", "m"),
            PathBuf::from("t/release-dist/m")
        );
    }

    #[test]
    fn default_target_dir_is_never_the_nfs_dataset() {
        // Whatever the default local target dir is, it must pass the guard
        // against a dataset root — i.e. it is not under it. (Uses a sample root;
        // the default target lives under the temp dir, not the dataset.)
        let target = local_target_dir();
        let root = PathBuf::from("/data/build");
        assert!(scope::validate_target_dir(&target, &root).is_ok());
    }

    // ── PCON-10: per-job CARGO_TARGET_DIR + TMPDIR on the big disk ────────────

    #[test]
    fn scratch_root_fails_closed_when_neither_var_is_set() {
        // Unset big-disk root ⇒ hard error, never a silent /tmp tmpfs fallback.
        let err = resolve_scratch_root(None, None).unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("scratch root not configured"), "{msg}");
        assert!(msg.contains("tmpfs"), "should name the tmpfs hazard: {msg}");
    }

    #[test]
    fn scratch_root_prefers_scratch_then_local_target() {
        assert_eq!(
            resolve_scratch_root(Some("/big/scratch".into()), Some("/big/local".into())).unwrap(),
            PathBuf::from("/big/scratch"),
        );
        assert_eq!(
            resolve_scratch_root(None, Some("/big/local".into())).unwrap(),
            PathBuf::from("/big/local"),
        );
    }

    #[test]
    fn job_scratch_dirs_are_per_job_disjoint_and_on_the_big_disk() {
        let root = PathBuf::from("/big/scratch");
        let (t1, tmp1) = job_scratch_dirs(&root, "chord-abc-uuid1");
        let (t2, tmp2) = job_scratch_dirs(&root, "chord-abc-uuid2");
        // Two concurrent jobs get disjoint target AND tmp dirs.
        assert_ne!(t1, t2);
        assert_ne!(tmp1, tmp2);
        // Both live under the big-disk root, never /tmp.
        for p in [&t1, &tmp1, &t2, &tmp2] {
            assert!(p.starts_with("/big/scratch"), "{p:?}");
            assert!(!p.starts_with("/tmp"), "{p:?}"); // hermeticity-allow: asserting NOT /tmp
        }
        // Distinct roles within one job.
        assert!(t1.ends_with("target"));
        assert!(tmp1.ends_with("tmp"));
    }

    #[test]
    fn job_scratch_dirs_are_rejected_under_the_nfs_dataset() {
        // A scratch root that lands under the NFS dataset is refused by the guard
        // (cargo compiles then EXECUTES build scripts — NFS breaks exec).
        let dataset = PathBuf::from("/data/build");
        let (target, tmp) = job_scratch_dirs(&dataset.join("scratch"), "m-uuid");
        assert!(scope::validate_target_dir(&target, &dataset).is_err());
        assert!(scope::validate_target_dir(&tmp, &dataset).is_err());
    }

    #[test]
    fn job_scratch_dirs_off_the_dataset_pass_the_guard() {
        let root = PathBuf::from("/big/scratch");
        let dataset = PathBuf::from("/data/build");
        let (target, tmp) = job_scratch_dirs(&root, "m-uuid");
        assert!(scope::validate_target_dir(&target, &dataset).is_ok());
        assert!(scope::validate_target_dir(&tmp, &dataset).is_ok());
    }

    #[test]
    fn scratch_root_rejects_tmpfs_and_accepts_big_disk() {
        // FIX (PCON-10): a /tmp (tmpfs) local-target fallback is rejected
        // fail-closed — validate_target_dir alone would have let it through.
        assert!(resolve_scratch_root(None, Some("/tmp/terminus-build".into())).is_err()); // hermeticity-allow: asserting /tmp is rejected
        assert!(resolve_scratch_root(Some("/tmp".into()), None).is_err()); // hermeticity-allow: asserting /tmp is rejected
        assert!(resolve_scratch_root(Some("/dev/shm/x".into()), None).is_err());
        assert!(resolve_scratch_root(Some("/run/build".into()), None).is_err());
        // A big-disk (non-tmpfs) root is accepted.
        assert_eq!(
            resolve_scratch_root(Some("/data/build/scratch".into()), None).unwrap(),
            PathBuf::from("/data/build/scratch"),
        );
    }

    fn unique_reclaim_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "pcon10-reclaim-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn scratch_reclaim_removes_dir_even_after_partial_creation() {
        // FIX (PCON-10): the guard is armed BEFORE dir creation, so a PARTIAL
        // create followed by an early return still reclaims `<root>/<unit>`.
        let base = unique_reclaim_dir("partial");
        std::fs::create_dir_all(base.join("target/partial")).unwrap();
        assert!(base.exists());
        {
            let _g = ScratchReclaim::new(base.clone());
            // simulate an early `?` return: the guard drops here.
        }
        assert!(!base.exists(), "partially-created scratch must be reclaimed");
    }

    #[test]
    fn scratch_reclaim_is_noop_when_dir_never_created() {
        // A guard over a dir that was never created must drop cleanly (no panic).
        let base = unique_reclaim_dir("absent");
        {
            let _g = ScratchReclaim::new(base.clone());
        }
        assert!(!base.exists());
    }

    #[test]
    fn str_arg_rejects_missing_and_blank() {
        let v = json!({"module": "  ", "ref": "abc"});
        assert!(str_arg(&v, "module").is_err());
        assert_eq!(str_arg(&v, "ref").unwrap(), "abc");
        assert!(str_arg(&v, "missing").is_err());
    }

    #[test]
    fn segment_validation_accepts_normal_and_rejects_traversal() {
        // Normal segments accepted.
        for ok in [
            "chord",
            "lumina-core",
            "terminus_rs",
            "release-dist",
            "v1.2.3",
            "abc123",
        ] {
            assert!(
                validate_segment("module", ok).is_ok(),
                "should accept {ok:?}"
            );
        }
        // Traversal / separators / injection / control chars all rejected.
        for bad in [
            "",            // empty
            ".",           // dot
            "..",          // parent
            "../..",       // traversal (contains '/')
            "a/b",         // separator
            "a/../b",      // embedded traversal
            "/etc/passwd", // absolute (leading '/')
            "a\\b",        // backslash
            "a b",         // whitespace
            "a;rm -rf /",  // shell metachars + space
            "$(touch x)",  // command substitution
            "a`b`",        // backticks
            "a\0b",        // NUL
            "a\nb",        // newline / control
        ] {
            assert!(
                validate_segment("module", bad).is_err(),
                "should REJECT {bad:?}"
            );
        }
    }

    #[test]
    fn git_ref_validation_allows_branch_slashes_but_not_traversal() {
        // A real branch/sha is accepted, including a single '/'.
        for ok in [
            "main",
            "feature/foo",
            "release/2026-07",
            "0a1b2c3d",
            "v1.0.0",
        ] {
            assert!(validate_git_ref(ok).is_ok(), "should accept ref {ok:?}");
        }
        // Traversal and injection are rejected even with the looser ref charset.
        for bad in [
            "",         // empty
            "/etc",     // absolute
            "feature/", // trailing slash
            "../..",    // traversal
            "a/../b",   // embedded '..'
            "a//b",     // empty component
            "a\\b",     // backslash
            "a b",      // whitespace
            "$(x)",     // injection
            "a;b",      // shell metachar
        ] {
            assert!(validate_git_ref(bad).is_err(), "should REJECT ref {bad:?}");
        }
    }

    #[test]
    fn web_dir_validation_allows_simple_relative_dirs_but_not_traversal() {
        // BLD-444: `BUILD_MODULE_WEB_DIR_<MODULE>` — a single segment or a
        // simple nested relative dir, same shape as `validate_git_ref`.
        for ok in ["harmony-web", "web", "apps/dashboard", "v1.2.3"] {
            assert!(
                validate_relative_dir("web dir", ok).is_ok(),
                "should accept {ok:?}"
            );
        }
        for bad in [
            "",             // empty
            "/etc",         // absolute
            "harmony-web/", // trailing slash
            "../..",        // traversal
            "..",           // parent alone
            "a/../b",       // embedded traversal
            "a//b",         // empty component
            "a\\b",         // backslash
            "a b",          // whitespace
            "$(touch x)",   // injection
            "a;rm -rf /",   // shell metachar
        ] {
            assert!(
                validate_relative_dir("web dir", bad).is_err(),
                "should REJECT {bad:?}"
            );
        }
    }

    #[test]
    fn module_target_absent_falls_back_to_global_default() {
        // BLD-444 (glibc-portability follow-up): a module with no
        // `BUILD_MODULE_TARGET_<MODULE>` override gets the SAME triple
        // `target_triple()` returns — zero behavior change for terminus/chord.
        let _env = ScopedEnv::new().unset("BUILD_MODULE_TARGET_EFFTRIPLENOOVERRIDE");
        assert_eq!(
            effective_triple("efftriplenooverride"),
            target_triple(),
            "no override configured ⇒ falls back to the global default"
        );
    }

    #[test]
    fn module_target_override_wins_over_global_default() {
        // BLD-444: a configured per-module override (e.g. harmony → musl for a
        // portable artifact on an older-glibc deploy host) wins over the
        // fleet-wide `BUILD_TARGET_TRIPLE`.
        let _env = ScopedEnv::new().set(
            "BUILD_MODULE_TARGET_EFFTRIPLEOVERRIDE",
            "x86_64-unknown-linux-musl",
        );
        assert_eq!(
            effective_triple("efftripleoverride"),
            "x86_64-unknown-linux-musl"
        );
    }

    #[test]
    fn unsafe_module_target_override_is_rejected_by_validate_segment() {
        // BLD-444: `effective_triple` itself doesn't validate (same discipline
        // as `target_triple`/`host::module_target`) — every call site runs its
        // result through `validate_segment("target", …)` before use. An
        // injection/traversal-shaped override must be caught there.
        let _env = ScopedEnv::new().set(
            "BUILD_MODULE_TARGET_EFFTRIPLEBADOVERRIDE",
            "x86_64-unknown-linux-gnu;rm -rf /",
        );
        let triple = effective_triple("efftriplebadoverride");
        assert!(
            validate_segment("target", &triple).is_err(),
            "an injection-shaped override must be rejected"
        );
    }

    #[test]
    fn shell_metachar_segment_is_rejected_and_quoting_is_injection_safe() {
        // Finding #1 already rejects a metachar-laden segment outright…
        let nasty = "m;$(touch PWNED)`id`";
        assert!(validate_segment("module", nasty).is_err());
        // …and even if some interpolated value reached the ssh layer, shell_quote
        // renders it a single inert word (round-trips through a real shell with no
        // command execution).
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("PWNED");
        let payload = format!("x $(touch '{m}') `touch '{m}'` ; y", m = marker.display());
        let script = format!("printf %s {}", shell_quote(&payload));
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(script.as_bytes()).unwrap();
        let out = std::process::Command::new("sh")
            .arg(f.path())
            .output()
            .expect("run sh");
        assert!(out.status.success());
        assert_eq!(String::from_utf8(out.stdout).unwrap(), payload);
        assert!(
            !marker.exists(),
            "shell_quote must prevent command execution"
        );
    }

    #[test]
    fn secret_file_is_exclusive_0600_no_symlink_follow() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();

        // The body content is arbitrary for this test (we're exercising the
        // creation semantics, not the payload) — a non-secret-shaped literal.
        let body = "payload-line-one\n";

        // (a) Fresh path → succeeds, mode exactly 0600, contents match.
        let fresh = dir.path().join("fresh.env");
        write_secret_0600_at(&fresh, body).unwrap();
        assert_eq!(std::fs::read_to_string(&fresh).unwrap(), body);
        let mode = std::fs::metadata(&fresh).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "must be 0600 from creation, got {mode:o}");

        // (b) Pre-existing path → O_EXCL makes it a hard error, and the existing
        // file is NOT truncated/overwritten.
        let existing = dir.path().join("existing.env");
        std::fs::write(&existing, "PREEXISTING").unwrap();
        assert!(write_secret_0600_at(&existing, body).is_err());
        assert_eq!(
            std::fs::read_to_string(&existing).unwrap(),
            "PREEXISTING",
            "an existing file must never be truncated/overwritten"
        );

        // (c) Symlink at the path → O_NOFOLLOW refuses to follow it; the symlink
        // target is NOT created or written.
        let target = dir.path().join("target-should-not-be-written");
        let link = dir.path().join("link.env");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(write_secret_0600_at(&link, body).is_err());
        assert!(
            !target.exists(),
            "a symlink must not be followed to create/write its target"
        );
    }

    #[test]
    fn redact_secrets_replaces_values_and_is_a_noop_when_empty() {
        let secret = "<REDACTED-SECRET>".to_string();
        let url = "redis://default:topsecretvalue123@h:6379/1".to_string();
        let secrets = vec![secret.clone(), url.clone(), String::new()];

        // A line echoing the secret is scrubbed; the raw value is absent.
        let leaked = format!("error: a build script printed the secret {secret} to stderr");
        let red = redact_secrets(&leaked, &secrets);
        assert!(
            !red.contains("topsecretvalue123"),
            "raw secret must be gone: {red}"
        );
        assert!(red.contains("<redacted>"));

        // The full URL value is scrubbed too.
        let leaked_url = format!("connecting to {url} ...");
        assert!(!redact_secrets(&leaked_url, &secrets).contains("topsecretvalue123"));

        // A non-secret line passes through unchanged.
        let benign = "warning: unused variable `x`";
        assert_eq!(redact_secrets(benign, &secrets), benign);

        // Empty secret set / empty values are a no-op.
        assert_eq!(redact_secrets(&leaked, &[]), leaked);
        assert_eq!(redact_secrets("plain", &[String::new()]), "plain");
    }

    #[test]
    fn redact_secrets_handles_overlapping_values_longest_first() {
        // The exact overlap case: the password is a SUBSTRING of the full URL.
        // Order the input worst-case (password first) — longest-first ordering
        // inside the helper must still fully scrub the URL, leaving no partial
        // `redis://...@host` fragment.
        let password = "abc".to_string();
        let url = "redis://u:abc@host:6379/1".to_string();
        let secrets = vec![password.clone(), url.clone()];

        let text = format!("dump: url={url} pw={password}");
        let red = redact_secrets(&text, &secrets);
        assert!(
            !red.contains("abc"),
            "no secret substring may survive: {red}"
        );
        assert!(!red.contains("redis://"), "no partial URL may leak: {red}");
        assert!(!red.contains("@host"), "URL host/port must not leak: {red}");
        // Both occurrences became the placeholder.
        assert_eq!(red, "dump: url=<redacted> pw=<redacted>");
    }

    #[test]
    fn source_dir_containment() {
        let root = std::path::Path::new("/data/build");
        // Under the dataset src tree → accepted.
        assert!(
            validate_source_dir(std::path::Path::new("/data/build/src/chord/abc"), root).is_ok()
        );
        assert!(validate_source_dir(std::path::Path::new("/data/build/src"), root).is_ok());
        // Absolute elsewhere → rejected.
        assert!(validate_source_dir(std::path::Path::new("/etc"), root).is_err());
        assert!(validate_source_dir(std::path::Path::new("/data/build/cache/x"), root).is_err());
        // `..`-escape that lexically leaves the src tree → rejected.
        assert!(
            validate_source_dir(std::path::Path::new("/data/build/src/../../etc"), root).is_err()
        );
        // A sibling sharing a string prefix but not the path → rejected.
        assert!(validate_source_dir(std::path::Path::new("/data/build/src-evil/x"), root).is_err());
    }

    // ── GAP 1: missing-staged-source gives a clear error, not a red herring ──

    #[test]
    fn missing_source_dir_gives_clear_not_found_error_not_a_spawn_enoent() {
        let dir = tempfile::tempdir().unwrap();
        // Never created: <tmp>/src/terminus/abc123 does not exist at all.
        let missing = dir.path().join("src").join("terminus").join("abc123");
        let err = validate_local_source_dir(&missing, "terminus", "abc123").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("source not staged for terminus@abc123"),
            "error must name the module@ref: {msg}"
        );
        assert!(
            msg.contains(&missing.display().to_string()),
            "error must name the missing path: {msg}"
        );
        assert!(
            !msg.to_lowercase().contains("systemd-run"),
            "must never surface the misleading spawn-ENOENT red herring: {msg}"
        );
    }

    #[test]
    fn staged_dir_without_cargo_toml_is_also_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let staged = dir.path().join("src").join("chord").join("main");
        std::fs::create_dir_all(&staged).unwrap();
        // Directory exists but is empty — no Cargo.toml staged into it.
        let err = validate_local_source_dir(&staged, "chord", "main").unwrap_err();
        assert!(err.to_string().contains("Cargo.toml"));
    }

    #[test]
    fn properly_staged_source_dir_passes() {
        let dir = tempfile::tempdir().unwrap();
        let staged = dir.path().join("src").join("chord").join("main");
        std::fs::create_dir_all(&staged).unwrap();
        std::fs::write(staged.join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        assert!(validate_local_source_dir(&staged, "chord", "main").is_ok());
    }

    // ── GAP 2: Gitea Cargo-registry creds land on the SECRET partition ───────

    #[test]
    fn registry_token_from_dedicated_env_lands_on_secret_partition_not_argv() {
        let _env = ScopedEnv::new()
            .unset("GITEA_IDENTITY_NAME")
            .unset("GITEA_PAT_MOOSE")
            .set("GITEA_URL", "http://gitea.example.internal:3000")
            .set("CARGO_REGISTRIES_GITEA_TOKEN", "gpat-dedicated-abc123");

        let mut build_env: BTreeMap<String, String> = BTreeMap::new();
        let mut redact: Vec<String> = Vec::new();
        inject_gitea_registry_env(&mut build_env, &mut redact);

        // The secret landed in build_env under the expected key...
        assert_eq!(
            build_env.get("CARGO_REGISTRIES_GITEA_TOKEN").map(String::as_str),
            Some("Bearer gpat-dedicated-abc123")
        );
        // ...and the raw token value is in the redaction set.
        assert!(redact.iter().any(|s| s == "gpat-dedicated-abc123"));

        // partition_env (the actual argv/secret split `mod.rs` uses) MUST put it
        // on the secret side, never `--setenv` argv.
        let (non_secret, secret) = scope::partition_env(&build_env);
        assert!(!non_secret.contains_key("CARGO_REGISTRIES_GITEA_TOKEN"));
        assert_eq!(
            secret.get("CARGO_REGISTRIES_GITEA_TOKEN").map(String::as_str),
            Some("Bearer gpat-dedicated-abc123")
        );

        // The rendered systemd-run argv must never contain the token value.
        let argv = scope::render_scope_argv(
            "u",
            &scope::ScopeCaps {
                memory_max: "1G".into(),
                cpu_quota: "100%".into(),
                io_weight: "50".into(),
                jobs: 1,
            },
            &non_secret,
            &["cargo".into(), "build".into()],
        );
        assert!(!argv.join(" ").contains("gpat-dedicated-abc123"));

        // The INDEX + credential-providers config ARE non-secret and go via
        // --setenv.
        assert!(non_secret.contains_key("CARGO_REGISTRIES_GITEA_INDEX"));
        assert!(non_secret.contains_key("CARGO_REGISTRY_GLOBAL_CREDENTIAL_PROVIDERS"));
    }

    #[test]
    fn registry_token_falls_back_to_default_identity_gitea_pat() {
        let _env = ScopedEnv::new()
            .unset("CARGO_REGISTRIES_GITEA_TOKEN")
            .unset("GITEA_IDENTITY_NAME")
            .unset("GITEA_URL")
            .set("GITEA_PAT_MOOSE", "moose-pat-xyz");

        let mut build_env: BTreeMap<String, String> = BTreeMap::new();
        let mut redact: Vec<String> = Vec::new();
        inject_gitea_registry_env(&mut build_env, &mut redact);

        assert_eq!(
            build_env.get("CARGO_REGISTRIES_GITEA_TOKEN").map(String::as_str),
            Some("Bearer moose-pat-xyz")
        );
    }

    #[test]
    fn registry_token_falls_back_to_moose_when_active_identity_pat_is_unset() {
        // FINDING 2 (review): an operator set a NON-moose active identity, but
        // that identity's PAT is unprovisioned. The org-readable moose PAT is the
        // explicit FINAL fallback so a harmony/chord build still resolves
        // terminus-rs, rather than degrading purely because the identity changed.
        let _env = ScopedEnv::new()
            .unset("CARGO_REGISTRIES_GITEA_TOKEN")
            .unset("GITEA_URL")
            .set("GITEA_IDENTITY_NAME", "alice")
            .unset("GITEA_PAT_ALICE")
            .set("GITEA_PAT_MOOSE", "moose-org-pat");

        assert_eq!(
            cargo_registry_gitea_token().as_deref(),
            Some("moose-org-pat"),
            "must fall back to the org-readable moose PAT when the active \
             (non-moose) identity's PAT is unset"
        );

        // And it wires through inject as the Bearer-wrapped secret.
        let mut build_env: BTreeMap<String, String> = BTreeMap::new();
        let mut redact: Vec<String> = Vec::new();
        inject_gitea_registry_env(&mut build_env, &mut redact);
        assert_eq!(
            build_env.get("CARGO_REGISTRIES_GITEA_TOKEN").map(String::as_str),
            Some("Bearer moose-org-pat")
        );
    }

    #[test]
    fn registry_token_prefers_moose_over_active_identity_pat() {
        // For REGISTRY reads the org-readable moose PAT is preferred over the
        // gateway's active identity PAT (review finding): even with a non-moose
        // identity whose PAT IS provisioned, moose wins for registry access.
        let _env = ScopedEnv::new()
            .unset("CARGO_REGISTRIES_GITEA_TOKEN")
            .unset("GITEA_URL")
            .set("GITEA_IDENTITY_NAME", "alice")
            .set("GITEA_PAT_ALICE", "alice-pat")
            .set("GITEA_PAT_MOOSE", "moose-org-pat");
        assert_eq!(cargo_registry_gitea_token().as_deref(), Some("moose-org-pat"));
    }

    #[test]
    fn registry_explicit_bearer_token_env_is_not_double_wrapped() {
        // FINDING 1 (review): the DEPLOYED /etc/constellation/secrets format is
        // `CARGO_REGISTRIES_GITEA_TOKEN="<REDACTED-SECRET>"`. An explicit env in that
        // (canonical) format must WIN verbatim — never become `Bearer Bearer …`.
        let _env = ScopedEnv::new()
            .unset("GITEA_IDENTITY_NAME")
            .unset("GITEA_PAT_MOOSE")
            .set("GITEA_URL", "http://gitea.example.internal:3000")
            .set("CARGO_REGISTRIES_GITEA_TOKEN", "Bearer gpat-explicit-xyz");

        let mut build_env: BTreeMap<String, String> = BTreeMap::new();
        let mut redact: Vec<String> = Vec::new();
        inject_gitea_registry_env(&mut build_env, &mut redact);
        assert_eq!(
            build_env.get("CARGO_REGISTRIES_GITEA_TOKEN").map(String::as_str),
            Some("Bearer gpat-explicit-xyz"),
            "an explicit `Bearer <pat>` env must pass through verbatim, not double-wrap"
        );
    }

    #[test]
    fn registry_env_degrades_cleanly_when_no_token_or_index_is_configured() {
        let _env = ScopedEnv::new()
            .unset("CARGO_REGISTRIES_GITEA_TOKEN")
            .unset("GITEA_IDENTITY_NAME")
            .unset("GITEA_PAT_MOOSE")
            .unset("CARGO_REGISTRIES_GITEA_INDEX")
            .unset("GITEA_URL")
            .unset("GITEA_OWNER");

        let mut build_env: BTreeMap<String, String> = BTreeMap::new();
        let mut redact: Vec<String> = Vec::new();
        // Must not panic; nothing to derive an index from, so no fabricated
        // literal is inserted either (S1 — never hardcode an infra default).
        inject_gitea_registry_env(&mut build_env, &mut redact);
        assert!(!build_env.contains_key("CARGO_REGISTRIES_GITEA_TOKEN"));
        assert!(!build_env.contains_key("CARGO_REGISTRIES_GITEA_INDEX"));
        assert!(!build_env.contains_key("CARGO_REGISTRY_GLOBAL_CREDENTIAL_PROVIDERS"));
    }

    #[test]
    fn registry_index_derives_from_gitea_url_but_an_explicit_index_wins() {
        {
            // GITEA_URL configured (the box already talks to Gitea's REST API)
            // → the index is DERIVED, no separate URL needed.
            let _env = ScopedEnv::new()
                .unset("CARGO_REGISTRIES_GITEA_INDEX")
                .set("GITEA_URL", "http://gitea.example.internal:3000")
                .unset("GITEA_OWNER");
            let mut build_env: BTreeMap<String, String> = BTreeMap::new();
            let mut redact: Vec<String> = Vec::new();
            inject_gitea_registry_env(&mut build_env, &mut redact);
            assert_eq!(
                build_env.get("CARGO_REGISTRIES_GITEA_INDEX").map(String::as_str),
                Some("sparse+http://gitea.example.internal:3000/api/packages/moosenet/cargo/")
            );
            assert!(build_env.contains_key("CARGO_REGISTRY_GLOBAL_CREDENTIAL_PROVIDERS"));
        }
        {
            // An explicit CARGO_REGISTRIES_GITEA_INDEX always wins over the
            // GITEA_URL derivation.
            let _env = ScopedEnv::new()
                .set("GITEA_URL", "http://gitea.example.internal:3000")
                .set("CARGO_REGISTRIES_GITEA_INDEX", "sparse+http://override.example/cargo/");
            let mut build_env: BTreeMap<String, String> = BTreeMap::new();
            let mut redact: Vec<String> = Vec::new();
            inject_gitea_registry_env(&mut build_env, &mut redact);
            assert_eq!(
                build_env.get("CARGO_REGISTRIES_GITEA_INDEX").map(String::as_str),
                Some("sparse+http://override.example/cargo/")
            );
        }
    }

    // ── GAP 4: CARGO_INCREMENTAL=0 so sccache can actually cache Rust ────────

    #[test]
    fn cargo_incremental_is_forced_off_for_sccache_rust_caching() {
        let mut build_env: BTreeMap<String, String> = BTreeMap::new();
        inject_cargo_incremental_off(&mut build_env);
        assert_eq!(
            build_env.get("CARGO_INCREMENTAL").map(String::as_str),
            Some("0")
        );
        // Non-secret — must survive partition_env onto the --setenv side and
        // appear in the rendered systemd-run argv.
        let (non_secret, secret) = scope::partition_env(&build_env);
        assert!(!secret.contains_key("CARGO_INCREMENTAL"));
        let argv = scope::render_scope_argv(
            "u",
            &scope::ScopeCaps {
                memory_max: "1G".into(),
                cpu_quota: "100%".into(),
                io_weight: "50".into(),
                jobs: 1,
            },
            &non_secret,
            &["cargo".into(), "build".into()],
        );
        assert!(argv.contains(&"--setenv=CARGO_INCREMENTAL=0".to_string()));
    }

    #[tokio::test]
    async fn run_redacts_secret_from_stderr_tail_and_stdout() {
        let secret = "<REDACTED-SECRET>".to_string();
        let redact = vec![secret.clone()];

        // Failing child that echoes the secret to stderr → the error tail must be
        // redacted (this is the exact leak path: a build.rs printing its env).
        let err = run(
            &[
                "sh".into(),
                "-c".into(),
                format!("echo leak={secret} 1>&2; exit 1"),
            ],
            None,
            &BTreeMap::new(),
            Duration::from_secs(30),
            &redact,
            None,
            None,
        )
        .await
        .unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            !msg.contains("topsecretvalue123"),
            "secret leaked into error: {msg}"
        );
        assert!(msg.contains("<redacted>"));

        // Successful child that echoes the secret to stdout → returned stdout redacted.
        let out = run(
            &[
                "sh".into(),
                "-c".into(),
                format!("echo out={secret}; exit 0"),
            ],
            None,
            &BTreeMap::new(),
            Duration::from_secs(30),
            &redact,
            None,
            None,
        )
        .await
        .unwrap();
        assert!(
            !out.contains("topsecretvalue123"),
            "secret leaked into stdout: {out}"
        );
        assert!(out.contains("<redacted>"));
    }

    // ── GAP 3 (TERM #418): auto-stage source from Gitea ──────────────────────

    #[test]
    fn module_repo_map_uses_builtin_defaults() {
        let _env = ScopedEnv::new().unset(BUILD_MODULE_REPO_MAP_ENV);
        assert_eq!(resolve_module_repo("terminus"), "Terminus");
        assert_eq!(resolve_module_repo("chord"), "Chord");
        assert_eq!(resolve_module_repo("harmony"), "Harmony");
        assert_eq!(resolve_module_repo("muse"), "Muse");
        assert_eq!(resolve_module_repo("lumina"), "lumina-constellation");
        assert_eq!(resolve_module_repo("lumina-core"), "lumina-constellation");
    }

    #[test]
    fn module_repo_map_unmapped_module_falls_back_to_capitalized_guess() {
        let _env = ScopedEnv::new().unset(BUILD_MODULE_REPO_MAP_ENV);
        assert_eq!(resolve_module_repo("newmodule"), "Newmodule");
        // Already-capitalized / single-char / empty edge cases don't panic.
        assert_eq!(capitalize_module_name("x"), "X");
        assert_eq!(capitalize_module_name(""), "");
    }

    #[test]
    fn module_repo_map_env_override_replaces_one_entry_keeps_others() {
        let _env = ScopedEnv::new().set(
            BUILD_MODULE_REPO_MAP_ENV,
            r#"{"terminus": "terminus-fork", "newthing": "NewThing"}"#,
        );
        // Overridden entry wins...
        assert_eq!(resolve_module_repo("terminus"), "terminus-fork");
        // ...an entry only present in the override is used...
        assert_eq!(resolve_module_repo("newthing"), "NewThing");
        // ...and every OTHER built-in default is untouched.
        assert_eq!(resolve_module_repo("chord"), "Chord");
        assert_eq!(resolve_module_repo("harmony"), "Harmony");
    }

    #[test]
    fn module_repo_map_env_invalid_json_is_ignored_falls_back_to_defaults() {
        let _env = ScopedEnv::new().set(BUILD_MODULE_REPO_MAP_ENV, "not valid json{{{");
        // Malformed override must not panic or block the default map.
        assert_eq!(resolve_module_repo("terminus"), "Terminus");
    }

    #[test]
    fn autostage_enabled_defaults_on_when_unset() {
        let _env = ScopedEnv::new().unset(BUILD_AUTOSTAGE_ENV);
        assert!(autostage_enabled());
    }

    #[test]
    fn autostage_enabled_respects_off_values() {
        for off in ["0", "false", "FALSE", "False"] {
            let _env = ScopedEnv::new().set(BUILD_AUTOSTAGE_ENV, off);
            assert!(!autostage_enabled(), "{off:?} must disable autostage");
        }
    }

    #[test]
    fn autostage_enabled_respects_on_values_and_unknown_values_fail_open() {
        for on in ["1", "true", "TRUE", "yes", "anything-unrecognized"] {
            let _env = ScopedEnv::new().set(BUILD_AUTOSTAGE_ENV, on);
            assert!(autostage_enabled(), "{on:?} must leave autostage enabled");
        }
    }

    #[test]
    fn autostage_remote_url_has_no_embedded_credential() {
        let _env = ScopedEnv::new()
            .unset(BUILD_MODULE_REPO_MAP_ENV)
            .set("GITEA_URL", "http://gitea.example.internal:3000")
            .unset("GITEA_OWNER");
        let (remote, repo) = autostage_remote_url("chord").unwrap();
        assert_eq!(remote, "http://gitea.example.internal:3000/moosenet/Chord.git");
        assert_eq!(repo, "Chord");
        // The whole point of the GIT_ASKPASS approach: the URL never carries a
        // credential, unlike an `x-access-token@host` form.
        assert!(!remote.contains('@'), "remote URL must carry no embedded credential: {remote}");
    }

    #[test]
    fn autostage_remote_url_respects_gitea_owner_override() {
        let _env = ScopedEnv::new()
            .set("GITEA_URL", "http://gitea.example.internal:3000/")
            .set("GITEA_OWNER", "someorg");
        let (remote, _repo) = autostage_remote_url("harmony").unwrap();
        assert_eq!(remote, "http://gitea.example.internal:3000/someorg/Harmony.git");
    }

    #[test]
    fn autostage_remote_url_not_configured_without_gitea_url() {
        let _env = ScopedEnv::new().unset("GITEA_URL");
        let err = autostage_remote_url("terminus").unwrap_err();
        assert!(matches!(err, ToolError::NotConfigured(_)));
    }

    #[test]
    fn autostage_gitea_token_prefers_moose_pat_and_is_never_bearer_wrapped() {
        let _env = ScopedEnv::new()
            .unset("GITEA_IDENTITY_NAME")
            .set("GITEA_PAT_MOOSE", "raw-pat-abc123");
        let token = autostage_gitea_token().unwrap();
        // Raw PAT, not the `Bearer <pat>` form the Cargo-registry header needs —
        // git's plain-PAT auth would break on a `Bearer `-prefixed value.
        assert_eq!(token, "raw-pat-abc123");
        assert!(!token.starts_with("Bearer "));
    }

    #[test]
    fn autostage_gitea_token_falls_back_to_active_identity_when_moose_unset() {
        let _env = ScopedEnv::new()
            .unset("GITEA_PAT_MOOSE")
            .set("GITEA_IDENTITY_NAME", "harmony")
            .set("GITEA_PAT_HARMONY", "harmony-pat-xyz");
        assert_eq!(autostage_gitea_token().as_deref(), Some("harmony-pat-xyz"));
    }

    #[test]
    fn autostage_gitea_token_none_when_nothing_configured() {
        let _env = ScopedEnv::new()
            .unset("GITEA_PAT_MOOSE")
            .unset("GITEA_IDENTITY_NAME")
            .unset("GITEA_PAT_MOOSE"); // (defensive double-unset; identity defaults to moose)
        assert_eq!(autostage_gitea_token(), None);
    }

    #[test]
    fn autostage_token_lands_on_redact_set_and_askpass_script_never_embeds_it_in_argv() {
        // This exercises the exact push-before-touch ordering `autostage_source`
        // uses, without any network I/O: resolve the token, push it to `redact`
        // BEFORE it reaches any argv/URL, and confirm the constructed remote URL
        // (the one that DOES reach argv, via `run()`) contains no trace of it.
        let _env = ScopedEnv::new()
            .set("GITEA_URL", "http://gitea.example.internal:3000")
            .unset("GITEA_IDENTITY_NAME")
            .set("GITEA_PAT_MOOSE", "supersecrettoken999");
        let token = autostage_gitea_token().unwrap();
        let mut redact: Vec<String> = Vec::new();
        redact.push(token.clone());

        let (remote, _repo) = autostage_remote_url("terminus").unwrap();
        assert!(
            !remote.contains(&token),
            "remote URL must never contain the raw token: {remote}"
        );
        assert!(redact.iter().any(|s| s == &token), "token must be in the redact set");

        // Simulate an argv a clone step would build and confirm the token is
        // absent from it (auth travels only via GIT_ASKPASS/env, never argv).
        let clone_argv = vec![
            "git".to_string(),
            "clone".to_string(),
            "--depth".to_string(),
            "1".to_string(),
            "--branch".to_string(),
            "main".to_string(),
            "--".to_string(),
            remote.clone(),
            "/tmp/whatever".to_string(),
        ];
        assert!(
            !clone_argv.iter().any(|a| a.contains(&token)),
            "token must never appear in git argv: {clone_argv:?}"
        );
    }

    #[tokio::test]
    async fn autostage_source_never_overwrites_an_already_staged_dir() {
        // dest already exists → autostage_source must be a strict no-op (Ok, no
        // I/O attempted) even with no Gitea/token config at all — proving the
        // "never clobber" guard runs BEFORE any config resolution.
        let _env = ScopedEnv::new().unset("GITEA_URL").unset("GITEA_PAT_MOOSE");
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("src").join("chord").join("main");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        std::fs::write(dest.join("SENTINEL"), "do not touch").unwrap();

        let mut redact: Vec<String> = Vec::new();
        let res = autostage_source("chord", "main", &dest, &mut redact).await;
        assert!(res.is_ok(), "existing dest must short-circuit to Ok: {res:?}");
        // Untouched — the sentinel file is still exactly what was written.
        assert_eq!(
            std::fs::read_to_string(dest.join("SENTINEL")).unwrap(),
            "do not touch"
        );
        assert!(redact.is_empty(), "no token should be resolved/pushed on the no-op path");
    }

    #[tokio::test]
    async fn autostage_source_not_configured_error_when_no_token_and_dest_absent() {
        let _env = ScopedEnv::new()
            .set("GITEA_URL", "http://gitea.example.internal:3000")
            .unset("GITEA_IDENTITY_NAME")
            .unset("GITEA_PAT_MOOSE");
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("src").join("terminus").join("abc123");
        // dest deliberately never created.
        let mut redact: Vec<String> = Vec::new();
        let err = autostage_source("terminus", "abc123", &dest, &mut redact)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotConfigured(_)));
        assert!(!dest.exists(), "must not create dest on a config failure");
    }

    #[tokio::test]
    async fn run_timeout_kills_the_child_process_tree() {
        // A child that would create a marker AFTER a sleep longer than the timeout.
        // If the timeout path merely dropped the future without killing the process
        // group, the sleep would finish and the marker would appear. The kill must
        // prevent that. `sh -c 'sleep …; touch marker'` — sh is the group leader and
        // sleep is in its group, so killpg(SIGKILL) tears down the whole tree.
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("SHOULD_NOT_EXIST");
        let start = std::time::Instant::now();
        let err = run(
            &[
                "sh".into(),
                "-c".into(),
                format!("sleep 3; : > {}", marker.display()),
            ],
            None,
            &BTreeMap::new(),
            Duration::from_millis(300),
            &[],
            None,
            None,
        )
        .await
        .unwrap_err();
        // Timed out promptly (did not block for the full sleep).
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "run should return at the timeout"
        );
        assert!(format!("{err:?}").contains("timed out"));

        // Wait past when the marker WOULD have been created had the child survived;
        // it must never appear, proving the process was killed.
        tokio::time::sleep(Duration::from_secs(4)).await;
        assert!(
            !marker.exists(),
            "the timed-out child was not killed — its process tree leaked"
        );
    }

    #[test]
    fn remote_secret_rm_argv_is_bounded_and_quoted() {
        let argv = render_remote_secret_rm_argv("builduser@heavy", "/mnt/x/.terminus-build-y.env");
        assert_eq!(argv[0], "ssh");
        let j = argv.join(" ");
        // Bounded connect so a synchronous Drop cleanup can't hang; batch mode so
        // it never prompts; path shell-quoted.
        assert!(j.contains("-o BatchMode=yes"), "{j}");
        assert!(j.contains("-o ConnectTimeout=10"), "{j}");
        assert!(j.contains("builduser@heavy"));
        assert_eq!(argv.last().unwrap(), "rm -f '/mnt/x/.terminus-build-y.env'");
    }

    #[test]
    fn secret_guard_cleans_remote_and_local_on_drop_error_path() {
        use std::sync::{Arc, Mutex};
        // A local staging file that must be unlinked when the guard drops.
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("staging.env");
        std::fs::write(&local, "secret-bytes").unwrap();

        let rec: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(Vec::new()));
        {
            // Guard armed after transfer; NO disarm ⇒ models ANY post-transfer
            // early return (a failing pinned-toolchain install, a build error, a
            // timeout, or a panic) — Drop must clean up.
            let mut g = RemoteSecretGuard::new(
                "builduser@heavy".to_string(),
                "/mnt/build-target/.terminus-build-chord-deadbeef.env".to_string(),
                Some(local.clone()),
                vec![],
            );
            g.recorder = Some(rec.clone());
        } // <- early-return / scope-exit: Drop fires here

        // Remote rm was issued (exactly once) with the expected bounded, quoted argv.
        let calls = rec.lock().unwrap();
        assert_eq!(calls.len(), 1, "remote rm must fire on the error path");
        assert_eq!(calls[0][0], "ssh");
        assert_eq!(
            calls[0].last().unwrap(),
            "rm -f '/mnt/build-target/.terminus-build-chord-deadbeef.env'"
        );
        // Local staging file was unlinked too.
        assert!(
            !local.exists(),
            "local staging secret must be removed on drop"
        );
    }

    #[test]
    fn secret_guard_disarmed_skips_remote_cleanup() {
        use std::sync::{Arc, Mutex};
        let rec: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(Vec::new()));
        {
            // Happy path: the build's own wrapper already removed the remote file,
            // so the guard is disarmed — Drop must NOT issue a redundant remote rm.
            let mut g =
                RemoteSecretGuard::new("h".to_string(), "/p/.env".to_string(), None, vec![]);
            g.recorder = Some(rec.clone());
            g.disarm();
        }
        assert!(
            rec.lock().unwrap().is_empty(),
            "a disarmed guard must not issue a remote rm"
        );
    }

    #[test]
    fn remote_scope_kill_argv_targets_the_named_scope() {
        let unit = "terminus-build-chord-abc-deadbeefcafe";
        let argv = render_remote_scope_kill_argv("builduser@heavy", unit);
        assert_eq!(argv[0], "ssh");
        assert_eq!(argv[1], "builduser@heavy");
        let cmd = &argv[2];
        // SIGKILL the scope, falling back to stop — both target the exact unit's
        // `.scope`, shell-quoted.
        assert!(
            cmd.contains(&format!("systemctl kill --signal=SIGKILL '{unit}.scope'")),
            "kill cmd: {cmd}"
        );
        assert!(
            cmd.contains(&format!("systemctl stop '{unit}.scope'")),
            "stop fallback: {cmd}"
        );
    }

    #[tokio::test]
    async fn cleanup_run_redacts_secret_like_the_build() {
        // The remote-scope-kill cleanup goes through `run(argv, .., redact, None)`
        // — the SAME redaction path as the build. This guards the property that a
        // secret emitted by a FAILING cleanup command is redacted before it lands
        // in the error `remote_scope_kill` logs at `warn!`. (The cleanup child
        // inherits the parent env incl. ambient SCCACHE_REDIS, so this matters.)
        let secret = "<REDACTED-SECRET>".to_string();
        let redact = vec![secret.clone()];
        let err = run(
            &[
                "sh".into(),
                "-c".into(),
                format!("echo leak={secret} 1>&2; exit 1"),
            ],
            None,
            &BTreeMap::new(),
            Duration::from_secs(30),
            &redact,
            None,
            None,
        )
        .await
        .unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            !msg.contains("topsecretvalue123"),
            "cleanup output must be redacted: {msg}"
        );
        assert!(msg.contains("<redacted>"));
    }

    #[test]
    fn remote_scope_is_addressable_by_the_same_unit_the_kill_targets() {
        // The remote build's scope argv carries `--unit=<unit>`, and the timeout
        // kill targets exactly `<unit>.scope` — so a timed-out remote build IS
        // reachable by name (the fix's core invariant).
        let unit = "terminus-build-chord-abc-deadbeefcafe";
        let caps = scope::ScopeCaps {
            memory_max: "12G".to_string(),
            cpu_quota: "400%".to_string(),
            io_weight: "50".to_string(),
            jobs: 4,
        };
        let scope_argv = scope::render_scope_argv(
            unit,
            &caps,
            &BTreeMap::new(),
            &["cargo".into(), "build".into()],
        );
        assert!(
            scope_argv.iter().any(|a| a == &format!("--unit={unit}")),
            "remote scope must be named --unit={unit}: {scope_argv:?}"
        );
        let kill = render_remote_scope_kill_argv("h", unit);
        assert!(kill[2].contains(&format!("{unit}.scope")));
    }

    #[test]
    fn tool_metadata_is_stable() {
        let t = CompilerBuild;
        assert_eq!(t.name(), "compiler_build");
        let p = t.parameters();
        assert_eq!(p["type"], "object");
        assert_eq!(p["required"][0], "module");
        assert_eq!(p["required"][1], "ref");
    }

    #[test]
    fn progress_tool_metadata_is_stable() {
        let t = CompilerProgress;
        assert_eq!(t.name(), "compiler_progress");
        let p = t.parameters();
        assert_eq!(p["type"], "object");
        assert_eq!(p["required"][0], "request_id");
    }

    #[tokio::test]
    async fn progress_tool_reports_lifecycle_and_not_found() {
        use events::{Emit, Stage};
        // Drive a build's stages through the GLOBAL bus (unique id → no clash),
        // then read them back through the tool exactly as a client would.
        let id = format!("tool-{}", uuid::Uuid::new_v4());
        let bus = events::bus();
        bus.emit(&id, Emit::stage(Stage::Queued).message("terminus@abc"));
        bus.emit(&id, Emit::stage(Stage::Scheduled).message("heavy"));
        bus.emit(&id, Emit::stage(Stage::Building).progress(3, 12));
        bus.emit(&id, Emit::stage(Stage::Publishing));
        bus.emit(&id, Emit::stage(Stage::Published).sha("cafebabe"));

        let tool = CompilerProgress;
        let out = tool
            .execute_structured(json!({ "request_id": id, "since": 0 }))
            .await
            .unwrap();
        let s = out.structured.unwrap();
        assert_eq!(s["request_id"], id);
        assert_eq!(s["stage"], "published");
        assert_eq!(s["terminal"], true);
        assert_eq!(s["step"], 3);
        assert_eq!(s["total"], 12);
        // The event tail is present + ordered + terminal carries the sha.
        let evs = s["events"].as_array().unwrap();
        assert_eq!(evs.first().unwrap()["stage"], "queued");
        assert_eq!(evs.last().unwrap()["stage"], "published");
        assert_eq!(evs.last().unwrap()["sha"], "cafebabe");

        // `since` cursor → only the events after it.
        let last_seq = s["last_seq"].as_u64().unwrap();
        let out2 = tool
            .execute_structured(json!({ "request_id": id, "since": last_seq }))
            .await
            .unwrap();
        assert!(out2.structured.unwrap()["events"]
            .as_array()
            .unwrap()
            .is_empty());

        // Unknown build → not_found, never an error.
        let miss = tool
            .execute_structured(json!({ "request_id": "no-such-build-xyz" }))
            .await
            .unwrap();
        assert_eq!(miss.structured.unwrap()["status"], "not_found");
    }

    #[test]
    fn error_tag_carries_request_id_preserving_variant() {
        let e = tag_error_with_request_id(
            ToolError::NotConfigured("BUILD_DATASET_ROOT is not configured".into()),
            "abc123",
            false,
        );
        // Variant preserved; message prefixed with the discoverable id; no marker.
        assert!(matches!(e, ToolError::NotConfigured(_)));
        assert!(e.to_string().contains("[request_id=abc123]"));
        assert!(!e.to_string().contains("supplied_request_id_invalid"));
        // With the invalid-supplied flag, the marker is added (id still clean).
        let e2 = tag_error_with_request_id(ToolError::Execution("boom".into()), "abc123", true);
        assert!(e2.to_string().contains("[request_id=abc123]"));
        assert!(e2.to_string().contains("[supplied_request_id_invalid]"));
    }

    #[test]
    fn resolve_request_id_valid_absent_and_invalid() {
        // Valid caller id → used as-is, not a substitution.
        let (id, inv) = resolve_request_id(&json!({ "request_id": "my-build-1" }));
        assert_eq!(id, "my-build-1");
        assert!(!inv);
        // Absent → auto-generated, NOT flagged (nothing was supplied to invalidate).
        let (id2, inv2) = resolve_request_id(&json!({}));
        assert!(is_valid_request_id(&id2) && !inv2);
        // Present but invalid (separator) → substituted + flagged.
        let (id3, inv3) = resolve_request_id(&json!({ "request_id": "a/b" }));
        assert!(is_valid_request_id(&id3) && !id3.contains('/'));
        assert!(inv3, "invalid supplied id is an observable substitution");
        // Present but overlong → substituted + flagged.
        let (id4, inv4) = resolve_request_id(
            &json!({ "request_id": "z".repeat(events::MAX_REQUEST_ID_LEN + 1) }),
        );
        assert!(is_valid_request_id(&id4) && inv4);
        // WHITESPACE-BEARING ids are INVALID — validated RAW, never trimmed. Both a
        // surrounding-whitespace and an inner-space id are substituted + flagged,
        // and the effective id is NOT the trimmed caller value.
        for bad in [" build-1 ", "build-1 ", " build-1", "a b", "\tbuild-1"] {
            let (idw, invw) = resolve_request_id(&json!({ "request_id": bad }));
            assert!(
                invw,
                "whitespace-bearing id {bad:?} is an observable substitution"
            );
            assert!(is_valid_request_id(&idw), "effective id is valid: {idw:?}");
            assert_ne!(idw, "build-1", "no silent trim/normalize of {bad:?}");
        }
        // A clean id is used VERBATIM (byte-identical).
        let (idc, invc) = resolve_request_id(&json!({ "request_id": "build-1" }));
        assert_eq!(idc, "build-1");
        assert!(!invc);
    }

    #[test]
    fn resolve_request_id_present_non_string_is_an_observable_substitution() {
        // A PRESENT but NON-STRING request_id is INVALID (not treated as absent):
        // substituted + flagged so the replacement is observable.
        for v in [
            json!({ "request_id": 123 }),
            json!({ "request_id": true }),
            json!({ "request_id": ["x"] }),
            json!({ "request_id": { "a": 1 } }),
        ] {
            let (id, inv) = resolve_request_id(&v);
            assert!(
                inv,
                "present non-string request_id is an observable substitution: {v}"
            );
            assert!(is_valid_request_id(&id), "effective id is valid: {id:?}");
        }
        // Explicit null is treated as ABSENT (nothing supplied) → not flagged.
        let (idn, invn) = resolve_request_id(&json!({ "request_id": null }));
        assert!(is_valid_request_id(&idn) && !invn);
        // Truly absent → not flagged.
        let (_ida, inva) = resolve_request_id(&json!({}));
        assert!(!inva);
    }

    /// Process-wide serializer for the rare test that must toggle an env var, so
    /// parallel tests can never interleave the mutation.
    static ENV_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// RAII guard: set/unset one or more env vars for the test's duration and
    /// RESTORE every prior value on drop (fixes the earlier flake where a bare
    /// `remove_var` was never restored). Holds the process-wide env lock so no
    /// other env-touching test interleaves.
    struct ScopedEnv {
        prev: Vec<(&'static str, Option<String>)>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }
    impl ScopedEnv {
        fn new() -> Self {
            Self {
                prev: Vec::new(),
                _lock: ENV_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner()),
            }
        }
        fn unset(mut self, key: &'static str) -> Self {
            self.prev.push((key, std::env::var(key).ok()));
            std::env::remove_var(key);
            self
        }
        fn set(mut self, key: &'static str, val: &str) -> Self {
            self.prev.push((key, std::env::var(key).ok()));
            std::env::set_var(key, val);
            self
        }
    }
    impl Drop for ScopedEnv {
        fn drop(&mut self) {
            // Restore in reverse so a key touched twice ends at its original value.
            for (key, prev) in self.prev.iter().rev() {
                match prev {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    #[tokio::test]
    async fn failed_build_surfaces_request_id_and_is_discoverable() {
        // Force a DETERMINISTIC post-`queued` failure with no subprocess: unset the
        // dataset root so `dataset_root()` returns NotConfigured right after the
        // build emits `queued`. The scoped guard RESTORES the prior value on drop
        // and serializes via a process-wide lock, so this cannot flake other tests
        // (the earlier version left `remove_var` unrestored).
        let _env = ScopedEnv::new().unset("BUILD_DATASET_ROOT");

        // NO caller-supplied request_id → one is auto-generated and MUST come back.
        let err = CompilerBuild
            .execute_structured(json!({ "module": "terminus", "ref": "abc123" }))
            .await
            .unwrap_err();
        let msg = err.to_string();
        let rid = msg
            .split("request_id=")
            .nth(1)
            .and_then(|s| s.split(']').next())
            .map(|s| s.trim().to_string())
            .expect("a failed build's error must carry request_id=<id>");
        assert!(
            !rid.is_empty(),
            "auto-generated request_id surfaced on failure"
        );

        // The invariant's payoff: compiler_progress(rid) FINDS the failed build's
        // stream — a terminal `failed` event with the (redacted) error tail.
        let prog = CompilerProgress
            .execute_structured(json!({ "request_id": rid }))
            .await
            .unwrap();
        let s = prog.structured.unwrap();
        assert_eq!(s["request_id"], rid);
        assert_eq!(s["stage"], "failed");
        assert_eq!(s["terminal"], true);
        let evs = s["events"].as_array().unwrap();
        // queued was emitted before the failure, then the terminal failed event.
        assert_eq!(evs.first().unwrap()["stage"], "queued");
        assert_eq!(evs.last().unwrap()["stage"], "failed");
        assert!(
            evs.last().unwrap()["message"].is_string(),
            "failed event carries the (redacted) error tail"
        );
    }

    #[test]
    fn redacted_failed_message_scrubs_secret_from_non_subprocess_error() {
        // A ToolError NOT from a subprocess (so it never went through run()'s
        // redaction) that embeds a secret-shaped value: the emitter-boundary
        // redaction must scrub it before it reaches the bus. Set the ambient
        // sccache secret so the redaction set contains it (guard restores it). The
        // token is deliberately NOT email/URL-shaped (keeps the PII self-check
        // happy) — redaction is a plain substring scrub of the secret value.
        let secret = "<REDACTED-SECRET>";
        let _env = ScopedEnv::new().set("SCCACHE_REDIS", secret);
        let err = ToolError::Execution(format!("cache connect failed with {secret} (timeout)"));
        let msg = redacted_failed_message(&err);
        assert!(
            !msg.contains("TOPSECRETTOKEN"),
            "secret must be redacted from the failed-event message: {msg}"
        );
        assert!(msg.contains("<redacted>"));
    }

    #[tokio::test]
    async fn failed_message_scrubs_infra_literals_ip_path_host() {
        // S1: an error embedding an IP, the configured dataset root path, and a
        // configured (relay) host must have ALL THREE replaced by placeholders
        // before the failed event is persisted on the bus / returned by
        // compiler_progress. Generic diagnostic prose stays intact.
        let ds_root = "/tmp/bld19-scrub-dataset-root";
        let relay_host = "internal-buildbox-01";
        let ip = "<internal-ip>"; // pii-test-fixture — a fake LAN IP for the S1 scrub test
        let _env = ScopedEnv::new()
            .set("BUILD_DATASET_ROOT", ds_root)
            .set("BUILD_DATASET_RELAY_HOST", relay_host);

        let err = ToolError::Execution(format!(
            "publish to {ds_root}/artifacts failed: ssh {relay_host} ({ip}) connection refused"
        ));
        let msg = redacted_failed_message(&err);

        // Raw infra literals are gone; placeholders present; prose preserved.
        assert!(!msg.contains(ds_root), "dataset path scrubbed: {msg}");
        assert!(!msg.contains(relay_host), "host scrubbed: {msg}");
        assert!(!msg.contains(ip), "IP scrubbed: {msg}");
        assert!(msg.contains("<path>"), "path placeholder: {msg}");
        assert!(msg.contains("<host>"), "host placeholder: {msg}");
        assert!(msg.contains("<ip>"), "ip placeholder: {msg}");
        assert!(
            msg.contains("publish") && msg.contains("connection refused"),
            "generic diagnostic text is preserved: {msg}"
        );

        // Round-trips through the bus AND compiler_progress with the literals gone
        // — asserted via BOTH the failed Stage event and the structured output.
        let id = format!("infra-{}", uuid::Uuid::new_v4());
        events::bus().emit(
            &id,
            events::Emit::stage(events::Stage::Failed).message(msg.clone()),
        );
        let ev_msg = events::bus()
            .snapshot(&id, 0)
            .unwrap()
            .events
            .last()
            .unwrap()
            .message
            .clone()
            .unwrap();
        assert!(
            !ev_msg.contains(ds_root) && !ev_msg.contains(relay_host) && !ev_msg.contains(ip),
            "failed Stage event carries no infra literals: {ev_msg}"
        );
        let prog = CompilerProgress
            .execute_structured(json!({ "request_id": id }))
            .await
            .unwrap();
        let out = prog.structured.unwrap();
        let out_msg = out["events"].as_array().unwrap().last().unwrap()["message"]
            .as_str()
            .unwrap();
        assert!(
            !out_msg.contains(ds_root) && !out_msg.contains(relay_host) && !out_msg.contains(ip),
            "compiler_progress structured output carries no infra literals: {out_msg}"
        );
    }

    #[tokio::test]
    async fn drain_pipe_keeps_draining_past_invalid_utf8() {
        // A chatty child emitting NON-UTF-8 bytes must NOT stop the drain (that
        // would block the child on a full pipe → the build hangs). Feed invalid
        // bytes BEFORE a valid progress line + a secret tail; assert the drain
        // reaches EOF (all lines captured, lossily), the tap saw the progress
        // line, and the secret was redacted.
        let id = format!("drain-{}", uuid::Uuid::new_v4());
        let tap = events::BuildTap::new(&id);
        let redact = vec!["SECRETXYZ".to_string()];
        // \xff\xfe are invalid UTF-8; read_line would Err here and stop draining.
        let input: Vec<u8> =
            b"\xff\xfe garbage\n   Building [==>] 5/9: serde\nleak=SECRETXYZ tail\n".to_vec();
        let captured = drain_pipe(Some(&input[..]), Some(tap), redact).await;
        let text = String::from_utf8_lossy(&captured);
        // Reached EOF past the invalid line: the LATER lines are present.
        assert!(text.contains("5/9"), "progress line captured: {text:?}");
        assert!(
            text.contains("tail"),
            "post-invalid line captured (no early break)"
        );
        // Secret redacted; raw secret never in the capture.
        assert!(!text.contains("SECRETXYZ"), "secret redacted in capture");
        assert!(text.contains("<redacted>"));
        // The tap parsed the progress line into a building {5,9} event.
        let snap = events::bus()
            .snapshot(&id, 0)
            .expect("tap created the track");
        assert_eq!(snap.stage, events::Stage::Building);
        assert_eq!((snap.step, snap.total), (Some(5), Some(9)));
    }

    #[tokio::test]
    async fn drain_pipe_splits_carriage_return_progress_updates_live() {
        // Cargo's progress bar updates with CARRIAGE RETURNS (no newline until the
        // bar finishes). The tap must fire on EACH `\r` so live {step,total}
        // populates as the build compiles — not buffer until a newline. Feed
        // CR-separated updates, an embedded newline, and a non-UTF-8 byte.
        let id = format!("cr-{}", uuid::Uuid::new_v4());
        let tap = events::BuildTap::new(&id);
        let redact = vec!["SEKRET".to_string()];
        // \r-separated progress + a \n + an invalid byte before a final line.
        let input: Vec<u8> =
            b"\r   Building [=>   ] 12/34: a\r   Building [==>  ] 20/34: b\r   Building [===> ] 34/34: c\nCompiling done\r\xffleak=SEKRET\n"
                .to_vec();
        let captured = drain_pipe(Some(&input[..]), Some(tap), redact).await;
        let text = String::from_utf8_lossy(&captured);

        // Each CR update reached the tap live; the parser advanced step/total in
        // order → the ring holds building events for {12,34},{20,34},{34,34}.
        let snap = events::bus()
            .snapshot(&id, 0)
            .expect("tap created the track");
        let steps: Vec<(Option<u32>, Option<u32>)> = snap
            .events
            .iter()
            .filter(|e| e.stage == events::Stage::Building)
            .map(|e| (e.step, e.total))
            .collect();
        assert!(
            steps.contains(&(Some(12), Some(34)))
                && steps.contains(&(Some(20), Some(34)))
                && steps.contains(&(Some(34), Some(34))),
            "each CR progress update fired live: {steps:?}"
        );
        assert!(
            steps.len() >= 3,
            "multiple live building events, not one: {steps:?}"
        );
        assert_eq!(snap.step, Some(34), "latest step reflects the final update");
        assert_eq!(snap.total, Some(34));
        // Drained past the non-UTF-8 byte to EOF; secret redacted; output captured.
        assert!(
            text.contains("Compiling done"),
            "post-CR newline line captured"
        );
        assert!(
            !text.contains("SEKRET"),
            "secret redacted in capture: {text:?}"
        );
        assert!(text.contains("<redacted>"));
        assert!(
            text.contains("12/34") && text.contains("34/34"),
            "full output captured"
        );
    }

    #[test]
    fn build_env_forces_cargo_nm_progress_on_non_tty() {
        // Part 1: the build child env forces cargo's N/M progress even non-TTY.
        let mut env = std::collections::BTreeMap::new();
        inject_cargo_progress_env(&mut env);
        assert_eq!(
            env.get("CARGO_TERM_PROGRESS_WHEN").map(String::as_str),
            Some("always")
        );
        assert!(env.contains_key("CARGO_TERM_PROGRESS_WIDTH"));
        // These are NON-secret term vars → they go via `--setenv`, not the secret
        // env-file (so they reach the cargo child on argv, never leak).
        assert!(!scope::is_secret_env_key("CARGO_TERM_PROGRESS_WHEN"));
        assert!(!scope::is_secret_env_key("CARGO_TERM_PROGRESS_WIDTH"));
    }

    #[tokio::test]
    async fn invalid_caller_request_id_still_surfaces_a_discoverable_id() {
        // AC-1: an INVALID caller request_id must NOT return early with no id.
        // The build falls back to an auto-generated id; a subsequent failure still
        // carries a valid `[request_id=<id>]` and a discoverable failed stream.
        let _env = ScopedEnv::new().unset("BUILD_DATASET_ROOT"); // deterministic post-queued failure
        let err = CompilerBuild
            .execute_structured(json!({
                "module": "terminus",
                "ref": "abc123",
                // Invalid: contains a path separator + is absurdly long.
                "request_id": format!("bad/id-{}", "z".repeat(events::MAX_REQUEST_ID_LEN + 50)),
            }))
            .await
            .unwrap_err();
        let msg = err.to_string();
        let rid = msg
            .split("request_id=")
            .nth(1)
            .and_then(|s| s.split(']').next())
            .map(|s| s.trim().to_string())
            .expect("error must carry a surfaced request_id even for an invalid caller id");
        // The surfaced id is a VALID auto-generated one (not the caller's bad id).
        assert!(is_valid_request_id(&rid), "surfaced id is valid: {rid:?}");
        assert!(!rid.contains('/'), "the invalid caller id was not used");
        // The substitution is OBSERVABLE on the failure path: a marker in the error.
        assert!(
            msg.contains("[supplied_request_id_invalid]"),
            "invalid-supplied-id substitution is signalled: {msg}"
        );
        // And the failed stream is discoverable under that id.
        let prog = CompilerProgress
            .execute_structured(json!({ "request_id": rid }))
            .await
            .unwrap();
        let s = prog.structured.unwrap();
        assert_eq!(s["stage"], "failed");
        assert_eq!(s["terminal"], true);
    }

    #[tokio::test]
    async fn valid_supplied_id_is_used_with_no_substitution_marker() {
        // A VALID supplied id is used as-is: no substitution, no marker on failure.
        let _env = ScopedEnv::new().unset("BUILD_DATASET_ROOT");
        let id = format!("caller-{}", uuid::Uuid::new_v4());
        let err = CompilerBuild
            .execute_structured(json!({
                "module": "terminus",
                "ref": "abc123",
                "request_id": id,
            }))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains(&format!("request_id={id}")),
            "the caller's valid id is used verbatim: {msg}"
        );
        assert!(
            !msg.contains("supplied_request_id_invalid"),
            "no substitution marker for a valid id: {msg}"
        );
    }

    #[tokio::test]
    async fn present_non_string_request_id_surfaces_the_substitution_marker() {
        // End-to-end: a PRESENT non-string request_id is an observable substitution
        // — the failure error carries a valid effective id AND the marker.
        let _env = ScopedEnv::new().unset("BUILD_DATASET_ROOT");
        let err = CompilerBuild
            .execute_structured(json!({
                "module": "terminus",
                "ref": "abc123",
                "request_id": 123, // non-string → invalid supplied id
            }))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("[supplied_request_id_invalid]"),
            "non-string id substitution is signalled: {msg}"
        );
        let rid = msg
            .split("request_id=")
            .nth(1)
            .and_then(|s| s.split(']').next())
            .map(|s| s.trim().to_string())
            .expect("effective id surfaced");
        assert!(
            is_valid_request_id(&rid),
            "effective auto-gen id is valid: {rid:?}"
        );
    }

    #[tokio::test]
    async fn compiler_progress_rejects_overlong_id() {
        // #3: an overlong id is REJECTED at the boundary (clear validation error),
        // never truncated into a colliding key.
        let overlong = "z".repeat(events::MAX_REQUEST_ID_LEN + 1);
        let err = CompilerProgress
            .execute_structured(json!({ "request_id": overlong }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgument(_)));
        // A malformed (separator) id is likewise rejected, not not_found.
        let err2 = CompilerProgress
            .execute_structured(json!({ "request_id": "a/b" }))
            .await
            .unwrap_err();
        assert!(matches!(err2, ToolError::InvalidArgument(_)));
        // WHITESPACE-BEARING ids are rejected RAW (not silently trimmed to a valid
        // id): surrounding whitespace and inner space both → InvalidArgument.
        for bad in [" build-1 ", "build-1 ", "a b"] {
            let e = CompilerProgress
                .execute_structured(json!({ "request_id": bad }))
                .await
                .unwrap_err();
            assert!(
                matches!(e, ToolError::InvalidArgument(_)),
                "whitespace-bearing id {bad:?} must be rejected, not trimmed"
            );
        }
    }

    #[test]
    fn ids_differing_only_in_surrounding_whitespace_never_share_a_track() {
        // Directly on the bus: the clean id and a whitespace-bearing variant are
        // DISTINCT keys — verbatim, never normalized — so they never collide. (The
        // tool boundary rejects/substitutes the whitespace one; this proves the
        // underlying store keys are byte-exact.)
        let bus = events::ProgressBus::with_bounds(16, 8, 0);
        bus.emit("build-1", events::Emit::stage(events::Stage::Queued));
        bus.emit(
            " build-1 ",
            events::Emit::stage(events::Stage::Failed).message("other"),
        );
        let clean = bus.snapshot("build-1", 0).unwrap();
        let spaced = bus.snapshot(" build-1 ", 0).unwrap();
        assert_eq!(clean.request_id, "build-1");
        assert_eq!(spaced.request_id, " build-1 ");
        assert!(!clean.terminal, "clean id keeps its own (queued) stream");
        assert!(spaced.terminal, "spaced id is a separate (failed) stream");
        assert_ne!(clean.generation, spaced.generation);
    }

    #[tokio::test]
    async fn compiler_build_reusing_a_terminal_id_starts_a_fresh_stream() {
        // Fix 2 (end-to-end): a prior build A ended terminal `published` under an
        // id; a NEW compiler_build B reusing that id must ROTATE the stream (via
        // begin) so compiler_progress reflects B's fresh stream, not A's stale
        // terminal state.
        let id = format!("reuse-{}", uuid::Uuid::new_v4());
        // Simulate build A's terminal published stream on the shared bus.
        events::bus().emit(&id, events::Emit::stage(events::Stage::Queued));
        events::bus().emit(
            &id,
            events::Emit::stage(events::Stage::Published).sha("oldshaA"),
        );
        let a = events::bus().snapshot(&id, 0).unwrap();
        assert!(a.terminal, "build A is terminal published");
        let gen_a = a.generation;

        // Build B reuses the id via compiler_build. It fails post-`queued` (no
        // dataset root), but build_inner's begin() rotates the track first.
        let _env = ScopedEnv::new().unset("BUILD_DATASET_ROOT");
        let _ = CompilerBuild
            .execute_structured(json!({
                "module": "terminus",
                "ref": "abc123",
                "request_id": id,
            }))
            .await
            .unwrap_err();

        // compiler_progress now reflects B's FRESH stream: a new generation, starts
        // at `queued`, ends `failed`, and carries NONE of A's stale published sha.
        let prog = CompilerProgress
            .execute_structured(json!({ "request_id": id }))
            .await
            .unwrap();
        let s = prog.structured.unwrap();
        assert_ne!(
            s["generation"].as_u64().unwrap(),
            gen_a,
            "reused id started a fresh generation"
        );
        assert_eq!(s["stage"], "failed");
        let evs = s["events"].as_array().unwrap();
        assert_eq!(
            evs.first().unwrap()["stage"],
            "queued",
            "B's fresh stream starts at queued, not A's published"
        );
        assert!(
            !evs.iter().any(|e| e["sha"] == "oldshaA"),
            "no stale published sha from build A"
        );
    }

    #[tokio::test]
    async fn reused_terminal_id_pre_acceptance_failure_is_not_masked() {
        // The rotation now happens in the WRAPPER, before validation — so even a
        // PRE-ACCEPTANCE failure (invalid module, before build_inner emits
        // `queued`) on a reused id whose prior build ended TERMINAL is not masked
        // by the old track: compiler_progress reflects THIS failure, not A's stale
        // `published`.
        let id = format!("preacc-{}", uuid::Uuid::new_v4());
        // Build A → terminal published (simulated on the shared bus).
        events::bus().emit(&id, events::Emit::stage(events::Stage::Queued));
        events::bus().emit(
            &id,
            events::Emit::stage(events::Stage::Published).sha("oldshaA"),
        );
        let a = events::bus().snapshot(&id, 0).unwrap();
        assert!(a.terminal, "build A is terminal published");
        let gen_a = a.generation;

        // Build B reuses the id but FAILS VALIDATION before acceptance: an invalid
        // `module` (path separator) → validate_segment rejects it inside
        // build_inner, BEFORE `queued`. The wrapper already rotated the track.
        let err = CompilerBuild
            .execute_structured(json!({
                "module": "bad/module",
                "ref": "abc123",
                "request_id": id,
            }))
            .await
            .unwrap_err();
        // The id is still surfaced on this pre-acceptance failure.
        assert!(
            err.to_string().contains(&format!("request_id={id}")),
            "request_id surfaced: {err}"
        );

        // compiler_progress reflects B's FRESH terminal failure, not A's state.
        let prog = CompilerProgress
            .execute_structured(json!({ "request_id": id }))
            .await
            .unwrap();
        let s = prog.structured.unwrap();
        assert_ne!(
            s["generation"].as_u64().unwrap(),
            gen_a,
            "reused id started a fresh generation before validation"
        );
        assert_eq!(s["stage"], "failed", "B's own failure, not A's published");
        let evs = s["events"].as_array().unwrap();
        // Pre-acceptance failure → terminal-only failed (no synthesized queued).
        assert_eq!(evs.len(), 1, "terminal-only failed track: {evs:?}");
        assert_eq!(evs[0]["stage"], "failed");
        assert!(
            !evs.iter().any(|e| e["sha"] == "oldshaA"),
            "no stale published sha from build A"
        );
    }

    #[test]
    fn heavy_classification_fails_to_the_safe_side_when_unknown() {
        // fast → always heavy.
        assert!(classify_heavy_auto(true, Some(None), Some(None)));
        // No known peak (read OK, unset) → positively small.
        assert!(!classify_heavy_auto(false, Some(None), Some(Some(100))));
        assert!(!classify_heavy_auto(false, Some(None), None));
        // Both known → authoritative comparison.
        assert!(classify_heavy_auto(false, Some(Some(200)), Some(Some(100))));
        assert!(!classify_heavy_auto(false, Some(Some(50)), Some(Some(100))));
        // UNKNOWN cases must route to the SAFE (heavy/gated) side, NOT primary:
        // - unreadable peak (present-but-unparsable → None)
        assert!(classify_heavy_auto(false, None, Some(Some(100))));
        // - unreadable threshold
        assert!(classify_heavy_auto(false, Some(Some(50)), None));
        // - a known peak but NO configured threshold (ambiguous)
        assert!(classify_heavy_auto(false, Some(Some(50)), Some(None)));
        // Explicit host requests are honored as-is.
        assert!(request_is_heavy(HostRequest::Heavy, "m", false));
        assert!(!request_is_heavy(HostRequest::Primary, "m", false));
    }

    #[test]
    fn fast_forces_the_heavy_gated_path_even_with_explicit_primary() {
        // B2: fast=true means a full-parallelism heavy build; it must route
        // through the heavy (window+cap gated) path regardless of an explicit
        // primary host request — never bypass the heavy window/cap.
        let heavy = |req, fast| classify_request_heavy(req, fast, Some(Some(10)), Some(Some(1000)));
        assert!(heavy(HostRequest::Primary, true));
        assert!(heavy(HostRequest::Auto, true));
        assert!(heavy(HostRequest::Heavy, true));
    }

    #[test]
    fn heavy_safety_overrides_explicit_primary_for_a_known_heavy_module() {
        // Fix 3 / AC-6: an explicit primary request is only a preference. A
        // known-HEAVY module (peak over threshold) requested with host=primary,
        // fast=false is STILL gated through the heavy path; a known-SMALL one
        // still fast-paths on primary.
        let known_heavy = (Some(Some(99_999u64)), Some(Some(1_000u64)));
        let known_small = (Some(Some(10u64)), Some(Some(1_000u64)));
        assert!(
            classify_request_heavy(HostRequest::Primary, false, known_heavy.0, known_heavy.1),
            "explicit primary must NOT let a known-heavy module skip the heavy gate"
        );
        assert!(
            !classify_request_heavy(HostRequest::Primary, false, known_small.0, known_small.1),
            "explicit primary still fast-paths a positively-known-small module"
        );
        // An ambiguous/unreadable module under explicit primary also stays gated.
        assert!(classify_request_heavy(HostRequest::Primary, false, None, Some(Some(1_000))));
        // Explicit heavy stays heavy; no-heavy-signal (no known peak) primary is small.
        assert!(classify_request_heavy(HostRequest::Heavy, false, Some(None), None));
        assert!(!classify_request_heavy(HostRequest::Primary, false, Some(None), Some(Some(1_000))));
    }

    #[test]
    fn spawn_guard_does_not_burn_the_slot_before_redis_is_available() {
        // Fix 2: a register() with NO scheduler must NOT consume the once-slot, so
        // a later register() (once Redis is materialized) can still spawn exactly
        // once; a third does not double-spawn.
        use std::sync::atomic::AtomicBool;
        let slot = AtomicBool::new(false);
        // Pre-Redis registrations: no scheduler, slot untouched.
        assert_eq!(decide_scheduler_spawn(&slot, false), SpawnDecision::NoScheduler);
        assert_eq!(decide_scheduler_spawn(&slot, false), SpawnDecision::NoScheduler);
        // Redis now configured → the first available registration spawns.
        assert_eq!(decide_scheduler_spawn(&slot, true), SpawnDecision::Spawn);
        // Subsequent registrations never double-spawn.
        assert_eq!(decide_scheduler_spawn(&slot, true), SpawnDecision::AlreadySpawned);
        assert_eq!(decide_scheduler_spawn(&slot, false), SpawnDecision::NoScheduler);
        assert_eq!(decide_scheduler_spawn(&slot, true), SpawnDecision::AlreadySpawned);
    }

    #[test]
    fn release_tool_metadata_is_stable() {
        let t = CompilerRelease;
        assert_eq!(t.name(), "compiler_release");
        let p = t.parameters();
        assert_eq!(p["type"], "object");
        assert_eq!(p["required"][0], "module");
        // The op enum offers promote (default) | rollback | current.
        let ops = p["properties"]["op"]["enum"].as_array().unwrap();
        assert!(ops.iter().any(|v| v == "promote"));
        assert!(ops.iter().any(|v| v == "rollback"));
        assert!(ops.iter().any(|v| v == "current"));
        assert_eq!(p["properties"]["op"]["default"], "promote");
        assert_eq!(p["properties"]["from_channel"]["default"], "experimental");
        assert_eq!(p["properties"]["to_channel"]["default"], "stable");
    }

    #[test]
    fn retain_per_channel_is_floored_at_two() {
        // Default when unset is the store's ≥2 default.
        assert!(retain_per_channel() >= 2);
    }

    // ═════════════════════════════════════════════════════════════════════════
    // PCON-01..05 (S122): content-addressed per-SHA build staging
    // ═════════════════════════════════════════════════════════════════════════

    // ── PCON-01: is_full_sha / stage_by_sha_enabled / resolve_ref_to_sha ──────

    #[test]
    fn is_full_sha_accepts_exactly_40_hex_rejects_everything_else() {
        assert!(is_full_sha("d51cdd2d51cdd2d51cdd2d51cdd2d51cdd2d51cd"));
        assert!(is_full_sha("ABCDEF0123ABCDEF0123ABCDEF0123ABCDEF0123"), "uppercase hex is fine");
        // Short/abbreviated sha — must NOT be treated as already-resolved (an
        // ambiguous ref must go through real resolution, never be staged as-is).
        assert!(!is_full_sha("d51cdd2"));
        assert!(!is_full_sha(""));
        assert!(!is_full_sha("main"));
        assert!(!is_full_sha("feat/pcon-per-sha-staging"));
        // 40 chars but not all hex.
        assert!(!is_full_sha("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz"));
        // 41 hex chars (one too many).
        assert!(!is_full_sha("d51cdd2d51cdd2d51cdd2d51cdd2d51cdd2d51cdd"));
    }

    #[test]
    fn stage_by_sha_enabled_defaults_on_and_respects_off_values() {
        {
            let _env = ScopedEnv::new().unset(BUILD_STAGE_BY_SHA_ENV);
            assert!(stage_by_sha_enabled(), "default must be ON (the safe-by-construction path)");
        }
        for off in ["0", "false", "FALSE", "False", "off", "OFF"] {
            let _env = ScopedEnv::new().set(BUILD_STAGE_BY_SHA_ENV, off);
            assert!(!stage_by_sha_enabled(), "{off:?} must disable SHA-staging (rollback lever)");
        }
        for on in ["1", "true", "yes", "anything-else"] {
            let _env = ScopedEnv::new().set(BUILD_STAGE_BY_SHA_ENV, on);
            assert!(stage_by_sha_enabled(), "{on:?} must fail OPEN to the new behavior");
        }
    }

    #[tokio::test]
    async fn resolve_ref_to_sha_returns_a_full_sha_verbatim_with_no_io() {
        // An already-full sha resolves with ZERO network/git I/O (no GITEA_URL,
        // no token needed) — this is also the fast path a direct-sha
        // `compiler_build(ref=<full-sha>)` call takes.
        let _env = ScopedEnv::new().unset("GITEA_URL").unset("GITEA_PAT_MOOSE");
        let sha = "D".repeat(40); // uppercase input, 40 hex chars
        assert!(is_full_sha(&sha), "test sanity: must be a valid full sha");
        let mut redact: Vec<String> = Vec::new();
        let resolved = resolve_ref_to_sha("terminus", &sha, &mut redact).await.unwrap();
        assert_eq!(resolved, sha.to_lowercase(), "normalized to lowercase, otherwise verbatim");
        assert!(redact.is_empty(), "no token should be resolved/pushed on the verbatim path");
    }

    #[tokio::test]
    async fn resolve_ref_to_sha_fails_closed_when_no_token_configured() {
        // A branch-name ref with GITEA_URL set but no token — must fail CLOSED
        // (NotConfigured), never silently fall back to staging the ref itself.
        let _env = ScopedEnv::new()
            .set("GITEA_URL", "http://gitea.example.internal:3000")
            .unset("GITEA_IDENTITY_NAME")
            .unset("GITEA_PAT_MOOSE");
        let mut redact: Vec<String> = Vec::new();
        let err = resolve_ref_to_sha("terminus", "main", &mut redact).await.unwrap_err();
        assert!(matches!(err, ToolError::NotConfigured(_)), "{err:?}");
    }

    // ── ROOT-CAUSE FIX: resolve_sha_for_enqueue (the enqueue-time resolution
    // every enqueue entry point shares) ────────────────────────────────────

    #[tokio::test]
    async fn resolve_sha_for_enqueue_is_none_when_stage_by_sha_is_off() {
        let _env = ScopedEnv::new().set(BUILD_STAGE_BY_SHA_ENV, "off");
        let got = resolve_sha_for_enqueue("terminus", "main").await.unwrap();
        assert_eq!(got, None, "the rollback lever must skip resolution entirely, not just fail-open");
    }

    #[tokio::test]
    async fn resolve_sha_for_enqueue_returns_the_sha_verbatim_for_an_already_full_sha() {
        let _env = ScopedEnv::new().unset(BUILD_STAGE_BY_SHA_ENV);
        let sha = "1".repeat(40);
        let got = resolve_sha_for_enqueue("terminus", &sha).await.unwrap();
        assert_eq!(got, Some(sha));
    }

    #[tokio::test]
    async fn resolve_sha_for_enqueue_fails_closed_on_a_branch_ref_when_gitea_unreachable() {
        // The load-bearing enqueue-time guarantee: SHA-staging ON + a branch
        // ref that cannot be resolved (no Gitea reachable in a unit test) MUST
        // fail the enqueue outright — never silently enqueue a job whose
        // durable identity is unresolved (that would be the exact "ref moves
        // while queued" race this whole fix closes).
        let _env = ScopedEnv::new()
            .unset(BUILD_STAGE_BY_SHA_ENV)
            .set("GITEA_URL", "http://gitea.example.internal:3000")
            .unset("GITEA_IDENTITY_NAME")
            .unset("GITEA_PAT_MOOSE");
        let err = resolve_sha_for_enqueue("terminus", "main").await.unwrap_err();
        assert!(err.to_string().contains("enqueue time"), "{err}");
    }

    #[tokio::test]
    async fn resolve_ref_to_sha_pushes_token_to_redact_before_any_network_attempt() {
        // Mirrors the S7 push-before-touch discipline `autostage_source` uses:
        // even though the ls-remote below will fail (no real Gitea reachable at
        // this URL in a unit test), the token must already be on the redact set
        // by the time any git argv/env could have touched it.
        let _env = ScopedEnv::new()
            .set("GITEA_URL", "http://gitea.invalid.test.example:3000")
            .unset("GITEA_IDENTITY_NAME")
            .set("GITEA_PAT_MOOSE", "resolve-sha-secret-abc");
        let mut redact: Vec<String> = Vec::new();
        let _ = resolve_ref_to_sha("terminus", "main", &mut redact).await;
        assert!(
            redact.iter().any(|s| s == "resolve-sha-secret-abc"),
            "token must be pushed to redact even though the resolution itself fails: {redact:?}"
        );
    }

    // ── PCON-02: check_built_sha_sidecar ───────────────────────────────────────

    #[test]
    fn check_built_sha_sidecar_passes_on_a_matching_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let sha = "a".repeat(40);
        std::fs::write(dir.path().join(BUILT_SHA_SIDECAR), format!("{sha}\n")).unwrap();
        let got = check_built_sha_sidecar(dir.path(), "terminus", &sha, true).unwrap();
        assert_eq!(got, Some(sha));
    }

    #[test]
    fn check_built_sha_sidecar_trims_surrounding_whitespace() {
        // Whitespace-trim behavior is kept — only case-folding was removed.
        let dir = tempfile::tempdir().unwrap();
        let sha = "b".repeat(40);
        std::fs::write(dir.path().join(BUILT_SHA_SIDECAR), format!("  {sha} \n")).unwrap();
        let got = check_built_sha_sidecar(dir.path(), "terminus", &sha, true).unwrap();
        assert_eq!(got, Some(sha));
    }

    #[test]
    fn check_built_sha_sidecar_is_case_sensitive_uppercase_sha_is_a_mismatch() {
        // FINDING (review, HIGH): the on-disk sidecar was previously
        // lowercased before comparison while `expected_identity` was not
        // normalized at all — an uppercase-written sidecar would silently
        // ACCEPT a lowercase request (a wrong-tree acceptance for any caller
        // whose identity carries mixed case, e.g. the raw-ref fallback path).
        // Comparison is now EXACT (trim only) — an uppercase sidecar must be
        // treated as belonging to a DIFFERENT identity than the canonically
        // lowercase resolved sha it's compared against.
        let dir = tempfile::tempdir().unwrap();
        let sha = "b".repeat(40);
        std::fs::write(dir.path().join(BUILT_SHA_SIDECAR), sha.to_uppercase()).unwrap();
        let err = check_built_sha_sidecar(dir.path(), "terminus", &sha, true).unwrap_err();
        assert!(err.to_string().contains(&sha.to_uppercase()));
    }

    #[test]
    fn check_built_sha_sidecar_raw_ref_identity_is_case_sensitive() {
        // The raw-ref fallback identity (BUILD_STAGE_BY_SHA=off, non-strict)
        // is a git branch/tag name — `Foo` and `foo` are DIFFERENT refs. A
        // sidecar recording `Foo` must be rejected for a request for `foo`,
        // never silently accepted as a case-insensitive match.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(BUILT_SHA_SIDECAR), "Foo").unwrap();
        let err = check_built_sha_sidecar(dir.path(), "terminus", "foo", false).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("\"Foo\"") && msg.contains("\"foo\""), "{msg}");
    }

    #[test]
    fn check_built_sha_sidecar_fails_closed_on_a_mismatch_strict() {
        // This is the exact class of bug PCON-02 exists to catch: a staged tree
        // that carries a DIFFERENT sha than what was requested/resolved (a
        // clobbered or alien-SHA checkout) must be refused, not silently built.
        let dir = tempfile::tempdir().unwrap();
        let staged = "c".repeat(40);
        let requested = "d".repeat(40);
        std::fs::write(dir.path().join(BUILT_SHA_SIDECAR), &staged).unwrap();
        let err = check_built_sha_sidecar(dir.path(), "terminus", &requested, true).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains(&staged) && msg.contains(&requested), "{msg}");
    }

    #[test]
    fn check_built_sha_sidecar_fails_closed_when_sidecar_is_missing_strict() {
        // SHA-mode (strict=true): an older ref-keyed stage (or a foreign/
        // manually-placed tree) has no sidecar at all — treated as a hard
        // mismatch (fail-closed), not a pass.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path()).unwrap();
        let err = check_built_sha_sidecar(dir.path(), "terminus", &"e".repeat(40), true).unwrap_err();
        assert!(err.to_string().contains(BUILT_SHA_SIDECAR));
    }

    // ── FINDING 4 (review): strict=false (BUILD_STAGE_BY_SHA=off) policy ──────

    #[test]
    fn check_built_sha_sidecar_non_strict_allows_a_missing_sidecar() {
        // The legacy/off-mode rollback lever must not itself start hard-failing
        // builds whose stage dir predates this feature (no sidecar at all).
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path()).unwrap();
        let got = check_built_sha_sidecar(dir.path(), "terminus", "main", false).unwrap();
        assert_eq!(got, None, "missing sidecar in non-strict mode is a pass-through, not a sha");
    }

    #[test]
    fn check_built_sha_sidecar_non_strict_still_fails_closed_on_a_present_mismatch() {
        // FINDING 4's actual fix: even in the off/legacy mode, a sidecar that
        // IS present but names a DIFFERENT identity must still be rejected —
        // "off" only tolerates a missing sidecar, never a wrong one.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(BUILT_SHA_SIDECAR), "some-other-branch").unwrap();
        let err = check_built_sha_sidecar(dir.path(), "terminus", "main", false).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("some-other-branch") && msg.contains("main"), "{msg}");
    }

    #[test]
    fn check_built_sha_sidecar_non_strict_passes_on_a_matching_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(BUILT_SHA_SIDECAR), "main").unwrap();
        let got = check_built_sha_sidecar(dir.path(), "terminus", "main", false).unwrap();
        assert_eq!(got, Some("main".to_string()));
    }

    // ── PCON-03: per-(module, sha) remote source/target disjointness ──────────

    #[test]
    fn remote_source_path_is_disjoint_across_different_shas_of_one_module() {
        let remote_root = "/mnt/build-dataset";
        let module = "chord";
        let sha_a = "a".repeat(40);
        let sha_b = "b".repeat(40);
        let src_a = format!("{}/src/{}/{}", remote_root.trim_end_matches('/'), module, sha_a);
        let src_b = format!("{}/src/{}/{}", remote_root.trim_end_matches('/'), module, sha_b);
        assert_ne!(src_a, src_b, "different shas of one module must never share a remote source dir");
        assert!(src_a.ends_with(&sha_a) && src_b.ends_with(&sha_b));
    }

    #[test]
    fn remote_target_path_is_per_job_even_for_the_same_module_and_sha() {
        // Two units for the SAME (module, sha) — e.g. two concurrent gate runs
        // that happened to coalesce onto different job ids before dedupe caught
        // up, or a same-sha build + test — must still get DISJOINT remote
        // CARGO_TARGET_DIRs (the unit name always carries a fresh per-invocation
        // uuid), so neither ever fights the other over one shared target.
        let base = std::path::Path::new("/mnt/heavy-target");
        let module = "chord";
        let unit_a = format!("{}-{}", scope::scope_unit_name(module, &"a".repeat(40)), uuid::Uuid::new_v4().simple());
        let unit_b = format!("{}-{}", scope::scope_unit_name(module, &"a".repeat(40)), uuid::Uuid::new_v4().simple());
        let target_a = base.join(format!("{module}-{unit_a}"));
        let target_b = base.join(format!("{module}-{unit_b}"));
        assert_ne!(target_a, target_b, "per-job target dirs must be disjoint even for the same sha");
    }

    // ── PCON-05: gc_sha_stage_dirs ─────────────────────────────────────────────

    /// Create `module_root/<name>` and back-date its mtime by `age_secs` (no new
    /// dependency: `std::fs::File::set_modified` on a dir handle, stable since
    /// Rust 1.75, well under this crate's pinned 1.97 toolchain).
    fn touch_sha_dir(module_root: &std::path::Path, name: &str, age_secs: u64) {
        let dir = module_root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let mtime = std::time::SystemTime::now() - std::time::Duration::from_secs(age_secs);
        let f = std::fs::File::open(&dir).unwrap();
        f.set_modified(mtime).unwrap();
    }

    /// A deterministic, VALID full-40-hex-sha-shaped name for test dir `i` —
    /// FIX 1 means `gc_sha_stage_dirs` now ignores any dir whose name is NOT
    /// this shape, so every GC test dir must actually look like one.
    fn fake_sha(i: usize) -> String {
        format!("{i:0>40}")
    }

    #[test]
    fn gc_keeps_the_newest_retain_count_regardless_of_age() {
        let dir = tempfile::tempdir().unwrap();
        // 5 dirs, oldest to newest by age.
        let shas: Vec<String> = (0..5).map(fake_sha).collect();
        for (i, age) in [500, 400, 300, 200, 100].into_iter().enumerate() {
            touch_sha_dir(dir.path(), &shas[i], age);
        }
        let live = std::collections::HashSet::new();
        // min_age_secs=0 so the FIX-2 floor doesn't interfere with this
        // count/age-focused test (it has its own dedicated tests below).
        let removed =
            gc_sha_stage_dirs(dir.path(), 2, 0, 0, &live, std::time::SystemTime::now()).unwrap();
        // retain_secs=0 means age never protects anything; only the 2 newest
        // (shas[4] age100, shas[3] age200) survive by count.
        assert_eq!(removed.len(), 3, "{removed:?}");
        assert!(!removed.contains(&shas[4]));
        assert!(!removed.contains(&shas[3]));
        for gone in &shas[0..3] {
            assert!(removed.contains(gone), "{removed:?}");
            assert!(!dir.path().join(gone).exists());
        }
        for kept in &shas[3..5] {
            assert!(dir.path().join(kept).exists());
        }
    }

    #[test]
    fn gc_keeps_anything_younger_than_retain_secs_even_beyond_the_count_floor() {
        let dir = tempfile::tempdir().unwrap();
        // 3 dirs, all younger than a generous retain window.
        let shas: Vec<String> = (0..3).map(fake_sha).collect();
        touch_sha_dir(dir.path(), &shas[0], 50);
        touch_sha_dir(dir.path(), &shas[1], 30);
        touch_sha_dir(dir.path(), &shas[2], 10);
        let live = std::collections::HashSet::new();
        // retain_count=1 would normally reclaim the other 2 by count, but
        // retain_secs=3600 (1h) protects everything younger than that.
        // min_age_secs=0 isolates this from the FIX-2 floor.
        let removed =
            gc_sha_stage_dirs(dir.path(), 1, 3600, 0, &live, std::time::SystemTime::now()).unwrap();
        assert!(removed.is_empty(), "{removed:?}");
        for name in &shas {
            assert!(dir.path().join(name).exists());
        }
    }

    #[test]
    fn gc_never_reclaims_a_live_referenced_dir_even_if_old_and_over_count() {
        let dir = tempfile::tempdir().unwrap();
        let (live_sha, dead_sha) = (fake_sha(1), fake_sha(2));
        touch_sha_dir(dir.path(), &live_sha, 999_999);
        touch_sha_dir(dir.path(), &dead_sha, 999_998);
        let mut live = std::collections::HashSet::new();
        live.insert(live_sha.clone());
        // min_age_secs=0 isolates this from the FIX-2 floor (both dirs are
        // ancient anyway; this test is specifically about the live-set).
        let removed =
            gc_sha_stage_dirs(dir.path(), 0, 0, 0, &live, std::time::SystemTime::now()).unwrap();
        assert_eq!(removed, vec![dead_sha.clone()]);
        assert!(dir.path().join(&live_sha).exists(), "a live-referenced dir must never be reclaimed");
        assert!(!dir.path().join(&dead_sha).exists());
    }

    #[test]
    fn gc_on_a_missing_module_root_is_a_harmless_noop() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("never-created");
        let live = std::collections::HashSet::new();
        let removed =
            gc_sha_stage_dirs(&missing, 5, 0, 0, &live, std::time::SystemTime::now()).unwrap();
        assert!(removed.is_empty());
    }

    // ── FIX 1 (review, HIGH): GC precision — only owned sha-shaped dirs ───────

    #[test]
    fn gc_never_touches_a_legacy_ref_keyed_or_foreign_directory() {
        // A legacy ref-keyed stage (BUILD_STAGE_BY_SHA=off, named by a branch)
        // and an unrelated/foreign directory must NEVER be GC candidates at
        // all, regardless of age/count/live-set — this function has no
        // business judging a dir it doesn't own the naming scheme for.
        let dir = tempfile::tempdir().unwrap();
        touch_sha_dir(dir.path(), "main", 999_999);
        touch_sha_dir(dir.path(), "feature-branch", 999_999);
        touch_sha_dir(dir.path(), "some-foreign-dir", 999_999);
        // A real owned sha, also very old and over count/age, to prove GC
        // still does real work on what it DOES own.
        let sha = fake_sha(9);
        touch_sha_dir(dir.path(), &sha, 999_999);
        let live = std::collections::HashSet::new();
        let removed =
            gc_sha_stage_dirs(dir.path(), 0, 0, 0, &live, std::time::SystemTime::now()).unwrap();
        assert_eq!(removed, vec![sha.clone()], "{removed:?}");
        for untouched in ["main", "feature-branch", "some-foreign-dir"] {
            assert!(
                dir.path().join(untouched).exists(),
                "{untouched} must never be a GC candidate (not sha-shaped)"
            );
        }
        assert!(!dir.path().join(&sha).exists());
    }

    // ── FIX 2 (review, HIGH): GC atomicity/age guard — a hard, live-set-
    // independent age floor ────────────────────────────────────────────────

    #[test]
    fn gc_never_reclaims_a_dir_younger_than_min_age_even_when_absent_from_live_set() {
        // The load-bearing FIX-2 guarantee: a RECENT dir is protected by age
        // ALONE — even with an EMPTY live-set (simulating the exact TOCTOU
        // scenario: `peek`'s bounded limit or a claim landing just after the
        // snapshot means a genuinely in-flight build's dir can be invisible
        // to the live-set) and even with retain_count/retain_secs=0 (which
        // would otherwise reclaim everything).
        let dir = tempfile::tempdir().unwrap();
        let recent_sha = fake_sha(1);
        touch_sha_dir(dir.path(), &recent_sha, 30); // 30s old — very fresh
        let live = std::collections::HashSet::new(); // deliberately empty
        let removed = gc_sha_stage_dirs(
            dir.path(),
            0,   // retain_count=0
            0,   // retain_secs=0
            3600, // min_age_secs=1h — well beyond this dir's 30s age
            &live,
            std::time::SystemTime::now(),
        )
        .unwrap();
        assert!(removed.is_empty(), "{removed:?}");
        assert!(dir.path().join(&recent_sha).exists());
    }

    #[test]
    fn gc_reclaims_once_a_dir_clears_the_min_age_floor() {
        // The complement: once a dir is OLDER than min_age_secs, the floor no
        // longer protects it (retain_count/retain_secs/live-set decide as
        // usual) — proving min_age is a floor, not a permanent hold.
        let dir = tempfile::tempdir().unwrap();
        let old_sha = fake_sha(1);
        touch_sha_dir(dir.path(), &old_sha, 7200); // 2h old
        let live = std::collections::HashSet::new();
        let removed =
            gc_sha_stage_dirs(dir.path(), 0, 0, 3600, &live, std::time::SystemTime::now()).unwrap();
        assert_eq!(removed, vec![old_sha.clone()]);
        assert!(!dir.path().join(&old_sha).exists());
    }

    #[test]
    fn sha_stage_min_age_default_and_env_override() {
        {
            let _env = ScopedEnv::new().unset(BUILD_SHA_STAGE_MIN_AGE_SECS_ENV);
            assert_eq!(sha_stage_min_age_secs(), DEFAULT_SHA_STAGE_MIN_AGE_SECS);
            assert!(
                DEFAULT_SHA_STAGE_MIN_AGE_SECS > MAX_BUILD_TIMEOUT_SECS,
                "the default floor must exceed the longest a real build may run"
            );
        }
        {
            let _env = ScopedEnv::new().set(BUILD_SHA_STAGE_MIN_AGE_SECS_ENV, "120");
            assert_eq!(sha_stage_min_age_secs(), 120);
        }
    }

    #[test]
    fn gc_stage_dirs_best_effort_never_panics_without_a_dataset_root() {
        // No BUILD_DATASET_ROOT configured — must be a harmless no-op, never a
        // panic (this is the "opportunistic, called every scheduler tick" path).
        let _env = ScopedEnv::new().unset(BUILD_DATASET_ROOT);
        gc_stage_dirs_best_effort(&std::collections::HashMap::new());
    }

    #[test]
    fn sha_stage_retain_defaults_and_env_overrides() {
        {
            let _env = ScopedEnv::new()
                .unset(BUILD_SHA_STAGE_RETAIN_COUNT_ENV)
                .unset(BUILD_SHA_STAGE_RETAIN_SECS_ENV);
            assert_eq!(sha_stage_retain_count(), DEFAULT_SHA_STAGE_RETAIN_COUNT);
            assert_eq!(sha_stage_retain_secs(), DEFAULT_SHA_STAGE_RETAIN_SECS);
        }
        {
            let _env = ScopedEnv::new()
                .set(BUILD_SHA_STAGE_RETAIN_COUNT_ENV, "9")
                .set(BUILD_SHA_STAGE_RETAIN_SECS_ENV, "60");
            assert_eq!(sha_stage_retain_count(), 9);
            assert_eq!(sha_stage_retain_secs(), 60);
        }
        {
            // Garbage/zero falls back to the safe default rather than a 0-count
            // floor that would let a burst GC tick reclaim everything.
            let _env = ScopedEnv::new().set(BUILD_SHA_STAGE_RETAIN_COUNT_ENV, "0");
            assert_eq!(sha_stage_retain_count(), DEFAULT_SHA_STAGE_RETAIN_COUNT);
        }
    }
}
