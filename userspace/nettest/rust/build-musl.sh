#!/bin/bash
# Build the two delayed-first-byte probes for the Akuma VM.
#
#   stdlib/   -> bootstrap/bin/nettest-std       (std::net + poll(2) + sync rustls)
#   reqwest/  -> bootstrap/bin/nettest-reqwest   (tokio + hyper + reqwest + rustls)
#   connect/  -> bootstrap/bin/nettest-connect   (raw connect(2) + poll/select/epoll)
#
# `connect/` belongs to a third investigation (cargo cannot reach crates.io,
# `docs/runbooks/cargo-cannot-reach-crates-io.md`) but shares this build path
# because it has the same requirement: no runtime, no TLS, nothing between the
# probe and the syscall.
#
# This is NOT the sibling curl probe's build path. That one (./build.sh) runs
# cargo inside an Alpine arm64 container because it has to build libcurl +
# OpenSSL from source with autotools. These two are pure host cross-builds with
# the SAME toolchain `userspace/nca` uses for nca itself
# (`userspace/nca/build.rs`): aarch64-unknown-linux-musl + aarch64-linux-musl-gcc.
#
# Matching nca's toolchain is the point, not a convenience. The probes exist to
# answer "does nca's network stack hang, or does nca hang?" — an answer that is
# worthless if the probe and nca were built by different compilers against
# different libcs.
#
#   ./build-musl.sh            # all three probes
#   ./build-musl.sh std        # just nettest-std
#   ./build-musl.sh reqwest    # just nettest-reqwest
#   ./build-musl.sh connect    # just nettest-connect
#
# After building: scripts/populate_disk.sh copies bootstrap/bin/* into /bin.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
OUT_DIR="$REPO_ROOT/bootstrap/bin"
TARGET="aarch64-unknown-linux-musl"

want="${1:-all}"

command -v aarch64-linux-musl-gcc >/dev/null 2>&1 || {
    echo "error: aarch64-linux-musl-gcc not found (brew install FiloSottile/musl-cross/musl-cross)" >&2
    exit 1
}
rustup target list --installed 2>/dev/null | grep -qx "$TARGET" || {
    echo "error: rust target $TARGET not installed (rustup target add $TARGET)" >&2
    exit 1
}

# Same cross-compilation environment userspace/nca/build.rs exports. aws-lc-rs
# (rustls' default crypto provider, and what nca's Cargo.lock resolves) shells
# out to cc/ar for its C core, so these are load-bearing for the reqwest probe.
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=aarch64-linux-musl-gcc
export CC_aarch64_unknown_linux_musl=aarch64-linux-musl-gcc
export CXX_aarch64_unknown_linux_musl=aarch64-linux-musl-g++
export AR_aarch64_unknown_linux_musl=aarch64-linux-musl-ar

build_one() {
    local dir="$1" bin="$2"
    # Braces are load-bearing: bash treats the trailing multibyte character as
    # part of an unbraced variable name and dies with "TARGET…: unbound variable".
    echo "[nettest] building $bin ($dir) for ${TARGET}..."
    ( cd "$SCRIPT_DIR/$dir" && cargo build --release )
    local built="$SCRIPT_DIR/$dir/target/$TARGET/release/$bin"
    [ -f "$built" ] || { echo "BUILD FAILED: $built missing" >&2; exit 1; }
    mkdir -p "$OUT_DIR"
    cp "$built" "$OUT_DIR/$bin"
    chmod +x "$OUT_DIR/$bin"
    echo "[nettest] -> $OUT_DIR/$bin ($(wc -c < "$OUT_DIR/$bin" | tr -d ' ') bytes)"
}

case "$want" in
    all)     build_one stdlib nettest-std; build_one reqwest nettest-reqwest; build_one connect nettest-connect ;;
    std)     build_one stdlib nettest-std ;;
    reqwest) build_one reqwest nettest-reqwest ;;
    connect) build_one connect nettest-connect ;;
    *)       echo "usage: $0 [all|std|reqwest|connect]" >&2; exit 2 ;;
esac

echo "[nettest] done. Run scripts/populate_disk.sh to ship them to the disk image."
