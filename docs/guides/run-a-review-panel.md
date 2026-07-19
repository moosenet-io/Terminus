# Run a review panel

Dispatch one review to several providers concurrently with the `review_run`
tool and get a single aggregated verdict.

## Prerequisites

- A running Terminus server (see [Getting Started](../getting-started.md)).
- For the CLI-backed providers (`opus`, `codex`, `agy`): the `review_daemon`
  binary running on the same host, with the provider CLIs installed and
  logged in.
- For the OpenRouter providers (`nemotron`, `qwen_coder`): `OPENROUTER_API_KEY`
  set in the server's environment.

## Steps

1. **Start the review daemon.** It refuses to start without a bearer token
   (fail-closed):

   ```sh
   REVIEW_DAEMON_TOKEN=<from-vault> REVIEW_DAEMON_PORT=8790 \
     cargo run --release --bin review_daemon
   ```

   The daemon binds loopback only, validates the provider against a closed
   enum before any spawn, uses argv arrays (never a shell), and runs children
   with a sanitized environment. Providers whose binaries weren't found at
   startup report `binary_not_found` per request.

2. **Point the server at it.** In the Terminus process environment:
   `REVIEW_DAEMON_URL` (default matches the daemon's loopback default) and the
   same `REVIEW_DAEMON_TOKEN`. Unset token → daemon providers degrade to
   `"unavailable: REVIEW_DAEMON_TOKEN not configured"` rather than erroring the
   call.

3. **Check provider capacity first.** Call `review_provider_status` — it
   reports the per-provider cooldown/shelve state (rolling API quotas vs
   fixed-cliff subscription quotas), so you can pick a panel that will actually
   answer.

4. **Dispatch the review.** Call `review_run` with your prompt/diff context, a
   provider list, and a structure — one of `single`, `adversarial_pair`,
   `panel_majority`, `panel_unanimous`. Example shape (any MCP client):

   ```json
   {"name": "review_run",
    "arguments": {"providers": ["codex", "nemotron", "qwen_coder"],
                  "structure": "panel_majority",
                  "prompt": "<review request + diff>"}}
   ```

5. **Read the aggregate.** The result carries each provider's parsed verdict
   and findings, a merged outcome with disagreement flagged
   (`review::consistency::merge_and_flag_disagreement`), and a `complete` flag
   that is false if any requested provider degraded to unavailable.

## Expected outcome

One structured verdict with per-provider detail; a single provider's timeout,
quota exhaustion, or auth failure degrades that provider only.

## Troubleshooting

If a non-daemon provider returns REQUEST_CHANGES describing code it could not
have seen, check whether it actually received the diff — in some flows only
daemon providers get the inline diff, and a blind dissent should not deadlock a
merge (see [reference/review.md](../reference/review.md), "Notes and gaps").
