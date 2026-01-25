#!/bin/bash
# Build boa JavaScript interpreter for akuma userspace
#
# Boa requires std, so we build it separately targeting Linux musl.
# The akuma kernel implements Linux syscalls, so this works.

set -e

cd "$(dirname "$0")"

echo "Building boa for aarch64-unknown-linux-musl..."
cargo zigbuild --target aarch64-unknown-linux-musl --release

BINARY="../target/aarch64-unknown-linux-musl/release/boa"

echo ""
echo "Build complete!"
file "$BINARY"
ls -lh "$BINARY"
