#!/bin/sh
# Update the trashcan's **persistent** Akuma root (`/dev/sdb1`, 64 GB, label
# AKUMA) from the freshly staged image — without wiping what is already on it.
#
# Runs on the box's Ubuntu personality, as root. `scripts/utils/hpbox.py` drives
# it; `docs/runbooks/amd64-bare-metal-loop.md` is the operating manual.
#
#   /root/stage_akuma.sh        # build, image, install to /boot/akuma, arm GRUB
#   sh amd64/stage-baremetal.sh # then this, to refresh the persistent root
#
# # Why this is not `restage_disk()`
#
# `hpbox.restage_disk()` is `rsync -aH --delete`: it makes the partition
# *identical* to `/boot/akuma/root.img`. That is right exactly once — the first
# time — and wrong every time after, because the interesting things on that
# disk are the ones the image does not have: a 777 MiB Rust toolchain, ~190 MiB
# of `apk add gcc musl-dev binutils`, and whatever was being worked on. Wiping
# them to ship a new `sshd` costs half an hour of re-staging to change one
# binary.
#
# So the default here is **additive**: the fresh image is layered *over* the
# disk, so every binary the build just produced wins and nothing else is
# touched. `--wipe` is the old behaviour, kept for when the disk's state is
# suspect and you want it to match the image exactly.
#
# # The layering order, which is the whole trick
#
# With `--fat <image>` a second, larger image goes down **first** and the fresh
# one over it. That is how a disk gets both: the fat image carries the packages
# (it is where `apk add` was run, because the Firecracker guest can reach the
# network) and the fresh 512 MiB image carries the userspace this kernel was
# just built with. Reverse the order and a stale `sshd` from the fat image wins
# — and a stale `sshd` against a new kernel is the failure that reads as "the
# kernel broke ssh".
set -e

VOL=/dev/sdb1          # Ubuntu's name for it. **Akuma calls the same partition
                       # /dev/sda1**, because it enumerates only this disk.
IMG=/boot/akuma/root.img
FAT=""
WIPE=0

while [ $# -gt 0 ]; do
    case "$1" in
        --wipe)  WIPE=1 ;;
        --fat)   FAT="$2"; shift ;;
        --img)   IMG="$2"; shift ;;
        --vol)   VOL="$2"; shift ;;
        --rust)  RUST="${2:-$HOME/.rustup/toolchains/nightly-x86_64-unknown-linux-musl}"; shift ;;
        *) echo "usage: $0 [--wipe] [--fat <image>] [--img <image>] [--vol <part>] [--rust [dir]]" >&2; exit 2 ;;
    esac
    shift
done

[ -b "$VOL" ] || { echo "$VOL is not a block device" >&2; exit 1; }
[ -f "$IMG" ] || { echo "$IMG does not exist — run /root/stage_akuma.sh first" >&2; exit 1; }

mkdir -p /mnt/akvol /mnt/rimg /mnt/fatimg
for m in /mnt/akvol /mnt/rimg /mnt/fatimg; do
    mountpoint -q "$m" && umount "$m" || true
done
mount "$VOL" /mnt/akvol
trap 'sync; for m in /mnt/rimg /mnt/fatimg /mnt/akvol; do mountpoint -q $m && umount $m || true; done' EXIT

# `authorized_keys` is the one file worth saving across a `--wipe`: the image
# carries only the generated test key, and a key added by hand is how anything
# else reaches the box.
[ "$WIPE" = 1 ] && cp -f /mnt/akvol/etc/sshd/authorized_keys /tmp/ak_keys.save 2>/dev/null || true

if [ -n "$FAT" ]; then
    echo "== layering $FAT (packages, toolchain) =="
    mount -o loop,ro "$FAT" /mnt/fatimg
    rsync -aH --exclude=lost+found /mnt/fatimg/ /mnt/akvol/
    umount /mnt/fatimg
fi

echo "== layering $IMG (the userspace this kernel was built with) =="
mount -o loop,ro "$IMG" /mnt/rimg
if [ "$WIPE" = 1 ]; then
    rsync -aH --delete --exclude=lost+found /mnt/rimg/ /mnt/akvol/
    if [ -f /tmp/ak_keys.save ]; then
        while IFS= read -r k; do
            [ -n "$k" ] || continue
            grep -qF "$k" /mnt/akvol/etc/sshd/authorized_keys 2>/dev/null \
                || echo "$k" >> /mnt/akvol/etc/sshd/authorized_keys
        done < /tmp/ak_keys.save
    fi
else
    rsync -aH --exclude=lost+found /mnt/rimg/ /mnt/akvol/
fi
umount /mnt/rimg

if [ -n "${RUST:-}" ]; then
    [ -d "$RUST" ] || { echo "no toolchain at $RUST" >&2; exit 1; }
    echo "== installing $(basename "$RUST") at /usr/local/rust =="
    # Install rustup's musl-host toolchain. `rustc` there is a 9 KB shim over
    # `librustc_driver-*.so` found through `DT_RUNPATH` `$ORIGIN/../lib`, and
    # musl expands `$ORIGIN` by reading `/proc/self/exe` — which this target
    # does not have — so every invocation needs `LD_LIBRARY_PATH`. See the
    # runbook.
    rm -rf /mnt/akvol/usr/local/rust
    mkdir -p /mnt/akvol/usr/local
    cp -a "$RUST" /mnt/akvol/usr/local/rust
fi

echo "== on the disk now =="
ls -la /mnt/akvol/bin/sshd /mnt/akvol/usr/bin/gcc /mnt/akvol/usr/local/rust/bin/rustc 2>&1 | sed 's/^/   /'
df -h /mnt/akvol | tail -1
printf 'updated from %s%s on %s\nAdditive by default; --wipe makes it match the image exactly.\n' \
    "$IMG" "$([ -n "$FAT" ] && echo " over $FAT")" "$(date -Is)" > /mnt/akvol/AKUMA_DISK.txt
echo STAGED
