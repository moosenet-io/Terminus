## At a glance

| | |
| --- | --- |
| **Tools** | ~53, one per integrated service (GitHub, Plane, Prometheus, …). Each tool exposes a set of **actions** that vary with the backing service and change over time — ~306 individual MCP callables in total across all tools. |
| **Transport** | stdio (local/SSH) and HTTP, with an mTLS listener for federated/remote clients |
| **Auth** | per-identity mTLS client certificates (`crate::pki`); named-identity tokens (`GITEA_PAT_<NAME>`, `PLANE_PAT_<NAME>`) for outbound git-forge/tracker calls |
| **Governance** | path-jailed filesystem access, vault-only secrets (never a raw `env::var` for a credential), a mandatory Rust PII gate on every public-forge write, sanitized audit logging |
| **Flagship harness** | **MINT** — the model-intake/serving-profile CLI and tool suite. One `MintHarness` orchestrator drives both sweep families (`RunKind::Coder` for the code sweep, `RunKind::Assistant` for the Lumina seven-dimension sweep) through one lifecycle over the shared `lumina_intake` DB; the two standalone sweep binaries are thin `MintHarness::run(RunKind::…)` entrypoints. See [`docs/tools/README.md`](docs/tools/README.md#mint-flagship) |
| **Inference** | proxied to the separate [Chord](https://github.com/moosenet-io/Chord) process — Terminus does tool egress, Chord does inference egress |

