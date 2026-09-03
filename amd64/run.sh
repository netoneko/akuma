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
set -e

HERE=$(dirname "$0")
cd "$HERE/.."

MEMORY="${MEMORY:-512}"
KERNEL=target/x86_64-unknown-none/release/akuma-amd64

cargo build -p akuma-amd64 --target x86_64-unknown-none --release

# Fail loudly if the PVH note went missing. Without it both loaders silently
# fall back to a protocol this kernel does not implement, and the symptom is a
# guest that produces no output at all — indistinguishable from a hang in
# boot.s. Checking the note is cheaper than debugging that.
if ! rust-readobj --elf-output-style=GNU --notes "$KERNEL" | grep -q 'Xen'; then
    echo "FATAL: PVH note missing from $KERNEL — check linker.ld PHDRS" >&2
    exit 1
fi

exec qemu-system-x86_64 \
    -kernel "$KERNEL" \
    -m "$MEMORY" \
    -serial mon:stdio \
    -display none \
    -no-reboot \
    "$@"
