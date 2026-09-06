#!/bin/sh
# Boot the amd64 kernel under QEMU with a real xHCI controller and a USB disk.
#
# This is the rig that made the USB driver iterable. Before it, the only machine
# with an xHCI controller was the metal box, and the metal box is the one that
# crash-loops when the driver is wrong — so every attempt cost a cold reboot and
# a power cycle, and a wrong guess cost the box. The first run of this script
# reproduced the bug that had survived all of that (a Configure Endpoint command
# claiming EP0, answered with `TRB Error`) in about ninety seconds.
#
# It is deliberately a *different machine* from `run.sh`:
#
#   run.sh       -M microvm   PVH, virtio-MMIO   — the Firecracker stand-in
#   run-xhci.sh  -M q35       PVH, PCI           — the bare-metal stand-in
#
# `microvm` has no PCI bus at all, so it can never host a USB controller. q35
# has one, which is also why this needs `pci` on the command line: the PVH entry
# does not scan PCI unless asked, because Firecracker does not emulate the
# config ports and a scan there invents devices out of garbage. `pci` is the
# boot-time promise that the ports are real.
#
# What this rig does NOT cover: the real Intel controller's BIOS/SMM handoff,
# and the ASMedia enclosure's own quirks. It models a *correct* xHCI controller,
# so it catches every way the driver is wrong about the spec — which is most of
# them — and none of the ways a particular controller is wrong about it. The
# ladder past this point is in
# `docs/runbooks/amd64-bare-metal-loop.md` § "Iterating the USB driver".
set -e

HERE=$(dirname "$0")
cd "$HERE/.."

MEMORY="${MEMORY:-2048}"
DISK="${DISK:-target/usbdisk.img}"
LOG="${LOG:-target/xhci-serial.log}"
# Extra tokens for the kernel command line (`skiptests`, `strace`, `init=...`).
EXTRA="${EXTRA:-}"
KERNEL=target/x86_64-unknown-none/release/akuma-amd64

cargo build -p akuma-amd64 --target x86_64-unknown-none --release

# The fixture: an MBR, an ext2 `sda1` at LBA 2048 and a scratch `sda2` at LBA
# 134217728, matching the real drive. Sparse, so the 64 GiB costs ~256 MiB.
if [ ! -f "$DISK" ]; then
    echo "creating $DISK"
    python3 amd64/mkusbdisk.py "$DISK"
fi

# Same PVH-note check `run.sh` makes, for the same reason: without it QEMU
# silently falls back to a boot protocol this kernel does not implement, and the
# symptom is a guest that produces no output at all.
if ! rust-readobj --elf-output-style=GNU --notes "$KERNEL" | grep -q 'Xen'; then
    echo "FATAL: PVH note missing from $KERNEL — check linker.ld PHDRS" >&2
    exit 1
fi

# `-no-reboot` matters more here than anywhere else in this tree. The failure
# this rig exists to study *is* a reboot: a driver that takes the machine down
# would otherwise loop invisibly, exactly as it does on the metal, and the log
# would be an endless repeat with no way to tell attempt 1 from attempt 40.
# With `-no-reboot`, QEMU stops on the first reset and the log ends at the
# moment of death — where the last `[xhci] ..` breadcrumb names the step.
#
# `nosmp`: the driver is brought up on the BSP before secondaries start, so the
# extra cores add nothing but interleaved console output.
exec qemu-system-x86_64 \
    -M q35 \
    -m "$MEMORY" \
    -display none \
    -no-reboot \
    -kernel "$KERNEL" \
    -append "pci nosmp $EXTRA" \
    -drive file="$DISK",format=raw,if=none,id=usbdisk \
    -device qemu-xhci,id=xhci \
    -device usb-storage,bus=xhci.0,drive=usbdisk \
    -serial "file:$LOG" \
    "$@"
