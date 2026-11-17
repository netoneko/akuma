#!/bin/sh
# Disposable QEMU-only ext3 writable integration test. This opts into the
# guest-stop test assumption of atomic 512-byte writes; never use on live disks.
set -eu
cd "$(dirname "$0")/../.."
work=$(mktemp -d "${TMPDIR:-/tmp}/akuma-ext-rw.XXXXXXXX")
trap 'rm -rf "$work"' EXIT HUP INT TERM
cargo build -p akuma --target x86_64-unknown-none --release --features ext3-qemu-write-test
cargo build --manifest-path userspace/Cargo.toml -p ext2probe --bin ext-rw-probe --target x86_64-unknown-none --release
sh amd64/mkdisk.sh "$work/root.img" 128
debugfs -w -R 'mkdir /mnt' "$work/root.img" >/dev/null 2>&1
debugfs -w -R 'write userspace/target/x86_64-unknown-none/release/ext-rw-probe /bin/ext-rw-probe' "$work/root.img" >/dev/null 2>&1
truncate -s 32M "$work/data.img"
mke2fs -q -F -t ext3 -b 1024 "$work/data.img"
status=0
timeout 30s qemu-system-x86_64 -M microvm -cpu max \
    -global virtio-mmio.force-legacy=false \
    -kernel target/x86_64-unknown-none/release/akuma -m 2048 -smp 1 \
    -drive "id=d0,file=$work/root.img,format=raw,if=none" \
    -device virtio-blk-device,drive=d0,bus=virtio-mmio-bus.0 \
    -drive "id=d1,file=$work/data.img,format=raw,if=none,cache=writeback" \
    -device virtio-blk-device,drive=d1,bus=virtio-mmio-bus.1 \
    -append 'virtio_mmio.device=512@0xfeb00000:5 virtio_mmio.device=512@0xfeb00200:6 init=/bin/ext-rw-probe' \
    -serial "file:$work/serial.log" -display none -no-reboot || status=$?
if [ "$status" -ne 0 ] && [ "$status" -ne 124 ]; then cat "$work/serial.log"; exit "$status"; fi
if ! grep -q 'ext-rw-probe: PASS' "$work/serial.log" ||
   ! grep -q 'self-test: .*0 failed' "$work/serial.log" ||
   grep -qE 'STACK CANARY|SWITCH BADFRAME|\[EXCEPTION\]' "$work/serial.log"; then
    cat "$work/serial.log"
    exit 1
fi
e2fsck -fn "$work/data.img"
test "$(debugfs -R 'cat /result' "$work/data.img" 2>/dev/null)" = 'persistent guest result'
echo 'ext3 QEMU writable probe passed'
