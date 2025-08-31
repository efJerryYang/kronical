#!/usr/bin/env bash
# Usage: scripts/monitor_pid.sh <pid> [interval_seconds] [output_file]
# Logs: ts,cpu_pct,rss_bytes,disk_io_bytes   (disk_io_bytes = read+write cumulative)
# Notes:
# - Requires the 'pidio' helper (see pidio.c). We try to auto-build if pidio is missing.
# - The log is safe to append to; header is added if the file does not exist.

set -euo pipefail

PID=${1:-}
INTERVAL=${2:-1}
OUT=${3:-system-info.log}

if [[ -z "$PID" ]]; then
  echo "Usage: $0 <pid> [interval_seconds] [output_file]" >&2
  exit 1
fi

# Locate or build the pidio helper
PIDIO_BIN="${PIDIO_BIN:-$(dirname "$0")/pidio}"
if [[ ! -x "$PIDIO_BIN" ]]; then
  # Try PATH
  if command -v pidio >/dev/null 2>&1; then
    PIDIO_BIN="$(command -v pidio)"
  elif [[ -f "$(dirname "$0")/pidio.c" ]]; then
    echo "Compiling pidio helper..." >&2
    clang -O2 -Wall -Wextra -o "$(dirname "$0")/pidio" "$(dirname "$0")/pidio.c"
    PIDIO_BIN="$(dirname "$0")/pidio"
  else
    echo "Error: 'pidio' helper not found and pidio.c not present to build it." >&2
    echo "Please compile pidio.c (clang -O2 -Wall -Wextra -o pidio pidio.c) and rerun." >&2
    exit 1
  fi
fi

# Add header if file does not exist
if [[ ! -f "$OUT" ]]; then
  echo "# ts,cpu_pct,rss_bytes,disk_io_bytes" > "$OUT"
fi

# Monitor loop
while ps -p "$PID" > /dev/null; do
  TS=$(date -u +"%Y-%m-%dT%H:%M:%SZ")

  # CPU percent and RSS (KB) -> bytes
  # 'ps' can output blanks transiently if the process is short-lived; guard with defaults.
  read -r CPU RSS_KB < <(ps -o %cpu= -o rss= -p "$PID" | awk '{print ($1=="")?0:$1, ($2=="")?0:$2}')
  RSS_BYTES=$((RSS_KB * 1024))

  # Cumulative disk bytes (read + write) via pidio
  if BYTES_RW="$("$PIDIO_BIN" "$PID" 2>/dev/null)"; then
    # BYTES_RW is 'read write'
    read -r BYTES_READ BYTES_WRITTEN <<<"$BYTES_RW"
    DISK_BYTES=$((BYTES_READ + BYTES_WRITTEN))
  else
    # If pidio fails (e.g., process exited between checks), write 0 to keep CSV shape
    DISK_BYTES=0
  fi

  echo "$TS,$CPU,$RSS_BYTES,$DISK_BYTES" >> "$OUT"
  sleep "$INTERVAL"
done

exit 0

