# Operator Guides

Task-oriented walkthroughs. Every command names a real binary or registered
tool; every configuration key is a real env var name whose value comes from the
vault at runtime.

| Guide | Task |
|---|---|
| [Run a model-intake sweep](run-a-model-intake-sweep.md) | Profile one model or the whole fleet with the `mint` CLI: coder/assistant sweeps, single-case reruns, gap audits, GPU authority. |
| [Run a review panel](run-a-review-panel.md) | Stand up `review_daemon`, then dispatch a multi-provider `review_run` panel and read its aggregated verdict. |
| [Run the git-public mirror](run-the-git-public-mirror.md) | Produce and push a PII-swept public mirror pass with `git_public_mirror_run`, and verify cleanliness with the `pii_gate` binary. |

Background reading: [Getting Started](../getting-started.md) for build/run
basics, [Architecture](../architecture.md) for how these pieces relate, and the
per-subsystem [reference pages](../reference/index.md) for symbols and
configuration.
