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
`ReGate` + `MergeLockStore` so the delicate correctness is unit-tested deterministically with
fakes (no live Redis / forge). A **future** production `SpeculativeBatchOps` (once a
combined-state gate primitive exists) will wire to the SAME sanctioned PCON-06 helpers (single
door, S9 — no raw API calls), and CRITICALLY must gate the ONE combined SHA that lands, not
each member independently:

| Batch op | PCON-06 helper (future wiring) |
|---|---|
| `stack` (build the combined branch) | a combined-branch primitive (not yet available) atop `GiteaClient::update_pull_branch` |
| `gate` (batch gate) | `merge_queue::ReGate` → `compiler_build` (`mode=test`) on the **combined** SHA |
| `merge` (land, SHA-bound) | `GiteaClient::merge_pull_with_base` (`head_commit_id` + base/head/mergeable recheck) |

## Algorithm

`run_speculative_batch(ops, prs, budget)` where `prs` is already capped to
`BUILD_MERGE_BATCH_MAX` with the front PR first:

1. **Stack** the batch (`ops.stack`). A PR that **conflicts** during the speculative rebase
   is ejected **before** the gate (`BatchEjectReason::RebaseConflict`) and the batch reforms
   without it. An already-mergeable PR is captured as-is (no redundant rebase).
2. If nothing stacked cleanly → done (everything was ejected pre-gate).
3. **Gate once** on the stacked set:
   - **Green** → land all in order (SHA-bound). A land that **drifts** (head/base moved
     between gate and merge) is requeued into `merge_failures`, and — because later members
     were stacked on it — every survivor after it is requeued too.
   - **TimedOut / Unreachable** → fail-safe: **fall back to N=1 for the front PR**
     (`fell_back_to_single = Some(front)`). The caller runs the PCON-06 single-PR path for
     the front PR (which re-gates it fresh); the rest stay queued.
   - **Red** → **bisect**.

### Bisect-on-red

`bisect_red(prefix_confirmed_green, batch)` — gate `prefix + batch` (skipping the gate when
the verdict for exactly that set is already known — the top-level red is threaded in to
avoid re-gating it):

- **Green** → the whole `prefix + batch` survives (it was just gated green).
- **TimedOut / Unreachable** → fail-safe: eject ALL of `batch`
  (`BatchEjectReason::GateUnavailable`); `prefix` (already green) survives — never merge
  unproven.
- **Red**, `batch.len() == 1` → the lone PR IS the offender (it turned the already-green
  prefix red): eject it (`BatchEjectReason::RedGate`); `prefix` survives.
- **Red**, `batch.len() > 1` → split in half; recurse **left** (on `prefix`), then **right**
  (on the left's survivors) so each half is gated stacked on the confirmed prefix.

**Correctness invariant (what lands is what was gated):** by construction, the final
surviving set the bisection returns was gated **GREEN as one unit** at the deepest
establishing step — never an ad-hoc union of PRs only ever gated apart. Each individual land
then goes through the PCON-06 SHA-bound merge (`head_commit_id` + base recheck), so a PR that
drifted between the batch gate and its land is requeued rather than merged untested. `main`
stays green per landed PR.

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
| `merge_failures` | a green batch member drifted at land time | requeued (bound-merge refused); every later member requeued too |

Every ejected/failed PR bounces with a clear, distinct reason and is requeued; every merged
PR was part of the exact set gated green. `BUILD_MERGE_BATCH_MAX=1` (default) never enters
this layer.

## Known gap vs. the spec (flagged, not worked around)

The spec's APPROACH says "stack the front N PRs **of a base's queue**". Two facts about the
current code make the *live* "front-N-from-the-queue" discovery unavailable, so it is
**caller-supplied** instead:

1. **The queue stores tickets, not PRs.** `MergeQueue`'s wait ZSET holds opaque FIFO tickets;
   there is no ticket→PR association, so the queue cannot enumerate "the front N PRs" itself.
   A ticket→PR registry would be its own item.
2. **No combined-branch forge primitive.** Gitea's `update-branch` endpoint only merges a
   PR's *base* into its head — it cannot stack PR-B onto PR-A onto `main`. So the speculative
   gate tests each member rebased onto `main` **independently**, not a true combined stack,
   and a sequential land advances the base so later members' captured base SHA no longer
   matches at their recheck → they requeue (never merge untested). Each **landed** PR keeps
   the full PCON-06 per-PR SHA-bound guarantee (`main` stays green), but single-gate
   combined-stack **throughput** awaits a forge combined-branch primitive (or a local-git
   stack builder behind the single door).

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

- Algorithm (`gitea::merge_queue::tests`, fake `SpeculativeBatchOps`): all-green→one gate/N
  merges; one red→bisect ejects exactly the offender, remainder merges; multiple offenders;
  rebase-conflict→pre-gate eject + reform; gate-timeout & door-unreachable→fall back to N=1
  front; batch-of-one; all-conflict; green-but-land-drift requeues the drifter + every later
  member.
- Config (`config::tests`): default 1; honors >1; clamps 0/garbage to 1.
- Wiring / SAFETY (`gitea::tests`): `execute_with_queue_and_regate` —
  `pcon07_batch_request_degrades_to_single_pr_in_production` (BUILD_MERGE_BATCH_MAX=2 +
  `batch_prs` → only the front PR merges via the PCON-06 single path, the other member is
  NEVER touched, and nothing is gated on a combined state); and
  `pcon07_batch_max_one_ignores_batch_prs_and_takes_the_pcon06_single_path` (default 1
  reproduces the exact PCON-06 single-PR success string, byte-for-byte).
