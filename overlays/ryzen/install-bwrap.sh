#!/bin/sh
# overlays/ryzen — put a staged kernel, herd and bubblewrap on p3 (Akuma's root) from Pop. Run as root, Akuma NOT running.
#
#   sudo sh overlays/ryzen/install-bwrap.sh            # kernel -> p3:/boot/akuma-amd64.verify (primary untouched)
#   sudo sh overlays/ryzen/install-bwrap.sh --primary  # kernel -> p3:/boot/akuma-amd64 as well (the previous one -> .prev)
#   sudo sh overlays/ryzen/arm.sh 26                   # boot the VERIFY entry once (smp=8, wifi, sshd, stays up)
#
# Then confirm the boot by the commit in `uname -v` (it must be the one in $STAGE/COMMIT), run `scripts/bwrap_suite.py`
# against the box (docs/archive/BWRAP_ON_BOXES.md), and promote with --primary only after it has proven itself.
#
# What it writes, all on p3: /boot/akuma-amd64.verify, /bin/herd (previous kept as /bin/herd.prev), and Alpine's
# bubblewrap (/usr/bin/bwrap, /usr/lib/libcap.so.2, /lib/ld-musl-x86_64.so.1) **only where absent**; plus the mount
# points a sandbox needs, /dev /proc /sys /run /tmp, if the root lacks them (`bwrap --ro-bind / /` cannot create a
# mount point under a read-only bind of `/`, so they must already be real directories on the root filesystem).
# It never touches the ESP, GRUB, p1-p2, p4-p7, or any other file on p3. The stage is prepared by hand (see
# docs/archive/BWRAP_ON_BOXES.md); every file is checked against $STAGE/MANIFEST.md5 first.
set -e
PART=${PART:-/dev/nvme0n1p3}
LABEL=AKUMA-RYZEN
STAGE=${STAGE:-/home/netoneko/akuma-stage}
M=/mnt/akuma-p3
PRIMARY=0
[ "$1" = "--primary" ] && PRIMARY=1

[ "$(id -u)" = 0 ] || { echo "run as root" >&2; exit 1; }
[ "$(blkid -s LABEL -o value "$PART" 2>/dev/null)" = "$LABEL" ] || { echo "$PART is not labelled $LABEL: refusing" >&2; exit 1; }
if findmnt -rn "$PART" >/dev/null 2>&1; then echo "$PART is already mounted: refusing" >&2; exit 1; fi
[ -s "$STAGE/akuma-amd64" ] && [ -s "$STAGE/MANIFEST.md5" ] || { echo "no stage at $STAGE" >&2; exit 1; }
(cd "$STAGE" && md5sum -c --quiet MANIFEST.md5) || { echo "stage does not match its manifest" >&2; exit 1; }
od -A d -t x4 -N 32768 "$STAGE/akuma-amd64" | grep -qi e85250d6 || { echo "no multiboot2 header in the kernel" >&2; exit 1; }

mkdir -p "$M"
mount -t ext2 "$PART" "$M"
trap 'sync; umount "$M"' EXIT
[ -d "$M/boot" ] && [ -d "$M/bin" ] || { echo "p3 does not look like Akuma's root: refusing" >&2; exit 1; }

put() { # put <src> <dest-on-p3> <mode>: atomic, via a temp name beside the destination
    install -m "$3" "$1" "$M$2.new" && mv -f "$M$2.new" "$M$2"
}

# kernel
put "$STAGE/akuma-amd64" /boot/akuma-amd64.verify 755
if [ "$PRIMARY" = 1 ]; then
    [ -f "$M/boot/akuma-amd64" ] && cp -f "$M/boot/akuma-amd64" "$M/boot/akuma-amd64.prev"
    put "$STAGE/akuma-amd64" /boot/akuma-amd64 755
fi

# herd (init). The previous one is kept beside it.
[ -f "$M/bin/herd" ] && cp -f "$M/bin/herd" "$M/bin/herd.prev"
put "$STAGE/herd" /bin/herd 755

# mount points a sandbox needs
for d in dev proc sys run; do [ -d "$M/$d" ] || { mkdir "$M/$d" && chmod 755 "$M/$d"; }; done
[ -d "$M/tmp" ] || { mkdir "$M/tmp" && chmod 1777 "$M/tmp"; }

# bubblewrap, only where absent (an apk-installed one wins)
for f in usr/bin/bwrap usr/lib/libcap.so.2 lib/ld-musl-x86_64.so.1; do
    if [ -e "$M/$f" ]; then echo "keep   /$f (already there)"; continue; fi
    mkdir -p "$M/$(dirname "$f")"
    put "$STAGE/bwrap/$f" "/$f" 755
    echo "wrote  /$f"
done

sync
md5sum "$M/boot/akuma-amd64.verify" "$M/bin/herd"
[ "$PRIMARY" = 1 ] && md5sum "$M/boot/akuma-amd64"
cat "$STAGE/COMMIT" | sed 's/^/kernel commit: /'
echo "== INSTALLED (nothing armed). Next: sudo sh overlays/ryzen/arm.sh 26"
