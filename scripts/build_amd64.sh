#!/bin/sh
set -e
# amd64: the x86_64 bring-up target (akuma-amd64). Builds the kernel for
# x86_64-unknown-none; the ELF carries the PVH note both QEMU `microvm` and
# Firecracker enter at. Boot it with amd64/run.sh (QEMU, the Firecracker
# stand-in) or amd64/run-firecracker.sh; make a disk with amd64/mkdisk.sh.
#
# FEATURES=no-tests compiles the boot suite out — the same opt-in the AArch64
# devbox and extreme-size targets build with (amd64/run.sh passes it through):
#
#   FEATURES=no-tests sh scripts/build_amd64.sh
FEATURES="${FEATURES:-}"

# shellcheck disable=SC2086  # deliberately word-split (empty = no flag)
cargo build -p akuma-amd64 --target x86_64-unknown-none --release \
    ${FEATURES:+--features "$FEATURES"}
ls -lh target/x86_64-unknown-none/release/akuma-amd64
