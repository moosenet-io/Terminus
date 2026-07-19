# Run the git-public mirror

Maintain a PII-swept public mirror of an internal repository's `main` using the
forge mirror engine. The mirror is a derivative tree with its own linear git
history — never a force-push of internal history.

## How it works (30 seconds)

Per `mirror_ready` repo, the engine keeps a clean work dir: it syncs internal
`main`'s tree in, runs the mechanical PII sweep (private IPs, container ids,
internal paths/URLs, org terms → placeholder tokens), commits the swept state,
and only when the authoritative gate (`github::pii` — the same engine as the
git-hook `pii_gate`) reports **zero residual violations** does the state become
tag-able (`mirror-approved/<internal-sha>`) and pushable. Residuals that the
mechanical sweep can't fix go through a bounded (≤3 rounds) cleaning pass, then
escalate to the operator with exact `file:line` spots.

## Steps

1. **Check state.** Call `git_public_mirror_status` for the repo — it reports
   work-dir freshness, gate state, and whether the public remote is behind.

2. **Run a mirror pass.** The single idempotent orchestration
   (status → backfill + gate → fast-forward sync/push):

   ```json
   {"name": "git_public_mirror_run", "arguments": {"repo": "<repo-name>"}}
   ```

   In deployment this is driven on a schedule by
   `deploy/terminus-mirror-runner.timer` — the manual call is the same code
   path.

3. **Handle residuals.** If the pass reports residual violations, either let
   the bounded cleaning pass finish (`git_public_mirror_prepare` runs it) or
   remediate the listed `file:line` spots yourself, then re-run. Approval and
   push are explicit steps when not auto-enabled: `git_public_mirror_approve`,
   `git_public_mirror_push`. `git_public_mirror_replay_pr` mirrors an
   individual PR's shape; `git_public_mirror_sync_source` refreshes the
   work dir from internal `main`.

4. **Verify independently.** The same gate is runnable as a CLI over any tree:

   ```sh
   cargo run --release --bin pii_gate -- --tree <path-to-workdir> --json
   ```

   Zero findings here means the same thing the mirror gate means — it is the
   same engine. (The binary also serves as the repo's pre-push/pre-commit hook:
   default mode reads the git pre-push protocol on stdin; `--staged` scans the
   index.)

## Expected outcome

The public mirror advances by fast-forward with a PII-clean tree, sharing
ancestry with previous mirror commits but not with internal history.

## Troubleshooting

Repo-specific terms, extra patterns, and allowlisted strings live in the
repo-root `pii-gate.toml` (or the path in `TERMINUS_PII_CONFIG`) — a recurring
false positive belongs there, not in a bypass (there is no bypass flag; the
gate cannot be disabled). Mirror behavior knobs: `TERMINUS_MIRROR_AUTO_APPROVE`,
`TERMINUS_MIRROR_AUTO_BASELINE`, `TERMINUS_MIRROR_BLACKLIST`,
`TERMINUS_MIRROR_AUTHOR_MAP`, `TERMINUS_MIRROR_CLEAN_CMD`,
`TERMINUS_MIRROR_GITHUB_HOST`. Deeper detail:
[docs/tools/code-git/mirror-runner.md](../tools/code-git/mirror-runner.md).
