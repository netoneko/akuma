#!/bin/sh
# Boot gate for `mount -t vfat /dev/nvme0n1pN` (kernel feature `vfat`): the ESP
# path ryzen uses. Companion of `run-vfat.sh`, which covers the same filesystem
# over virtio-blk.
#
# A PCI machine (`-M q35`, like `run-xhci.sh`), because NVMe is PCI and
# `-M microvm` has none the kernel's 0xCF8 config mechanism can reach. The root
# filesystem therefore comes from the USB disk (`usbroot`, `/dev/sda1`: an ext2
# at LBA 2048 of an MBR disk), and the ESP is GPT partition 1 of an NVMe disk:
#
#   sda  (usb-storage)  MBR, sda1 = ext2 with /bin/vfatprobe and /mnt
#   nvme0n1             GPT,  p1 = FAT (the ESP), p2 = a decoy the probe must not touch
#
# Exit status 0 only if the guest printed VFATPROBE PASS, the decoy partition is
# byte-identical afterwards, and fsck.vfat accepts the ESP.
set -e

HERE=$(dirname "$0")
cd "$HERE/.."

FAT="${FAT:-16}"
ESP_MIB="${ESP_MIB:-64}"
MEMORY="${MEMORY:-2048}"
TIMEOUT="${TIMEOUT:-400}"
KERNEL=target/x86_64-unknown-none/release/akuma
ROOT=target/x86_64-unknown-none/release/amd64-root.img
USB=target/vfat-usb.img
NVME=target/vfat-nvme.img
LOG="${LOG:-target/vfat-nvme-serial.log}"

cargo build -p akuma --target x86_64-unknown-none --release \
    --no-default-features --features headless,vfat
(cd userspace && cargo build -q -p vfatprobe --target x86_64-unknown-none --release)

sh "$HERE/mkdisk.sh" "$ROOT" 64 >/dev/null
debugfs -w -R "mkdir /mnt" "$ROOT" >/dev/null 2>&1
debugfs -w -R "write userspace/target/x86_64-unknown-none/release/vfatprobe bin/vfatprobe" "$ROOT" >/dev/null 2>&1

# USB disk: MBR with sda1 at LBA 2048 (what `fs::mbr_looks_right` insists on), the ext2 copied in.
rm -f "$USB"
truncate -s 80M "$USB"
printf 'label: dos\nstart=2048, size=131072, type=83\n' | sfdisk -q "$USB"
dd if="$ROOT" of="$USB" bs=512 seek=2048 conv=notrunc status=none

# NVMe disk: GPT, p1 = ESP (FAT), p2 = a decoy full of a recognisable pattern.
rm -f "$NVME"
truncate -s 200M "$NVME"
DECOY_MIB=32
printf 'label: gpt\nstart=2048, size=%d, type=C12A7328-F81F-11D2-BA4B-00A0C93EC93B, name=ESP\nsize=%d, type=0FC63DAF-8483-4772-8E79-3D69D8477DE4, name=decoy\n' \
    $((ESP_MIB * 2048)) $((DECOY_MIB * 2048)) | sfdisk -q "$NVME"
ESP_PART=target/vfat-esp-part.img
rm -f "$ESP_PART"
truncate -s "${ESP_MIB}M" "$ESP_PART"
mkfs.vfat -F "$FAT" -n AKUMAESP "$ESP_PART" >/dev/null
dd if="$ESP_PART" of="$NVME" bs=512 seek=2048 conv=notrunc status=none
DECOY_LBA=$((2048 + ESP_MIB * 2048))
python3 - "$NVME" "$DECOY_LBA" "$DECOY_MIB" <<'PY'
import sys
path, lba, mib = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
with open(path, "r+b") as f:
    f.seek(lba * 512)
    f.write((b"DECOY-DO-NOT-TOUCH-" * 64)[:1024] * (mib * 1024))
PY
decoy_sum() { dd if="$NVME" bs=512 skip="$DECOY_LBA" count=$((DECOY_MIB * 2048)) status=none | sha256sum; }
BEFORE=$(decoy_sum)

rm -f "$LOG"
set +e
timeout "$TIMEOUT" qemu-system-x86_64 \
    -M q35 -m "$MEMORY" -display none -no-reboot \
    -kernel "$KERNEL" \
    -append "pci nosmp skiptests usbroot init=/bin/vfatprobe initargs=/dev/nvme0n1p1,/mnt" \
    -drive file="$USB",format=raw,if=none,id=usbdisk \
    -device qemu-xhci,id=xhci \
    -device usb-storage,bus=xhci.0,drive=usbdisk \
    -drive file="$NVME",format=raw,if=none,id=nv \
    -device nvme,drive=nv,serial=akuma \
    -serial "file:$LOG"
set -e

echo "--- guest output ---"
grep -a "nvme\|vfat\|VFATPROBE\|  ok \|root:\|EFI/akuma\|fs:" "$LOG" || true
grep -aq "VFATPROBE PASS" "$LOG" || { echo "FAIL: guest did not print VFATPROBE PASS (log: $LOG)"; exit 1; }
[ "$BEFORE" = "$(decoy_sum)" ] || { echo "FAIL: the decoy partition changed"; exit 1; }
echo "decoy partition untouched"
dd if="$NVME" of="$ESP_PART" bs=512 skip=2048 count=$((ESP_MIB * 2048)) status=none
echo "--- fsck.vfat -n (ESP partition) ---"
fsck.vfat -n "$ESP_PART"
echo "OK: guest PASS, decoy untouched, fsck clean"
