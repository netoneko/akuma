#!/bin/bash
# A/B `pin_reclaim` across two amd64 kernel binaries under local QEMU.
#
#   scripts/benchmarks/pin_reclaim_ab.sh <kernel-A> <kernel-B> [rootfs]
#
# Each arm gets its OWN COPY of the rootfs. The probe fills the filesystem and
# deletes it again, so a shared image would hand arm B whatever arm A's leak left
# behind — and "B reclaimed less" would be a property of the disk, not the
# kernel.
#
# # Why this does not call amd64/run.sh
#
# `run.sh` runs `cargo build` before it boots. Handing it a kernel binary staged
# at the target path does not pin that binary: the build overwrites it from
# whatever is in the working tree, so both arms run the same kernel and the A/B
# reports no difference — which reads exactly like a fix that does nothing. That
# trap has been paid for twice in this tree (`docs/archive/` on stale baked
# artifacts), so this drives QEMU directly with the same flags `run.sh` uses.
#
# `INITARGS` is **argv[1..]**, not argv[0]: the kernel supplies the program name
# itself. Passing the program's own name here makes it argv[1], which for this
# probe is the root directory — and it then finds nothing, creates nothing, and
# scores INCONCLUSIVE on both phases.
set -euo pipefail
cd "$(dirname "$0")/../.."

A="${1:?usage: pin_reclaim_ab.sh <kernel-A> <kernel-B> [rootfs]}"
B="${2:?usage: pin_reclaim_ab.sh <kernel-A> <kernel-B> [rootfs]}"
ROOT="${3:-target/x86_64-unknown-none/release/amd64-root.img}"
OUT="${OUT:-$(mktemp -d)}"
mkdir -p "$OUT"
MEMORY="${MEMORY:-2048}"
SMP="${SMP:-1}"

[ -f "$ROOT" ] || { echo "no rootfs at $ROOT — run amd64/mkdisk.sh first" >&2; exit 1; }

run_arm() {
    local label="$1" kernel="$2" img="$OUT/root-$1.img" log="$OUT/$1.log"
    cp "$ROOT" "$img"
    # No NIC: the probe needs no network, and leaving it out frees the host
    # forwards for whatever else is running on this machine.
    qemu-system-x86_64 \
        -M microvm -cpu max -global virtio-mmio.force-legacy=false \
        -kernel "$kernel" -m "$MEMORY" -smp "$SMP" \
        -drive "id=d0,file=$img,format=raw,if=none" \
        -device virtio-blk-device,drive=d0,bus=virtio-mmio-bus.0 \
        -append "virtio_mmio.device=512@0xfeb00000:5 init=/bin/pin_reclaim initargs=/tmp" \
        -serial mon:stdio -display none -no-reboot >"$log" 2>&1 &
    local pid=$!
    # The probe writes ~40 MB through a TCG-emulated virtio-blk; give it room,
    # but never wait forever — a wedge must report as a wedge.
    local waited=0
    while kill -0 "$pid" 2>/dev/null && [ "$waited" -lt 900 ]; do
        grep -aq "PIN_RECLAIM: done" "$log" && break
        sleep 3
        waited=$((waited + 3))
    done
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
    echo "=== $label ($kernel) ==="
    grep -a "PIN_RECLAIM" "$log" || echo "  NO PROBE OUTPUT (log: $log)"
}

run_arm A "$A"
run_arm B "$B"
echo
echo "logs in $OUT"
