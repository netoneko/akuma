#!/bin/bash
# Build Akuma for Firecracker (aarch64, DRAM base 0x80000000)
# Produces akuma-firecracker.bin — a flat binary with ARM64 Image header.
#
# Usage:
#   scripts/build-firecracker.sh            # release build
#   scripts/build-firecracker.sh --debug    # debug build
set -e

PROFILE="release"
CARGO_FLAGS="--release"
if [ "${1}" = "--debug" ]; then
    PROFILE="debug"
    CARGO_FLAGS=""
fi

ELF="target/aarch64-unknown-none/${PROFILE}/akuma"
BIN="akuma-firecracker.bin"

echo "Building Akuma with firecracker feature (linker: linker-firecracker.ld)..."
RUSTFLAGS="-C link-arg=-Tlinker-firecracker.ld" \
    cargo build ${CARGO_FLAGS} --features firecracker

echo "Converting ELF -> flat binary..."
if command -v llvm-objcopy &>/dev/null; then
    llvm-objcopy -O binary "${ELF}" "${BIN}"
elif command -v aarch64-linux-gnu-objcopy &>/dev/null; then
    aarch64-linux-gnu-objcopy -O binary "${ELF}" "${BIN}"
elif command -v rust-objcopy &>/dev/null; then
    rust-objcopy -O binary "${ELF}" "${BIN}"
else
    echo "ERROR: no objcopy found (tried llvm-objcopy, aarch64-linux-gnu-objcopy, rust-objcopy)" >&2
    exit 1
fi

SIZE=$(wc -c < "${BIN}")
echo "Done: ${BIN} ($(( SIZE / 1024 )) KB)"
echo ""
echo "To deploy:"
echo "  scp ${BIN} disk.img ubuntu@akuma.sh:~/"
echo "  # then on the instance: sudo firecracker --config-file akuma-fc.json"
