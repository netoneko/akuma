#!/bin/sh
# Stage the "Akuma boots under Akuma's hypervisor" run onto ryzen's p3, from Pop.
#
#   BRANCH=kvm-part-1 sh build.sh                 (netoneko, ryzen: builds the kernel ON the ryzen)
#   sh kvm-akuma-stage.sh [KERNEL]                (root, in Pop; p3 not mounted by Akuma; SRC=<clone> W=<dir> override)
#   sh arm.sh 28                                  (boots entry 28 once; ~3 min later Pop is back)
#
# Does, each step safe to repeat:
#   1. mounts p3 at /mnt/p3 (rw) unless it already is;
#   2. keeps the current /boot/akuma-amd64.verify as .verify.pre-akuma-guest (once),
#      then installs the freshly built no-tests kernel as /boot/akuma-amd64.verify
#      (entry 28 boots .verify when it exists) and as /boot/akuma-guest (the guest);
#   3. builds the guest's 16 MiB ext2 disk (busybox + /etc/guest.sh) with
#      scripts/kvm/akuma_guest_rootfs.sh and writes it to /boot/akuma-guest-root.img;
#   4. writes "akuma" to /boot/kvm-guest, which makes `kvmlinux` (entry 28) run the Akuma
#      guest; `rm /mnt/p3/boot/kvm-guest` gives the Linux guest back;
#   5. checks /boot/vmlinux is still there (the Linux guest's kernel) and unmounts p3.
#
# Rollback: arm another entry (the live /boot/akuma-amd64 is never touched).
# Verdict: on the booted Akuma, `dmesg | grep -a '\[KVM\] akuma guest'` ->
#   [KVM] akuma guest: PASS (banner=yes, init=yes)
# docs/runbooks/kvm-linux-guest.md § 3, docs/archive/AKUMA_KVM_AKUMA_GUEST.md.
set -eu
W=${W:-/home/netoneko/akuma-metal}
SRC=${SRC:-$W/akuma}   # the clone the kernel was built in (and the scripts come from)
K=${1:-$W/out/akuma-amd64}
BB=$SRC/target/x86_64-unknown-none/release/busybox-x86_64
P=/mnt/p3

[ -f "$K" ] || { echo "no kernel at $K (run build.sh first)"; exit 1; }
[ -f "$BB" ] || { echo "no busybox at $BB (build.sh's mkdisk step fetches it)"; exit 1; }
[ -f "$SRC/scripts/kvm/akuma_guest_rootfs.sh" ] || { echo "the clone at $SRC lacks scripts/kvm (wrong BRANCH?)"; exit 1; }

mountpoint -q $P || { mkdir -p $P; mount /dev/nvme0n1p3 $P; }
[ -f $P/boot/vmlinux ] || echo "note: $P/boot/vmlinux is missing; the Linux guest will not load (the Akuma guest does not need it)"

if [ -f $P/boot/akuma-amd64.verify ] && [ ! -f $P/boot/akuma-amd64.verify.pre-akuma-guest ]; then
    cp -p $P/boot/akuma-amd64.verify $P/boot/akuma-amd64.verify.pre-akuma-guest
fi
cp "$K" $P/boot/akuma-amd64.verify
cp "$K" $P/boot/akuma-guest

T=$(mktemp -d)
E2FSPROGS=/sbin sh "$SRC/scripts/kvm/akuma_guest_rootfs.sh" "$T/guest.img" "$BB"
cp "$T/guest.img" $P/boot/akuma-guest-root.img
rm -rf "$T"
printf akuma > $P/boot/kvm-guest

sync
ls -la $P/boot/akuma-amd64.verify $P/boot/akuma-guest $P/boot/akuma-guest-root.img $P/boot/kvm-guest
umount $P
echo "staged. Next: sh arm.sh 28"
