#!/bin/bash
# QEMU runner for `cargo run`. Accepts the kernel ELF path as $1.
# Set MEMORY env var to override RAM size (default: 256M).
#   MEMORY=512M cargo run --release
set -e

MEMORY="${MEMORY:-256M}"
ELF="$1"

exec qemu-system-aarch64 \
  -semihosting \
  -machine virt \
  -cpu max \
  -m "$MEMORY" \
  -L qemu-roms \
  -serial mon:stdio \
  -display none \
  -netdev user,id=net0,hostfwd=tcp::2323-:23,hostfwd=tcp::2222-:22,hostfwd=tcp::8080-:8080,hostfwd=tcp::44-:44,hostfwd=tcp::4444-:4444 \
  -global virtio-mmio.force-legacy=true \
  -device virtio-net-device,netdev=net0,bus=virtio-mmio-bus.0 \
  -drive file=disk.img,if=none,format=raw,id=hd0 \
  -device virtio-blk-device,drive=hd0,bus=virtio-mmio-bus.1 \
  -device virtio-rng-device,bus=virtio-mmio-bus.2 \
  -device ramfb \
  -kernel "$ELF"
