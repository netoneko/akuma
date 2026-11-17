#!/bin/sh
# Boot gate for `mount -t vfat` (kernel feature `vfat`): build the headless image
# with it, attach a FAT volume as a second virtio-blk disk, run `vfatprobe` as
# init, and then check the volume from the host with `fsck.vfat`.
#
#   amd64/run-vfat.sh                 # FAT16, 64 MiB
#   FAT=32 SIZE_MIB=128 amd64/run-vfat.sh
#   FAT=12 SIZE_MIB=8   amd64/run-vfat.sh
#
# Under QEMU `-M microvm` (virtio-MMIO, no PCI), the Firecracker stand-in, so the
# volume arrives as `/dev/vdb`. The ESP-on-NVMe path (`/dev/nvme0n1pN`) needs a
# PCI machine; see docs/reference/subsystems/vfat.md § "Testing the NVMe path".
#
# Exit status 0 only if the guest printed VFATPROBE PASS *and* fsck.vfat accepts
# the image afterwards. Needs e2fsprogs (debugfs, mkfs.ext2) and dosfstools.
set -e

HERE=$(dirname "$0")
cd "$HERE/.."

FAT="${FAT:-16}"
SIZE_MIB="${SIZE_MIB:-64}"
MEMORY="${MEMORY:-1024}"
TIMEOUT="${TIMEOUT:-300}"
KERNEL=target/x86_64-unknown-none/release/akuma
ROOT=target/x86_64-unknown-none/release/amd64-root.img
ESP="${ESP:-target/vfat-esp.img}"
LOG="${LOG:-target/vfat-serial.log}"

cargo build -p akuma --target x86_64-unknown-none --release \
    --no-default-features --features headless,vfat
(cd userspace && cargo build -q -p vfatprobe --target x86_64-unknown-none --release)

sh "$HERE/mkdisk.sh" "$ROOT" 64 >/dev/null
debugfs -w -R "mkdir /mnt" "$ROOT" >/dev/null 2>&1
debugfs -w -R "write userspace/target/x86_64-unknown-none/release/vfatprobe bin/vfatprobe" "$ROOT" >/dev/null 2>&1

rm -f "$ESP"
truncate -s "${SIZE_MIB}M" "$ESP"
mkfs.vfat -F "$FAT" -n AKUMAESP "$ESP" >/dev/null

rm -f "$LOG"
set +e
timeout "$TIMEOUT" qemu-system-x86_64 \
    -M microvm -cpu max -global virtio-mmio.force-legacy=false \
    -kernel "$KERNEL" -m "$MEMORY" -smp 1 \
    -drive id=d0,file="$ROOT",format=raw,if=none \
    -device virtio-blk-device,drive=d0,bus=virtio-mmio-bus.0 \
    -drive id=d1,file="$ESP",format=raw,if=none \
    -device virtio-blk-device,drive=d1,bus=virtio-mmio-bus.1 \
    -append "virtio_mmio.device=512@0xfeb00000:5 virtio_mmio.device=512@0xfeb00200:6 skiptests init=/bin/vfatprobe initargs=/dev/vdb,/mnt" \
    -serial "file:$LOG" -display none -no-reboot
set -e

echo "--- guest output (vfat lines) ---"
grep -a "vfat\|VFATPROBE\|  ok \|root:\|EFI/akuma" "$LOG" || true
grep -aq "VFATPROBE PASS" "$LOG" || { echo "FAIL: guest did not print VFATPROBE PASS (log: $LOG)"; exit 1; }
echo "--- fsck.vfat -n $ESP ---"
fsck.vfat -n "$ESP"
echo "OK: guest PASS and fsck clean ($ESP)"
