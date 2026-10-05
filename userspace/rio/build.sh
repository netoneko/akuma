#!/bin/bash
# Cross-build rio (the Akuma fork) for the amd64 fbdev stack and push it to
# the box over HTTP (the box has wget, no scp — same transport as
# akuma-cli-wgpu/deploy.sh).
#
# Usage:  userspace/rio/build.sh [--serve-only]
# Needs:  x86_64-linux-musl-gcc (musl-cross), rustup toolchain 1.96.1 with
#         the x86_64-unknown-linux-musl target, ssh akuma working, and the
#         forks checked out side by side (RIO_DIR, default
#         ~/github.com/netoneko/rio).
#
# Full story: docs/archive/AKUMA_AMD64_RIO_FBDEV_BUILD.md
set -euo pipefail

RIO_DIR=${RIO_DIR:-"$HOME/github.com/netoneko/rio"}
HOST_IP=${HOST_IP:-$(ipconfig getifaddr en0)}
PORT=${PORT:-8100}
TARGET=x86_64-unknown-linux-musl
OUT="$RIO_DIR/target/$TARGET/release"

cd "$RIO_DIR"

export CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=x86_64-linux-musl-gcc
export CC_x86_64_unknown_linux_musl=x86_64-linux-musl-gcc
export AR_x86_64_unknown_linux_musl=x86_64-linux-musl-ar

if [ "${1:-}" != "--serve-only" ]; then
    # the pin in rio's rust-toolchain.toml; the target must be installed
    # for that toolchain, not the default one
    rustup target add "$TARGET" --toolchain 1.96.1
    cargo +1.96.1 build --release -p rioterm \
        --no-default-features --features wgpu,fb \
        --target "$TARGET"
fi

python3 -m http.server "$PORT" --bind 0.0.0.0 --directory "$OUT" >/dev/null 2>&1 &
SRV=$!
trap 'kill $SRV 2>/dev/null' EXIT
sleep 1
ssh akuma "wget -q -O /bin/rio.new http://$HOST_IP:$PORT/rio && chmod +x /bin/rio.new && mv /bin/rio.new /bin/rio && ls -l /bin/rio"
echo "on the box: . /etc/akuma-dev.env && /bin/rio"
