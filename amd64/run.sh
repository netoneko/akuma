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
# device model matches Firecracker's *default*: virtio over MMIO. See below.
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
# Firecracker runs virtio over MMIO **by default**, announcing each device on the
# kernel command line. QEMU's `pc`/`q35` machines put virtio on PCI, so a local
# run against them would exercise a different transport than the one this kernel
# drives — the stand-in would stop standing in. `microvm` is x86-only and is the
# analogue of Firecracker's default: PVH entry, virtio-MMIO.
# Measured layout (`info mtree`): eight transports at 0xfeb00000, 0x200 apart.
#
# Not because PCI is unavailable — Firecracker v1.16.1 has `--enable-pci` and
# builds a real segment (ECAM 0xeec00000, measured). MMIO is what this kernel
# drives today, on both architectures, with a driver that already works. If that
# ever changes, `pc`/`q35` becomes the right local machine and this comment is
# the note saying so.
#
# `-M virt`'s equivalent does not exist on the aarch64 side, which is why
# `docs/archive/FIRECRACKER_PORT.md` §6 could not validate that memory map
# locally at all. Here it can.
MACHINE="-M microvm"

# A CPU model with RDRAND. The default `microvm` model does not expose it, and
# the kernel's network stack wants hardware entropy for TCP sequence numbers —
# and `sshd` will want it for key exchange. The kernel checks CPUID and falls
# back to a non-cryptographic PRNG rather than faulting, but the fallback is not
# something a local run should silently be testing against: both Ryzen and
# Graviton have RDRAND, so the stand-in should too.
CPU="-cpu max"

# Modern virtio, not legacy.
#
# QEMU's virtio-mmio transports default to `force-legacy=true` — version 1, the
# pre-1.0 layout. Firecracker implements **only** the modern interface, so a
# local run without this exercises a transport the target machine does not have:
# different register layout, different feature negotiation, different queue
# setup. `virtio-drivers` handles both, which is exactly why the divergence is
# easy to miss — the driver works and the code path is wrong.
#
# `blk::smoke_test` asserts version 2 for this reason, and it caught the default
# on its first run. `scripts/cargo_runner.sh` passes the same `-global` on
# aarch64 (`docs/archive/` on the rng v2-only work).
LEGACY_OFF="-global virtio-mmio.force-legacy=false"

# The drive, and the command line that announces it.
#
# QEMU does not synthesise `virtio_mmio.device=` the way Firecracker does — for a
# Linux guest the operator writes it — so we write it, with the base and stride
# measured above. The guest parses the identical token either way, which is the
# point: one discovery path, two machines.
DRIVE=""
NIC=""
CMDLINE=""
if [ "$DISK" != "none" ]; then
    if [ -z "$DISK" ]; then
        # Rebuilt every run, on purpose: it contains the guest ELF that was just
        # compiled, and a stale image would silently run the previous one.
        DISK=target/x86_64-unknown-none/release/amd64-root.img
        sh "$HERE/mkdisk.sh" "$DISK" 8 >/dev/null
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
    # A NIC on the next transport. QEMU's user-mode stack needs no tap and no
    # root: it NATs, and answers DHCP itself, so a local run exercises the same
    # discovery and the same driver the Firecracker host does with dnsmasq.
    # `hostfwd` puts the guest's port 8080 on the host's, so `httpd` can be
    # reached with `curl http://localhost:8080/`, and the guest's 2222 (sshd's
    # default) on `SSH_PORT` — default 2222, override it when an aarch64 devbox
    # (which also maps 2222) is already running: `SSH_PORT=2223 INIT=/bin/sshd`.
    NIC="-netdev user,id=n0,hostfwd=tcp::${HTTP_PORT:-8080}-:8080,hostfwd=tcp::${SSH_PORT:-2222}-:2222 -device virtio-net-device,netdev=n0,bus=virtio-mmio-bus.1"
    # Two tokens now, one per device, dense at the 0x200 stride. This is the
    # multi-slot geometry `MmioDevices::geometry` computes — until now only one
    # device was ever announced, so the stride was never exercised on this
    # machine.
    # One shell word, however many tokens. `-append` takes a single argument,
    # and the surrounding variables are deliberately word-split — so this one is
    # kept separate and quoted at the call, or QEMU reads the second token as
    # another device and fails with "drive with bus=0, unit=0 exists".
    # `init=` picks the program that gets the console after the self-tests.
    # Default `/bin/paws`; `INIT=/bin/httpd amd64/run.sh` for the server.
    CMDLINE="virtio_mmio.device=512@0xfeb00000:5 virtio_mmio.device=512@0xfeb00200:6 init=${INIT:-/bin/paws}"
fi

# shellcheck disable=SC2086  # these are deliberately word-split
exec qemu-system-x86_64 \
    $MACHINE \
    $CPU \
    $LEGACY_OFF \
    -kernel "$KERNEL" \
    -m "$MEMORY" \
    $DRIVE \
    $NIC \
    ${CMDLINE:+-append "$CMDLINE"} \
    -serial mon:stdio \
    -display none \
    -no-reboot \
    "$@"
