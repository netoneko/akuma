#!/bin/sh
# DESTRUCTIVE — run as root on ryzen, only when the user has said to.
#
# Replaces whatever is on nvme0n1p3 (the Windows partition, "Windows-SSD") with
# the Akuma root image, grown to fill the partition. Windows, its files and its
# recovery path stop working; there is no undo.
#
#   sh format-p3.sh --yes-destroy-p3 [size]     # size for resize2fs, default: all
#
# It refuses unless p3 is exactly the partition measured 2026-10-06 — same
# start, same length, same PARTUUID — and is not mounted. That is the same
# window the kernel's NVMe driver confines itself to.
set -e
W=/home/netoneko/akuma-metal
IMG=$W/out/root.img
DEV=/dev/nvme0n1p3
WANT_START=567296
WANT_SIZE=585951232
WANT_PARTUUID=47402c1a-c192-4c03-b8fd-e29de5957d84

[ "$1" = --yes-destroy-p3 ] || { sed -n '2,12p' "$0"; exit 2; }
[ -s "$IMG" ] || { echo "no $IMG — run build.sh first" >&2; exit 1; }
start=$(cat /sys/class/block/nvme0n1p3/start)
size=$(cat /sys/class/block/nvme0n1p3/size)
partuuid=$(blkid -s PARTUUID -o value $DEV)
if [ "$start" != $WANT_START ] || [ "$size" != $WANT_SIZE ] || [ "$partuuid" != $WANT_PARTUUID ]; then
    echo "refusing: $DEV is start=$start size=$size partuuid=$partuuid, not the partition this was written for" >&2
    exit 1
fi
if grep -q "^$DEV " /proc/mounts; then
    echo "refusing: $DEV is mounted" >&2
    exit 1
fi
echo "before: $(blkid $DEV)"
dd if=$IMG of=$DEV bs=4M conv=fsync status=none
e2fsck -fy $DEV >/dev/null 2>&1 || true
resize2fs $DEV ${2:-} 2>&1 | tail -1
tune2fs -L AKUMA-RYZEN $DEV >/dev/null
e2fsck -fn $DEV 2>&1 | tail -1
echo "after:  $(blkid $DEV)"
