#!/bin/bash
# Build tokioprobe, a static musl binary that proves tokio works on a kernel.
#
#   ./build-musl.sh                 # aarch64 -> bootstrap/bin/tokioprobe
#   ARCH=x86_64 ./build-musl.sh     # amd64   -> bootstrap/bin/tokioprobe.x86_64
#   ./build-musl.sh --serve         # ... then serve it over HTTP for a live VM
#
# Same host cross-build as userspace/ncaprobe and nettest/rust (the toolchain
# userspace/nca uses). Naming follows nettest/rust/build-musl.sh: aarch64 output
# keeps the bare name, anything else gets an arch suffix so the two cannot be
# mistaken for each other in bootstrap/bin/.
#
# After building: scripts/populate_disk.sh copies bootstrap/bin/* into /bin, or
# copy the file into a guest image yourself (debugfs -w, write).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
OUT_DIR="$REPO_ROOT/bootstrap/bin"
PORT="${PROBE_PORT:-8899}"

case "${ARCH:-aarch64}" in
    aarch64|arm64) TARGET="aarch64-unknown-linux-musl"; SUFFIX="" ;;
    x86_64|amd64)  TARGET="x86_64-unknown-linux-musl";  SUFFIX=".x86_64" ;;
    *) echo "error: ARCH must be aarch64 or x86_64 (got '$ARCH')" >&2; exit 2 ;;
esac
CROSS="${TARGET%%-*}-linux-musl"
TARGET_VAR="$(echo "$TARGET" | tr '-' '_')"

command -v "${CROSS}-gcc" >/dev/null 2>&1 || {
    echo "error: ${CROSS}-gcc not found (brew install FiloSottile/musl-cross/musl-cross)" >&2
    exit 1
}
rustup target list --installed 2>/dev/null | grep -qx "$TARGET" || {
    echo "error: rust target $TARGET not installed (rustup target add $TARGET)" >&2
    exit 1
}

# ring builds its C/asm core with cc, so the cross compiler must be named here
# too; the linker and -static come from .cargo/config.toml.
export "CC_${TARGET_VAR}=${CROSS}-gcc"
export "AR_${TARGET_VAR}=${CROSS}-ar"

echo "[tokioprobe] building for ${TARGET}..."
( cd "$SCRIPT_DIR" && cargo build --release --locked --target "$TARGET" )

BUILT="$SCRIPT_DIR/target/$TARGET/release/tokioprobe"
[ -f "$BUILT" ] || { echo "BUILD FAILED: $BUILT missing" >&2; exit 1; }
mkdir -p "$OUT_DIR"
cp "$BUILT" "$OUT_DIR/tokioprobe${SUFFIX}"
chmod +x "$OUT_DIR/tokioprobe${SUFFIX}"
echo "[tokioprobe] -> $OUT_DIR/tokioprobe${SUFFIX} ($(wc -c < "$OUT_DIR/tokioprobe${SUFFIX}" | tr -d ' ') bytes)"

if [ "${1:-}" == "--serve" ]; then
    echo "[tokioprobe] serving on :$PORT - in the guest:"
    echo "    curl -s -o /tmp/tokioprobe http://10.0.2.2:$PORT/tokioprobe${SUFFIX} && chmod +x /tmp/tokioprobe"
    exec python3 -m http.server "$PORT" --bind 0.0.0.0 --directory "$OUT_DIR"
fi
