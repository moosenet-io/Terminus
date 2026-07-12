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
REDIS_UNIT_NAME="${REDIS_UNIT_NAME:-terminus-redis}"
REDIS_DB_DURABLE="${REDIS_DB_DURABLE:-0}"
REDIS_DB_VOLATILE="${REDIS_DB_VOLATILE:-1}"

if [[ -z "${REDIS_SERVER_BIN}" ]]; then
  echo "FATAL: redis-server not found (install it via the sanctioned admin path first)" >&2
  exit 1
fi

# ── Assert bind is loopback/mesh-only (never a public/all-interfaces bind) ────
for addr in ${REDIS_BIND}; do
  if [[ "${addr}" == "0.0.0.0" || "${addr}" == "::" || "${addr}" == "*" ]]; then
    echo "FATAL: REDIS_BIND contains a public/all-interfaces address (${addr}); refusing." >&2
    exit 1
  fi
done
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

CONF_CHANGED=0
if ! cmp -s "${TMP_CONF}" "${REDIS_CONF_PATH}" 2>/dev/null; then
  install -m 0640 -o "${REDIS_USER}" -g "${REDIS_USER}" "${TMP_CONF}" "${REDIS_CONF_PATH}"
  CONF_CHANGED=1
fi

UNIT_PATH="/etc/systemd/system/${REDIS_UNIT_NAME}.service"
TMP_UNIT="$(mktemp)"
trap 'rm -f "${TMP_CONF}" "${TMP_UNIT}"' EXIT
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
if [[ "${CONF_CHANGED}" == 1 || "${UNIT_CHANGED}" == 1 ]] || ! systemctl is-active --quiet "${REDIS_UNIT_NAME}"; then
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
