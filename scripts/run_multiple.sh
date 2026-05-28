#!/bin/bash
# Launch N parallel Akuma boots for hang hunting.
#
# Each instance gets:
#   - its own host-side port band (INSTANCE shifts ssh/http/etc by 100*i,
#     gdbstub by +i — see scripts/cargo_runner.sh)
#   - its own fresh disk image at tmp/vms/<i>/disk.img (64 MB ext2)
#   - its own log at logs/daif/hunt-<timestamp>/<i>.log
#
# Usage:
#   scripts/run_multiple.sh [N] [duration_seconds]
#
# Defaults: N=4, duration=unbounded (Ctrl-C to stop).
#
# Env vars:
#   MEMORY    - per-VM RAM, default 128M (kept small so 4 fits comfortably)
#   DISK_MB   - size of the per-VM disk image, default 64
#   NO_GDB=1  - skip gdbstub (default: GDB=1 so any instance can be attached)
#   STALL_SECS - seconds without new log lines before an instance is flagged
#                as a suspected hang (default 20)
#
# Before launching, all existing qemu-system-aarch64 processes are killed.
set -euo pipefail

N="${1:-4}"
DURATION="${2:-}"
MEMORY="${MEMORY:-128M}"
DISK_MB="${DISK_MB:-64}"

if ! [[ "$N" =~ ^[0-9]+$ ]] || [ "$N" -lt 1 ] || [ "$N" -gt 16 ]; then
  echo "N must be 1..16 (got '$N')" >&2
  exit 1
fi

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

# Locate mkfs.ext2 (macOS Homebrew puts it under e2fsprogs).
MKFS=""
for cand in mkfs.ext2 /opt/homebrew/opt/e2fsprogs/sbin/mkfs.ext2 /usr/local/sbin/mkfs.ext2; do
  if command -v "$cand" >/dev/null 2>&1; then MKFS="$cand"; break; fi
done
if [ -z "$MKFS" ]; then
  echo "mkfs.ext2 not found — install e2fsprogs (brew install e2fsprogs)" >&2
  exit 1
fi

echo "[run_multiple] killing existing qemu-system-aarch64…"
pkill -9 qemu-system-aarch64 2>/dev/null || true
sleep 1

echo "[run_multiple] building kernel (release)…"
cargo build --release
ELF="$ROOT/target/aarch64-unknown-none/release/akuma"

TS="$(date +%Y%m%d-%H%M%S)"
LOG_DIR="$ROOT/logs/daif/hunt-$TS"
mkdir -p "$LOG_DIR"
echo "[run_multiple] logs -> $LOG_DIR"

GDB_FLAG=1
if [ -n "${NO_GDB:-}" ]; then GDB_FLAG=""; fi

PIDS=()
for i in $(seq 0 $((N-1))); do
  VM_DIR="$ROOT/tmp/vms/$i"
  DISK="$VM_DIR/disk.img"
  mkdir -p "$VM_DIR"

  # Always create a fresh disk per launch so each boot starts from a known state.
  rm -f "$DISK"
  dd if=/dev/zero of="$DISK" bs=1m count="$DISK_MB" status=none 2>/dev/null \
    || dd if=/dev/zero of="$DISK" bs=1M count="$DISK_MB" status=none
  "$MKFS" -F -q -b 4096 -L "AKUMA$i" "$DISK" >/dev/null

  GDB_PORT=$((1234 + i))
  LOG="$LOG_DIR/$i.log"
  echo "[run_multiple] launching instance=$i  gdb=:${GDB_PORT}  disk=$DISK  log=$LOG"
  (
    INSTANCE=$i \
    MEMORY="$MEMORY" \
    DISK="$DISK" \
    SNAPSHOT=0 \
    GDB="${GDB_FLAG:-}" \
    "$ROOT/scripts/cargo_runner.sh" "$ELF" \
      </dev/null >"$LOG" 2>&1
  ) &
  PIDS+=($!)
done

echo "[run_multiple] $N instances launched. pids: ${PIDS[*]}"
echo "[run_multiple] tail any with: tail -f $LOG_DIR/<i>.log"
echo "[run_multiple] attach gdb to instance i: aarch64-elf-gdb $ELF -ex 'target remote :\$((1234+i))'"

# --- hang watchdog ---
# Watches each log's mtime. After the kernel has emitted at least one
# heartbeat (Uptime / Thread0 / TMR line), if the log stops growing for
# more than STALL_SECS, print a one-shot [HANG?] notice. Continues
# watching after the notice so a recovered instance gets a second
# warning if it stalls again.
STALL_SECS="${STALL_SECS:-20}"
watchdog() {
  # Indexed arrays (macOS bash 3.2 has no associative arrays).
  # FLAGGED[i] = last mtime we warned about (0 = none).
  # SEEN_HEARTBEAT[i] = 1 once kernel has emitted any heartbeat line.
  FLAGGED=()
  SEEN_HEARTBEAT=()
  for i in $(seq 0 $((N-1))); do
    FLAGGED[$i]=0
    SEEN_HEARTBEAT[$i]=0
  done
  while true; do
    sleep 5
    NOW=$(date +%s)
    for i in $(seq 0 $((N-1))); do
      LOG="$LOG_DIR/$i.log"
      [ -f "$LOG" ] || continue

      if [ "${SEEN_HEARTBEAT[$i]}" = "0" ]; then
        if grep -qE '^\[(Mem|Thread0|TMR|Heartbeat)\]' "$LOG" 2>/dev/null; then
          SEEN_HEARTBEAT[$i]=1
        else
          continue
        fi
      fi

      MTIME=$(stat -f %m "$LOG" 2>/dev/null || stat -c %Y "$LOG" 2>/dev/null || echo 0)
      AGE=$((NOW - MTIME))
      if [ "$AGE" -ge "$STALL_SECS" ]; then
        if [ "${FLAGGED[$i]}" != "$MTIME" ]; then
          FLAGGED[$i]=$MTIME
          LAST_UPTIME=$(grep -oE 'Uptime [0-9]+' "$LOG" | tail -1 || true)
          GDB_PORT=$((1234 + i))
          echo
          echo "================================================================"
          echo "[HANG?] instance=$i  no log growth for ${AGE}s  ($LAST_UPTIME)"
          echo "        log:    $LOG"
          echo "        attach: aarch64-elf-gdb $ELF -ex 'target remote :$GDB_PORT'"
          echo "================================================================"
          echo
        fi
      else
        FLAGGED[$i]=0
      fi
    done
  done
}
watchdog &
WATCHDOG_PID=$!

cleanup() {
  echo
  echo "[run_multiple] stopping QEMU instances…"
  kill "$WATCHDOG_PID" 2>/dev/null || true
  for pid in "${PIDS[@]}"; do kill "$pid" 2>/dev/null || true; done
  wait 2>/dev/null || true
  echo "[run_multiple] done. logs in $LOG_DIR"
}
trap cleanup INT TERM EXIT

if [ -n "$DURATION" ]; then
  echo "[run_multiple] running for ${DURATION}s, then auto-stopping"
  sleep "$DURATION"
else
  echo "[run_multiple] running until Ctrl-C"
  wait
fi
