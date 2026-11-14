#!/bin/sh
# Build the Denver kernel (the Clojure/JVM gate feature set) from this checkout.
#
#   overlays/denver/build-kernel.sh          -> overlays/denver/out/denver
#
# The feature set is the one the experiment's last gate used
# (denver/docs/archive/CLOJURE_AND_JAVAC_ON_DENVER.md); every feature is off by default
# in the crates, so this is the only place that turns them on together.
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
cd "$HERE/../.."
FEATURES=${DENVER_FEATURES:-wx-relaxed,vsyscall,procfs-diag,trace-err,fold-regpreserve,lazy-release-sweep,sysinfo,getcpu,fault-signal,shell}
cargo build -p denver --target x86_64-unknown-none --release --features "$FEATURES"
mkdir -p "$HERE/out"
cp target/x86_64-unknown-none/release/denver "$HERE/out/denver"
if command -v rust-readobj >/dev/null 2>&1 &&
   ! rust-readobj --elf-output-style=GNU --notes "$HERE/out/denver" | grep -q Xen; then
    echo "FATAL: no PVH note in out/denver; QEMU -kernel would not boot it" >&2; exit 1
fi
ls -l "$HERE/out/denver"
