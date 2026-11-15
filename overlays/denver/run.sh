#!/bin/sh
# Boot Denver under QEMU and run the Clojure web server inside it.
#
#   overlays/denver/run.sh                 # amd64:   http://localhost:8080/ ; Ctrl-A X quits QEMU
#   ARCH=aarch64 overlays/denver/run.sh    # aarch64: same, on QEMU's `virt` machine
#
# amd64: QEMU stands in for the KVM host Denver was developed on (microvm + PVH + virtio-MMIO, the
# local machine amd64/run.sh uses). No -accel: on a Mac this is x86 under TCG, correct but slow.
# aarch64: `-machine virt` with HVF on an Apple Silicon Mac (TCG elsewhere), the machine
# scripts/cargo_runner.sh uses for the main kernel.
# The guest's init is the JDK itself: no shell, no sshd.
#
# Env knobs: ARCH (amd64|aarch64)  MEMORY (MiB, default 3072)  SMP (default 1)  HTTP_PORT (default 8080)
#            DENVER_INITARGS  comma-separated java arguments (default: the Clojure server)
#            DENVER_INIT      the init program (default /opt/jdk/bin/java)
#            DENVER_DISK      the image to attach (it is attached with snapshot=on: a run never writes it)
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
OUT=$HERE/out
ARCH=${ARCH:-amd64}
KERNEL=$OUT/denver-$ARCH
[ "$ARCH" = aarch64 ] && KERNEL=$KERNEL.bin
DISK=${DENVER_DISK:-$OUT/denver-clojure-$ARCH.img}
[ -f "$KERNEL" ] || { echo "no $KERNEL: run ARCH=$ARCH overlays/denver/build-kernel.sh" >&2; exit 1; }
[ -f "$DISK" ] || { echo "no $DISK: run ARCH=$ARCH overlays/denver/build-image.sh" >&2; exit 1; }

CP=/opt/clojure/clojure-1.12.0.jar:/opt/clojure/spec.alpha-0.5.238.jar:/opt/clojure/core.specs.alpha-0.4.74.jar
INIT=${DENVER_INIT:-/opt/jdk/bin/java}
# aarch64: the kernel ignores mmap address hints, which HotSpot's compressed class space needs there
# (docs/archive/DENVER_BRANCH_FIXES.md), so the JVM runs with plain 64-bit pointers.
A64OPT=; [ "$ARCH" = aarch64 ] && A64OPT=-XX:-UseCompressedClassPointers,-XX:-UseCompressedOops,-XX:+UseSerialGC,-Xmx512m,
INITARGS=${DENVER_INITARGS-${A64OPT}-cp,$CP,clojure.main,/opt/server.clj}
MEMORY=${MEMORY:-3072}
SMP=${SMP:-1}
PORT=${HTTP_PORT:-8080}

case "$ARCH" in
amd64)
    exec qemu-system-x86_64 \
        -M microvm -cpu max -global virtio-mmio.force-legacy=false \
        -m "$MEMORY" -smp "$SMP" -kernel "$KERNEL" \
        -drive id=d0,file="$DISK",format=raw,if=none,snapshot=on \
        -device virtio-blk-device,drive=d0,bus=virtio-mmio-bus.0 \
        -netdev user,id=n0,hostfwd=tcp::"$PORT"-:8080 \
        -device virtio-net-device,netdev=n0,bus=virtio-mmio-bus.1 \
        -append "virtio_mmio.device=512@0xfeb00000:5 virtio_mmio.device=512@0xfeb00200:6 smp=$SMP init=$INIT initargs=$INITARGS" \
        -serial mon:stdio -display none -no-reboot
    ;;
aarch64)
    if [ "$(uname -s)" = Darwin ] && [ "$(uname -m)" = arm64 ] && [ "${HVF:-1}" != 0 ]; then
        ACCEL="-accel hvf -cpu host"
    else
        ACCEL="-accel tcg -cpu max"
    fi
    # shellcheck disable=SC2086  # ACCEL is deliberately word-split
    exec qemu-system-aarch64 \
        -semihosting -machine virt,gic-version=3 $ACCEL \
        -smp "$SMP" -m "$MEMORY" -global virtio-mmio.force-legacy=false \
        -netdev user,id=n0,hostfwd=tcp::"$PORT"-:8080 \
        -device virtio-net-device,netdev=n0,bus=virtio-mmio-bus.0 \
        -drive id=d0,file="$DISK",format=raw,if=none,snapshot=on \
        -device virtio-blk-device,drive=d0,bus=virtio-mmio-bus.1 \
        -device virtio-rng-device,bus=virtio-mmio-bus.2 \
        -kernel "$KERNEL" -append "smp=$SMP init=$INIT initargs=$INITARGS" \
        -serial mon:stdio -display none -no-reboot
    ;;
*) echo "ARCH must be amd64 or aarch64" >&2; exit 1 ;;
esac
