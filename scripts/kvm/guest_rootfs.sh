#!/bin/sh
# Build the ext2 root image the Linux KVM rung boots from.
#
#   scripts/kvm/guest_rootfs.sh OUT.img BUSYBOX [INITTAB]
#
# BUSYBOX is a static x86_64 busybox. The image is 16 MiB ext2 with /sbin/init and
# /bin/sh as busybox, a /dev/console and /dev/null, and an /etc/inittab that
# (default) mounts /proc, prints markers, checks a read (md5 of busybox) and a
# write (a 256 KiB pattern file) and reboots. Needs e2fsprogs (mke2fs, debugfs).
# See docs/archive/AKUMA_KVM_LINUX_RUNG.md, "Rung 8".
set -eu
OUT=${1:?usage: guest_rootfs.sh OUT.img BUSYBOX [INITTAB]}
BB=${2:?usage: guest_rootfs.sh OUT.img BUSYBOX [INITTAB]}
INITTAB=${3:-}
E2=${E2FSPROGS:-$(dirname "$(command -v mke2fs 2>/dev/null || echo /opt/homebrew/opt/e2fsprogs/sbin/mke2fs)")}
T=$(mktemp -d)
trap 'rm -rf "$T"' EXIT
mkdir -p "$T/bin" "$T/sbin" "$T/etc" "$T/proc" "$T/sys" "$T/dev"
cp "$BB" "$T/bin/busybox"
chmod 755 "$T/bin/busybox"
ln -s ../bin/busybox "$T/sbin/init"
ln -s busybox "$T/bin/sh"
if [ -n "$INITTAB" ]; then
    cp "$INITTAB" "$T/etc/inittab"
else
    cat > "$T/etc/inittab" <<'TAB'
::sysinit:/bin/busybox mount -t proc proc /proc
::sysinit:/bin/busybox echo GUEST-ROOTFS-UP
::sysinit:/bin/busybox md5sum /bin/busybox
::sysinit:/bin/sh -c "busybox yes | busybox head -c 262144 > /wtest; busybox sync; busybox md5sum /wtest"
::sysinit:/bin/busybox echo GUEST-ROOTFS-DONE
::sysinit:/bin/busybox reboot -f
TAB
fi
rm -f "$OUT"
"$E2/mke2fs" -q -t ext2 -d "$T" -L guestroot "$OUT" 16M
# Device nodes need root on the host; debugfs makes them inside the image.
printf 'cd /dev\nmknod console c 5 1\nmknod null c 1 3\n' | "$E2/debugfs" -w -f - "$OUT" >/dev/null
echo "wrote $OUT"
