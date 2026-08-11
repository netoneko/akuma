#!/bin/bash
# Build nettest (Rust) for the Akuma VM. Two-step:
#   1. docker build (Alpine arm64) → installs libcurl/openssl/nghttp2 dev pkgs
#   2. docker run → cargo build the probe natively, copy the binary out
#
# The binary dynamically links libcurl.so + libssl.so + libnghttp2.so — the
# exact same shared libraries apk-installed cargo links against inside the VM.
# This is intentional: we want cargo's TLS/HTTP/2 behaviour byte-for-byte.
#
# Run from anywhere; output goes to $REPO/bootstrap/bin/nettest.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
IMAGE_TAG="nettest-builder:latest"

echo "[nettest] building docker image (Alpine arm64 + libcurl-dev)…"
docker build --platform linux/arm64 -t "$IMAGE_TAG" -f "$SCRIPT_DIR/Dockerfile" "$SCRIPT_DIR"

echo "[nettest] running cargo build inside the container…"
docker run --rm --platform linux/arm64 \
    -v "$REPO_ROOT:/repo" \
    -e CARGO_HOME=/tmp/cargo \
    "$IMAGE_TAG" \
    bash -c '
        set -e
        cd /repo/userspace/nettest/rust
        cargo build --release
        # The target dir is target/<triple>/release because .cargo/config.toml
        # pins a non-default triple. The bin name is `nettest`.
        BIN=$(find target -type f -name nettest -path "*/release/nettest" | head -1)
        [ -n "$BIN" ] || { echo "BUILD FAILED: no nettest binary found"; ls -la target/; exit 1; }
        cp "$BIN" /repo/userspace/nettest/rust/nettest
        echo "[container] built: $(file /repo/userspace/nettest/rust/nettest)"
        ldd /repo/userspace/nettest/rust/nettest 2>&1 || true
    '

echo "[nettest] staging into bootstrap/bin/"
mkdir -p "$REPO_ROOT/bootstrap/bin"
cp "$SCRIPT_DIR/nettest" "$REPO_ROOT/bootstrap/bin/nettest"
chmod +x "$REPO_ROOT/bootstrap/bin/nettest"
echo "[nettest] done → bootstrap/bin/nettest"
