#!/bin/bash
# Run Akuma under QEMU, using KVM if available, falling back to TCG.
#
# KVM mode (bare-metal host: AWS metal, Oracle Cloud A1):
#   Requires a pre-configured tap0 device:
#     sudo ip tuntap add tap0 mode tap
#     sudo ip addr add 172.16.0.1/24 dev tap0
#     sudo ip link set tap0 up
#   Guest SSH:  ssh -p 2222 akuma@172.16.0.2
#   Guest HTTP: curl http://172.16.0.2:8080
#
# TCG mode (Nitro-virtualized t4g, no /dev/kvm):
#   Falls back automatically. Slower but functional.
#   Guest SSH:  ssh -p 2222 akuma@localhost
#   Guest HTTP: curl http://localhost:8080
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
    pkill -9 qemu-system-aarch64 || true
fi

if [ -w /dev/kvm ]; then
    echo "KVM available — using hardware acceleration"
    qemu-system-aarch64 \
      -semihosting \
      -accel kvm \
      -machine virt,gic-version=host \
      -cpu host \
      -m 256M \
      -serial mon:stdio \
      -display none \
      -netdev tap,id=net0,ifname=tap0,script=no,downscript=no \
      -global virtio-mmio.force-legacy=false \
      -device virtio-net-device,netdev=net0,bus=virtio-mmio-bus.0 \
      -drive file=disk.img,if=none,format=raw,id=hd0,cache=none,aio=io_uring \
      -device virtio-blk-device,drive=hd0,bus=virtio-mmio-bus.1 \
      -device virtio-rng-device,bus=virtio-mmio-bus.2 \
      -kernel $KERNEL_PATH \
      $EXTRA_ARGS
else
    echo "KVM not available — falling back to TCG (slower)"
    echo "For KVM: use a bare-metal host (AWS metal or Oracle Cloud A1.Flex)"
    qemu-system-aarch64 \
      -semihosting \
      -accel tcg,thread=multi \
      -machine virt \
      -cpu max \
      -m 256M \
      -serial mon:stdio \
      -display none \
      -netdev user,id=net0,hostfwd=tcp::2222-:22,hostfwd=tcp::8080-:8080,hostfwd=tcp::2323-:23 \
      -global virtio-mmio.force-legacy=true \
      -device virtio-net-device,netdev=net0,bus=virtio-mmio-bus.0 \
      -drive file=disk.img,if=none,format=raw,id=hd0 \
      -device virtio-blk-device,drive=hd0,bus=virtio-mmio-bus.1 \
      -device virtio-rng-device,bus=virtio-mmio-bus.2 \
      -kernel $KERNEL_PATH \
      $EXTRA_ARGS
fi
