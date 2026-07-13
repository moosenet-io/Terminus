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
