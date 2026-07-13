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
# ALLOWLIST, not denylist: a bind address is accepted ONLY if it is a loopback
# address or a private (non-routable) address — loopback, RFC1918 IPv4, IPv4
# link-local, IPv6 ULA (fc00::/7), or IPv6 link-local (fe80::/10). ANYTHING else
# — every publicly-routable / unknown address — is REJECTED before the service
# is enabled, so "Redis is never publicly bound" holds even if a config drifts.
# No infra literal is encoded here — these are universal private ranges, exactly
# like `127.0.0.1` is universal loopback.
is_allowed_bind_addr() {
  local a="${1,,}"  # lower-case for IPv6 hex
  case "${a}" in
    127.*|::1|::ffff:127.*)                  return 0 ;;  # IPv4/IPv6 loopback
    10.*)                                    return 0 ;;  # RFC1918 10/8
    192.168.*)                               return 0 ;;  # RFC1918 192.168/16
    172.1[6-9].*|172.2[0-9].*|172.3[0-1].*)  return 0 ;;  # RFC1918 172.16/12
    169.254.*)                               return 0 ;;  # IPv4 link-local
    fc[0-9a-f][0-9a-f]:*|fd[0-9a-f][0-9a-f]:*|fc[0-9a-f]:*|fd[0-9a-f]:*) return 0 ;;  # IPv6 ULA fc00::/7
    fe8[0-9a-f]:*|fe9[0-9a-f]:*|fea[0-9a-f]:*|feb[0-9a-f]:*)             return 0 ;;  # IPv6 link-local fe80::/10
    *)                                       return 1 ;;  # anything routable/unknown → reject
  esac
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
  allowed_cases=(127.0.0.1 ::1 <internal-ip> <internal-ip> <internal-ip> <internal-ip> 169.254.1.1 fd12:3456::1 fe80::1) # pii-test-fixture
  rejected_cases=(0.0.0.0 :: '*' 203.0.113.7 198.51.100.9 192.0.2.5 172.32.0.1 172.15.0.1 2001:db8::1 example.com) # pii-test-fixture
  for ok in "${allowed_cases[@]}"; do
    if ! is_allowed_bind_addr "${ok}"; then echo "SELFTEST FAIL: '${ok}' should be allowed"; fail=1; fi
  done
  for bad in "${rejected_cases[@]}"; do
    if is_allowed_bind_addr "${bad}"; then echo "SELFTEST FAIL: '${bad}' should be rejected"; fail=1; fi
  done
  if [[ "${fail}" == "0" ]]; then echo "SELFTEST OK: bind allowlist accepts private/loopback, rejects public"; fi
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
# Require at least one loopback address present (a mesh-only bind is allowed, but
# loopback must always be reachable for the local health checks below).
case " ${REDIS_BIND} " in
  *" 127.0.0.1 "*|*" ::1 "*) : ;; # loopback present — good
  *) echo "FATAL: REDIS_BIND must include a loopback address (127.0.0.1/::1)." >&2; exit 1 ;;
esac
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
