# PCON-07 — Speculative Merge Batching (design note)

Spec: `S122-pipeline-concurrency` · Item: **PCON-07** · Prefix: `TERM`
Builds on: **PCON-06** (merge-queue rebase-and-re-gate, the Bors / GitHub-merge-queue
single-PR model). Read `docs/specs/S120-gitea-merge-queue.md` (GMQ-02..05) and the PCON-06
section of the S122 spec first — this layer sits directly on top of both.

A throughput optimization **on top of** PCON-06's serialized rebase-and-re-gate: instead
of gating one PR at a time, stack the front N same-base PRs into ONE speculative rebased
batch, gate the batch **once**, and merge all N if green; on a **red** batch, **bisect**
(binary-split, re-gate halves, eject the offender, merge the green remainder, requeue the
offender with its failure reason). This is the GitHub-merge-queue "speculative batches"
model.

## The N-cap (opt-in, safe baseline)

`BUILD_MERGE_BATCH_MAX` (`crate::config::build_merge_batch_max`) — the maximum PRs stacked
into one batch.

- **Default (unset) = `1` = NO batching.** Every merge takes the exact PCON-06 single-PR
  rebase-and-re-gate path, **byte-for-byte**. This is the safe baseline; the batch layer is
  never entered.
- A caller can *request* a batch (value `> 1` **and** a `batch_prs` set with more than one
  entry to `gitea_merge_pr`), but **production currently DEGRADES that request to N=1** —
  see "Production status" below. Nothing untested can land.
- `0` / garbage clamps up to `1` (a batch always contains at least the front PR).

## Production status — BUILT + TESTED, but the production land is gated OFF

**A real N>1 land is intentionally NOT performed in production today**, because it cannot be
made safe with the current forge (frontier-gate finding, confirmed). Gating each member
rebased onto `main` **independently** does NOT prove the **combined** N-PR state that
actually lands is green — so landing such a state (or a member atop an advanced base) would
bypass PCON-06's exact-landing-state guarantee. A safe batch needs a **combined-state gate**:
gating the ONE combined SHA that actually lands (from a forge combined-branch primitive, or a
single-door local-git stack builder). No such primitive exists in the single-door path yet.

Therefore `MergePr::execute_with_queue_and_regate`, when it sees a batch request with
`BUILD_MERGE_BATCH_MAX > 1`, **logs once** ("speculative batching requires a
combined-branch/combined-state gate primitive not yet available; running N=1 per PCON-06")
and runs the exact PCON-06 single-PR path for the **front PR** — the other requested members
take their own separate merge calls, each with its own PCON-06 single-PR gate. No branch is
speculatively rebased, so there is no half-rebased member to clean up on any path.

The `run_speculative_batch` algorithm + `SpeculativeBatchOps` trait + the full fake-based
test matrix ARE the deliverable (the spec's "design + acceptance" Phase) — complete, proven,
and referenced only by tests today (`#[allow(dead_code)]`), ready to wire the day a
combined-state gate primitive lands.

## Where it composes with PCON-06

A batch is designed to run **inside one `MergeQueue::with_merge_slot` acquisition** for the
base key — the slot still serializes per base; batching only changes what ONE slot would
process. The algorithm (`merge_queue::run_speculative_batch`) is a **pure orchestration** over
an abstract `merge_queue::SpeculativeBatchOps`, exactly mirroring how PCON-06 abstracts
`ReGate` + `MergeLockStore` so the algorithm is unit-tested deterministically with fakes.

**What the fake tests prove (and don't):** they exercise the batch-formation / single-gate /
bisect-on-red **algorithm** against the **trait** with an in-memory fake — NOT the real
PCON-06 rebase/gate/merge (no forge, no compiler door, no Redis). They prove the algorithm is
correct **given** a SHA-bound, deadline-bounded, same-base combined-state gate; they do **not**
prove any production behavior (there is none — production runs N=1, above).

### Safe-by-construction trait (so a future adapter can't be unsafe)

The trait itself forces the exact-landing-state guarantee onto any implementer — a conforming
adapter is safe by construction:

| Trait method | What the contract forces |
|---|---|
| `stack(prs, deadline) -> BatchStack` | Produce ONE **combined SHA** (`StackedBatch`) for the whole stack; **eject any member not on the front PR's base** (mixed-base would break per-base serialization); bounded by `deadline`. |
| `gate(&StackedBatch, deadline) -> BatchGateVerdict` | On green, **RETURN the exact combined SHA it proved** (`Green(sha)`) — cannot claim green without naming what it gated; bounded by `deadline`. |
| `merge(&StackedBatch, gated_sha, deadline) -> Result` | **Bind the land to `gated_sha`** and verify the stack still resolves to it (fail-closed on drift) before ONE combined fast-forward; bounded by `deadline`. |

A **future** production impl (once a combined-state gate primitive exists) wires these to the
sanctioned single door (S9): a combined-branch builder for `stack`, `ReGate` → `compiler_build`
on the **combined** SHA for `gate`, and `merge_pull_with_base` (`head_commit_id` + recheck) for
the SHA-bound `merge`. It **cannot** gate members independently — the trait shape does not
permit it (there is one `StackedBatch` with one SHA).

## Algorithm

`run_speculative_batch(ops, prs, deadline)` where `prs` is already capped to
`BUILD_MERGE_BATCH_MAX` with the front PR first, and `deadline` is the slot lease:

1. **Stack** the batch into ONE combined branch (`ops.stack`). A PR that **conflicts** or is
   **not on the front's base** is ejected **before** the gate (`BatchEjectReason::RebaseConflict`)
   and the batch reforms without it.
2. If nothing combined cleanly → done (everything ejected).
3. **Gate once** on the combined SHA:
   - **Green(sha)** → **land once**, bound to `sha` (one combined fast-forward). A combined FF
     is all-or-nothing: if it drifts/fails, the WHOLE gated set is requeued
     (`merge_failures`), never a partial untested land. `BatchOutcome.landed_sha` records the
     exact SHA that landed (== the gated SHA).
   - **TimedOut / Unreachable** → fail-safe: **fall back to N=1 for the front PR**
     (`fell_back_to_single = Some(front)`); the rest stay queued.
   - **Red** → **bisect**.

### Bisect-on-red

`bisect_red(prefix, prefix_winner, batch, deadline, known)` — re-stack `prefix + batch` into
ONE combined SHA and gate it (skipping when the verdict for exactly that set is already known —
the top-level red is threaded in to avoid re-stacking/re-gating it):

- **Green(sha)** → the whole `prefix + batch` survives as one combined stack with SHA `sha`
  (the new winner).
- **TimedOut / Unreachable** → fail-safe: eject ALL of `batch`
  (`BatchEjectReason::GateUnavailable`); the `prefix_winner` survives — never land unproven.
- **Red**, `batch.len() == 1` → the lone PR IS the offender: eject it
  (`BatchEjectReason::RedGate`); the `prefix_winner` survives.
- **Red**, `batch.len() > 1` → split; recurse **left** (on `prefix`), then **right** (on the
  left's surviving combined stack) so the final winner is ONE combined stack gated green as a
  unit.

**Correctness invariant (machine-checked in the tests):** the surviving set the bisection
returns was gated **GREEN as one combined SHA**, and the single land is **bound to that exact
SHA** — so `BatchOutcome.landed_sha` equals the SHA the gate returned for that same set (the
tests assert this equality). Never an ad-hoc union of PRs only ever gated apart.

### Worked example (single offender)

Batch `[p1, p2, p3]`, `p2` red:
`gate[p1,p2,p3]`→red → split → `gate[p1]`→green → `gate[p1,p2,p3]` (known, skipped) recurse
→ `gate[p1,p2]`→red,len1→**eject p2** → `gate[p1,p3]`→green. Result: **merge `[p1, p3]`,
eject `p2`** (requeued with its red reason). `[1,3]` was gated green as a unit.

## Failure semantics (distinct, author-facing)

| Reason | When | Disposition |
|---|---|---|
| `RebaseConflict` | PR conflicts stacking the speculative batch | ejected **pre-gate**, batch reforms, requeued |
| `RedGate` | bisection isolates the PR as the batch-red offender | green remainder merges; offender requeued with the gate reason |
| `GateUnavailable` | gate timed out / door unreachable for a **sub-batch** during bisection | that sub-batch requeued (never merged unproven); `prefix` survives |
| `fell_back_to_single` | **top-level** gate timed out / door unreachable | front PR runs the PCON-06 single path (N=1); rest stay queued |
| `merge_failures` | the green combined fast-forward land drifted / failed | the WHOLE gated set requeued (a combined FF is all-or-nothing — never a partial untested land) |

Every ejected/failed PR bounces with a clear, distinct reason and is requeued; every merged
PR was part of the exact combined SHA gated green (== `landed_sha`).
`BUILD_MERGE_BATCH_MAX=1` (default) never enters this layer.

## Known gap vs. the spec (flagged, not worked around)

The spec's APPROACH says "stack the front N PRs **of a base's queue**". Two facts about the
current code make the *live* "front-N-from-the-queue" discovery unavailable, so it is
**caller-supplied** instead:

1. **The queue stores tickets, not PRs.** `MergeQueue`'s wait ZSET holds opaque FIFO tickets;
   there is no ticket→PR association, so the queue cannot enumerate "the front N PRs" itself.
   A ticket→PR registry would be its own item.
2. **No combined-branch / combined-state gate primitive.** Gitea's `update-branch` endpoint
   only merges a PR's *base* into its head — it cannot stack PR-B onto PR-A onto `main` into
   ONE combined branch. Gating each member rebased onto `main` **independently** does NOT
   prove the COMBINED N-PR state that actually lands is green, so it would bypass PCON-06's
   exact-landing-state guarantee (frontier-gate finding). A safe batch REQUIRES a
   combined-state gate: gating the ONE combined SHA that lands (a forge combined-branch API,
   or a single-door local-git stack builder producing one combined SHA). No such primitive
   exists in the single door yet — so no safe production adapter can exist, and the
   `SpeculativeBatchOps` trait is deliberately shaped around ONE `StackedBatch`/one combined
   SHA so a future adapter is forced to gate the combined state, not members apart.

Because of (2), `BUILD_MERGE_BATCH_MAX` **defaults to 1**, and a requested N>1 batch is
**degraded to N=1 in production** (see "Production status" above): the algorithm, trait, and
SHA-bound land are all implemented and tested (the spec's "documented Phase — design +
acceptance"), ready to light up fully the day a combined-state gate primitive lands. This
matches the spec's own framing ("design + optional build … the build is optional/follow-on").

### Same-base requirement (per-base serialization)

The `with_merge_slot` critical section is keyed off the **front PR's base**
(`{owner}/{repo}/{base}`). A batch that mixed bases would violate per-base serialization
(members of a different base would run under the wrong slot). In production this is **moot**:
the N=1 degrade only ever merges the front PR, under its own base's slot, so a mixed-base
`batch_prs` cannot affect slot keying. Any **future** combined land MUST validate that every
member shares the front PR's base **before** keying the slot — this is a documented
precondition of `SpeculativeBatchOps` (the caller supplies same-base members) and of the
combined-branch producer.

## Guarantee scope & the protected-`main` ops step

Identical to PCON-06: the exact-landing-state invariant holds for every **queue-mediated**
change; the one irreducible residual is a **direct, out-of-queue push to `main`** in the
sub-second window between a pre-merge base-recheck and the merge POST (Gitea has no
server-side base guard, only `head_commit_id` for the head). Batching does **not** widen this
window. The complete closure is the same ops step: configure `main` as a **protected branch**
so only the merge queue's identity can push to it.

## Tests

- Algorithm (`gitea::merge_queue::tests`, fake `SpeculativeBatchOps` — exercises the ALGORITHM
  against the TRAIT, not real PCON-06): all-green → one gate + one combined land bound to the
  gated SHA (`landed_sha` == the gate's returned SHA); one red → bisect ejects exactly the
  offender, the gated remainder lands (SHA-checked); multiple offenders; rebase-conflict →
  pre-gate eject + reform; **wrong-base member → pre-gate eject (same-base precondition)**;
  gate-timeout & door-unreachable → fall back to N=1 front; batch-of-one; all-conflict;
  green-but-combined-land-drift → whole set requeued (all-or-nothing); **deadline threaded to
  stack + gate + merge** (whole op bounded by the slot lease).
- Config (`config::tests`): default 1; honors >1; clamps 0/garbage to 1.
- Wiring / SAFETY (`gitea::tests`): `execute_with_queue_and_regate` —
  `pcon07_batch_request_degrades_to_single_pr_in_production` (BUILD_MERGE_BATCH_MAX=2 +
  `batch_prs` → only the front PR merges via the PCON-06 single path, the other member is
  NEVER touched, and nothing is gated on a combined state); and
  `pcon07_batch_max_one_ignores_batch_prs_and_takes_the_pcon06_single_path` (default 1
  reproduces the exact PCON-06 single-PR success string, byte-for-byte).
