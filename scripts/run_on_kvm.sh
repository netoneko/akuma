#!/bin/bash
# Run Akuma under QEMU with KVM acceleration (AWS metal / Oracle Cloud A1).
# Requires /dev/kvm and a pre-configured tap0 device:
#
#   sudo ip tuntap add tap0 mode tap
#   sudo ip addr add 172.16.0.1/24 dev tap0
#   sudo ip link set tap0 up
#
# Guest SSH:  ssh -p 2222 akuma@172.16.0.2
# Guest HTTP: curl http://172.16.0.2:8080
set -e

KERNEL_PATH="target/aarch64-unknown-none/release/akuma"

if [ ! -f "$KERNEL_PATH" ]; then
    echo "Kernel not found at $KERNEL_PATH"
    echo "Run 'cargo build --release' first"
    exit 1
fi

if [ ! -e /dev/kvm ]; then
    echo "ERROR: /dev/kvm not found. This script requires a bare-metal host."
    echo "On AWS use a metal instance; on Oracle Cloud use VM.Standard.A1.Flex."
    exit 1
fi

EXTRA_ARGS=""
if [ "$1" == "--test" ]; then
    EXTRA_ARGS="-append TEST=1"
    pkill -9 qemu-system-aarch64 || true
fi

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
  -device loader,file=virt.dtb,addr=0x4ff00000,force-raw=on \
  -kernel $KERNEL_PATH \
  $EXTRA_ARGS
