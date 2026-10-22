#!/bin/sh
# Rehearse the Linux KVM rung under QEMU (TCG, no ryzen needed).
#
#   scripts/kvm/linux_rehearse.sh LOG [SECONDS]
#
# Env: KERNEL   the amd64 kernel (default target/x86_64-unknown-none/release/akuma-amd64,
#               built with: cargo build -p akuma-amd64 --target x86_64-unknown-none
#               --release --features no-tests)
#      ROOT     the host's root ext2 image; it must contain /boot/vmlinux
#               (the Firecracker CI vmlinux-6.1.128; `debugfs -w -R "write ..."`)
#      GUEST    the guest's root image (scripts/kvm/guest_rootfs.sh); optional —
#               without it the guest has no disk and panics at "VFS: Unable to mount root fs"
# Copies both images first (the run writes to them), stops at the verdict line.
# The guest disk is the machine's *second* virtio-blk device — the first stays
# the host's root, which the guest is never given.
set -eu
LOG=${1:?usage: linux_rehearse.sh LOG [SECONDS]}
MAX=${2:-150}
KERNEL=${KERNEL:-target/x86_64-unknown-none/release/akuma-amd64}
ROOT=${ROOT:?set ROOT to the host root image}
W=$(mktemp -d)
trap 'rm -rf "$W"' EXIT
cp "$ROOT" "$W/root.img"
DISK1=
if [ -n "${GUEST:-}" ]; then
    cp "$GUEST" "$W/guest.img"
    DISK1="-drive id=d1,file=$W/guest.img,format=raw,if=none -device virtio-blk-device,drive=d1,bus=virtio-mmio-bus.1"
fi
# shellcheck disable=SC2086  # DISK1 is deliberately word-split
qemu-system-x86_64 -M microvm -cpu EPYC-Milan,svm=on -global virtio-mmio.force-legacy=false -m 2048 -smp 1 \
    -kernel "$KERNEL" \
    -drive id=d0,file="$W/root.img",format=raw,if=none -device virtio-blk-device,drive=d0,bus=virtio-mmio-bus.0 \
    $DISK1 \
    -append "virtio_mmio.device=512@0xfeb00000:5 virtio_mmio.device=512@0xfeb00200:6 init=/bin/hello kvmtest kvmlinux noladder" \
    -serial file:"$LOG" -display none -no-reboot &
Q=$!
i=0
while [ "$i" -lt "$MAX" ] && kill -0 "$Q" 2>/dev/null && ! grep -aq "linux guest:" "$LOG" 2>/dev/null; do
    sleep 1
    i=$((i + 1))
done
kill "$Q" 2>/dev/null || true
wait "$Q" 2>/dev/null || true
grep -a "\[KVM\] linux guest:" "$LOG" || { echo "no verdict in $LOG"; exit 1; }
# With a guest disk the real test is that its init ran to the end.
if [ -n "${GUEST:-}" ]; then
    if grep -aq "GUEST-ROOTFS-DONE" "$LOG" && ! grep -aq "\[KVM\] linux guest: FAIL" "$LOG"; then
        echo "guest rootfs: init ran to completion"
    else
        echo "guest rootfs: FAIL (no GUEST-ROOTFS-DONE from the guest's init)"
        exit 1
    fi
fi
