#!/bin/sh
# Build the amd64 target's ext2 disk image.
#
# Replaces the raw two-sector probe disk `mkdisk.py` made. That disk existed to
# prove the DMA path end to end before there was a filesystem; now the
# filesystem proves it better, and the low-level check reads the **ext2
# superblock** instead — a structure the next layer up is about to parse, rather
# than a pattern invented for the test.
#
# # No Docker, no mount, no root
#
# `mkfs.ext2` creates the image and `debugfs -w -R write` puts files in it. Both
# ship with e2fsprogs and neither needs to mount anything, which is what makes
# this work unprivileged on macOS. `scripts/populate_disk.sh` uses Docker for the
# aarch64 image because that one holds a whole distro; this holds a handful of
# files and does not need the machinery.
#
#   amd64/mkdisk.sh [image] [size-mib]
set -e

HERE=$(dirname "$0")
cd "$HERE/.."

IMG="${1:-target/x86_64-unknown-none/release/amd64-root.img}"
SIZE_MIB="${2:-8}"

# e2fsprogs is keg-only under Homebrew, so its tools are not on PATH by default.
find_tool() {
    for p in "$1" "/opt/homebrew/opt/e2fsprogs/sbin/$1" "/usr/local/sbin/$1" "/sbin/$1"; do
        command -v "$p" >/dev/null 2>&1 && { echo "$p"; return 0; }
    done
    return 1
}
MKFS=$(find_tool mkfs.ext2) || {
    echo "mkfs.ext2 not found. On macOS: brew install e2fsprogs" >&2
    exit 1
}
DEBUGFS=$(find_tool debugfs) || {
    echo "debugfs not found (it ships with e2fsprogs)" >&2
    exit 1
}

# The guest ELF, wherever cargo put it. Built by amd64/build.rs into OUT_DIR, so
# the path carries a hash and has to be searched for rather than spelled.
HELLO=$(find target/x86_64-unknown-none/release/build -name hello.elf 2>/dev/null | head -1)
FDPROBE=$(find target/x86_64-unknown-none/release/build -name fdprobe.elf 2>/dev/null | head -1)
[ -n "$HELLO" ] || {
    echo "hello.elf not found — run 'cargo build -p akuma-amd64 --target x86_64-unknown-none --release' first" >&2
    exit 1
}

# paws, the shell. Built here rather than by `userspace/build.sh`, which targets
# aarch64-musl: this is the same source compiled for `x86_64-unknown-none`
# against the ported `libakuma`. Best-effort — a tree where paws does not build
# still gets a bootable image with the probes on it.
PAWS=""
if (cd userspace && cargo build -q -p paws --target x86_64-unknown-none --release 2>/dev/null); then
    PAWS=$(find userspace/target/x86_64-unknown-none/release -maxdepth 1 -name paws -type f | head -1)
fi

mkdir -p "$(dirname "$IMG")"
rm -f "$IMG"

# 1 KiB blocks, not the 4 KiB the aarch64 image uses. Deliberate: it makes the
# indirect-block paths reachable in a small image, and the ext2 driver's block
# size handling is a thing worth exercising rather than pinning to one value.
"$MKFS" -q -F -b 1024 -L AKUMA-AMD64 "$IMG" "${SIZE_MIB}m"

# A known text file, so a read can be checked against content rather than
# against "no error". The bytes are position-dependent for the same reason the
# old raw pattern was: a short read shows up as a mismatch at the first byte
# that was not transferred.
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
printf 'AKUMA/amd64 ext2 probe\n' > "$TMP/probe.txt"
i=0
while [ $i -lt 200 ]; do
    printf 'line %03d padding padding padding\n' "$i" >> "$TMP/probe.txt"
    i=$((i + 1))
done

"$DEBUGFS" -w -R "mkdir /bin" "$IMG" >/dev/null 2>&1
"$DEBUGFS" -w -R "write $HELLO bin/hello" "$IMG" >/dev/null 2>&1
[ -n "$FDPROBE" ] && "$DEBUGFS" -w -R "write $FDPROBE bin/fdprobe" "$IMG" >/dev/null 2>&1
[ -n "$PAWS" ] && "$DEBUGFS" -w -R "write $PAWS bin/paws" "$IMG" >/dev/null 2>&1
"$DEBUGFS" -w -R "write $TMP/probe.txt probe.txt" "$IMG" >/dev/null 2>&1

echo "$IMG: ${SIZE_MIB} MiB ext2, /bin/hello, /probe.txt$([ -n "$PAWS" ] && echo ", /bin/paws ($(wc -c < "$PAWS" | tr -d ' ') bytes)")"
