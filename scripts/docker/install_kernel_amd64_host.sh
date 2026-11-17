#!/bin/sh
# Install a kernel onto p3 (label AKUMA-RYZEN) FROM POP, through a throwaway
# privileged docker container (no sudo on the host: only the container mounts p3).
#
#   scripts/docker/install_kernel_amd64_host.sh [elf] [dest-name]
#
# Counterpart of scripts/install_kernel_amd64.sh (inside Akuma): same multiboot2
# check, `.prev` backup and md5 read-back, onto <p3>/boot/<dest-name> (default
# akuma-amd64, the GRUB default entry's kernel). Does NOT arm or reboot.
set -eu
SRC=$(cd "$(dirname "$0")/../.." && pwd)
ELF=$(realpath "${1:-$SRC/target/docker-kernel/akuma-amd64}")
NAME=${2:-akuma-amd64}
[ -f "$ELF" ] || { echo "no ELF at $ELF — build it first (scripts/build_kernel_amd64_host.sh)" >&2; exit 1; }
od -A d -t x4 -N 32768 "$ELF" | grep -qi e85250d6 || { echo "NO multiboot2 header in $ELF" >&2; exit 1; }
DEV=$(lsblk -nrpo NAME,LABEL | awk '$2=="AKUMA-RYZEN"{print $1}')
[ -n "$DEV" ] || { echo "no partition labelled AKUMA-RYZEN" >&2; exit 1; }
docker run --rm --privileged -v /dev:/dev -v "$ELF":/k:ro -e DEV="$DEV" -e NAME="$NAME" alpine:3.20 sh -eu -c '
    mkdir /m && mount "$DEV" /m
    D=/m/boot/$NAME
    [ -f "$D" ] && cp -f "$D" "$D.prev"
    cp -f /k "$D"; sync
    A=$(md5sum </k | cut -d" " -f1); B=$(md5sum <"$D" | cut -d" " -f1)
    umount /m
    [ "$A" = "$B" ] || { echo "MD5 MISMATCH $A != $B" >&2; exit 1; }
    echo "installed -> $DEV:/boot/$NAME (md5 $A verified; previous kept as $NAME.prev)"'
echo "next: sh overlays/ryzen/arm.sh   (boots Akuma once; default entry arms KVM)"
