## Documentation

This README is the front door; everything past "at a glance" lives in
[`docs/`](docs/README.md), organized by area:

| Area | What's there |
| --- | --- |
| [`docs/README.md`](docs/README.md) | The documentation site index — start here for the full table of contents. |
| [`docs/architecture/`](docs/architecture/) | Federation (how `terminus-primary` aggregates core + personal tools), the [mesh](docs/architecture/mesh.md) (N-upstream federation, identity/RBAC, tailnet exposure, onboarding, known gaps), auth (mTLS identity model), and the Chord-integration boundary/wire contract. |
| [`docs/networking/`](docs/networking/) | WireGuard and Tailscale transport options for reaching a Terminus deployment off-LAN, including the optional embedded-tsnet mode (MESH-04, `tsnet` Cargo feature — no host `tailscaled` required; see [`docs/networking/tailscale.md`](docs/networking/tailscale.md#alternative-embedded-tsnet-mesh-04--no-host-tailscaled-at-all)). |
| [`docs/deploy/`](docs/deploy/) | Client enrollment/deploy guide and the personal-services (`terminus_personal`/`terminus_primary`) deployment guide. |
| [`docs/tools/`](docs/tools/README.md) | The full tool index — all 53 modules grouped by domain, plus the **MINT** flagship harness. |
| [`docs/house-style.md`](docs/house-style.md) | The Tier-A house-style rule catalog (deterministic `syn`-AST checks run in the test gate via `cargo test -p terminus-rs`) — secret-shaped env vars, non-empty tool descriptions, no `panic!` in `execute`, and the `// house-style-allow: <reason>` waiver convention. |
| [`docs/constellation/`](docs/constellation/) | The Constellation control-plane GUI: the harmony-web adaptation plan (CONST-01) and the aggregation API layer (CONST-02) this crate hosts at `/api/*` for `constellation-web`. |

