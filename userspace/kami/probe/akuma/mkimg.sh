#!/bin/sh
# Build the ext2 image Chromium runs from under Firecracker, on the trashcan's
# Ubuntu side (needs docker, e2fsprogs). Run from this directory there:
#   sh mkimg.sh [out.img] [size]        default: kami-root.img 2G
#
# The rootfs is the Alpine container from ../Dockerfile (chromium, fonts,
# python3 + pillow, strace), exported as-is. mke2fs -d writes short symlinks
# as fast symlinks, which is why the guest must boot with init=/bin/busybox
# and not init=/bin/sh (init= does not follow symlinks yet).
set -e
cd "$(dirname "$0")"
OUT=${1:-kami-root.img}
SIZE=${2:-2G}
docker image inspect akuma-cdp-probe >/dev/null 2>&1 || docker build -q -t akuma-cdp-probe ..
rm -rf rootfs "$OUT"
mkdir rootfs
cid=$(docker create akuma-cdp-probe)
docker export "$cid" | tar -C rootfs -xf -
docker rm "$cid" >/dev/null
mkdir -p rootfs/tmp rootfs/proc rootfs/dev
chmod 1777 rootfs/tmp
du -sh rootfs | tail -1
mke2fs -q -F -t ext2 -b 4096 -L KAMI -d rootfs "$OUT" "$SIZE"
rm -rf rootfs
ls -la "$OUT"
