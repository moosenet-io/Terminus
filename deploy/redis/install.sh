#!/usr/bin/env bash
# BLD-20 — provision the terminus-primary-owned Redis (idempotent).
#
# Renders deploy/redis/{redis.conf,redis.service} from VAULT-MATERIALIZED
# environment values (no infra literal is committed to any tracked file — S1/S7)
# and installs/enables the systemd unit. Safe to re-run: it re-renders, diffs,
# and only reloads/restarts when something actually changed.
#
# Required env (materialized from the vault before invoking — NEVER hardcode):
#   REDIS_BIND         space-separated bind list, e.g. "127.0.0.1 <mesh-ip>"
#                      (loopback + the mesh interface ONLY; 0.0.0.0 is rejected)
#   REDIS_PORT         listen port
#   REDIS_REQUIREPASS  the requirepass value (from the vault)
#   REDIS_DATA_DIR     appendonly/RDB data dir (durable)
#   REDIS_MAXMEMORY    e.g. "512mb"
# Optional env (sensible defaults):
#   REDIS_USER (default: redis)      REDIS_SERVER_BIN (default: $(command -v redis-server))
#   REDIS_CLI_BIN (default: redis-cli)   REDIS_CONF_PATH (default: /etc/redis/terminus-primary.conf)
#   REDIS_UNIT_NAME (default: terminus-redis)   REDIS_DB_DURABLE (0)   REDIS_DB_VOLATILE (1)
#
# This script provisions ONLY. It never prints the password. It refuses to
# enable the service unless the rendered config binds loopback/mesh-only AND
# sets a non-empty requirepass (EDGE CASE: an auth/bind misconfig exposing Redis).

set -euo pipefail

SRC_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# ── Bind-address allowlist (BLD-20 review fix) ────────────────────────────────
# ALLOWLIST, not denylist, and a STRICT IP-LITERAL parse — never a glob prefix
# match (which would wrongly accept `10.example.com` or malformed strings). Each
# bind token must PARSE as a valid IPv4 or IPv6 literal AND classify as private:
# loopback, RFC1918 IPv4, IPv4 link-local (169.254/16), IPv6 ULA (fc00::/7), or
# IPv6 link-local (fe80::/10). A hostname, a public address, or anything that is
# not a parseable IP literal is REJECTED — so "Redis is never publicly bound"
# holds even if config drifts. No infra literal is encoded here (these are
# universal private ranges, exactly like `127.0.0.1` is universal loopback).

# Strict dotted-quad IPv4 literal: exactly 4 numeric octets, each 0-255, no
# leading zeros, no host labels, no trailing junk.
_is_ipv4_literal() {
  local ip="$1"
  [[ "${ip}" =~ ^[0-9]{1,3}\.[0-9]{1,3}\.[0-9]{1,3}\.[0-9]{1,3}$ ]] || return 1
  local IFS=. o
  local -a oct
  read -ra oct <<< "${ip}"
  for o in "${oct[@]}"; do
    [[ ${#o} -gt 1 && ${o:0:1} == 0 ]] && return 1   # reject leading zeros (010)
    (( 10#$o <= 255 )) || return 1
  done
  return 0
}

# Classify a validated IPv4 literal as private (loopback/RFC1918/link-local).
_ipv4_is_private() {
  local IFS=. a b
  local -a oct
  read -ra oct <<< "$1"
  a=${oct[0]}; b=${oct[1]}
  (( 10#$a == 127 )) && return 0                       # loopback 127/8
  (( 10#$a == 10 )) && return 0                        # RFC1918 10/8
  (( 10#$a == 192 && 10#$b == 168 )) && return 0       # RFC1918 192.168/16
  (( 10#$a == 172 && 10#$b >= 16 && 10#$b <= 31 )) && return 0  # RFC1918 172.16/12
  (( 10#$a == 169 && 10#$b == 254 )) && return 0       # IPv4 link-local 169.254/16
  return 1
}

# Strict-ish IPv6 literal: only hex + colons, ≥1 colon, at most one '::', each
# group 1-4 hex digits, ≤8 groups (exactly 8 when there is no '::'). Rejects
# v4-mapped/dotted forms (a bind literal never needs them) and any non-IP string.
_is_ipv6_literal() {
  local ip="${1,,}"
  [[ "${ip}" =~ ^[0-9a-f:]+$ ]] || return 1
  [[ "${ip}" == *:* ]] || return 1
  local tmp="${ip}" doubles=0
  while [[ "${tmp}" == *"::"* ]]; do doubles=$((doubles + 1)); tmp=${tmp/::/:}; done
  (( doubles <= 1 )) || return 1
  local IFS=: grp nonempty=0
  local -a groups
  read -ra groups <<< "${ip}"
  for grp in "${groups[@]}"; do
    [[ -z "${grp}" ]] && continue
    [[ "${grp}" =~ ^[0-9a-f]{1,4}$ ]] || return 1
    nonempty=$((nonempty + 1))
  done
  (( nonempty <= 8 )) || return 1
  if (( doubles == 0 )); then (( nonempty == 8 )) || return 1; fi
  return 0
}

# Classify a validated IPv6 literal as private (loopback/ULA/link-local).
_ipv6_is_private() {
  local ip="${1,,}"
  [[ "${ip}" == "::1" ]] && return 0                   # loopback
  local first
  if [[ "${ip}" == ::* ]]; then first=0; else first=${ip%%:*}; fi
  [[ "${first}" =~ ^[0-9a-f]{1,4}$ ]] || return 1
  local n=$((16#${first}))
  (( n >= 0xfc00 && n <= 0xfdff )) && return 0          # ULA fc00::/7
  (( n >= 0xfe80 && n <= 0xfebf )) && return 0          # link-local fe80::/10
  return 1
}

is_allowed_bind_addr() {
  local a="${1,,}"
  if _is_ipv4_literal "${a}"; then _ipv4_is_private "${a}"; return $?; fi
  if _is_ipv6_literal "${a}"; then _ipv6_is_private "${a}"; return $?; fi
  return 1   # not a parseable IP literal (hostname/garbage) → reject
}

# Validate a full REDIS_BIND value (space-separated). Bind-policy contract:
#   - 127.0.0.1 (IPv4 loopback) is REQUIRED: the post-install health checks and
#     redis.service ExecStop call `redis-cli -p` WITHOUT `-h`, which resolves to
#     127.0.0.1, so an IPv6-loopback-only (::1) bind — though otherwise permitted
#     — would fail those. Requiring it keeps every redis-cli call consistent.
#   - A private MESH address (RFC1918/ULA/link-local) is PERMITTED and typical
#     for a FLEET deploy (federated consumers reach Redis over the mesh) but is
#     NOT required: a single-node / co-located-consumer deploy is a valid
#     loopback-only bind. We do NOT force a mesh address.
#   - Any PUBLIC / non-parseable address is always REJECTED (per is_allowed_bind_addr).
# Silent (returns 0/1) so the self-test can assert on it.
validate_bind_list() {
  local list="$1" addr have_ipv4_loopback=0
  for addr in ${list}; do
    is_allowed_bind_addr "${addr}" || return 1
    [[ "${addr}" == "127.0.0.1" ]] && have_ipv4_loopback=1
  done
  (( have_ipv4_loopback == 1 )) || return 1
  return 0
}

# Self-test hook: `REDIS_INSTALL_SELFTEST=1 bash install.sh` asserts the
# allowlist accepts private/loopback and rejects public addresses, then exits.
# Runs with NO required env / NO Redis — the test-gate can invoke it directly.
if [[ "${REDIS_INSTALL_SELFTEST:-0}" == "1" ]]; then
  fail=0
  # Security-test vectors ONLY (analogous to an SSRF guard's own private-range
  # test cases) — none is a real fleet host: the "allowed" set uses generic
  # RFC1918/ULA/link-local sample addresses, the "rejected" set uses RFC5737
  # (192.0.2/198.51.100/203.0.113) + RFC3849 (2001:db8::/32) documentation
  # ranges. Tagged so the PII gate exempts these deliberate literals.
  allowed_cases=(127.0.0.1 ::1 <internal-ip> <internal-ip> <internal-ip> <internal-ip> 169.254.1.1 fc00::1 fd12:3456::1 fe80::1) # pii-test-fixture
  # Rejections: public (RFC5737/RFC3849 docs), all-interfaces, RFC1918 boundary
  # misses, AND — the point of the strict parse — hostnames and malformed
  # literals that a glob prefix-match would have wrongly accepted.
  rejected_cases=(
    0.0.0.0 :: '*'                                   # all-interfaces / unspecified
    203.0.113.7 198.51.100.9 192.0.2.5 2001:db8::1   # public documentation ranges
    172.32.0.1 172.15.0.1 fec0::1                    # just outside RFC1918 / not ULA-or-LL
    10.example.com 192.168.0.example example.com hostname  # hostnames (glob would accept 1st/2nd)
    <internal-ip> 256.1.1.1 10.0.0 <internal-ip>.5 <internal-ip> # malformed IPv4
    '<internal-ip> ' ' <internal-ip>' 10..0.1 ''               # whitespace / empty / empty octet
    'fe80::z' 'fizz::1' '12345::1' 'gggg::'          # malformed IPv6
  ) # pii-test-fixture
  for ok in "${allowed_cases[@]}"; do
    if ! is_allowed_bind_addr "${ok}"; then echo "SELFTEST FAIL: '${ok}' should be allowed"; fail=1; fi
  done
  for bad in "${rejected_cases[@]}"; do
    if is_allowed_bind_addr "${bad}"; then echo "SELFTEST FAIL: '${bad}' should be rejected"; fail=1; fi
  done

  # Whole-REDIS_BIND validation: 127.0.0.1 (IPv4 loopback) must be present so
  # every `redis-cli` call (health checks + ExecStop, no `-h`) resolves.
  bind_ok_cases=(
    "127.0.0.1"                       # IPv4 loopback only
    "127.0.0.1 ::1"                   # + IPv6 loopback
    "127.0.0.1 <internal-ip>"             # + mesh (RFC1918)
    "127.0.0.1 ::1 fd12:3456::1"     # + IPv6 loopback + ULA mesh
  )
  bind_bad_cases=(
    "::1"                             # IPv6-loopback-only → redis-cli would miss it
    "::1 <internal-ip>"                   # IPv6 loopback + mesh, NO 127.0.0.1
    "<internal-ip>"                        # mesh only, no loopback at all
    "127.0.0.1 8.8.8.8"             # a public address slipped in
    ""                                # empty
  )
  for b in "${bind_ok_cases[@]}"; do
    if ! validate_bind_list "${b}"; then echo "SELFTEST FAIL: REDIS_BIND '${b}' should be accepted"; fail=1; fi
  done
  for b in "${bind_bad_cases[@]}"; do
    if validate_bind_list "${b}"; then echo "SELFTEST FAIL: REDIS_BIND '${b}' should be rejected (needs 127.0.0.1, all-private)"; fail=1; fi
  done

  if [[ "${fail}" == "0" ]]; then
    echo "SELFTEST OK: per-addr allowlist + REDIS_BIND requires 127.0.0.1 (rejects ::1-only), rejects public"
  fi
  exit "${fail}"
fi

# ── Resolve inputs (fail loudly on missing REQUIRED secrets/config) ───────────
: "${REDIS_BIND:?REDIS_BIND is required (loopback + mesh only, vault-materialized)}"
: "${REDIS_PORT:?REDIS_PORT is required}"
: "${REDIS_REQUIREPASS:?REDIS_REQUIREPASS is required (from the vault)}"
: "${REDIS_DATA_DIR:?REDIS_DATA_DIR is required}"
: "${REDIS_MAXMEMORY:?REDIS_MAXMEMORY is required (e.g. 512mb)}"

REDIS_USER="${REDIS_USER:-redis}"
REDIS_SERVER_BIN="${REDIS_SERVER_BIN:-$(command -v redis-server || true)}"
REDIS_CLI_BIN="${REDIS_CLI_BIN:-$(command -v redis-cli || echo redis-cli)}"
REDIS_CONF_PATH="${REDIS_CONF_PATH:-/etc/redis/terminus-primary.conf}"
# systemd EnvironmentFile holding REDISCLI_AUTH so ExecStop's `redis-cli
# shutdown` authenticates (the server always sets requirepass) WITHOUT a secret
# literal in the unit and WITHOUT the password landing on argv (`ps`). Same
# vault-materialized value as requirepass; rendered 0600, owned by REDIS_USER.
REDIS_ENV_FILE="${REDIS_ENV_FILE:-/etc/redis/terminus-primary.env}"
REDIS_UNIT_NAME="${REDIS_UNIT_NAME:-terminus-redis}"
REDIS_DB_DURABLE="${REDIS_DB_DURABLE:-0}"
REDIS_DB_VOLATILE="${REDIS_DB_VOLATILE:-1}"

if [[ -z "${REDIS_SERVER_BIN}" ]]; then
  echo "FATAL: redis-server not found (install it via the sanctioned admin path first)" >&2
  exit 1
fi

# ── Assert every bind address is on the private/loopback ALLOWLIST ────────────
# (reject any publicly-routable/unknown address before enabling the service).
for addr in ${REDIS_BIND}; do
  if ! is_allowed_bind_addr "${addr}"; then
    echo "FATAL: REDIS_BIND address '${addr}' is not a loopback/private address; refusing to bind Redis to a routable/unknown interface." >&2
    exit 1
  fi
done
# Require 127.0.0.1 (IPv4 loopback) SPECIFICALLY: the health checks below and the
# unit's ExecStop call `redis-cli -p` without `-h`, which resolves to 127.0.0.1.
# A ::1-only bind (though allowlisted) would fail those, so mandate IPv4 loopback
# — add it alongside any ::1/mesh bind. `validate_bind_list` is the single source
# of truth (also asserted by the self-test).
if ! validate_bind_list "${REDIS_BIND}"; then
  echo "FATAL: REDIS_BIND must include 127.0.0.1 (IPv4 loopback) so redis-cli health checks and ExecStop resolve consistently; add 127.0.0.1 alongside any ::1/mesh bind." >&2
  exit 1
fi
if [[ -z "${REDIS_REQUIREPASS//[[:space:]]/}" ]]; then
  echo "FATAL: REDIS_REQUIREPASS is empty; refusing to provision an unauthenticated Redis." >&2
  exit 1
fi

# ── Render config + unit from the templates (placeholder substitution) ────────
render() {
  # Reads a template on stdin, writes the rendered result on stdout. Uses a
  # here-doc-free sed so the password never appears on a command line / in ps.
  sed \
    -e "s|__REDIS_BIND__|${REDIS_BIND}|g" \
    -e "s|__REDIS_PORT__|${REDIS_PORT}|g" \
    -e "s|__REDIS_DATA_DIR__|${REDIS_DATA_DIR}|g" \
    -e "s|__REDIS_MAXMEMORY__|${REDIS_MAXMEMORY}|g" \
    -e "s|__REDIS_DB_DURABLE__|${REDIS_DB_DURABLE}|g" \
    -e "s|__REDIS_DB_VOLATILE__|${REDIS_DB_VOLATILE}|g" \
    -e "s|__REDIS_USER__|${REDIS_USER}|g" \
    -e "s|__REDIS_SERVER_BIN__|${REDIS_SERVER_BIN}|g" \
    -e "s|__REDIS_CLI_BIN__|${REDIS_CLI_BIN}|g" \
    -e "s|__REDIS_ENV_FILE__|${REDIS_ENV_FILE}|g" \
    -e "s|__REDIS_CONF_PATH__|${REDIS_CONF_PATH}|g"
}

TMP_CONF="$(mktemp)"
trap 'rm -f "${TMP_CONF}"' EXIT
# Render everything EXCEPT the password with sed, then inject requirepass via a
# value substitution that keeps it off any argv.
render < "${SRC_DIR}/redis.conf" \
  | REQ="${REDIS_REQUIREPASS}" perl -pe 's/__REDIS_REQUIREPASS__/$ENV{REQ}/g' \
  > "${TMP_CONF}"

install -d -o "${REDIS_USER}" -g "${REDIS_USER}" "${REDIS_DATA_DIR}"
install -d "$(dirname "${REDIS_CONF_PATH}")"
install -d "$(dirname "${REDIS_ENV_FILE}")"

CONF_CHANGED=0
if ! cmp -s "${TMP_CONF}" "${REDIS_CONF_PATH}" 2>/dev/null; then
  install -m 0640 -o "${REDIS_USER}" -g "${REDIS_USER}" "${TMP_CONF}" "${REDIS_CONF_PATH}"
  CONF_CHANGED=1
fi

# Render the EnvironmentFile: REDISCLI_AUTH=<requirepass> (same vault value),
# via perl so the secret never appears on argv. 0600, owned by the service user.
# This is what lets ExecStop's `redis-cli shutdown` authenticate — otherwise the
# always-on requirepass makes shutdown NOAUTH and systemd SIGKILLs the server,
# defeating the graceful `nosave` stop.
TMP_ENV="$(mktemp)"
trap 'rm -f "${TMP_CONF}" "${TMP_ENV}"' EXIT
REQ="${REDIS_REQUIREPASS}" perl -e 'print "REDISCLI_AUTH=$ENV{REQ}\n"' > "${TMP_ENV}"
ENV_CHANGED=0
if ! cmp -s "${TMP_ENV}" "${REDIS_ENV_FILE}" 2>/dev/null; then
  install -m 0600 -o "${REDIS_USER}" -g "${REDIS_USER}" "${TMP_ENV}" "${REDIS_ENV_FILE}"
  ENV_CHANGED=1
fi

UNIT_PATH="/etc/systemd/system/${REDIS_UNIT_NAME}.service"
TMP_UNIT="$(mktemp)"
trap 'rm -f "${TMP_CONF}" "${TMP_ENV}" "${TMP_UNIT}"' EXIT
render < "${SRC_DIR}/redis.service" > "${TMP_UNIT}"
UNIT_CHANGED=0
if ! cmp -s "${TMP_UNIT}" "${UNIT_PATH}" 2>/dev/null; then
  install -m 0644 "${TMP_UNIT}" "${UNIT_PATH}"
  UNIT_CHANGED=1
fi

# ── Enable/(re)start only when something changed (idempotent) ──────────────────
if [[ "${UNIT_CHANGED}" == 1 ]]; then
  systemctl daemon-reload
fi
systemctl enable "${REDIS_UNIT_NAME}" >/dev/null 2>&1 || true
if [[ "${CONF_CHANGED}" == 1 || "${UNIT_CHANGED}" == 1 || "${ENV_CHANGED}" == 1 ]] || ! systemctl is-active --quiet "${REDIS_UNIT_NAME}"; then
  systemctl restart "${REDIS_UNIT_NAME}"
fi

# ── Post-provision assertions ─────────────────────────────────────────────────
# Auth is required (a no-auth PING must be refused) and an authed PING works.
if "${REDIS_CLI_BIN}" -p "${REDIS_PORT}" ping 2>/dev/null | grep -qi PONG; then
  echo "FATAL: Redis answered an UNAUTHENTICATED ping — auth is not enforced." >&2
  exit 1
fi
if ! REDISCLI_AUTH="${REDIS_REQUIREPASS}" "${REDIS_CLI_BIN}" -p "${REDIS_PORT}" ping 2>/dev/null | grep -qi PONG; then
  echo "FATAL: authenticated ping failed — Redis did not come up healthy." >&2
  exit 1
fi

echo "OK: ${REDIS_UNIT_NAME} provisioned (bind='${REDIS_BIND}' port=${REDIS_PORT} durable-db=${REDIS_DB_DURABLE} volatile-db=${REDIS_DB_VOLATILE}); auth enforced."
