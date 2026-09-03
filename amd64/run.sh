#!/bin/sh
# Build and boot the amd64 kernel under QEMU via PVH.
#
# QEMU is the local stand-in for Firecracker, not a second target: both find the
# PVH note in the ELF and enter at the same 32-bit entry point with
# hvm_start_info in %ebx, so what boots here boots there. The observable
# difference is the address of that block (QEMU 0x1580, Firecracker's
# PVH_INFO_START), which is exactly why kmain prints it.
#
# No -accel: on an Apple Silicon host this is x86 under TCG. It is slow and it
# is correct, and HVF cannot help because the guest ISA is not the host's.
#
# Since Stage M this uses `-M microvm` rather than the default `pc`, so the local
# device model matches Firecracker's: virtio over MMIO, no PCI. See below.
set -e

HERE=$(dirname "$0")
cd "$HERE/.."

MEMORY="${MEMORY:-512}"
KERNEL=target/x86_64-unknown-none/release/akuma-amd64
# DISK=<path> attaches an existing image; otherwise a probe disk is generated.
# DISK=none boots with no drive, which is the pre-Stage-M shape and still valid.
DISK="${DISK:-}"

cargo build -p akuma-amd64 --target x86_64-unknown-none --release

# Fail loudly if the PVH note went missing. Without it both loaders silently
# fall back to a protocol this kernel does not implement, and the symptom is a
# guest that produces no output at all — indistinguishable from a hang in
# boot.s. Checking the note is cheaper than debugging that.
if ! rust-readobj --elf-output-style=GNU --notes "$KERNEL" | grep -q 'Xen'; then
    echo "FATAL: PVH note missing from $KERNEL — check linker.ld PHDRS" >&2
    exit 1
fi

# `-M microvm`, not the default `pc`, and this is the whole reason the local run
# still means anything.
#
# Firecracker has no PCI bus: it presents virtio over MMIO and announces each
# device on the kernel command line. QEMU's `pc`/`q35` machines put virtio on
# PCI, so a local run against them would exercise a device model the target
# machine does not have — the stand-in would stop standing in. `microvm` is
# x86-only and is Firecracker's analogue: PVH entry, virtio-MMIO, no PCI.
# Measured layout (`info mtree`): eight transports at 0xfeb00000, 0x200 apart.
#
# `-M virt`'s equivalent does not exist on the aarch64 side, which is why
# `docs/archive/FIRECRACKER_PORT.md` §6 could not validate that memory map
# locally at all. Here it can.
MACHINE="-M microvm"

# The drive, and the command line that announces it.
#
# QEMU does not synthesise `virtio_mmio.device=` the way Firecracker does — for a
# Linux guest the operator writes it — so we write it, with the base and stride
# measured above. The guest parses the identical token either way, which is the
# point: one discovery path, two machines.
DRIVE=""
APPEND=""
if [ "$DISK" != "none" ]; then
    if [ -z "$DISK" ]; then
        DISK=target/x86_64-unknown-none/release/amd64-probe.img
        python3 "$HERE/mkdisk.py" "$DISK" 4 >/dev/null
    fi
    # `bus=virtio-mmio-bus.0` is load-bearing. QEMU fills transports from the
    # TOP down — measured with `info qtree`, a lone virtio-blk lands on bus 23 of
    # 24 at 0xfeb02e00, and the aarch64 `-M virt` machine does the same thing at
    # bus 31 — so an unpinned device is nowhere near the base of the array and
    # the probe reads a page of zeroes at slot 0. `scripts/cargo_runner.sh` pins
    # every aarch64 device to a numbered bus for exactly this reason; this is the
    # same fix on the same QEMU behaviour.
    #
    # Firecracker needs no equivalent: it packs devices densely from its own base
    # and announces each one, so slot order and announcement order agree.
    DRIVE="-drive id=d0,file=$DISK,format=raw,if=none -device virtio-blk-device,drive=d0,bus=virtio-mmio-bus.0"
    APPEND="-append virtio_mmio.device=512@0xfeb00000:5"
fi

# shellcheck disable=SC2086  # DRIVE/APPEND/MACHINE are deliberately word-split
exec qemu-system-x86_64 \
    $MACHINE \
    -kernel "$KERNEL" \
    -m "$MEMORY" \
    $DRIVE \
    $APPEND \
    -serial mon:stdio \
    -display none \
    -no-reboot \
    "$@"
