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
