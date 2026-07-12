# `git_public_mirror_run` — the mirror-runner (MRUN-01)

Closes an S115 audit finding: `TERMINUS_MIRROR_AUTO_APPROVE=true` was set on
the git-public mirror engine (`src/forge/mirror/`), but nothing ever *drove*
it on a schedule — no runner, no timer. A repo's public mirror could quietly
stop advancing (diverge, or just fall behind) until an operator happened to
notice by hand.

`src/forge/mirror/runner.rs` is the fix: a single idempotent "run once" pass
per repo, `run_once`, wrapped as the core tool `git_public_mirror_run` and
driven by `deploy/terminus-mirror-runner.{service,timer}`.

## What it does — orchestration only

`run_once` calls, in order, the SAME three tools an operator would call by
hand — nothing about git, PII scanning, or transport is reimplemented:

1. **`git_public_history_status`** (read-only). No established lineage yet →
   `needs_operator_rebaseline` (a first baseline is always an operator
   action — see below). Its `commits_behind` compares the SOURCE checkout to
   the LOCAL history work dir only — it does **not** inspect the public
   remote — so it is used ONLY to decide whether a backfill is needed, never
   to conclude the mirror is current. (A prior tick may have replayed but
   failed to push, leaving `commits_behind == 0` while the public mirror is
   still behind; trusting status here would let the mirror sit behind forever
   and defeat the runner.)
2. **`ensure_push_boundary`** — pins the going-forward `pushed-head` boundary
   from the pre-backfill baseline (see the ff-vs-force section) and confirms
   remote lineage; divergence / un-bootstrapped remote →
   `needs_operator_rebaseline` before anything is replayed or pushed.
3. **`git_public_history_backfill`** — replays new internal commits into the
   scrubbed full-history work dir and gates every commit's tree. Run only when
   the local mirror is actually behind source; NEVER pushes. Residual PII →
   `gate_dirty` (with the `commit:file:line` spots), nothing published.
4. **`git_public_history_sync`** — the REMOTE-aware step: fast-forward-only
   push onto the established, operator-blessed baseline. `up_to_date` is
   returned ONLY here, when sync confirms the remote is genuinely at head; a
   remote that is behind (including self-healing a prior failed push) is
   fast-forward-pushed → `pushed`; a refusal (diverged / un-bootstrapped /
   non-fast-forward — surfaced as `ToolError::Conflict`) →
   `needs_operator_rebaseline`; residual PII in the sync's own incremental
   gate → `gate_dirty`. Because up-to-date is decided from the remote, a
   behind/failed-push mirror is retried and pushed on the next tick
   (idempotent self-heal).

`git_public_mirror_run` wraps `run_once` per repo. With an explicit `repo`
arg it runs one pass; with none, it discovers every `mirror_ready` repo by
scanning `.moosenet-pipeline.yaml` (`mirror_ready: true`) under
`TERMINUS_MIRROR_SOURCE_ROOT` and runs a pass for each, returning one report
per repo.

## Fast-forward vs. force — never forces

The runner has **no code path that force-pushes**, and no path ever calls
one. It only ever calls the fast-forward-only `git_public_history_sync`
(itself force-guarded — see `assert_never_force` in
`src/forge/mirror/workdir.rs` / `src/forge/mirror/tools.rs`) and never
retries with a different tool or flag. The ff-vs-force decision is made
entirely *inside* `git_public_history_sync`, from a merge-base ancestry
check against the public remote's current tip (`ff_state` in
`src/forge/mirror/tools.rs`) — the runner just classifies that tool's
response:

- Success with `pushed: true` → a clean fast-forward happened.
- `Err(ToolError::Conflict(..))` (non-fast-forward, diverged, or an
  un-bootstrapped remote) → `needs_operator_rebaseline`. The runner reports
  this and stops; it never attempts a `--force`/`-f`/`--force-with-lease`
  push itself, and never will — the one sanctioned force is the operator's
  one-time GHIST-07 bootstrap re-baseline, performed by a human, outside
  this module, using a separate operator-blessed flow.

## The parking-lot source-sync is a SEPARATE, dev-box-side prerequisite

The host that runs this runner — wherever the mirror engine's work dirs and
git-public credential live (referred to elsewhere as "terminus-primary") —
is expected to mount `TERMINUS_MIRROR_SOURCE_ROOT` (the "parking lot" of
internal-`main` checkouts, one per repo) **READ-ONLY**. Keeping that parking
lot current with internal `main` (`git fetch` + `checkout` + `reset --hard
origin/<branch>`) is a *different* tool's job —
`git_public_mirror_sync_source` (GHMR-04 / MIRR-04) — run from the dev box
that actually holds the Gitea credential, because the mirror-engine host
cannot write there.

`run_once` assumes the parking lot is already current for `repo` and only
mirrors what it finds. If a repo's `commits_behind` stays nonzero across
runs, the fix is on the source-sync side (is `git_public_mirror_sync_source`
actually running on a cadence for that repo?), not in this runner.

## The one-time bootstrap is still manual (GHIST-07)

A repo with no established full-history lineage — `lineage_established:
false` from `git_public_history_status` — can never be auto-onboarded by
this runner. Establishing the FIRST baseline is `git_public_history_backfill`
+ an operator spot-check + the one-time force re-baseline (GHIST-07),
performed deliberately once per repo. Only after that exists does
`git_public_history_sync` (and therefore this runner) have anything to
fast-forward.

## Deploy

- `deploy/terminus-mirror-runner.service` — oneshot, calls
  `deploy/terminus-mirror-runner.sh`, which makes one `git_public_mirror_run`
  MCP tool call (no `repo` arg — every `mirror_ready` repo) over the local
  sanctioned MCP door and translates the per-repo outcome array into a
  fail-closed exit code (0 = all `up_to_date`/`pushed`, 2 = at least one
  `gate_dirty`, 3 = at least one `needs_operator_rebaseline`/`error`).
- `deploy/terminus-mirror-runner.timer` — every 30 minutes, jittered. Each
  no-op run (repo already current) is cheap: status plus a single
  read-only remote check via sync, no replay when nothing is behind.

This complements, and does not replace, the pre-existing
`deploy/terminus-mirror-history-sync.{service,timer}` (GHIST-08), which
calls `git_public_history_sync` directly for a fixed, hand-maintained repo
list and skips the backfill+gate step. `git_public_mirror_run` is the fuller
orchestration (status → backfill+gate → sync in one call, with automatic
`mirror_ready` discovery) and is the one intended to actually be enabled on
a cadence going forward; the GHIST-08 units may be retired in a follow-up
once `terminus-mirror-runner.timer` is confirmed running in production.
