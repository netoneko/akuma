#!/bin/bash
# Build the async-subprocess / terminal probe for the Akuma VM.
#
#   ./build-musl.sh            # -> bootstrap/bin/ncaprobe
#   ./build-musl.sh --serve    # ... then serve it over HTTP for a LIVE VM
#
# Same host cross-build and the SAME toolchain `userspace/nca` uses for nca
# itself (`userspace/nca/build.rs`), for the same reason `nettest/rust/
# build-musl.sh` gives: a probe compiled by a different compiler against a
# different libc cannot answer "does the runtime hang, or does the kernel?".
# This one is deliberately plain std + musl + pthreads — that is the exact
# surface tokio uses, and the bug it was written for
# (`docs/archive/TOKIO_PIPE_EPOLL_HANG.md`) reproduces on no other stack.
#
# Two ways in:
#   * normal — this script drops it in bootstrap/bin, then
#     `scripts/populate_disk.sh` ships it to /bin.
#   * live VM — `--serve` publishes it on :8899 and the guest fetches it with
#     `curl -s -o /tmp/ncaprobe http://10.0.2.2:8899/ncaprobe` (QEMU slirp
#     always routes the host as 10.0.2.2). Use this rather than touching
#     disk.img: a running VM holds the image open and populate_disk.sh under it
#     corrupts it.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
OUT_DIR="$REPO_ROOT/bootstrap/bin"
TARGET="aarch64-unknown-linux-musl"
PORT="${PROBE_PORT:-8899}"

command -v aarch64-linux-musl-gcc >/dev/null 2>&1 || {
    echo "error: aarch64-linux-musl-gcc not found (brew install FiloSottile/musl-cross/musl-cross)" >&2
    exit 1
}
rustup target list --installed 2>/dev/null | grep -qx "$TARGET" || {
    echo "error: rust target $TARGET not installed (rustup target add $TARGET)" >&2
    exit 1
}

export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=aarch64-linux-musl-gcc
export CC_aarch64_unknown_linux_musl=aarch64-linux-musl-gcc
export CXX_aarch64_unknown_linux_musl=aarch64-linux-musl-g++
export AR_aarch64_unknown_linux_musl=aarch64-linux-musl-ar
export RUSTFLAGS="${RUSTFLAGS:-} -C link-arg=-static"

echo "[ncaprobe] building for ${TARGET}..."
( cd "$SCRIPT_DIR" && cargo build --release --target "$TARGET" )

BUILT="$SCRIPT_DIR/target/$TARGET/release/ncaprobe"
[ -f "$BUILT" ] || { echo "BUILD FAILED: $BUILT missing" >&2; exit 1; }
mkdir -p "$OUT_DIR"
cp "$BUILT" "$OUT_DIR/ncaprobe"
chmod +x "$OUT_DIR/ncaprobe"
echo "[ncaprobe] -> $OUT_DIR/ncaprobe ($(wc -c < "$OUT_DIR/ncaprobe" | tr -d ' ') bytes)"

if [ "${1:-}" == "--serve" ]; then
    echo "[ncaprobe] serving on :$PORT — in the guest:"
    echo "    curl -s -o /tmp/ncaprobe http://10.0.2.2:$PORT/ncaprobe && chmod +x /tmp/ncaprobe"
    exec python3 -m http.server "$PORT" --bind 0.0.0.0 --directory "$(dirname "$BUILT")"
fi

echo "[ncaprobe] done. Run scripts/populate_disk.sh to ship it to the disk image."
