#!/usr/bin/env bash
# Reproduce the SSH accept-loop stall under connect-storm.
#
# Drives `ssh_harness.py parallel` against a running akuma VM, then prints
# the [SSH] and [NET] log tail so we can see the Phase-1 instrumentation
# fingerprint (which of candidates a/b/c in
# docs/STABILITY_URGENT_ISSUES.md is responsible).
#
# Prerequisite: the VM is already booted and listening on port 2222. The
# operator runs `cargo run --release` separately and pipes its output to a
# log file. Pass that file as $1 (or set LOG=...).

set -euo pipefail

LOG="${1:-${LOG:-}}"
COUNT="${COUNT:-4}"
DURATION="${DURATION:-15}"
WAIT_AFTER="${WAIT_AFTER:-12}"

if [[ -z "$LOG" ]]; then
  echo "usage: $0 <kernel-log-path>" >&2
  echo "       or LOG=<path> $0" >&2
  exit 2
fi
if [[ ! -f "$LOG" ]]; then
  echo "log file does not exist: $LOG" >&2
  exit 2
fi

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PY="$REPO_ROOT/venv/bin/python"
if [[ ! -x "$PY" ]]; then
  echo "$PY not found; create venv via scripts (or install requests) before running" >&2
  exit 2
fi

echo "[stall-repro] running ssh_harness.py parallel --count=$COUNT --duration=$DURATION" >&2
"$PY" "$REPO_ROOT/scripts/ssh_harness.py" parallel --count "$COUNT" --duration "$DURATION" || true

echo "[stall-repro] sleeping ${WAIT_AFTER}s for the supervisor heartbeat to fire" >&2
sleep "$WAIT_AFTER"

echo "[stall-repro] tail of [SSH]/[NET] lines:"
grep -E '\[SSH\]|\[NET\]' "$LOG" | tail -30
