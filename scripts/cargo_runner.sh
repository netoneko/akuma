#!/bin/bash
# QEMU runner for `cargo run`. Accepts the kernel ELF path as $1.
#
# Env vars:
#   MEMORY    - RAM size, default 256M (e.g. MEMORY=512M cargo run --release)
#   GDB       - if set, exposes a gdbstub on :1234 (-s)
#   GDB_WAIT  - if set, also halts the CPU at reset until gdb attaches (-S).
#               Implies GDB.
#
# Attach with:
#   aarch64-elf-gdb target/aarch64-unknown-none/release/akuma \
#     -ex 'target remote :1234'
set -e

MEMORY="${MEMORY:-256M}"
ELF="$1"
BIN="${ELF}.bin"

# Convert ELF to flat binary.
# The binary starts with a branch instruction (not ARM64 Image magic),
# so QEMU loads it at RAM_BASE (0x40000000) without any offset.
rust-objcopy -O binary "$ELF" "$BIN"

GDB_ARGS=()
if [ -n "$GDB_WAIT" ]; then
  GDB_ARGS+=(-s -S)
  echo "[cargo_runner] gdbstub on :1234, CPU halted — attach gdb to start" >&2
elif [ -n "$GDB" ]; then
  GDB_ARGS+=(-s)
  echo "[cargo_runner] gdbstub on :1234 (kernel runs; attach any time)" >&2
fi

exec qemu-system-aarch64 \
  -semihosting \
  -machine virt \
  -cpu max \
  -m "$MEMORY" \
  -serial mon:stdio \
  -display none \
  -netdev user,id=net0,hostfwd=tcp::2323-:23,hostfwd=tcp::2222-:22,hostfwd=tcp::8080-:8080,hostfwd=tcp::44-:44,hostfwd=tcp::4444-:4444 \
  -global virtio-mmio.force-legacy=true \
  -device virtio-net-device,netdev=net0,bus=virtio-mmio-bus.0 \
  -drive file=disk.img,if=none,format=raw,id=hd0 \
  -device virtio-blk-device,drive=hd0,bus=virtio-mmio-bus.1 \
  -device virtio-rng-device,bus=virtio-mmio-bus.2 \
  -device ramfb \
  -kernel "$BIN" \
  "${GDB_ARGS[@]}"
