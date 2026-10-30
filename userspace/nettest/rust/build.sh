#!/bin/bash
# Build nettest (the libcurl probe) for an Akuma guest.
#
#   ./build.sh                 native cross-build for x86_64 (the Taoxichuan / amd64 guest), no Docker
#   ./build.sh aarch64         native cross-build for aarch64 (musl)
#   ./build.sh --docker        the original Alpine arm64 container build (apk-style libcurl env)
#
# Native cross-build (default, 2026-10-27): `curl-sys` with `static-curl` + `static-ssl`
# builds libcurl, nghttp2 and OpenSSL from source with the target's musl gcc
# (`<arch>-linux-musl-gcc`, brew: FiloSottile/musl-cross/musl-cross) plus perl + make.
# It needs no Docker: it took 1 min 15 s for x86_64 on an M-series Mac and produced a
# static-PIE ELF (libcurl + OpenSSL 3.6.3 inside, no .so dependencies). Output:
#   x86_64  -> $REPO/bootstrap/bin/x86_64/nettest
#   aarch64 -> $REPO/bootstrap/bin/nettest   (what the aarch64 VM has always used)
#
# The --docker path is kept for one reason only: it links the container's apk libcurl
# environment on arm64, which is not what the vendored build above does.
#
# Run from anywhere.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"

MODE=native
ARCH=x86_64
for a in "$@"; do
    case "$a" in
        --docker) MODE=docker ;;
        x86_64|aarch64) ARCH="$a" ;;
        *) echo "usage: build.sh [--docker] [x86_64|aarch64]"; exit 2 ;;
    esac
done

build_native() {
    local triple="$ARCH-unknown-linux-musl"
    local cc="$ARCH-linux-musl-gcc"
    local ar="$ARCH-linux-musl-ar"
    for tool in "$cc" perl make cargo; do
        command -v "$tool" >/dev/null 2>&1 || {
            echo "build.sh: '$tool' not found (musl cross: brew install FiloSottile/musl-cross/musl-cross)" >&2
            exit 1
        }
    done
    local var=${triple//-/_}
    local VAR
    VAR=$(printf '%s' "$var" | tr 'a-z' 'A-Z')
    echo "[nettest] native cross-build for $triple with $cc (no Docker)…"
    # The crate's own .cargo/config.toml pins the Alpine-container target; --target overrides it.
    (
        cd "$SCRIPT_DIR"
        export "CC_${var}=$cc" "AR_${var}=$ar" "CARGO_TARGET_${VAR}_LINKER=$cc"
        cargo build --release --target "$triple"
    )
    local bin="$SCRIPT_DIR/target/$triple/release/nettest"
    [ -f "$bin" ] || { echo "BUILD FAILED: $bin not found"; exit 1; }
    local out_dir="$REPO_ROOT/bootstrap/bin"
    [ "$ARCH" = x86_64 ] && out_dir="$out_dir/x86_64"
    mkdir -p "$out_dir"
    cp "$bin" "$out_dir/nettest"
    chmod +x "$out_dir/nettest"
    echo "[nettest] done → ${out_dir#$REPO_ROOT/}/nettest ($(file -b "$out_dir/nettest" | cut -c1-80))"
}

build_docker() {
    local IMAGE_TAG="nettest-builder:latest"
    # Build nettest (Rust) for the Akuma VM. Two-step:
    #   1. docker build (Alpine arm64) → installs libcurl/openssl/nghttp2 dev pkgs
    #   2. docker run → cargo build the probe natively, copy the binary out
    #
    # The binary dynamically links libcurl.so + libssl.so + libnghttp2.so — the
    # exact same shared libraries apk-installed cargo links against inside the VM.
    # This is intentional: we want cargo's TLS/HTTP/2 behaviour byte-for-byte.
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
}

if [ "$MODE" = docker ]; then build_docker; else build_native; fi
