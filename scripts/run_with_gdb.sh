#!/bin/bash
# Run Akuma kernel with GDB server enabled
# QEMU will wait for GDB to connect before starting execution

set -e

KERNEL_PATH="${1:-target/aarch64-unknown-none/release/akuma}"

if [ ! -f "$KERNEL_PATH" ]; then
    echo "Kernel not found at $KERNEL_PATH"
    echo "Run 'cargo build --release' first"
    exit 1
fi

echo "Starting QEMU with GDB server on port 1234..."
echo "Connect with: gdb-multiarch -ex 'target remote :1234' $KERNEL_PATH"
echo "Or: lldb -o 'gdb-remote 1234' $KERNEL_PATH"
echo ""

qemu-system-aarch64 \
  -machine virt \
  -cpu max \
  -m 128M \
  -nographic \
  -serial mon:stdio \
  -netdev user,id=net0,hostfwd=tcp::2323-:23,hostfwd=tcp::2222-:22,hostfwd=tcp::8080-:8080 \
  -global virtio-mmio.force-legacy=true \
  -device virtio-net-device,netdev=net0,bus=virtio-mmio-bus.0 \
  -drive file=disk.img,if=none,format=raw,id=hd0 \
  -device virtio-blk-device,drive=hd0,bus=virtio-mmio-bus.1 \
  -device virtio-rng-device,bus=virtio-mmio-bus.2 \
  -device loader,file=virt.dtb,addr=0x47f00000,force-raw=on \
  -s \
  -S \
  -kernel "$KERNEL_PATH"

