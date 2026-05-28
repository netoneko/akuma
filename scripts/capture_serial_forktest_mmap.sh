#!/usr/bin/env bash
# Capture QEMU serial (mon:stdio) to a file while running Akuma for forktest / mmap investigation.
#
# Usage:
#   ./scripts/capture_serial_forktest_mmap.sh [logfile]
# Default logfile: full.log in repo root.
#
# Then in another terminal (or after SSH comes up):
#   ssh -o StrictHostKeyChecking=no -p 2222 user@localhost
#   export GOMAXPROCS=1
#   forktest_parent --duration 10s -mmap_test
#   # optional: forktest_parent --duration 10s -mmap_test -mmap_alloc_mb=4 -num_children=1
#
# After exit, analyze:
#   rg '\[mmap\]|\[DA-MISS\]|\[DA-DP\]|\[WILD-DA\]|\[Fault\]|exit_group' "$LOG"
#
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
LOG="${1:-$ROOT/full.log}"

cd "$ROOT"
if [[ ! -f Cargo.toml ]]; then
  echo "Run from repo root; Cargo.toml not found" >&2
  exit 1
fi

echo "Serial capture -> $LOG"
echo "Stop with Ctrl+C when done (or close QEMU)."
echo ""
MEMORY="${MEMORY:-2048M}" cargo run --release 2>&1 | tee "$LOG"
