#!/usr/bin/env bash
# TCLI-06 validation script.
#
# Confirms a running `terminus-client-daemon` (TCLI-05) is actually serving
# the primary's tool catalog before a dev-box `.mcp.json` is cut over to
# point at it. This is the manual-verification procedure the TCLI-06 spec
# item's TEST PLAN calls for ("daemon starts cleanly, tool listing via the
# new path is confirmed ..., representative read-only tool call succeeds
# through the new path") -- it is deliberately a shell script, not a
# `cargo test`, because it exercises a LIVE daemon process + its live mTLS
# session to a real primary, not an in-process mock.
#
# Usage:
#   TERMINUS_CLIENT_LOCAL_PORT=8310 ./validate-daemon.sh
#
# No secrets are read or required by this script -- it only speaks to the
# daemon's already-established local loopback endpoint. It never touches
# `.mcp.json` itself; it is a precondition check for the swap script.
#
# Exit code 0 = daemon healthy + tools/list round-trip succeeded.
# Any nonzero exit = do NOT proceed with the `.mcp.json` swap.

set -euo pipefail

PORT="${TERMINUS_CLIENT_LOCAL_PORT:-8310}"
BASE="http://127.0.0.1:${PORT}"
TOOL_NAME="${TERMINUS_CLIENT_VALIDATE_TOOL:-health}"

fail() {
    echo "FAIL: $1" >&2
    exit 1
}

command -v curl >/dev/null 2>&1 || fail "curl is required"

echo "== TCLI-06 daemon validation against ${BASE} =="

# --- 1. /healthz -------------------------------------------------------
echo "-- checking GET ${BASE}/healthz"
health_body="$(curl -fsS --max-time 5 "${BASE}/healthz")" \
    || fail "GET /healthz did not return 2xx -- is terminus-client-daemon running on port ${PORT}?"
echo "   ${health_body}"
echo "   OK: daemon process is up and its initial mTLS session to the primary was established"
# (per TCLI-05: main() never binds the local listener until the initial
# mTLS handshake to the primary succeeds -- so a 2xx /healthz already
# implies the primary hop is alive at daemon-startup time. It does NOT by
# itself prove the CURRENT tools/list round trip works below.)

# --- 2. tools/list over POST /mcp (SSE-framed JSON-RPC 2.0) -----------
echo "-- checking POST ${BASE}/mcp tools/list"
list_raw="$(curl -fsS --max-time 15 -X POST "${BASE}/mcp" \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' \
    -d '{"jsonrpc":"2.0","id":"tcli06-validate-list","method":"tools/list","params":{}}')" \
    || fail "POST /mcp tools/list request failed"

# Response is SSE-framed: "event: message\ndata: {...}\n\n" -- extract the
# data: line's JSON payload, same framing terminus_rs's own /mcp serves.
list_json="$(printf '%s' "${list_raw}" | grep '^data:' | head -1 | sed 's/^data: *//')"
[ -n "${list_json}" ] || fail "could not find an SSE 'data:' line in the tools/list response: ${list_raw}"

echo "${list_json}" | grep -q '"error"' && fail "tools/list returned a JSON-RPC error: ${list_json}"
echo "${list_json}" | grep -q '"tools"' || fail "tools/list response has no 'tools' field: ${list_json}"

tool_count="$(echo "${list_json}" | grep -o '"name"' | wc -l | tr -d ' ')"
echo "   OK: tools/list returned ${tool_count} tool name occurrences (catalog forwarded from the primary)"
[ "${tool_count}" -gt 0 ] || fail "tools/list returned zero tools -- catalog forward is not actually working"

# --- 3. representative read-only tool call -----------------------------
# Only attempt this if the catalog actually advertises the configured probe
# tool (default: "health") -- don't fail validation just because a
# deployment's catalog uses a different name for its health/echo tool;
# surface that as a warning instead so a human decides.
if echo "${list_json}" | grep -q "\"name\":\"${TOOL_NAME}\""; then
    echo "-- checking POST ${BASE}/mcp tools/call name=${TOOL_NAME}"
    call_raw="$(curl -fsS --max-time 15 -X POST "${BASE}/mcp" \
        -H 'Content-Type: application/json' \
        -H 'Accept: application/json, text/event-stream' \
        -d "{\"jsonrpc\":\"2.0\",\"id\":\"tcli06-validate-call\",\"method\":\"tools/call\",\"params\":{\"name\":\"${TOOL_NAME}\",\"arguments\":{}}}")" \
        || fail "POST /mcp tools/call (${TOOL_NAME}) request failed"
    call_json="$(printf '%s' "${call_raw}" | grep '^data:' | head -1 | sed 's/^data: *//')"
    [ -n "${call_json}" ] || fail "could not find an SSE 'data:' line in the tools/call response: ${call_raw}"
    echo "${call_json}" | grep -q '"isError":true' && fail "tools/call ${TOOL_NAME} returned isError=true: ${call_json}"
    echo "   OK: tools/call ${TOOL_NAME} round-tripped through the daemon to the primary and back"
else
    echo "   WARNING: probe tool '${TOOL_NAME}' not present in the catalog -- skipped the representative"
    echo "            tools/call check. Set TERMINUS_CLIENT_VALIDATE_TOOL to a tool name that IS in the"
    echo "            catalog above and re-run before treating validation as complete."
fi

echo "== validation PASSED: daemon is healthy and serving the primary's catalog =="
