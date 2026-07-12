#!/usr/bin/env bash
#
# terminus-mirror-runner.sh — MRUN-01 mirror-runner (fills the gap the S115
# audit found: TERMINUS_MIRROR_AUTO_APPROVE=true was set, but nothing ever
# actually drove the git-public mirror on a schedule).
#
# Calls the single `git_public_mirror_run` Terminus tool with NO `repo`
# argument, so the tool itself discovers every `mirror_ready` repo under
# TERMINUS_MIRROR_SOURCE_ROOT and runs one idempotent status → backfill+gate →
# fast-forward-sync pass per repo (see src/forge/mirror/runner.rs). This
# script is intentionally thin: it makes ONE tool call, parses the per-repo
# report array, and translates the outcomes into a process exit code — it
# does not itself touch git, PII scanning, or credentials, and it holds no
# secrets (the credential + TERMINUS_MIRROR_AUTHOR_MAP live only in
# terminus-primary's runtime env, resolved inside the tool call).
#
# Calls the tool over the LOCAL MCP endpoint (the single sanctioned door) —
# same convention as deploy/terminus-mirror-history-sync.sh's `call_sync`
# helper. No repo, no credential, and no Plane/Gitea/GitHub API is touched
# directly here.
#
# This is an OPS artifact: install + enable the .service/.timer on the SAME
# host that runs the mirror engine (referred to elsewhere as
# "terminus-primary" — the host holding the mirror work dirs, the git-public
# credential, and TERMINUS_MIRROR_AUTHOR_MAP). It is version-controlled here
# so the wiring is auditable, not to be run ad hoc from a dev box.
#
# IMPORTANT — this runner only MIRRORS; it does not SOURCE-SYNC. The host
# running this script is expected to mount TERMINUS_MIRROR_SOURCE_ROOT (the
# "parking lot" of internal-main checkouts) READ-ONLY. Keeping that parking
# lot current with internal main is `git_public_mirror_sync_source`'s job
# (GHMR-04/MIRR-04), run separately from the dev box that holds the Gitea
# credential — see src/forge/mirror/runner.rs's module doc. If this runner
# reports `commits_behind` staying nonzero (via the underlying tool's
# `up_to_date`/`pushed` never firing), check the source-sync side, not this
# script.
#
# Env:
#   TERMINUS_MCP_URL  MCP endpoint (default http://127.0.0.1:8310/mcp — the
#                     local client daemon that mTLS-proxies to
#                     terminus-primary).
#
# Exit codes (fail-closed — a timer-driven oneshot must NOT report success
# when a repo's mirror did not actually advance and needed to):
#   0  every repo: up_to_date or pushed
#   2  at least one repo: gate_dirty (residual PII WITHHELD the push)
#   3  at least one repo: needs_operator_rebaseline or error (transport
#      failure, JSON-RPC error, un-bootstrapped/diverged remote, missing
#      author map, or no established lineage yet — all require an operator,
#      NEVER auto-resolved by force)
#
set -euo pipefail

MCP_URL="${TERMINUS_MCP_URL:-http://127.0.0.1:8310/mcp}"

python3 - "$MCP_URL" <<'PY'
import json, sys, urllib.request

url = sys.argv[1]

def post(payload, sid=None):
    data = json.dumps(payload).encode()
    req = urllib.request.Request(url, data=data, headers={
        "Content-Type": "application/json",
        "Accept": "application/json, text/event-stream",
    })
    if sid:
        req.add_header("Mcp-Session-Id", sid)
    resp = urllib.request.urlopen(req, timeout=1800)
    sid = resp.headers.get("Mcp-Session-Id", sid)
    body = resp.read().decode()
    for line in body.splitlines():
        line = line.strip()
        if line.startswith("data:"):
            line = line[5:].strip()
        if line.startswith("{"):
            try:
                return json.loads(line), sid
            except json.JSONDecodeError:
                continue
    return (json.loads(body) if body.strip().startswith("{") else {}), sid

try:
    _, sid = post({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                   "params": {"protocolVersion": "2024-11-05", "capabilities": {},
                              "clientInfo": {"name": "mirror-runner", "version": "1"}}})
    post({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}}, sid)
    res, _ = post({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                   "params": {"name": "git_public_mirror_run", "arguments": {}}}, sid)
except Exception as e:  # transport/HTTP failure
    print(f"[mirror-runner] TRANSPORT ERROR: {e}")
    sys.exit(3)

if isinstance(res, dict) and res.get("error"):
    print(f"[mirror-runner] REFUSED (jsonrpc error): {json.dumps(res['error'])}")
    sys.exit(3)

result = res.get("result", {}) if isinstance(res, dict) else {}
if result.get("isError"):
    print(f"[mirror-runner] REFUSED (tool isError): {json.dumps(result)[:1200]}")
    sys.exit(3)

text = ""
for item in result.get("content", []) or []:
    if item.get("type") == "text":
        text += item.get("text", "")
try:
    out = json.loads(text) if text.strip().startswith("{") else {}
except json.JSONDecodeError:
    out = {}

reports = out.get("reports", [])
if not reports:
    print(f"[mirror-runner] no reports returned: {text[:800]}")
    sys.exit(3)

exit_code = 0
for r in reports:
    repo = r.get("repo", "?")
    outcome = r.get("outcome", "?")
    if outcome == "up_to_date":
        print(f"  {repo}: up to date")
    elif outcome == "pushed":
        print(f"  {repo}: pushed {r.get('from')} -> {r.get('to')}")
    elif outcome == "gate_dirty":
        print(f"  {repo}: GATE DIRTY — {r.get('residual_count')} residual PII spot(s); WITHHELD")
        exit_code = max(exit_code, 2)
    elif outcome == "needs_operator_rebaseline":
        print(f"  {repo}: NEEDS OPERATOR REBASELINE — {r.get('reason')}")
        exit_code = max(exit_code, 3)
    elif outcome == "error":
        print(f"  {repo}: ERROR — {r.get('message')}")
        exit_code = max(exit_code, 3)
    else:
        print(f"  {repo}: UNRECOGNISED outcome: {r}")
        exit_code = max(exit_code, 3)

sys.exit(exit_code)
PY
