#!/bin/bash
# QEMU runner for `cargo run`. Accepts the kernel ELF path as $1.
#
# Env vars:
#   MEMORY    - RAM size, default 256M (e.g. MEMORY=512M cargo run --release)
#   GDB       - if set, exposes a gdbstub on :$GDB_PORT (-s)
#   GDB_WAIT  - if set, also halts the CPU at reset until gdb attaches (-S).
#               Implies GDB.
#   GDB_PORT  - gdbstub TCP port. Default: 1234 + INSTANCE.
#   INSTANCE  - integer 0..98. Shifts every host-side port so multiple
#               QEMUs can run in parallel for hang-hunting:
#                 ssh   2222 + 100*INSTANCE
#                 http  8080 + 100*INSTANCE
#                 tel   2323 + 100*INSTANCE
#                 :44   44   + 100*INSTANCE   (skipped if would collide w/ low port)
#                 :4444 4444 + 100*INSTANCE
#                 gdb   1234 + INSTANCE
#               Defaults to 0 (legacy ports unchanged). With INSTANCE>0 the
#               disk is auto-snapshotted (writes discarded) to avoid the
#               concurrent-mount corruption that two raw mounts would cause.
#   SNAPSHOT  - "1" forces snapshot=on regardless of INSTANCE.
#               "0" forces it off (DANGEROUS with parallel runs).
#   DISK      - path to disk image. Default: ./disk.img.
#
# Parallel hunt example (build once, then launch N boots in parallel —
# don't call `cargo run` N times, it would serialize on the build lock):
#   cargo build --release
#   ELF=target/aarch64-unknown-none/release/akuma
#   for i in 0 1 2 3; do
#     INSTANCE=$i GDB=1 scripts/cargo_runner.sh "$ELF" \
#       2>&1 | tee logs/daif/hunt-$i.log &
#   done; wait
#
# Attach with:
#   aarch64-elf-gdb target/aarch64-unknown-none/release/akuma \
#     -ex "target remote :$((1234 + INSTANCE))"
set -e

MEMORY="${MEMORY:-256M}"
INSTANCE="${INSTANCE:-0}"
ELF="$1"
BIN="${ELF}.bin"

if ! [[ "$INSTANCE" =~ ^[0-9]+$ ]] || [ "$INSTANCE" -gt 98 ]; then
  echo "[cargo_runner] INSTANCE must be an integer 0..98 (got '$INSTANCE')" >&2
  exit 1
fi

PORT_SHIFT=$((100 * INSTANCE))
SSH_PORT=$((2222 + PORT_SHIFT))
HTTP_PORT=$((8080 + PORT_SHIFT))
TEL_PORT=$((2323 + PORT_SHIFT))
P44_PORT=$((44 + PORT_SHIFT))
P4444_PORT=$((4444 + PORT_SHIFT))
GDB_PORT="${GDB_PORT:-$((1234 + INSTANCE))}"

# Convert ELF to flat binary.
# The binary starts with a branch instruction (not ARM64 Image magic),
# so QEMU loads it at RAM_BASE (0x40000000) without any offset.
# Skip if $BIN is already up-to-date — keeps parallel runs from racing on
# the same output file.
if [ ! -f "$BIN" ] || [ "$ELF" -nt "$BIN" ]; then
  rust-objcopy -O binary "$ELF" "$BIN"
fi

# Size guard: catch binary bloat before it silently breaks boot.
BIN_BYTES=$(wc -c < "$BIN")
if echo "$ELF" | grep -q "/size/"; then
  SIZE_LIMIT=$((1 * 1024 * 1024))   # 1 MB for size profile
  SIZE_LABEL="1 MB"
else
  SIZE_LIMIT=$((3 * 1024 * 1024))   # 3 MB for release profile
  SIZE_LABEL="3 MB"
fi
if [ "$BIN_BYTES" -gt "$SIZE_LIMIT" ]; then
  echo "[cargo_runner] ERROR: kernel binary is $(( BIN_BYTES / 1024 )) KB, exceeds ${SIZE_LABEL} limit" >&2
  exit 1
fi
echo "[cargo_runner] kernel size: $(( BIN_BYTES / 1024 )) KB (limit ${SIZE_LABEL})" >&2

GDB_ARGS=()
if [ -n "$GDB_WAIT" ]; then
  GDB_ARGS+=(-gdb "tcp::$GDB_PORT" -S)
  echo "[cargo_runner] instance=$INSTANCE gdbstub on :$GDB_PORT, CPU halted — attach gdb to start" >&2
elif [ -n "$GDB" ]; then
  GDB_ARGS+=(-gdb "tcp::$GDB_PORT")
  echo "[cargo_runner] instance=$INSTANCE gdbstub on :$GDB_PORT (kernel runs; attach any time)" >&2
fi

# Default: snapshot the disk if INSTANCE>0 so parallel boots don't corrupt disk.img.
if [ -z "$SNAPSHOT" ]; then
  if [ "$INSTANCE" -gt 0 ]; then SNAPSHOT=1; else SNAPSHOT=0; fi
fi
DISK_PATH="${DISK:-disk.img}"
DRIVE_OPTS="file=${DISK_PATH},if=none,format=raw,id=hd0"
if [ "$SNAPSHOT" = "1" ]; then
  DRIVE_OPTS="$DRIVE_OPTS,snapshot=on"
  echo "[cargo_runner] $DISK_PATH mounted snapshot=on (writes discarded)" >&2
fi

echo "[cargo_runner] instance=$INSTANCE ssh=$SSH_PORT http=$HTTP_PORT tel=$TEL_PORT disk=$DISK_PATH" >&2

exec qemu-system-aarch64 \
  -semihosting \
  -machine virt \
  -cpu max \
  -m "$MEMORY" \
  -serial mon:stdio \
  -display none \
  -netdev "user,id=net0,hostfwd=tcp::${TEL_PORT}-:23,hostfwd=tcp::${SSH_PORT}-:22,hostfwd=tcp::${HTTP_PORT}-:8080,hostfwd=tcp::${P44_PORT}-:44,hostfwd=tcp::${P4444_PORT}-:4444" \
  -global virtio-mmio.force-legacy=true \
  -device virtio-net-device,netdev=net0,bus=virtio-mmio-bus.0 \
  -drive "$DRIVE_OPTS" \
  -device virtio-blk-device,drive=hd0,bus=virtio-mmio-bus.1 \
  -device virtio-rng-device,bus=virtio-mmio-bus.2 \
  -device ramfb \
  -kernel "$BIN" \
  "${GDB_ARGS[@]}"
