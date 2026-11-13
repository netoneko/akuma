#!/bin/bash
# A second, independent Taoxichuan guest on the ryzen's Pop OS side, for experiments while
# another agent drives the main one (scripts/kvm/taoxichuan_pop.sh, port 2224).
#
#   scripts/kvm/taoxichuan_pop_side.sh boot [fresh]   # freeze a copy of the current kernel, boot it
#   scripts/kvm/taoxichuan_pop_side.sh stop           # kill THIS guest only (its own pidfile)
#   scripts/kvm/taoxichuan_pop_side.sh log [N]        # tail this guest's serial console
#
# Shares nothing writable with the main rig: its own run dir, overlay, pidfile and port. The
# kernel is COPIED from the main rig's build output at boot, so a rebuild there cannot change
# the binary under this guest; the copy's provenance goes to $RUN/kernel.txt. The disk is the
# same Akuma root partition through this guest's own qcow2 overlay (QEMU opens the backing
# read-only, so two guests can share it). Unlike the main script it never unmounts /mnt/p3:
# it refuses to boot if p3 is mounted, because that mount belongs to someone else.
#
# Reach it from the LAN: ssh -p 2234 root@192.168.1.126
# Env: SIDE_RUN (/root/tx-side-run), SIDE_PORT (2234), SIDE_MEM (3072), SIDE_SMP (1),
#      SIDE_KERNEL_SRC (/root/tx-target/x86_64-unknown-none/release/taoxichuan),
#      SIDE_DISK (/dev/nvme0n1p3), SIDE_APPEND (extra kernel command line).
set -eu

RUN=${SIDE_RUN:-/root/tx-side-run}
PORT=${SIDE_PORT:-2234}
MEM=${SIDE_MEM:-3072}
SMP=${SIDE_SMP:-1}
SRC=${SIDE_KERNEL_SRC:-/root/tx-target/x86_64-unknown-none/release/taoxichuan}
DISK=${SIDE_DISK:-/dev/nvme0n1p3}
KERNEL=$RUN/taoxichuan
OVERLAY=$RUN/overlay.qcow2

stop() {
    if [ -f "$RUN/qemu.pid" ]; then
        kill "$(cat "$RUN/qemu.pid")" 2>/dev/null || true
        rm -f "$RUN/qemu.pid"
    fi
}

boot() {
    [ -x "$SRC" ] || { echo "no kernel at $SRC" >&2; exit 1; }
    if grep -q "^$DISK " /proc/mounts; then
        echo "refusing: $DISK is mounted; it belongs to someone else" >&2; exit 1
    fi
    mkdir -p "$RUN"
    stop
    cp "$SRC" "$KERNEL"
    { echo "copied from $SRC"; stat -c 'built %y, %s bytes' "$SRC"; sha256sum "$KERNEL"; } > "$RUN/kernel.txt"
    local cl="init=/bin/sshd initargs=--port,22 epoch=$(date +%s) ${SIDE_APPEND:-}"
    for i in 0 1 2 3; do
        cl="$cl virtio_mmio.device=512@$(printf '0x%x' $((0xfeb00000 + i * 512))):$((5 + i))"
    done
    if [ "${1:-}" = fresh ] || [ ! -f "$OVERLAY" ]; then
        rm -f "$OVERLAY"
        qemu-img create -q -f qcow2 -b "$DISK" -F raw "$OVERLAY"
    fi
    : > "$RUN/console.log"
    qemu-system-x86_64 -M microvm,accel=kvm -enable-kvm -cpu host -smp "$SMP" -m "$MEM" \
        -kernel "$KERNEL" -append "$cl" -nodefaults -no-reboot -display none \
        -serial "file:$RUN/console.log" \
        -drive "file=$OVERLAY,format=qcow2,if=none,id=d0,cache=unsafe" \
        -device virtio-blk-device,drive=d0,bus=virtio-mmio-bus.0 \
        -netdev "user,id=n0,hostfwd=tcp::$PORT-:22" \
        -device virtio-net-device,netdev=n0,bus=virtio-mmio-bus.1 \
        -pidfile "$RUN/qemu.pid" -daemonize
    echo "side guest up (pid $(cat "$RUN/qemu.pid")), ssh -p $PORT root@<this host>; console: $RUN/console.log"
    cat "$RUN/kernel.txt"
}

case "${1:-}" in
    boot) boot "${2:-}" ;;
    stop) stop ;;
    log) tail -n "${2:-40}" "$RUN/console.log" ;;
    *) sed -n '2,20p' "$0"; exit 2 ;;
esac
