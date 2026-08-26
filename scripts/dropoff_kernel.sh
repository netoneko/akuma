#!/bin/sh
# Flatten an in-guest kernel build and drop it onto the KERNEL_DROPOFF drive,
# ready for `reboot -f` to boot into. Runs INSIDE the Akuma guest under
# busybox ash — POSIX sh only, no bashisms.
#
# docs/archive/RAW_BLOCK_DEVICE_FD.md, docs/runbooks/selfhost-kernel-build.md
# section "Swap the running kernel in place".
#
# Usage: dropoff_kernel.sh [elf-path] [drive]
#   elf-path defaults to target/aarch64-unknown-none/release/akuma
#   drive    defaults to /dev/vdb (the drop-off drive under KERNEL_DROPOFF=1)
#
# Does NOT reboot itself — run `reboot -f` yourself once this exits 0 (plain
# `reboot` fails EPERM here; it tries to signal an init process this kernel
# doesn't have).

set -e

if [ "$(uname -s)" != "Akuma" ]; then
    echo "dropoff_kernel.sh: refusing to run — this is not Akuma (uname -s says '$(uname -s)')" >&2
    exit 1
fi

ELF="${1:-target/aarch64-unknown-none/release/akuma}"
DRIVE="${2:-/dev/vdb}"
BIN="/tmp/dropoff_kernel.bin.$$"

if [ ! -f "$ELF" ]; then
    echo "dropoff_kernel.sh: no ELF at $ELF — build it first" >&2
    exit 1
fi

if [ ! -b "$DRIVE" ]; then
    echo "dropoff_kernel.sh: $DRIVE is not a block device" >&2
    exit 1
fi

# Flattening (and objcopy discovery) is scripts/mkbin.sh's job — the same
# helper scripts/link_kernel.sh and scripts/cargo_runner.sh use, so there is one
# discovery chain rather than three. It tries rust-objcopy, llvm-objcopy, then
# plain `objcopy` — and `objcopy` is the one that matters here: the self-host
# rootfs gets it from apk `binutils` (installed by scripts/populate_disk.sh
# --with-rust-toolchain), while rustup's rust-objcopy needs the llvm-tools
# component that image does not carry. GNU objcopy's `-O binary` output was
# verified byte-identical to rust-objcopy's on 2026-08-26.
HERE=$(dirname "$0")
echo "dropoff_kernel.sh: flattening $ELF -> $BIN"
"$HERE/mkbin.sh" "$ELF" "$BIN" >/dev/null

echo "dropoff_kernel.sh: dd $BIN -> $DRIVE"
dd if="$BIN" of="$DRIVE" bs=1M
rm -f "$BIN"

echo "dropoff_kernel.sh: done — run 'reboot -f' to boot into it."
