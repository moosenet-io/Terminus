# Building the constellation (BLD-02 — pinned toolchain)

Every buildable repo in the `moosenet` org pins **one** rustc version via a
`rust-toolchain.toml` at its root:

```toml
[toolchain]
channel = "1.97.0"
targets = ["x86_64-unknown-linux-musl"]
components = ["rust-src"]
```

## Rules
- **One version, fleet-wide.** The pin is authoritative. Do not override it per host.
- **Never `rustup update` on a shared build host.** A mid-build `rustup update` on the
  primary build host removed its self-contained linker and broke every cold build until
  the toolchain was reinstalled. The compiler tool (BLD-05) installs the pinned version
  idempotently (`rustup toolchain install $(pin)`) before a build instead.
- **Bumping the pin** is a deliberate, reviewed change: update `rust-toolchain.toml` in
  every buildable repo in the same sprint (Terminus, Chord, harmony, lumina-constellation,
  Muse), confirm a clean build on every build host, then merge.
- **musl target + `rust-src`** are pinned so portable static builds resolve without an
  ad-hoc `rustup target add` mid-build.

## Build hosts (current)
Pinned/installed at rustc **1.97.0** on: the GPU primary build host, the harmony build
host, and the dev box. New build hosts install the pinned version on first use.

# Build discipline — one door (BLD-14)

Building on a **shared** host is a single-door operation, exactly like `review_run` is the
one door for reviews: **no agent, pipeline, or Harmony build-space runs an ad-hoc
`cargo build` / `cargo test` on a shared host.** Every shared-host build goes through the
compiler queue (`compiler_request` → the scheduler → `compiler_build`).

## Why
Ad-hoc `cargo build`/`cargo test` on shared infrastructure has repeatedly caused real
outages: a cold target dir filling a host's disk, a parallel build OOM-ing the box,
uncoordinated toolchain drift (`rustup update` mid-build breaking the linker), and GPU/RAM
contention with the always-on serving workloads (e.g. the permanent coder serve, Plex).
The compiler exists to make shared-host builds **capped, serialized, idle-coordinated, and
observable** (per-host concurrency caps, per-module locks, heavy-build windows + idle-mode
lease, sccache, live progress) — running `cargo` directly bypasses all of that.

## The rule
- On a **shared** host: build **only** via `compiler_*`. A direct `cargo build`/`cargo test`
  on a shared host is a **reviewable violation** — treat it the way an unsanctioned Plane or
  git access path is treated.
- **Boundary.** A quick local `cargo check`/`cargo test` on the **dev box's own capped,
  appdata-backed disk** (not a shared host) is allowed — the rule targets shared-host
  contention, not a developer's own isolated, capacity-bounded workspace. State this boundary
  explicitly at any call site that could build.

## The pattern (worktree-local → compiler relay)
An agent/gate that needs a shared-host build submits the work to the compiler rather than
compiling inline:
1. Produce the exact source to build as a **committed** tree — scope it to only the intended
   change (a throwaway index staging only the candidate paths → `commit-tree`, without
   advancing any branch), so what's built is exactly what's under test and a failed candidate
   never pollutes history.
2. **Stage** that committed tree where the compiler resolves sources
   (`${BUILD_DATASET_ROOT}/src/<module>/<ref>`) — or, if the source can't be made resolvable
   from this host, **do not submit**; report the build as not-verified.
3. Submit `compiler_request(module, ref)`, follow `compiler_progress` to a terminal state, and
   consume the resulting artifact sha (from the build's own terminal `published` event).
4. There is **no inline-`cargo` fallback** on the shared-host path: if the compiler is
   unavailable, the gate reports *not verified* (which blocks advance) rather than silently
   building locally.

Harmony's Stage-4 test-gate implements exactly this via `harmony-core/src/compiler_client.rs`
(gated by `HARMONY_COMPILER_ROUTING`), routing through the sanctioned Terminus egress door.

# Artifact store & channels (BLD-07)

Deploy is **build-once → publish → promote** — the `stable` train is never rebuilt, only
re-pointed at an already-built sha. Every path derives from the configured dataset root
(`BUILD_DATASET_ROOT`); there are no hardcoded infra paths.

## Store layout

```
${BUILD_DATASET_ROOT}/artifacts/<module>/<channel>/
├── <sha>/                        # immutable, content-addressed (sha = SHA-256 of the binary)
│   ├── <target>/<bin>            #   the built binary
│   ├── <target>/<bin>.sha256     #   its sha256sum-format sidecar (updater verifies this)
│   └── dist-manifest.json        #   per-sha manifest (module/channel/target/bin + rel paths + created_at)
├── current                       # pointer file → the blessed sha (what the updater fetches)
├── current.prev                  # pointer file → the previous blessed sha (one-step rollback target)
└── history.jsonl                 # append-only audit: {at, action, sha, previous, from_channel}
```

- **Channels:** `experimental` (every fresh build publishes + blesses here) and `stable`
  (promoted, blessed builds). Channels are independent trees — each holds its OWN copy of a
  blessed sha, so pruning `experimental` can never strand `stable` (Rust-train model).
- **`current` is the contract.** The constellation-updater reads `<module>/<channel>/current`
  to learn the blessed sha, then fetches that sha dir + verifies the `.sha256`.

## Pointer discipline

- **Atomic flips.** `current`/`current.prev` are written to a uniquely-named temp file in the
  same directory and `rename(2)`d into place — a reader sees the old or new sha, never a
  partial/truncated pointer.
- **Verify before bless.** The pointer flip itself is the fail-closed choke point: `current`
  is moved onto a sha only after the store confirms the binary AND its `.sha256` exist and the
  content-address dir name equals both the binary's actual SHA-256 and the sidecar's recorded
  sha. A missing/corrupt/checksum-mismatched sha is refused — for promote, the build-time
  flip, AND rollback alike, so no caller can bless an unverified sha.
- **Rollback.** Each flip records the prior `current` as `current.prev` and appends a
  `history.jsonl` entry, so a channel can be reverted one step (and the rollback is itself
  reversible). The rollback TARGET is verified the same way before the flip — if the previous
  sha was pruned or corrupted, the rollback is refused and `current` is left untouched.

## Tools

- **`compiler_build`** — builds a sha, publishes it to `experimental/<sha>/…`, writes its
  manifest, and (on a local publish) flips `experimental/current` onto it, then prunes.
- **`compiler_release`** — the pointer surface, **no rebuild**:
  - `op=promote` (default): `compiler_release(module, sha, from_channel=experimental,
    to_channel=stable)` — verifies the sha in `from_channel`, gives `to_channel` its own
    verified copy, atomically flips `to_channel/current`, then prunes.
  - `op=rollback`: revert `to_channel` to its previous blessed sha.
  - `op=current`: query the blessed sha (and previous) for `(module, to_channel)`.

## Retention

Pruning keeps the newest **`BUILD_RETAIN_PER_CHANNEL`** shas per channel (default **2**,
floored at 2), and never prunes the `current` or `current.prev` targets — which it reads from
the pointer FILES at prune time, so an older rollback target is always protected.
