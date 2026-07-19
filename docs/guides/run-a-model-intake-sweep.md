# Run a model-intake sweep

Profile fleet models with the unified `mint` CLI. `mint` is a front door over
the same library entry points as the standalone `intake_coder_sweep`,
`intake_assistant_sweep`, `intake_coder_case`, and `intake_coder_gaps` binaries
— either interface works; this guide uses `mint`.

## Prerequisites

- A built workspace (`cargo build --release`).
- `INTAKE_DATABASE_URL` (or the shared `DATABASE_URL`) pointing at the intake
  Postgres — results and derived operational profiles are stored there.
- The serving backend (Ollama-compatible `/api/generate`) reachable from the
  host running the sweep.
- For the assistant suite's judge panel: the judge CLIs (`claude`, `gemini`,
  `codex`) on `PATH` in a logged-in operator shell, or overridden per judge via
  `JUDGE_<ID>_CLI` (leave a judge's command empty to disable it).

## Steps

1. **Check GPU authority.** Sweeps respect a single-hot-model VRAM policy and a
   GPU-authority lock so they never fight production serving:

   ```sh
   mint gpu status
   ```

   If another consumer holds the GPU and you are authorized to take it:

   ```sh
   mint gpu acquire      # claim exclusive use; hand back with `mint gpu release`
   ```

2. **Run the sweep.**

   ```sh
   mint sweep coder          # fleet-level coder profiling sweep
   mint sweep assistant      # fleet-level assistant profiling sweep
   ```

   The coder sweep accepts language and case-limit filters (`mint sweep coder
   --help` lists them). If the target model isn't local yet:

   ```sh
   mint fetch-model --model <registry-key>    # or set INTAKE_CASE_MODEL
   ```

3. **Rerun or audit specific cases.**

   ```sh
   mint case --model <registry-key>    # ad hoc single/multi-case rerun
   mint gaps --model <registry-key>    # audit which case ids a model is missing
   ```

4. **Read the results.** From any MCP client connected to the hub, the stored
   profiles are queryable via the intake tools: `model_intake_status` (one
   model's operational profile — safe/absolute context, degradation point,
   recommended timeouts), `model_intake_compare` (cross-model table for one
   metric), and `model_intake_job_status` (durable fleet-job progress). Fleet
   runs can also be launched tool-side via `model_intake_fleet`.

5. **Release the GPU** if you acquired it: `mint gpu release`.

## Expected outcome

Each profiled model has a row-set in the intake DB and a derived operational
profile; `model_intake_status` returns it, and downstream model routing can
consume it.

## Troubleshooting

A sweep that dies mid-run is resumable — jobs are durable and the supervisor is
restart-resilient (see
[docs/tools/mint/restart-resilience.md](../tools/mint/restart-resilience.md)).
If a case times out on a cold model, the reload-adjusted timeout policy
(`intake::timeouts::reload_adjusted_timeout_secs`) is usually the knob to
inspect before blaming the model. Full CLI detail:
[docs/tools/mint/cli.md](../tools/mint/cli.md).
