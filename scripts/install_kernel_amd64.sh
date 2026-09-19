#!/bin/sh
# Install a kernel Akuma/amd64 just built onto the partition GRUB boots it from.
# Runs INSIDE Akuma on the bare-metal box, under busybox ash — POSIX sh only.
#
# docs/runbooks/amd64-bare-metal-loop.md § "Install the kernel from inside Akuma"
#
# Usage: install_kernel_amd64.sh [elf-path] [dest]
#   elf-path defaults to /root/ktarget/x86_64-unknown-none/release/akuma-amd64
#   dest     defaults to /boot/akuma-amd64
#
# This is the amd64 counterpart of scripts/dropoff_kernel.sh, and it is smaller
# because the mechanism is smaller. AArch64 has no bootloader — QEMU's `-kernel`
# IS the bootloader — so KERNEL_DROPOFF exposes that exact file as a raw block
# device and the guest dd's a FLATTENED image onto it, with a fixed capacity
# that can ENOSPC mid-write onto the live file.
#
# Here GRUB is the bootloader, it already speaks ext2, and /boot/akuma-amd64
# lives on Akuma's OWN root (sdb1 to Ubuntu, /dev/sda1 here). So installing a
# kernel is an ordinary file write on a normal filesystem: no raw block device,
# no objcopy flatten (multiboot2 parses the ELF), no capacity ceiling, and
# nothing that needs vfat — which this kernel does not have, so the ESP and
# Ubuntu's ext4 are both unreachable and deliberately not in the path.
#
# Does NOT reboot. Run `reboot -f` yourself once this exits 0.

set -e

if [ "$(uname -s)" != "Akuma" ]; then
    echo "install_kernel_amd64.sh: refusing — this is not Akuma (uname -s says '$(uname -s)')" >&2
    exit 1
fi

ELF="${1:-/root/ktarget/x86_64-unknown-none/release/akuma-amd64}"
DEST="${2:-/boot/akuma-amd64}"

[ -f "$ELF" ] || { echo "install_kernel_amd64.sh: no ELF at $ELF — build it first" >&2; exit 1; }

# The multiboot2 magic, or GRUB will not boot what we install. This matters more
# here than it would with a one-shot entry: /boot/akuma-amd64 is the DEFAULT
# entry, so a headerless kernel installed over it means the box comes up at the
# GRUB prompt and needs someone at the machine. `od -t x4` prints each 4-byte
# little-endian word, which is exactly how the header's u32 magic reads.
if ! od -A d -t x4 -N 32768 "$ELF" 2>/dev/null | grep -qi e85250d6; then
    echo "install_kernel_amd64.sh: NO multiboot2 header in $ELF — refusing to install" >&2
    echo "  GRUB would fail to boot it, and this is the default entry." >&2
    exit 1
fi

SZ=$(wc -c < "$ELF")
echo "install_kernel_amd64.sh: $ELF ($SZ bytes) -> $DEST"

# Keep the kernel we are about to overwrite as `.prev`. `.good` is the *verified*
# fallback and is promoted by hand after a boot that passed its self-tests
# (docs/runbooks/amd64-bare-metal-loop.md); `.prev` is simply the one that was
# there a moment ago, which is what you want after installing two kernels in a
# row without rebooting between them. Both are GRUB-reachable by editing the
# menu entry's path; neither costs anything but 3 MB.
if [ -f "$DEST" ]; then
    cp -f "$DEST" "$DEST.prev" || echo "install_kernel_amd64.sh: warning — could not save $DEST.prev" >&2
fi

cp -f "$ELF" "$DEST"
sync

# Read it back. A short write on this kernel's ext2 would otherwise only show up
# as a machine that does not come back, at which point the evidence is gone.
DSZ=$(wc -c < "$DEST")
if [ "$SZ" != "$DSZ" ]; then
    echo "install_kernel_amd64.sh: SHORT WRITE — source $SZ bytes, installed $DSZ" >&2
    exit 1
fi
if command -v md5sum >/dev/null 2>&1; then
    A=$(md5sum "$ELF"  | cut -d' ' -f1)
    B=$(md5sum "$DEST" | cut -d' ' -f1)
    [ "$A" = "$B" ] || { echo "install_kernel_amd64.sh: MD5 MISMATCH $A != $B" >&2; exit 1; }
    echo "install_kernel_amd64.sh: md5 $A verified"
fi

echo "install_kernel_amd64.sh: installed. Run 'reboot -f' to boot into it."
echo "  fallback if it does not come up: pick 'Akuma/amd64 (known good)' at the"
echo "  GRUB menu (10s), or 'Ubuntu'."
