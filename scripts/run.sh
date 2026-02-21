#!/bin/bash
set -e

KERNEL_PATH="target/aarch64-unknown-none/release/akuma"

if [ ! -f "$KERNEL_PATH" ]; then
    echo "Kernel not found at $KERNEL_PATH"
    echo "Run 'cargo build --release' first"
    exit 1
fi

EXTRA_ARGS=""
if [ "$1" == "--test" ]; then
    EXTRA_ARGS="-append TEST=1"
    # Ensure no other QEMU is running that might lock the disk
    pkill -9 qemu-system-aarch64 || true
fi

qemu-system-aarch64 \
  -semihosting \
  -machine virt \
  -cpu max \
  -m 128M \
  -nographic \
  -serial mon:stdio \
  -netdev user,id=net0 \
  -global virtio-mmio.force-legacy=true \
  -device virtio-net-device,netdev=net0,bus=virtio-mmio-bus.0 \
  -drive file=disk.img,if=none,format=raw,id=hd0 \
  -device virtio-blk-device,drive=hd0,bus=virtio-mmio-bus.1 \
  -device virtio-rng-device,bus=virtio-mmio-bus.2 \
  -device loader,file=virt.dtb,addr=0x47f00000,force-raw=on \
  -kernel $KERNEL_PATH \
  $EXTRA_ARGS