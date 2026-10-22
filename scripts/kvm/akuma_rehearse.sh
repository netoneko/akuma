#!/bin/sh
# Rehearse the Akuma-as-guest KVM phase under QEMU (TCG, no ryzen needed).
#
#   scripts/kvm/akuma_rehearse.sh LOG [SECONDS]
#
# Env: KERNEL  the amd64 kernel (default target/x86_64-unknown-none/release/akuma-amd64,
#              built with: cargo build -p akuma-amd64 --target x86_64-unknown-none
#              --release --features no-tests). It is both the host and, copied to
#              /boot/akuma-guest, the guest.
#      BUSYBOX static x86_64 busybox (default target/x86_64-unknown-none/release/busybox-x86_64)
#      MEM     QEMU RAM in MiB (default 3072: the host takes its own heap and the
#              guest takes 768 MiB contiguous)
#      HELLO=1     the tiny-root guest: /bin/hello is its only program (akuma_hello_rootfs.sh)
#      SMP=1       the busybox guest with two virtual processors (`kvmsmp`; passes only when
#                  the guest reports "smp:  2 cpus online")
#      SELECTOR=1  boot with `kvmlinux` and /boot/kvm-guest=akuma, as ryzen's entry 28 does
#      EXTRA   extra host command-line tokens (e.g. "nosmp")
# Builds both images fresh in a temp dir, stops at the verdict line, and checks
# the guest-written file from the host side (debugfs) when the run passed.
set -eu
LOG=${1:?usage: akuma_rehearse.sh LOG [SECONDS]}
MAX=${2:-240}
KERNEL=${KERNEL:-target/x86_64-unknown-none/release/akuma-amd64}
BUSYBOX=${BUSYBOX:-target/x86_64-unknown-none/release/busybox-x86_64}
EXTRA=${EXTRA:-}
HERE=$(dirname "$0")
E2=${E2FSPROGS:-/opt/homebrew/opt/e2fsprogs/sbin}
W=$(mktemp -d)
trap 'rm -rf "$W"' EXIT
mkdir -p "$W/host/boot"
cp "$KERNEL" "$W/host/boot/akuma-guest"
FLAG=kvmakuma
WHO=akuma
if [ -n "${HELLO:-}" ]; then
    FLAG=kvmhello
    WHO=akuma-hello
fi
if [ -n "${SMP:-}" ]; then
    FLAG=kvmsmp
    WHO=akuma-smp
fi
if [ -n "${SELECTOR:-}" ]; then
    printf '%s' "$WHO" > "$W/host/boot/kvm-guest"
    FLAG=kvmlinux
fi
"$E2/mke2fs" -q -t ext2 -d "$W/host" -L kvmhost "$W/root.img" 48M
if [ -n "${HELLO:-}" ]; then
    sh "$HERE/akuma_hello_rootfs.sh" "$W/guest.img" >/dev/null
else
    sh "$HERE/akuma_guest_rootfs.sh" "$W/guest.img" "$BUSYBOX" >/dev/null
fi
# shellcheck disable=SC2086  # EXTRA is deliberately word-split
qemu-system-x86_64 -M microvm -cpu EPYC-Milan,svm=on -global virtio-mmio.force-legacy=false -m ${MEM:-3072} -smp 1 \
    -kernel "$KERNEL" \
    -drive id=d0,file="$W/root.img",format=raw,if=none -device virtio-blk-device,drive=d0,bus=virtio-mmio-bus.0 \
    -drive id=d1,file="$W/guest.img",format=raw,if=none -device virtio-blk-device,drive=d1,bus=virtio-mmio-bus.1 \
    -append "virtio_mmio.device=512@0xfeb00000:5 virtio_mmio.device=512@0xfeb00200:6 kvmtest $FLAG noladder $EXTRA" \
    -serial file:"$LOG" -display none -no-reboot &
Q=$!
i=0
while [ "$i" -lt "$MAX" ] && kill -0 "$Q" 2>/dev/null && ! grep -aq "$WHO guest:" "$LOG" 2>/dev/null; do
    sleep 1
    i=$((i + 1))
done
kill "$Q" 2>/dev/null || true
wait "$Q" 2>/dev/null || true
grep -a "\[KVM\] $WHO guest:" "$LOG" || { echo "no verdict in $LOG"; exit 1; }
# The write went through the guest's driver, the hypervisor's virtio model and
# the host's: read the file back with an independent tool.
if grep -aq "\[KVM\] $WHO guest: PASS" "$LOG" && [ -z "${HELLO:-}" ]; then
    "$E2/debugfs" -R "dump /wtest $W/wtest" "$W/guest.img" >/dev/null 2>&1 || true
    if [ -f "$W/wtest" ]; then
        echo "host md5 of the guest's /wtest: $(md5 -q "$W/wtest" 2>/dev/null || md5sum "$W/wtest" | cut -d' ' -f1)"
    fi
    "$E2/e2fsck" -fn "$W/guest.img" 2>&1 | tail -2
fi
