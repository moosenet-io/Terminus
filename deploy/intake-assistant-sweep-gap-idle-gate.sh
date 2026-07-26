#!/usr/bin/env bash
#
# intake-assistant-sweep-gap-idle-gate.sh — TERM-c590e045
#
# Idle-gated wrapper for the GAP-ONLY assistant profiling sweep. Driven by
# intake-assistant-sweep-gap.service/.timer at 02:17 off-peak. It:
#
#   1. Probes fleet idleness: GET $CHORD_CONTROL_URL/admin/activity, and only
#      proceeds if idle_secs >= $SWEEP_IDLE_THRESHOLD_SECS AND the shared
#      gpu-authority lock (/run/gpu-authority.lock) is free. If busy → log +
#      exit 0 (retry next window). It NEVER forces a drain.
#   2. Drains Chord / frees the GPU: POST $CHORD_CONTROL_URL/admin/idle.
#   3. Runs the gap-only sweep under `timeout $SWEEP_MAX_LEASE_SECS` with
#      INTAKE_ASSISTANT_GAP_ONLY=1 INTAKE_ASSISTANT_GAP_MAX=$INTAKE_ASSISTANT_GAP_MAX.
#   4. ALWAYS restores Chord: POST $CHORD_CONTROL_URL/admin/activate in an EXIT
#      trap, so a failed/killed/timed-out sweep can NEVER leave Chord drained.
#
# ── Auth (verified against src/compiler/idle_lease.rs + src/federation/mod.rs) ──
# Chord's /admin/idle and /admin/activate handlers gate on
# `auth_check(&headers, &state.jwt_secret)` — 401 without a valid JWT. So this
# wrapper mints the SAME short-lived HS256 service JWT terminus-primary already
# mints for Chord's other protected routes: claims {"sub":"lumina","exp":now+120},
# alg HS256, signed with $TERMINUS_PRIMARY_CHORD_JWT_SECRET (the same secret value
# Chord validates as CHORD_JWT_SECRET). GET /admin/activity is read-only; it is
# probed WITHOUT auth first and retried WITH the Bearer if it 401s, so this works
# whether or not the read path is also gated.
#
# ── Secrets discipline (S1/S7) ──
# TERMINUS_PRIMARY_CHORD_JWT_SECRET is referenced BY NAME only and read from this
# host's runtime env (<secret-manager>-materialized at deploy time). No secret, host, IP,
# port, or DSN is baked in — CHORD_CONTROL_URL comes from the env, the sweep talks
# to its own intake DB via the binary's existing pool/config. This script holds no
# credentials of its own.
#
# ── DESIGN ARTIFACT — do NOT run from a dev box; the orchestrator installs the
#    unit files as an OPS action on the GPU host. ──
#
# Requires: bash, curl, openssl, jq (or falls back to a grep/sed idle_secs parse
# if jq is absent). Exit 0 on: swept OK, or skipped-because-busy (both are
# "nothing wrong"). Non-zero only if the sweep itself failed to complete.
set -euo pipefail

log() { printf '%s intake-gap-gate: %s\n' "$(date -Is)" "$*" >&2; }

# ── Config (env, safe defaults; S1) ───────────────────────────────────────────
CHORD_CONTROL_URL="${CHORD_CONTROL_URL:-}"
IDLE_THRESHOLD="${SWEEP_IDLE_THRESHOLD_SECS:-1800}"
MAX_LEASE="${SWEEP_MAX_LEASE_SECS:-3600}"
SWEEP_BIN="${INTAKE_ASSISTANT_SWEEP_BIN:-/opt/intake/intake_assistant_sweep}"
GAP_MAX="${INTAKE_ASSISTANT_GAP_MAX:-10}"
GPU_LOCK="${GPU_AUTHORITY_LOCK_PATH:-/run/gpu-authority.lock}"
HTTP_TIMEOUT="${SWEEP_CONTROL_HTTP_TIMEOUT_SECS:-10}"

if [[ -z "$CHORD_CONTROL_URL" ]]; then
  log "CHORD_CONTROL_URL is unset — cannot reach Chord's control API; exiting 0 (nothing done)."
  exit 0
fi

b64url() { openssl base64 -A | tr '+/' '-_' | tr -d '='; }

# Mint the short-lived Chord service JWT (HS256, sub=lumina, exp=now+120).
mint_jwt() {
  local secret="<REDACTED-SECRET>"
  if [[ -z "$secret" ]]; then
    log "TERMINUS_PRIMARY_CHORD_JWT_SECRET is unset — cannot auth to Chord control API."
    return 1
  fi
  local header payload signing_input sig
  header="$(printf '%s' '{"alg":"HS256","typ":"JWT"}' | b64url)"
  payload="$(printf '{"sub":"lumina","exp":%d}' "$(( $(date +%s) + 120 ))" | b64url)"
  signing_input="${header}.${payload}"
  sig="$(printf '%s' "$signing_input" \
        | openssl dgst -sha256 -hmac "$secret" -binary | b64url)"
  printf '%s.%s' "$signing_input" "$sig"
}

# POST an authed control action; returns curl's exit (non-zero on transport fail).
chord_post() {
  local path="$1" jwt
  jwt="$(mint_jwt)" || return 1
  curl -fsS --max-time "$HTTP_TIMEOUT" -X POST \
       -H "Authorization: Bearer ${jwt}" \
       "${CHORD_CONTROL_URL%/}${path}" >/dev/null
}

activate() {
  # ALWAYS restore Chord, even if minting/posting fails — log loudly on failure
  # so a stuck-drained Chord is surfaced (the systemd unit's TimeoutStartSec +
  # this trap are the two independent backstops).
  if chord_post /admin/activate; then
    log "Chord reactivated (/admin/activate)."
  else
    log "WARNING: /admin/activate FAILED — Chord may still be drained; operator check required."
  fi
}

# ── 1. Fleet-idle gate ─────────────────────────────────────────────────────────
# GET /admin/activity → {serving,inflight,idle_secs,last_request_unix}. Try
# unauthed first; if 401, retry with the Bearer.
fetch_activity() {
  local jwt
  curl -fsS --max-time "$HTTP_TIMEOUT" "${CHORD_CONTROL_URL%/}/admin/activity" 2>/dev/null && return 0
  jwt="$(mint_jwt)" || return 1
  curl -fsS --max-time "$HTTP_TIMEOUT" -H "Authorization: Bearer ${jwt}" \
       "${CHORD_CONTROL_URL%/}/admin/activity"
}

activity_json="$(fetch_activity)" || {
  log "could not read /admin/activity (Chord down or unreachable) — exiting 0, retry next window."
  exit 0
}

if command -v jq >/dev/null 2>&1; then
  idle_secs="$(printf '%s' "$activity_json" | jq -r '.idle_secs // 0')"
  inflight="$(printf '%s' "$activity_json" | jq -r '.inflight // 0')"
else
  idle_secs="$(printf '%s' "$activity_json" | grep -o '"idle_secs"[[:space:]]*:[[:space:]]*[0-9]*' | grep -o '[0-9]*$' || echo 0)"
  inflight="$(printf '%s' "$activity_json" | grep -o '"inflight"[[:space:]]*:[[:space:]]*[0-9]*' | grep -o '[0-9]*$' || echo 0)"
fi
idle_secs="${idle_secs:-0}"; inflight="${inflight:-0}"

if [[ "$inflight" != "0" || "$idle_secs" -lt "$IDLE_THRESHOLD" ]]; then
  log "fleet busy (inflight=$inflight, idle_secs=$idle_secs < $IDLE_THRESHOLD) — skipping, exit 0."
  exit 0
fi

# GPU-authority lock free? A live lock means a coder sweep / other MINT run owns
# the GPU. Conservative pre-check only — the sweep's OWN gpu_authority acquire
# (bounded-wait, one-model gate, VRAM release) remains the real safety net.
if [[ -s "$GPU_LOCK" ]]; then
  log "gpu-authority lock present at $GPU_LOCK — GPU held elsewhere; skipping, exit 0."
  exit 0
fi

log "fleet idle (idle_secs=$idle_secs) and GPU free — acquiring idle mode."

# ── 2. Drain Chord / free the GPU ──────────────────────────────────────────────
if ! chord_post /admin/idle; then
  log "POST /admin/idle failed — NOT proceeding (never run the sweep un-drained); exit 0."
  exit 0
fi
# From here on Chord is drained → reactivate on ANY exit path.
trap activate EXIT

log "Chord drained (/admin/idle). Running gap-only sweep (cap ${GAP_MAX}) under ${MAX_LEASE}s lease."

# ── 3. Run the bounded gap-only sweep ──────────────────────────────────────────
rc=0
INTAKE_ASSISTANT_GAP_ONLY=1 INTAKE_ASSISTANT_GAP_MAX="$GAP_MAX" \
  timeout --signal=TERM "$MAX_LEASE" "$SWEEP_BIN" || rc=$?

if [[ "$rc" == "0" ]]; then
  log "gap-only sweep completed cleanly."
elif [[ "$rc" == "124" ]]; then
  log "gap-only sweep hit the ${MAX_LEASE}s max-lease and was terminated (bounded, expected under long runs)."
else
  log "gap-only sweep exited non-zero (rc=$rc) — see the sweep's own logs."
fi

# ── 4. activate() runs here via the EXIT trap (guaranteed) ──────────────────────
exit "$rc"
