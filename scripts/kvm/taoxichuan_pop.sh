#!/bin/bash
# Run Taoxichuan under KVM on the ryzen while it is booted into Pop OS.
#
#   scripts/kvm/taoxichuan_pop.sh build     # check out + build the kernel here
#   scripts/kvm/taoxichuan_pop.sh boot [fresh]  # boot it on the Akuma root (p3) through a persistent overlay
#   scripts/kvm/taoxichuan_pop.sh stop      # kill that guest
#   scripts/kvm/taoxichuan_pop.sh log [N]   # tail the serial console
#
# No image is built: the guest's disk is the ryzen's own Akuma root partition
# (`/dev/nvme0n1p3`, label AKUMA-RYZEN) - the same ext2 the metal kernel boots,
# with sshd, busybox, gcc and the nightly Rust toolchain already on it. It is
# attached through a qcow2 overlay: QEMU reads the partition and sends every
# write to the overlay, so a Taoxichuan run can never touch the real root.
# Never mount p3 read-write while a guest is up.
#
# Reach the guest from anywhere on the LAN (the Mac, with its normal key -
# /etc/sshd/authorized_keys on p3 is the user's):
#   ssh -p 2224 root@192.168.1.126 'uname -a'
#
# Env: TX_BRANCH (default rust-org-il-experiment), TX_FEATURES (no-tests,trace-err),
#      TX_PORT (2224), TX_MEM (8192), TX_DISK (/dev/nvme0n1p3), TX_SMP (1),
#      TX_APPEND (extra kernel command line, e.g. "mmdbg=0").
set -eu

REPO=${TX_REPO:-/root/akuma}
WT=${TX_WT:-/root/tx-wt}
TARGET=${TX_TARGET:-/root/tx-target}
BRANCH=${TX_BRANCH:-rust-org-il-experiment}
FEATURES=${TX_FEATURES:-no-tests,trace-err}
PORT=${TX_PORT:-2224}
MEM=${TX_MEM:-8192}
SMP=${TX_SMP:-1}
DISK=${TX_DISK:-/dev/nvme0n1p3}
RUN=${TX_RUN:-/root/tx-run}
KERNEL=$TARGET/x86_64-unknown-none/release/taoxichuan
OVERLAY=$RUN/overlay.qcow2
export PATH=$HOME/.cargo/bin:$PATH

build() {
    git -C "$REPO" fetch -q origin "$BRANCH"
    # FETCH_HEAD is per-worktree: resolve it in the main repo, hand over the sha.
    local sha
    sha=$(git -C "$REPO" rev-parse FETCH_HEAD)
    if [ -d "$WT/.git" ] || [ -f "$WT/.git" ]; then
        git -C "$WT" checkout -q --detach "$sha"
    else
        git -C "$REPO" worktree add -q --detach "$WT" "$sha"
    fi
    echo "checked out $(git -C "$WT" log --oneline -1)"
    (cd "$WT/taoxichuan" &&
        cargo build --release --target x86_64-unknown-none \
            --features "$FEATURES" --target-dir "$TARGET")
    ls -l "$KERNEL"
}

stop() {
    # Only our own guest: matched on its pidfile, never `pkill -f qemu`.
    if [ -f "$RUN/qemu.pid" ]; then
        kill "$(cat "$RUN/qemu.pid")" 2>/dev/null || true
        rm -f "$RUN/qemu.pid"
    fi
}

boot() {
    [ -x "$KERNEL" ] || { echo "no kernel: run '$0 build' first" >&2; exit 1; }
    mkdir -p "$RUN"
    stop
    if mountpoint -q /mnt/p3 2>/dev/null; then umount /mnt/p3; fi
    # Same virtio-mmio slots as the TCG rig (runqemu_s1e.sh): blk at 0, net at 1.
    # `epoch=` anchors the guest wall clock (no RTC read yet): TLS refuses a
    # certificate "not yet valid" when the guest believes it is 1970.
    local cl="init=/bin/sshd initargs=--port,22 epoch=$(date +%s) ${TX_APPEND:-}"
    for i in 0 1 2 3; do
        cl="$cl virtio_mmio.device=512@$(printf '0x%x' $((0xfeb00000 + i * 512))):$((5 + i))"
    done
    # A qcow2 overlay on the Akuma root: every write lands in the overlay, p3
    # is never touched, and the overlay SURVIVES a reboot - the project you
    # staged in /root stays put across kernel rebuilds. `boot fresh` recreates it.
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
    echo "guest up (pid $(cat "$RUN/qemu.pid")), ssh -p $PORT root@<this host>; console: $RUN/console.log"
}

case "${1:-}" in
    build) build ;;
    boot) boot "${2:-}" ;;
    stop) stop ;;
    log) tail -n "${2:-40}" "$RUN/console.log" ;;
    *) sed -n '2,22p' "$0"; exit 2 ;;
esac
