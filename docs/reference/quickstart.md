## Quickstart

```sh
git clone <your-terminus-repo-url>
cd terminus-rs
cargo build --release
```

Terminus ships three binaries:

- **`terminus_personal`** — the personal/admin registry (ledger, vitals,
  crucible, relay, meridian, odyssey, and other life-admin + git/tracker
  tools), served over a plain listener plus an optional mTLS listener.
- **`terminus_primary`** — the gateway binary: serves the **core** registry
  locally and federates in the personal registry's tools from a
  `terminus_personal` deployment, over the same mTLS/`enroll` front door, plus
  forwards inference routes to Chord.
- **`pii_gate`** — the standalone PII/secret-scanning binary used as a git
  pre-push/pre-commit hook and by the public-mirror engine.

Configuration is entirely environment-driven — every credential is expected
to already be materialized into the process environment by your own secret
manager at startup (Terminus never reads a raw literal token from config).
For a full walkthrough of standing up a client against a running Terminus —
enrollment, mTLS certs, and the personal-services deployment shape — see the
deployment guides:

- [`docs/deploy/client.md`](docs/deploy/client.md) — connecting a new MCP
  client (enrollment, certs, transport selection).
- [`docs/deploy/personal-services.md`](docs/deploy/personal-services.md) —
  standing up `terminus_personal` / `terminus_primary`.

