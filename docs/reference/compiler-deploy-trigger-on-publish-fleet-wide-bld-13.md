## `compiler_deploy` — trigger-on-publish, fleet-wide (BLD-13)

`compiler_deploy` (`compiler/deploy.rs`) closes the CI/CD loop: after a successful
publish/promote moves the store's `current` sha, it **triggers the constellation-updater on
the fleet** so the change lands in **seconds** instead of waiting for the nightly timer (which
stays the catch-all). It is the *write*-side counterpart to `compiler_status`'s read-side
matrix — both fan out over the **same configured deploy hosts** and the **same existing
host-reach path**.

```
compiler_deploy(module, channel="stable", hosts="all")
```

- **What it does:** for each selected deploy host it fires the fetch-mode
  `constellation-update@<module>` systemd unit (BLD-12) over ssh — `systemctl start <unit>`,
  which blocks until the updater's whole `fetch → sha-verify → backup → atomic-mv → restart →
  health-gate → rollback → marker` flow finishes — then reads back the systemd `Result` and
  the updater's optional outcome-token file to classify a **per-host outcome**. It reuses the
  **single shared sanctioned reach** (`status::sanctioned_ssh_argv`) that `compiler_status`
  uses — one authoritative non-mutating ssh option set, no duplicate/drifting definition.
- **Division of responsibility:** the compiler **only triggers**. The updater still **owns the
  swap safety** (health-gate + rollback). `compiler_deploy` never touches a binary, symlink, or
  health check — it fires the unit and reports what the updater reports.
- **Per-host outcome** (unreachable / rollback / timeout are **reported, never masked**):
  `deployed` (swapped, health-gate passed), `skipped` (already on `current`, a no-op),
  `rolled_back` (swapped, health-gate failed, rolled back to backup), `failed` (the updater
  errored **or the `systemctl start` itself failed** — a non-zero start rc is never masked by a
  stale success `Result`/marker token), `timed_out` (the host was **reached** and the updater
  triggered, but the synchronous run exceeded the budget — an in-flight/hung deploy, surfaced
  **distinctly from** `unreachable`), `unknown` (the outcome can't be trusted as a success — the
  updater wrote an unrecognized token, OR the wrapper's exit code couldn't be parsed, so a
  stale/damaged `result=success` line is not trusted without a real `rc == 0`), or `unreachable`
  (an ssh-level
  **connect/auth** failure, never a run timeout). One bad host **never aborts** the fan-out — the
  others still proceed and the nightly timer catches the straggler.
- **No-masked-failures / no-raw-echo hardening:** the trigger forces **non-interactive sudo**
  (`sudo -n`) so a password prompt fails fast instead of hanging the whole per-host budget; the
  (only `-n` is permitted before `systemctl`, never an argument-taking sudo flag like `-u`/`-h`
  that could make sudo read `systemctl` as a username); the outcome marker is **truly run-scoped
  via an rm-succeeded gate** — at run start the wrapper `rm`s the marker and captures whether it is
  now **provably absent** (`[ -e marker ]` is false); the marker is trusted only when that clear
  succeeded, so any marker present afterward was written by THIS run. If a pre-existing marker could
  NOT be removed (a root-owned marker), it is **not trusted at all** (degrade to `Result`+`rc`) —
  no second-granularity mtime window; the marker token is **sanitized against sentinel spoofing**
  (`head -n1` + `tr -cd 'A-Za-z0-9_-'`), so a marker containing a newline + a forged
  `COMPILER_DEPLOY … token=deployed` line can neither inject a second sentinel line nor smuggle
  metacharacters (the Rust parser also refuses to trust a stream carrying more than one sentinel
  line); a **trusted non-success marker is authoritative over the exit code** — a `rolled_back`
  marker is reported as `rolled_back` and a `failed` marker as `failed` EVEN with a non-zero
  `systemctl start` rc (a rollback legitimately exits non-zero), so a rollback is never masked
  into a generic `failed`; the **rc gate applies only to SUCCESS outcomes** — a `deployed`/`skipped`
  marker is trusted only with a real `rc == 0`, else it degrades to `unknown` (success is never
  trusted without a clean exit); an **absent marker** classifies from the systemd `Result` **AND**
  `rc` — a non-zero rc → `failed`; `rc == 0` + `Result=success` → `deployed`; `rc == 0` + a
  non-success `Result` → `failed`; `rc == 0` + an indeterminate `Result` → `unknown` (exit code
  alone is not enough); the **outer wall-clock timeout is strictly
  greater than the ssh connect budget**, so a connect/auth hang surfaces as `unreachable` (never
  `timed_out`); the per-host `detail` is **fixed-vocabulary only** (`outcome=… rc=…`) — the raw
  updater marker token is **never echoed** into structured output; an **unknown requested host** is
  reported by **count only** (never reflecting arbitrary caller input / ssh targets back); and
  `COMPILER_DEPLOY_SYSTEMCTL` is a **constrained, executable-prefix-only** command whose
  **executable must be `systemctl`** with **NO trailing token at all** — no verb AND no flag. The
  trigger owns `start <unit>` and must stay **synchronous** (it classifies this run's authoritative
  outcome only after the BLD-12 unit finishes), so a trailing flag that changes blocking/result
  semantics — notably `--no-block`, which returns before the updater writes its marker — is
  rejected along with any verb; bare tokens only, shell metacharacters rejected — not arbitrary
  shell. A malformed `COMPILER_DEPLOY_SYSTEMCTL` is an
  **operator-config** failure, not a caller error: it surfaces **in the aggregate report** (every
  chosen host `failed` + a config-error note naming the problem, no raw value echoed) — identical
  for the direct tool and the auto-promote hook — rather than aborting with a bare error that
  would drop the per-host report. (Genuinely caller-supplied bad args — `module`/`channel`/`hosts`
  — remain a clean `InvalidArgument`.)
- **Aggregation:** the result carries every host's `{host, outcome, detail}` plus `counts`
  (`deployed`/`skipped`/`rolled_back`/`failed`/`timed_out`/`unknown`/`unreachable`/`total`), a
  `degraded` flag and
  `stragglers` count (the hosts that did not converge), and `notes`. A **partial fleet** result
  is surfaced as `degraded=true` with a note that the nightly timer remains the catch-all.
- **Host reach (S7):** the trigger uses the *same* non-mutating BatchMode ssh reach as BLD-08
  (`StrictHostKeyChecking=no` + `UserKnownHostsFile=/dev/null`, no `known_hosts` mutation) and
  authenticates with the **ambient ssh key** of the sanctioned reach path — it reads **no**
  token/key/password from the environment, so there is nothing secret-shaped to route through
  the vault here. The remote wrapper always `exit 0`s, so ssh's own exit reflects **only
  connectivity** (that is what distinguishes `unreachable` from a failed deploy) — the same
  tri-state trick `compiler_status` uses.

**Auto-trigger after promote (optional, non-blocking).** When `COMPILER_AUTO_DEPLOY` is truthy
(`1`/`true`/`yes`/`on`), a successful `compiler_release` **promote** that actually flips
`current` (not a no-op) auto-fires `compiler_deploy(module, to_channel)`. It is **best-effort and
never holds the promote response hostage**: the fleet fan-out runs on a **background task** awaited
only up to a small budget (`COMPILER_AUTO_DEPLOY_INLINE_BUDGET_SECS`, default 10s). If it finishes
within the budget, the per-host report is **attached** to the promote result under `auto_deploy`;
otherwise the promote returns **promptly** with a `{kicked_off, awaited:false}` note under
`auto_deploy` and the deploy **continues detached** (query `compiler_status` / `compiler_deploy`
for results). So a long/6h fleet deploy can never make a successful promote appear hung, and the
promote's own success/latency is independent of the deploy outcome. The **manual `compiler_deploy`
tool call stays fully synchronous** (that is its contract) — only the auto-after-promote path is
budgeted/detached. Left unset, `compiler_deploy` is simply exposed as a tool for the GUI / manual
use.

Config (all optional, no infra literals — S1): `COMPILER_DEPLOY_HOSTS` (shared with
`compiler_status`; `;`-separated `label|ssh_target`), `COMPILER_DEPLOY_UNIT_TEMPLATE` (default
`constellation-update@{module}.service`; `{module}`/`{channel}` substituted),
`COMPILER_DEPLOY_SYSTEMCTL` (default `systemctl`; an **executable-prefix-only** command — bare
tokens `[A-Za-z0-9._/-]` whose **executable must be `systemctl`** after an optional leading `sudo`
followed by ONLY the non-interactive flag `-n` (no other/argument-taking sudo flag), with **NO
trailing token — no verb and no flag** (the trigger supplies `start <unit>` and must stay
synchronous; a flag like `--no-block` would return before the updater finishes and is rejected).
The accepted set is exactly `systemctl`, `/usr/bin/systemctl`, `sudo systemctl`, `sudo -n
systemctl`, `sudo -n /usr/bin/systemctl`; shell metacharacters, a non-systemctl executable, and
any trailing token are rejected. A `sudo` prefix is auto-forced non-interactive with
`-n`), `COMPILER_DEPLOY_RESULT_MARKER_TEMPLATE` (default
`/opt/{module}/.deploy_result` — the updater's outcome-token file, trusted only when the wrapper's
pre-trigger `rm` cleared it (so any marker present after is this run's; a marker that could not be
removed is not trusted), its first line sanitized to `[A-Za-z0-9_-]`; absent/uncleared, the outcome
degrades to the systemd `Result` + exit code, and `deployed` requires `rc == 0` AND
`Result=success`),
`COMPILER_DEPLOY_TRIGGER_TIMEOUT_SECS` (default 300 — the post-connect RUN budget; larger than the
BLD-08 marker read since the trigger runs the updater synchronously; **clamped** to a 6-hour max so
a huge value can't overflow),
`COMPILER_DEPLOY_CONNECT_TIMEOUT_SECS` (default 10 — the ssh `ConnectTimeout`; also clamped; the
outer wall-clock is `connect + run + 1s` via **saturating** arithmetic, strictly greater, so a
connect hang is `unreachable` not `timed_out` and no combination can overflow/panic),
`COMPILER_DEPLOY_MAX_CONCURRENCY` (default 4 — the effective worker count is **bounded to
`min(configured, number-of-selected-hosts, 64)`**, so a huge/malformed value or an empty host list
can never spawn an absurd number of workers), `COMPILER_AUTO_DEPLOY`, and
`COMPILER_AUTO_DEPLOY_INLINE_BUDGET_SECS` (default 10 — the small budget the auto-after-promote
deploy is awaited inline before the promote returns and the deploy continues detached; clamped).
Every one of these is robust to malformed config — a `0`/unparseable/absent value falls back to its
safe default.

