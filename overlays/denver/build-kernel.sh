#!/bin/sh
# Build the Denver kernel (the Clojure/JVM gate feature set) from this checkout.
#
#   overlays/denver/build-kernel.sh                 -> overlays/denver/out/denver-amd64
#   ARCH=aarch64 overlays/denver/build-kernel.sh    -> overlays/denver/out/denver-aarch64
#
# The amd64 feature set is the one the experiment's last gate used
# (denver/docs/archive/CLOJURE_AND_JAVAC_ON_DENVER.md); every feature is off by default
# in the crates, so this is the only place that turns them on together. On aarch64 only
# `wx-relaxed` and `getcpu` exist so far (vsyscall, fold, fault-signal are amd64 half's): Denver
# boots there, HotSpot starts and runs Java init, and then dies reading a class from the
# memory-mapped `modules` file (a ClassFormatError), so the Clojure server runs on amd64 only.
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
cd "$HERE/../.."
ARCH=${ARCH:-amd64}
case "$ARCH" in
    amd64)   TARGET=x86_64-unknown-none;  DEFAULT=wx-relaxed,vsyscall,procfs-diag,trace-err,fold-regpreserve,lazy-release-sweep,sysinfo,getcpu,fault-signal,shell ;;
    aarch64) TARGET=aarch64-unknown-none; DEFAULT=shell,trace-err,wx-relaxed,getcpu ;;
    *) echo "ARCH must be amd64 or aarch64" >&2; exit 1 ;;
esac
FEATURES=${DENVER_FEATURES:-$DEFAULT}
cargo build -p denver --target "$TARGET" --release --features "$FEATURES"
mkdir -p "$HERE/out"
OUTK=$HERE/out/denver-$ARCH
cp "target/$TARGET/release/denver" "$OUTK"
if [ "$ARCH" = amd64 ] && command -v rust-readobj >/dev/null 2>&1 &&
   ! rust-readobj --elf-output-style=GNU --notes "$OUTK" | grep -q Xen; then
    echo "FATAL: no PVH note in $OUTK; QEMU -kernel would not boot it" >&2; exit 1
fi
# aarch64: QEMU passes the device tree only to a raw image, not an ELF (scripts/cargo_runner.sh does this too).
if [ "$ARCH" = aarch64 ]; then
    OBJCOPY=$(command -v rust-objcopy || command -v llvm-objcopy || command -v aarch64-linux-gnu-objcopy) ||
        { echo "need rust-objcopy (cargo install cargo-binutils) or llvm-objcopy" >&2; exit 1; }
    "$OBJCOPY" -O binary "$OUTK" "$OUTK.bin"
    OUTK=$OUTK.bin
fi
ls -l "$OUTK"
