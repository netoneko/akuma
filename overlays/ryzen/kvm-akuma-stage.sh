#!/bin/sh
# Stage the "Akuma boots under Akuma's hypervisor" run onto ryzen's p3, from Pop.
#
#   BRANCH=kvm-part-1 sh build.sh                 (netoneko, ryzen: builds the kernel ON the ryzen)
#   GUEST=hello sh scripts/kvm/akuma_hello_rootfs.sh $W/out/akuma-hello-root.img   (netoneko, optional)
#   [GUEST=hello|linux|linux-apic] sh kvm-akuma-stage.sh [KERNEL]                (root, in Pop; p3 not mounted by Akuma; SRC=<clone> W=<dir> override)
#   sh arm.sh 28                                  (boots entry 28 once; ~3 min later Pop is back)
#
# Does, each step safe to repeat:
#   1. mounts p3 at /mnt/p3 (rw) unless it already is;
#   2. keeps the current /boot/akuma-amd64.verify as .verify.pre-akuma-guest (once),
#      then installs the freshly built no-tests kernel as /boot/akuma-amd64.verify
#      (entry 28 boots .verify when it exists) and as /boot/akuma-guest (the guest);
#   3. builds the guest's 16 MiB ext2 disk (busybox + /etc/guest.sh) with
#      scripts/kvm/akuma_guest_rootfs.sh and writes it to /boot/akuma-guest-root.img;
#      with GUEST=hello it instead installs the prebuilt 2 MiB hello-world root
#      ($HELLO_IMG, default $W/out/akuma-hello-root.img) as /boot/akuma-hello-root.img;
#   4. writes "akuma" (or "akuma-hello") to /boot/kvm-guest, which makes `kvmlinux`
#      (entry 28) run that Akuma guest; `rm /mnt/p3/boot/kvm-guest` gives the Linux
#      guest back;
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
GUEST=${GUEST:-busybox}
HELLO_IMG=${HELLO_IMG:-$W/out/akuma-hello-root.img}

[ -f "$K" ] || { echo "no kernel at $K (run build.sh first)"; exit 1; }
if [ "$GUEST" = linux ] || [ "$GUEST" = linux-apic ]; then
    :   # the Linux guests need no image staged: /boot/vmlinux and /boot/guest-root.img are already on p3
elif [ "$GUEST" = hello ]; then
    [ -f "$HELLO_IMG" ] || { echo "no hello root at $HELLO_IMG (scripts/kvm/akuma_hello_rootfs.sh builds it)"; exit 1; }
else
    [ -f "$BB" ] || { echo "no busybox at $BB (build.sh's mkdisk step fetches it)"; exit 1; }
fi
[ -f "$SRC/scripts/kvm/akuma_guest_rootfs.sh" ] || { echo "the clone at $SRC lacks scripts/kvm (wrong BRANCH?)"; exit 1; }

mountpoint -q $P || { mkdir -p $P; mount /dev/nvme0n1p3 $P; }
[ -f $P/boot/vmlinux ] || echo "note: $P/boot/vmlinux is missing; the Linux guest will not load (the Akuma guest does not need it)"

if [ -f $P/boot/akuma-amd64.verify ] && [ ! -f $P/boot/akuma-amd64.verify.pre-akuma-guest ]; then
    cp -p $P/boot/akuma-amd64.verify $P/boot/akuma-amd64.verify.pre-akuma-guest
fi
cp "$K" $P/boot/akuma-amd64.verify
cp "$K" $P/boot/akuma-guest

if [ "$GUEST" = linux ]; then
    rm -f $P/boot/kvm-guest
elif [ "$GUEST" = linux-apic ]; then
    printf linux-apic > $P/boot/kvm-guest
elif [ "$GUEST" = hello ]; then
    cp "$HELLO_IMG" $P/boot/akuma-hello-root.img
    printf akuma-hello > $P/boot/kvm-guest
else
    T=$(mktemp -d)
    E2FSPROGS=/sbin sh "$SRC/scripts/kvm/akuma_guest_rootfs.sh" "$T/guest.img" "$BB"
    cp "$T/guest.img" $P/boot/akuma-guest-root.img
    rm -rf "$T"
    printf akuma > $P/boot/kvm-guest
fi

sync
ls -la $P/boot/akuma-amd64.verify $P/boot/akuma-guest $P/boot/akuma-*-root.img $P/boot/kvm-guest 2>&1
umount $P
echo "staged. Next: sh arm.sh 28"
