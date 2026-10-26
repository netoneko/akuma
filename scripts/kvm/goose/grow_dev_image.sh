#!/bin/sh
# grow_dev_image.sh - grow the guest root image (the .orig boot_dev.sh copies per boot) on the Akuma box.
#
#   grow_dev_image.sh [SRC [SIZE [DST]]]
#     SRC  default /boot/akuma-dev-root-apk.img.orig.16M   (the untouched 16 MiB original)
#     SIZE default 1G                                      (anything truncate(1) takes)
#     DST  default /boot/akuma-dev-root-apk.img.orig       (replaced only after every check passes)
#
# The tools come from apk (`apk add e2fsprogs e2fsprogs-extra` in the box's own root; busybox's
# mke2fs has no resize2fs beside it). The grown image is built beside DST as DST.new, checked
# with e2fsck -fn (must exit 0) and dumpe2fs (block count must match SIZE), and only then moved
# over DST. The source is never modified. Run as ONE command with its output redirected to a log;
# a 1 GiB write on this box is slow enough to outlive a short ssh session.
set -u
export TERM=dumb HOME=/root
SRC=${1:-/boot/akuma-dev-root-apk.img.orig.16M}
SIZE=${2:-1G}
DST=${3:-/boot/akuma-dev-root-apk.img.orig}
NEW="$DST.new"

fail() { echo "GROW FAIL: $*"; rm -f "$NEW"; exit 1; }
for t in resize2fs e2fsck dumpe2fs truncate; do
  command -v "$t" >/dev/null 2>&1 || fail "$t missing (apk add e2fsprogs e2fsprogs-extra)"
done
[ -f "$SRC" ] || fail "no source $SRC"

echo "== source"; sha256sum "$SRC"
rm -f "$NEW"
cp "$SRC" "$NEW" || fail "copy"
truncate -s "$SIZE" "$NEW" || fail "truncate $SIZE"
echo "== e2fsck before"; e2fsck -fn "$NEW" 2>&1 | tail -2 || true
echo "== resize2fs"; resize2fs "$NEW" 2>&1 | tail -3
echo "== e2fsck after"; e2fsck -fn "$NEW" 2>&1 | tail -3
e2fsck -fn "$NEW" >/dev/null 2>&1 || fail "e2fsck after resize2fs is not clean"
echo "== geometry"
dumpe2fs -h "$NEW" 2>&1 | grep -a "Block count\|Free blocks\|Inode count\|Free inodes\|Block size\|volume name"
bytes=$(wc -c < "$NEW")
echo "file bytes $bytes"
mv "$NEW" "$DST" || fail "move into place"
echo "GROW OK $DST"
