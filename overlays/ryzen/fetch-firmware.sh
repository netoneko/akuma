#!/bin/sh
# Put the RTL8852CE (rtw89) firmware on Akuma's root (nvme0n1p3), from Pop — run
# as root on ryzen. Akuma loads it for the wifi work (docs/archive/
# AKUMA_AMD64_ON_RYZEN_LAPTOP.md § 5).
#
# Source: Alpine's `linux-firmware-rtw89` package — no dependencies, a 3.7 MB
# tar.gz whose firmware files are zstd-compressed (`*.bin.zst`), so they are
# decompressed here rather than teaching the kernel zstd. Alpine ships
# `rtw8852c_fw-2.bin`, which Pop's linux-firmware lacks.
#
# Licence (linux-firmware WHENCE): "Redistributable. See
# LICENCE.rtlwifi_firmware.txt" — binary redistribution, unmodified, notice
# kept; no reverse engineering. The licence goes on the disk beside the files.
#
#   sh fetch-firmware.sh            # → /lib/firmware/rtw89/ on p3
set -e
VER=20250509-r0
URL=https://dl-cdn.alpinelinux.org/alpine/v3.22/main/x86_64/linux-firmware-rtw89-$VER.apk
LICENCE_URL=https://git.kernel.org/pub/scm/linux/kernel/git/firmware/linux-firmware.git/plain/LICENCE.rtlwifi_firmware.txt
T=$(mktemp -d)
M=/mnt/akp3
trap 'umount $M 2>/dev/null; rm -rf $T' EXIT
curl -sfL -o $T/fw.apk "$URL"
# The licence: Pop's linux-firmware ships it; git.kernel.org refuses scripted
# fetches from this host (it answered from the laptop), so it is the fallback.
L=/usr/share/doc/linux-firmware/licenses/LICENCE.rtlwifi_firmware.txt.gz
if [ -f $L ]; then zcat $L > $T/LICENCE.rtlwifi_firmware.txt; else curl -sfL -o $T/LICENCE.rtlwifi_firmware.txt "$LICENCE_URL"; fi
# An .apk is three gzip'd tar segments back to back (signature, control,
# data). GNU tar stops at the first segment's end-of-archive blocks unless told
# to read past them; bsdtar does not need telling, which hides this on a Mac.
tar --ignore-zeros -xzf $T/fw.apk -C $T --warning=no-unknown-keyword
for z in $T/lib/firmware/rtw89/rtw8852c_fw*.bin.zst; do zstd -q -d --rm "$z"; done
ls -la $T/lib/firmware/rtw89/rtw8852c_fw*.bin
grep -q "^$(readlink -f /dev/nvme0n1p3) " /proc/mounts && { echo "p3 is mounted elsewhere" >&2; exit 1; }
mkdir -p $M
mount /dev/nvme0n1p3 $M
mkdir -p $M/lib/firmware/rtw89
cp $T/lib/firmware/rtw89/rtw8852c_fw*.bin $T/LICENCE.rtlwifi_firmware.txt $M/lib/firmware/rtw89/
sha256sum $M/lib/firmware/rtw89/rtw8852c_fw*.bin
sync
