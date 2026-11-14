#!/bin/sh
# Boot Denver under QEMU and run the Clojure web server inside it.
#
#   overlays/denver/run.sh          # serves on http://localhost:8080/ ; Ctrl-A X quits QEMU
#
# QEMU stands in for the KVM host Denver was developed on (microvm + PVH + virtio-MMIO, the same
# local machine amd64/run.sh uses; no -accel, so on a Mac this is x86 under TCG: correct, slow).
# The guest's init is the JDK itself: no shell, no sshd.
#
# Env knobs: MEMORY (MiB, default 3072)  SMP (default 1)  HTTP_PORT (default 8080)
#            DENVER_INITARGS  comma-separated java arguments (default: the Clojure server)
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
OUT=$HERE/out
[ -f "$OUT/denver" ] || { echo "no $OUT/denver: run overlays/denver/build-kernel.sh" >&2; exit 1; }
[ -f "$OUT/denver-clojure.img" ] || { echo "no image: run overlays/denver/build-image.sh" >&2; exit 1; }

# A wildcard classpath, on purpose: a single boot argument of ~110 characters lost its last entry (the
# third jar was never found), so the kernel command line must keep each argument short.
CP='/opt/clojure/*'
INITARGS=${DENVER_INITARGS:--cp,$CP,clojure.main,/opt/server.clj}
exec qemu-system-x86_64 \
    -M microvm -cpu max -global virtio-mmio.force-legacy=false \
    -m "${MEMORY:-3072}" -smp "${SMP:-1}" \
    -kernel "$OUT/denver" \
    -drive id=d0,file="${DENVER_DISK:-$OUT/denver-clojure.img}",format=raw,if=none,snapshot=on \
    -device virtio-blk-device,drive=d0,bus=virtio-mmio-bus.0 \
    -netdev user,id=n0,hostfwd=tcp::"${HTTP_PORT:-8080}"-:8080 \
    -device virtio-net-device,netdev=n0,bus=virtio-mmio-bus.1 \
    -append "virtio_mmio.device=512@0xfeb00000:5 virtio_mmio.device=512@0xfeb00200:6 smp=${SMP:-1} init=/opt/jdk/bin/java initargs=$INITARGS" \
    -serial mon:stdio -display none -no-reboot
