#!/bin/bash
# Launch two QEMU VMs for the two-vm agent workflow.
#
# Network topology (QEMU user networking / SLIRP):
#   - Each VM sees the host at 10.0.2.2
#   - llama VM uses INSTANCE=1 → host port 8180 → llama VM port 8080
#   - meow VM uses INSTANCE=0
#   - meow VM connects to http://10.0.2.2:8180 to reach llama-server
#   - This IP is fixed and deterministic regardless of DHCP assignments
#
# Port map:
#   meow VM:  ssh=2222  http=8080
#   llama VM: ssh=2322  http=8180
#
# Usage:
#   scripts/run_two_vms.sh [--skip-build] [--skip-disks]
#
# Env vars:
#   MEOW_MEMORY   - RAM for meow VM (default: 64M)
#   LLAMA_MEMORY  - RAM for llama VM (default: 4096M)
#   LLAMA_PORT    - llama-server port inside llama VM (default: 8080)
#   LLAMA_MODEL   - model path inside llama VM (default: /qwen3.5-0.8b-q4.gguf)
#   LLAMA_DISK_MB - llama VM disk size in MiB (default: 1800, fits 508MB model)
#   MEOW_DISK_MB  - meow VM disk size in MiB (default: 256)
set -euo pipefail

MEOW_MEMORY="${MEOW_MEMORY:-64M}"
LLAMA_MEMORY="${LLAMA_MEMORY:-4096M}"
LLAMA_PORT="${LLAMA_PORT:-8080}"
LLAMA_MODEL="${LLAMA_MODEL:-/qwen3.5-0.8b-q4.gguf}"
LLAMA_DISK_MB="${LLAMA_DISK_MB:-1800}"
MEOW_DISK_MB="${MEOW_DISK_MB:-2048}"

SKIP_BUILD=0
SKIP_DISKS=0
for arg in "$@"; do
  case "$arg" in
    --skip-build) SKIP_BUILD=1 ;;
    --skip-disks) SKIP_DISKS=1 ;;
    *) echo "[run_two_vms] unknown arg: $arg" >&2; exit 1 ;;
  esac
done

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

MKFS=""
for cand in mkfs.ext2 /opt/homebrew/opt/e2fsprogs/sbin/mkfs.ext2 /usr/local/sbin/mkfs.ext2; do
  if command -v "$cand" >/dev/null 2>&1; then MKFS="$cand"; break; fi
done
if [ -z "$MKFS" ]; then
  echo "[run_two_vms] mkfs.ext2 not found — install e2fsprogs (brew install e2fsprogs)" >&2
  exit 1
fi

TS="$(date +%Y%m%d-%H%M%S)"
VM_DIR="$ROOT/tmp/two_vms"
LOG_DIR="$ROOT/logs/two_vms/$TS"
mkdir -p "$VM_DIR" "$LOG_DIR"

LLAMA_DISK="$VM_DIR/llama.img"
MEOW_DISK="$VM_DIR/meow.img"

# ─── 1. Build ────────────────────────────────────────────────────────────────
if [ "$SKIP_BUILD" = "0" ]; then
  echo "[run_two_vms] building kernel…"
  cargo build --release
fi
ELF="$ROOT/target/aarch64-unknown-none/release/akuma"

# ─── 2. Disk images ──────────────────────────────────────────────────────────
if [ "$SKIP_DISKS" = "0" ]; then
  echo "[run_two_vms] creating llama disk (${LLAMA_DISK_MB}MiB) at $LLAMA_DISK…"
  rm -f "$LLAMA_DISK"
  dd if=/dev/zero of="$LLAMA_DISK" bs=1m count="$LLAMA_DISK_MB" status=none 2>/dev/null \
    || dd if=/dev/zero of="$LLAMA_DISK" bs=1M count="$LLAMA_DISK_MB" status=none
  "$MKFS" -F -q -b 4096 -L "LLAMA" "$LLAMA_DISK" >/dev/null

  echo "[run_two_vms] populating llama disk (full bootstrap including model)…"
  docker run --rm --privileged \
    -v "$LLAMA_DISK:/disk.img" \
    -v "$ROOT/bootstrap:/bootstrap:ro" \
    alpine:latest sh -c "
      set -e
      mkdir -p /mnt
      mount -o loop /disk.img /mnt
      cp -rv /bootstrap/* /mnt/
      sync
      umount /mnt
    "

  echo "[run_two_vms] creating meow disk (${MEOW_DISK_MB}MiB) at $MEOW_DISK…"
  rm -f "$MEOW_DISK"
  dd if=/dev/zero of="$MEOW_DISK" bs=1m count="$MEOW_DISK_MB" status=none 2>/dev/null \
    || dd if=/dev/zero of="$MEOW_DISK" bs=1M count="$MEOW_DISK_MB" status=none
  "$MKFS" -F -q -b 4096 -L "MEOW" "$MEOW_DISK" >/dev/null

  echo "[run_two_vms] populating meow disk (bootstrap without model, llamacpp provider active)…"
  docker run --rm --privileged \
    -v "$MEOW_DISK:/disk.img" \
    -v "$ROOT/bootstrap:/bootstrap:ro" \
    alpine:latest sh -c "
      set -e
      mkdir -p /mnt
      mount -o loop /disk.img /mnt
      # Copy each bootstrap subdirectory individually, skipping .gguf model files
      for item in /bootstrap/bin /bootstrap/etc /bootstrap/lib /bootstrap/public /bootstrap/var /bootstrap/archives; do
        [ -e \"\$item\" ] && cp -rv \"\$item\" /mnt/
      done
      # Point the ollama provider at the llama VM (host port 8180 → llama VM:8080)
      # Only the URL port changes; provider name and model stay the same.
      sed -i 's|^base_url=http://10.0.2.2:11434|base_url=http://10.0.2.2:8180|' /mnt/etc/meow/config
      echo '[run_two_vms] meow config (base_url updated to llama VM):'
      cat /mnt/etc/meow/config
      sync
      umount /mnt
    "
fi

# ─── 3. Launch llama VM (INSTANCE=1) ─────────────────────────────────────────
LLAMA_SSH_PORT=2322
LLAMA_HTTP_HOST_PORT=8180
LLAMA_LOG="$LOG_DIR/llama.log"
echo "[run_two_vms] launching llama VM (INSTANCE=1 MEMORY=$LLAMA_MEMORY disk=$LLAMA_DISK)…"
echo "[run_two_vms]   ssh: ssh -p $LLAMA_SSH_PORT root@localhost"
echo "[run_two_vms]   log: $LLAMA_LOG"
(
  INSTANCE=1 \
  MEMORY="$LLAMA_MEMORY" \
  DISK="$LLAMA_DISK" \
  SNAPSHOT=0 \
  "$ROOT/scripts/cargo_runner.sh" "$ELF" \
    </dev/null >"$LLAMA_LOG" 2>&1
) &
LLAMA_PID=$!

# ─── 4. Wait for llama VM SSH ────────────────────────────────────────────────
echo "[run_two_vms] waiting for llama VM SSH on port $LLAMA_SSH_PORT…"
until grep -q "SSH Server\] Listening" "$LLAMA_LOG" 2>/dev/null; do sleep 2; done
sleep 3  # let SSH server finish initialising

echo "[run_two_vms] llama VM is up."

# ─── 5. Launch meow VM (INSTANCE=0) ──────────────────────────────────────────
MEOW_SSH_PORT=2222
MEOW_LOG="$LOG_DIR/meow.log"
echo "[run_two_vms] launching meow VM (INSTANCE=0 MEMORY=$MEOW_MEMORY disk=$MEOW_DISK)…"
echo "[run_two_vms]   ssh: ssh -p $MEOW_SSH_PORT root@localhost"
echo "[run_two_vms]   log: $MEOW_LOG"
(
  INSTANCE=0 \
  MEMORY="$MEOW_MEMORY" \
  DISK="$MEOW_DISK" \
  SNAPSHOT=0 \
  "$ROOT/scripts/cargo_runner.sh" "$ELF" \
    </dev/null >"$MEOW_LOG" 2>&1
) &
MEOW_PID=$!

# ─── 6. Wait for meow VM SSH ─────────────────────────────────────────────────
echo "[run_two_vms] waiting for meow VM SSH on port $MEOW_SSH_PORT…"
until grep -q "SSH Server\] Listening" "$MEOW_LOG" 2>/dev/null; do sleep 2; done
sleep 3

echo "[run_two_vms] meow VM is up."

# ─── 7. Print next steps ─────────────────────────────────────────────────────
cat <<EOF

================================================================
 Both VMs are running. Next steps:
================================================================

 STEP 1 — Start llama-server on the llama VM:

   ssh -o StrictHostKeyChecking=no -p $LLAMA_SSH_PORT root@localhost
   # llama-server is bundled in /bin (built from userspace/llama.cpp).
   # --no-mmap: Akuma's VFS doesn't support file-backed mmap.
   # --chat-template chatml: the Qwen3 Jinja2 template isn't supported by
   #   llama-server's built-in parser; chatml is a compatible fallback.
   llama-server --model $LLAMA_MODEL --host 0.0.0.0 --port $LLAMA_PORT \
     --no-mmap --chat-template chatml &

   # Wait ~60s for model to load (health returns 503 while loading, then 200)
   # Verify: curl http://localhost:$LLAMA_PORT/health

 STEP 2 — Run meow on the meow VM:

   ssh -o StrictHostKeyChecking=no -p $MEOW_SSH_PORT root@localhost
   # Then inside the meow VM:
   mkdir -p /akuma-playground
   meow -c "compile /akuma-playground/hello.c with tcc and verify that it runs and returns a greeting, write a report to /tmp/tcc_hello_c.md"

 Network path (fixed IP, no discovery needed):
   meow VM → 10.0.2.2:$LLAMA_HTTP_HOST_PORT → host → llama VM:$LLAMA_PORT
   (provider: llamacpp, model: Qwen3.5-0.8B — already set in meow config)

 Logs:
   llama VM: tail -f $LLAMA_LOG
   meow VM:  tail -f $MEOW_LOG

 To stop: Ctrl-C (kills both VMs)
================================================================

EOF

# ─── 8. Watchdog ─────────────────────────────────────────────────────────────
cleanup() {
  echo "[run_two_vms] stopping VMs…"
  kill "$LLAMA_PID" "$MEOW_PID" 2>/dev/null || true
  wait 2>/dev/null || true
  echo "[run_two_vms] done. logs in $LOG_DIR"
}
trap cleanup INT TERM EXIT

wait
