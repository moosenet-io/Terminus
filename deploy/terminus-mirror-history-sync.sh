#!/usr/bin/env bash
#
# terminus-mirror-history-sync.sh — GHIST-08 going-forward mirror sync runner.
#
# For each mirror_ready repo, calls the sanctioned `git_public_history_sync`
# Terminus tool, which:
#   1. appends the new internal commits onto the established, operator-blessed
#      full-history baseline (GHIST-07),
#   2. gates ONLY the newly-appended commits' trees for residual PII, and
#   3. — only when clean — fast-forward-pushes the new tip to the public GitHub
#      mirror (NEVER force; a non-fast-forward or residual PII WITHHOLDS the push).
#
# It calls the tool over the LOCAL MCP endpoint (the single sanctioned door, S9) —
# no repo, no credential, and no Plane/Gitea/GitHub API is touched directly here.
# The credential + author map live only in terminus-primary's runtime env.
#
# This is an OPS artifact: install + enable the .service/.timer on the mirror host
# (<host>) as an operator action. It is version-controlled here so the wiring is
# auditable, not to be run ad hoc from a dev box.
#
# Env:
#   TERMINUS_MCP_URL    MCP endpoint (default http://127.0.0.1:8310/mcp — the local
#                       client daemon that mTLS-proxies to terminus-primary).
#   MIRROR_SYNC_REPOS   space-separated logical repo names to sync
#                       (default: "Chord Terminus Harmony lumina-constellation").
#
set -euo pipefail

MCP_URL="${TERMINUS_MCP_URL:-http://127.0.0.1:8310/mcp}"
REPOS="${MIRROR_SYNC_REPOS:-Chord Terminus Harmony lumina-constellation}"

# Minimal MCP JSON-RPC call: initialize -> notifications/initialized -> tools/call.
# We keep a session-per-repo for simplicity (the runner is low-frequency).
#
# Exit codes are the fail-closed operational signal (a timer-driven oneshot must NOT
# report success when the mirror did not actually advance):
#   0  pushed, or verified already-current (up_to_date)
#   2  WITHHELD — residual PII in unpublished commits (mirror intentionally not updated)
#   3  REFUSED / ERROR — JSON-RPC error, MCP isError, non-fast-forward, un-bootstrapped
#      remote, missing author map, lineage loss, or a transport/parse failure
call_sync() {
  local repo="$1"
  python3 - "$MCP_URL" "$repo" <<'PY'
import json, sys, urllib.request

url, repo = sys.argv[1], sys.argv[2]

def post(payload, sid=None):
    data = json.dumps(payload).encode()
    req = urllib.request.Request(url, data=data, headers={
        "Content-Type": "application/json",
        "Accept": "application/json, text/event-stream",
    })
    if sid:
        req.add_header("Mcp-Session-Id", sid)
    resp = urllib.request.urlopen(req, timeout=600)
    sid = resp.headers.get("Mcp-Session-Id", sid)
    body = resp.read().decode()
    # Handle both plain JSON and SSE-framed responses.
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
    _, sid = post({"jsonrpc":"2.0","id":1,"method":"initialize",
                   "params":{"protocolVersion":"2024-11-05","capabilities":{},
                             "clientInfo":{"name":"mirror-history-sync","version":"1"}}})
    post({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}, sid)
    res, _ = post({"jsonrpc":"2.0","id":2,"method":"tools/call",
                   "params":{"name":"git_public_history_sync","arguments":{"repo":repo}}}, sid)
except Exception as e:  # transport/HTTP failure
    print(f"  {repo}: TRANSPORT ERROR: {e}")
    sys.exit(3)

# JSON-RPC-level error (e.g. the tool returned Err → Conflict/NotConfigured): REFUSED.
if isinstance(res, dict) and res.get("error"):
    print(f"  {repo}: REFUSED (jsonrpc error): {json.dumps(res['error'])}")
    sys.exit(3)

result = res.get("result", {}) if isinstance(res, dict) else {}
# MCP tool-level error flag.
if result.get("isError"):
    print(f"  {repo}: REFUSED (tool isError): {json.dumps(result)[:800]}")
    sys.exit(3)

# Extract the tool's text payload and inspect the sync outcome.
text = ""
for item in result.get("content", []) or []:
    if item.get("type") == "text":
        text += item.get("text", "")
try:
    out = json.loads(text) if text.strip().startswith("{") else {}
except json.JSONDecodeError:
    out = {}

if out.get("withheld"):
    rc = out.get("gate", {}).get("residual_count", "?")
    print(f"  {repo}: WITHHELD — {rc} residual PII in unpublished commits; mirror NOT updated")
    sys.exit(2)
if out.get("pushed"):
    print(f"  {repo}: pushed {out.get('new_commits')} commit(s) → {out.get('work_head')}")
    sys.exit(0)
if out.get("up_to_date"):
    print(f"  {repo}: already current ({out.get('work_head')})")
    sys.exit(0)

# Unrecognised shape — treat as an error rather than a silent success.
print(f"  {repo}: UNEXPECTED result: {text[:800]}")
sys.exit(3)
PY
}

rc=0
withheld=0
for repo in $REPOS; do
  echo "[mirror-history-sync] $repo …"
  # `if` provides a condition context so `set -e` does NOT abort on a non-zero
  # call_sync — every repo is attempted and the exit status is aggregated below.
  if call_sync "$repo"; then
    :                                         # pushed or verified current
  else
    status=$?
    case "$status" in
      2) withheld=1 ;;                        # residual PII — surfaced via exit 2 below
      *) echo "  ERROR syncing $repo" >&2; rc=1 ;;
    esac
  fi
done
# A withheld repo means the mirror is intentionally NOT current until the source is
# remediated — surface it as a non-zero exit so the timer job does not read as clean.
if [ "$rc" -ne 0 ]; then
  exit "$rc"
elif [ "$withheld" -ne 0 ]; then
  exit 2
fi
exit 0
