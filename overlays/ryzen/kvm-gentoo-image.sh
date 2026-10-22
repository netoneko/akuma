#!/bin/sh
# Build the Gentoo KVM guest's disk from p7 (Gentoo, entry 17) onto p3, from Pop.
#
#   sh kvm-gentoo-image.sh [SIZE]        (root, in Pop; SIZE default 3G)
#   GUEST=gentoo sh kvm-akuma-stage.sh   (installs the kernel and the selector)
#   sh arm.sh 28
#
# p7 (which has errors; see below) is mounted read-only inside a private mount namespace (a plain mount propagates into
# the daemons' namespaces and the umount then fails with "in use"). Its files are copied into
# a fresh ext2 image, /boot/gentoo-guest.img on p3, via a staging copy and `mke2fs -d`; the guest's init is
# added to it. The hypervisor copies the image into RAM when the guest starts, so the guest
# never touches p7 and its writes are gone when it stops. p7 is unchanged.
set -eu
SIZE=${1:-3G}
SRC=${SRC:-$(cd "$(dirname "$0")/.." && pwd)}
INIT=$SRC/ryzen/gentoo/akuma-kvm-init.sh
[ -f "$INIT" ] || INIT=$(dirname "$0")/gentoo/akuma-kvm-init.sh
[ -f "$INIT" ] || { echo "no init script"; exit 1; }
cp "$INIT" /tmp/akuma-kvm-init.sh
export SIZE
unshare -m --propagation private sh -eu -c '
    mkdir -p /mnt/p7 /mnt/p3
    mountpoint -q /mnt/p3 || mount /dev/nvme0n1p3 /mnt/p3
    mount -o ro /dev/nvme0n1p7 /mnt/p7
    OUT=/mnt/p3/boot/gentoo-guest.img
    rm -f "$OUT.tmp"
    # p7 is "clean with errors" (invalid symlink inodes, mostly in /var/lib/box/layers, from the
    # old Akuma ext2 bitmap bug): `mke2fs -d` dies on the first with EUCLEAN. Read it with tar,
    # which reports each bad entry and carries on, into a staging directory on p3. p7 itself is
    # never written: this is the way to build the image without repairing the partition.
    STAGE=/mnt/p3/gentoo-stage
    rm -rf "$STAGE"; mkdir -p "$STAGE"
    tar -C /mnt/p7 --exclude=./proc --exclude=./sys --exclude=./var/lib/box -cf - . 2>/root/gentoo-tar.err | tar -C "$STAGE" -xpf - || true
    echo "tar complaints: $(wc -l < /root/gentoo-tar.err) (see /root/gentoo-tar.err)"
    mkdir -p "$STAGE/proc" "$STAGE/sys"
    # -i 8192: the portage tree alone is ~150k small files.
    mke2fs -q -t ext2 -i 8192 -L gentoo-kvm -d "$STAGE" "$OUT.tmp" "$SIZE"
    rm -rf "$STAGE"
    debugfs -w -R "write /tmp/akuma-kvm-init.sh akuma-kvm-init.sh" "$OUT.tmp" >/dev/null
    debugfs -w -R "set_inode_field /akuma-kvm-init.sh mode 0100755" "$OUT.tmp" >/dev/null
    e2fsck -fn "$OUT.tmp" | tail -2
    mv "$OUT.tmp" "$OUT"
    sync
    ls -l "$OUT"
    umount /mnt/p7
    umount /mnt/p3
'
echo "image built. Next: GUEST=gentoo sh kvm-akuma-stage.sh && sh arm.sh 28"
