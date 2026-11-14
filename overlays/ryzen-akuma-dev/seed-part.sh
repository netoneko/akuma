#!/bin/sh
# seed-part.sh <root-image> <N> --yes-destroy-partition   (RUN FROM POP, as root, Akuma stopped)
#
# Writes an ext2 root image onto /dev/nvme0n1pN and grows it to fill the partition. Akuma exposes no
# nvme device node, so this is the one step that cannot happen from inside Akuma. DESTRUCTIVE.
# Make the image from the Akuma side (scripts/kvm/akuma_guest_rootfs.sh, or a copy of
# /boot/akuma-dev-root-ssh.img with sshd and its authorized_keys) and carry it over on p3.
set -eu
IMG=${1:?root image}; N=${2:?partition number}; [ "${3:-}" = --yes-destroy-partition ] || { echo "needs --yes-destroy-partition"; exit 2; }
case "$N" in ''|*[!0-9]*) echo "bad N"; exit 2 ;; 1|3|6) echo "p$N refused: ESP or the host's root"; exit 2 ;; esac
D=/dev/nvme0n1p$N
[ -b "$D" ] || { echo "$D is not a block device"; exit 1; }
if grep -q "^$D " /proc/mounts; then echo "$D is mounted"; exit 1; fi
echo "target: $D"; lsblk -o NAME,SIZE,FSTYPE,LABEL,MOUNTPOINT "$D"
[ "$(stat -c %s "$IMG")" -le "$(blockdev --getsize64 "$D")" ] || { echo "image larger than the partition"; exit 1; }
dd if="$IMG" of="$D" bs=4M conv=fsync status=progress
e2fsck -f -y "$D" || [ $? -le 1 ]
resize2fs "$D"
e2fsck -fn "$D"
echo "seeded $D"
