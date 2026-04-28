#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
STATE_DIR="${ROOT_DIR}/target/monitoring"
PID_FILE="${PID_FILE:-$HOME/.kronical/kronid.pid}"
OUT_FILE="${OUT_FILE:-${STATE_DIR}/kronid-rss-hourly.csv}"
LOCK_DIR="${LOCK_DIR:-${STATE_DIR}/kronid-rss-hourly.lock}"
INTERVAL_SECS="${INTERVAL_SECS:-3600}"
RUN_ONCE="${RUN_ONCE:-0}"

mkdir -p "${STATE_DIR}"

if ! mkdir "${LOCK_DIR}" 2>/dev/null; then
  echo "monitor already running: ${LOCK_DIR}" >&2
  exit 1
fi

cleanup() {
  rmdir "${LOCK_DIR}" 2>/dev/null || true
}

trap cleanup EXIT INT TERM

if [[ ! -f "${OUT_FILE}" ]]; then
  echo "timestamp_utc,pid,elapsed,rss_kb,rss_mb,command" > "${OUT_FILE}"
fi

while true; do
  ts="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"

  if [[ -r "${PID_FILE}" ]]; then
    pid="$(tr -d '[:space:]' < "${PID_FILE}")"
  else
    pid=""
  fi

  if [[ -n "${pid}" ]] && ps -p "${pid}" >/dev/null 2>&1; then
    line="$(ps -o etime= -o rss= -o command= -p "${pid}" | sed -e 's/^[[:space:]]*//' | head -n 1)"
    elapsed="$(awk '{print $1}' <<<"${line}")"
    rss_kb="$(awk '{print $2}' <<<"${line}")"
    command="$(cut -d' ' -f3- <<<"${line}")"
    rss_mb="$(awk -v rss_kb="${rss_kb}" 'BEGIN { printf "%.1f", rss_kb / 1024 }')"
    printf '%s,%s,%s,%s,%s,"%s"\n' \
      "${ts}" "${pid}" "${elapsed}" "${rss_kb}" "${rss_mb}" "${command//\"/\"\"}" >> "${OUT_FILE}"
  else
    printf '%s,,,,,\n' "${ts}" >> "${OUT_FILE}"
  fi

  if [[ "${RUN_ONCE}" == "1" ]]; then
    break
  fi

  sleep "${INTERVAL_SECS}"
done
