## `compiler_build` — the single build door (BLD-05)

The constellation CI/CD (S117) routes **every** shared-host build through one Terminus
tool, exactly as `review_run` is the single review door. `compiler_build` selects a build
host, ensures the pinned toolchain, runs an sccache-backed `cargo` build inside a
resource-capped systemd scope, and publishes a checksummed artifact to the shared build
dataset (`crate::compiler`).

```
compiler_build(module, ref, host="auto", profile="release", fast=false, bin?, source_dir?)
```

- **Host selection** (`compiler/host.rs`) — `auto` builds on the **primary** (dev box,
  moderate RAM, capped) unless the module's known peak (`BUILD_MODULE_PEAK_MB_<MODULE>`)
  exceeds `BUILD_HEAVY_THRESHOLD_MB`, or `fast=true`, in which case it uses the **heavy**
  host (`BUILD_HOST_HEAVY`). `host="primary"|"heavy"` forces a role. `BUILD_HEAVY_THRESHOLD_MB`
  has **no baked-in default** (S1) — it is required only when it would actually change the
  decision (an `auto`, non-`fast` build of a module with a known peak), else `NotConfigured`.
- **Resource caps — Plex protection** (`compiler/scope.rs`) — the build runs under
  `systemd-run --scope` with `MemoryMax` + **`MemorySwapMax=0`** + `CPUQuota` + `IOWeight`.
  The swap-off is load-bearing: an over-budget build is OOM-killed inside its own cgroup
  instead of triggering node-wide swap thrash that would interrupt Plex. Verify the live
  caps with `systemctl show <scope-unit>`. Every cap is **required from config** —
  `BUILD_{PRIMARY,HEAVY}_{MEMORY_MAX,CPU_QUOTA,IO_WEIGHT,JOBS}` — with **no hardcoded
  defaults** (an unset cap is a hard `NotConfigured` naming the var): the operator sizes
  the caps per host, because a wrong default could starve the build or under-protect Plex.
- **Bounded, leak-free subprocesses** — every subprocess runs in its **own process group**
  (`process_group(0)`) with `kill_on_drop`. On timeout the whole LOCAL group is
  `killpg(SIGKILL)`-ed — so a local build tree (`systemd-run` and its `cargo`/`rustc`
  descendants) is torn down, not just the direct child — then the child is reaped (no zombie,
  no leaked process keeping the secret-bearing inherited environment alive past the timeout).
  For a **remote heavy build** the local `ssh` kill can't reach the remote tree, so each build
  runs under a deterministic, unique named scope (`systemd-run --scope --unit=terminus-build-<module>-<ref>-<uuid>`)
  and a timeout ALSO issues a best-effort `ssh host systemctl kill --signal=SIGKILL <unit>.scope`
  (fallback `systemctl stop`) to terminate the remote scope + all its descendants; the remote
  secret env file is removed regardless.
- **Secrets never on a command line** (S7) — the sccache Redis **password** (and the full
  `SCCACHE_REDIS` URL) are never rendered into `--setenv=`/argv (which would leak into `ps`,
  shell history, and journald). `render_scope_argv` defensively drops any secret-shaped key;
  the secret reaches the scoped build through the **inherited process environment**
  (`systemd-run --scope` runs cargo as a direct child that inherits systemd-run's env). On
  the remote/heavy path the secret is written to a **0600 file on the build host** and
  `source`d inside the ssh wrapper immediately before `exec systemd-run`, then deleted —
  again never on a command line. Its removal is **RAII-guaranteed on every post-transfer exit
  path** — the happy path (the wrapper's own `rm`), any `?` error (e.g. a failing
  pinned-toolchain install), a timeout, or a panic — via a scope guard whose `Drop` issues a
  bounded (`ConnectTimeout`) best-effort `ssh host rm -f <file>` (with the local staging file
  unlinked as a backstop), so a leftover remote secret file can never survive an early return.
  The local staging file is created safely against a
  predictable-`/tmp`/symlink attack: an **unguessable random (v4-UUID) filename**, opened
  with **`O_EXCL`** (never opens/truncates an existing path) **+ `O_NOFOLLOW`** (never follows
  a symlink), so `0600` genuinely holds from creation. That file is **shell-injection-safe**: each value is emitted
  single-quoted with embedded quotes escaped as `'\''`, so a hostile Redis password (spaces,
  `$(...)`, backticks, `;`, `|`, newlines, quotes) is fully literal and can neither be
  corrupted nor execute during `source`. Non-secret vars (`SCCACHE_REDIS_ENDPOINT`/`_DB`/
  `_KEY_PREFIX`, `CARGO_TARGET_DIR`, `RUSTC_WRAPPER`) still travel via `--setenv`.
- **Child-output redaction** — a build script / proc-macro / wrapper could print its
  environment and echo a secret. The single subprocess choke point redacts every secret VALUE
  (the `SCCACHE_REDIS_PASSWORD` and the full `SCCACHE_REDIS` URL) from ALL captured child
  output — the failure stderr tail AND the returned stdout — replacing it with `<redacted>`
  before it can reach a `ToolError`, a log line, or a returned string. Covers both the local
  and remote (ssh) build paths.
- **Path-input validation** — every user-controlled value that becomes a path segment
  (`module`, `bin`, `profile`, `target`, `channel`) is validated at the tool entry as a safe
  single segment (allowlist `[A-Za-z0-9._-]`, no empty/`.`/`..`, no separators or shell
  metacharacters) BEFORE any path join / rsync / ssh; `ref` uses the same rules per `/`-segment
  (a branch may contain `/` but never a traversal). A caller-supplied `source_dir` is a full
  path (not a segment), so it is validated by **containment** instead — it must lexically
  resolve inside an allowed root (`${BUILD_DATASET_ROOT}/src`, plus any `BUILD_ALLOWED_SOURCE_ROOTS`)
  before it is used for `current_dir` / `--manifest-path` / rsync, so an absolute-elsewhere or
  `../`-escaping override can't build/sync source outside the dataset. This blocks path-traversal
  (an absolute or `../` value escaping `${BUILD_DATASET_ROOT}`) and command injection.
- **Exec-safe target dir** — `CARGO_TARGET_DIR` is a LOCAL/tmpfs path
  (`BUILD_LOCAL_TARGET_DIR` locally, `BUILD_HEAVY_LOCAL_TARGET_DIR` on the heavy host); a
  hard guard **rejects** any target dir inside the file-level NFS build dataset — applied to
  BOTH the local target and the remote target (cargo compiles then *executes* build scripts;
  NFS breaks exec + adds lock/mtime hazards). The guard **lexically normalizes** `.`/`..`
  (without touching the filesystem, so it works for non-existent paths) so a traversal like
  `/mnt/other/../build/target` that resolves under the dataset is caught. The NFS dataset is
  for source-staging + sccache + artifact publish only.
- **Heavy (remote) build** — for a heavy build the compiler `rsync`s the source to
  `<remote-dataset>/src/<module>/<ref>` on `BUILD_HOST_HEAVY`, runs the capped scoped cargo
  there over ssh with `--manifest-path` (so it needs no remote CWD) and a remote exec-safe
  `CARGO_TARGET_DIR`, then retrieves the built binary back so publish is host-agnostic. The
  remote dataset root is `BUILD_HEAVY_DATASET_ROOT` (falls back to `BUILD_DATASET_RELAY_ROOT`,
  then the local `BUILD_DATASET_ROOT`). Every interpolated value in the remote ssh command
  strings is shell-quoted, and rsync uses `-s`/`--protect-args`, so no path can inject into the
  remote shell (defense-in-depth on top of the segment validation above).
- **sccache → Redis** (`compiler/sccache.rs`) — the auth'd Redis URL is read from the
  vault-materialized `SCCACHE_REDIS` env var and parsed into the **split**
  `SCCACHE_REDIS_ENDPOINT`/`_USERNAME`/`_PASSWORD`/`_DB`/`_KEY_PREFIX` form (the reliable
  one; a bare `SCCACHE_REDIS` URL fell back to local disk in testing). It **fails OPEN**:
  when Redis is unconfigured, unparseable (including a **present-but-invalid port** — a
  non-numeric or out-of-`1..=65535` port fails the whole parse rather than silently defaulting
  to 6379), **or unreachable** — a fast sub-second bounded TCP probe of the resolved endpoint
  (`SCCACHE_REDIS_PROBE_MS`, default 300ms) gates Redis mode, so a syntactically-valid-but-dead
  endpoint degrades to a local dir (`${BUILD_DATASET_ROOT}/cache/sccache`) rather than making
  the build depend on sccache runtime behavior. A cache outage never blocks a build. The parsed
  password is never logged.
- **Pinned toolchain** — `RUST_TOOLCHAIN_PINNED` is installed idempotently
  (`rustup toolchain install`, never `rustup update`); when unset, rustup auto-installs
  from the source tree's `rust-toolchain.toml`.
- **Publish** (`compiler/publish.rs`) — on success the binary is SHA-256'd and copied to
  `${BUILD_DATASET_ROOT}/artifacts/<module>/<channel>/<sha>/<target>/<bin>` with a
  `<bin>.sha256` sidecar (the `sha256sum -c` format the constellation-updater verifies).
  It does **not** flip a `current` pointer — channel promotion is `compiler_release`
  (BLD-07). When the dataset is not mounted RW on the build host, publish relays the
  artifact over a single rsync hop to `BUILD_DATASET_RELAY_HOST` (interim path, pre-BLD-01) —
  relaying **both** the binary and its `<bin>.sha256` sidecar (bundled in one `RelayPlan`), so a
  relay-published artifact is verifiable by the updater, exactly like the local publish.

All hosts, paths, caps, thresholds, and the cache endpoint come from config env
(materialized from the vault where sensitive); there are no infrastructure literals in the
source (S1), and `SCCACHE_REDIS`/its password are read as secrets, never logged and never
placed on a command line (S7).

