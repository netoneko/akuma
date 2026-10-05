#!/bin/sh
# Rehearse a ryzen boot in QEMU/KVM on ryzen itself — run as root, after build.sh.
#
# Same chain as the metal: OVMF (UEFI) -> this overlay's grub.cfg embedded in a
# standalone grubx64.efi -> multiboot2 kernel + root.img module off a FAT ESP.
# QEMU has a 16550, so the whole boot lands in the serial log even when the
# entry under test is `nofb`. `-no-reboot` turns the guest's reboot into QEMU
# exiting, which is how the `autoreboot` entries prove they reached userspace
# and reset.
#
#   sh qemu.sh [entry] [timeout_s] [display]   # entry = grub.cfg menu index (default 0)
#   env: KERNEL=<path> boots that kernel in place of $OUT/akuma-amd64 (bisect.sh)
#        MEM=<MiB> (6144), ACCEL=kvm|tcg (kvm) — to separate this machine's CPU/RAM
#        layout from the kernel when a rehearsal fails
#   display: std (VGA, 32-bit BAR) | bochs (bochs-display; OVMF may place its
#            64-bit BAR above 4 GiB, as ryzen's Radeon is) — default bochs
#
# Prints the serial log's verdict lines; full log in $W/qemu/serial-<entry>.log.
W=/home/netoneko/akuma-metal
OUT=$W/out
Q=$W/qemu
ENTRY=${1:-0}
TMO=${2:-240}
DISP=${3:-bochs}
set -e
mkdir -p $Q
# The ESP image: big enough for two kernels and the 512 MiB root.
IMG=$Q/esp.img
rm -f $IMG
truncate -s 640M $IMG
mkfs.vfat -F 32 -n AKUMAESP $IMG >/dev/null
MNT=$Q/mnt
mkdir -p $MNT
mount -o loop $IMG $MNT
trap 'umount $MNT 2>/dev/null || true' EXIT
mkdir -p $MNT/EFI/BOOT $MNT/EFI/akuma
cp $OUT/akuma-amd64.tests $OUT/root.img $MNT/EFI/akuma/
cp ${KERNEL:-$OUT/akuma-amd64} $MNT/EFI/akuma/akuma-amd64
# The real menu with only `default`/`timeout` changed, so the rehearsal boots
# exactly the command line the metal will.
sed -e "s/^set default=.*/set default=$ENTRY/" -e "s/^set timeout=.*/set timeout=0/" \
    $W/akuma/overlays/ryzen/grub.cfg > $Q/grub.cfg
grep -A1 "^menuentry" $Q/grub.cfg | sed -n "$((ENTRY * 3 + 1)),$((ENTRY * 3 + 2))p"
grub-mkstandalone -O x86_64-efi -o $MNT/EFI/BOOT/BOOTX64.EFI "boot/grub/grub.cfg=$Q/grub.cfg"
sync; umount $MNT; trap - EXIT

case $DISP in
    std)   VGA="-vga std" ;;
    bochs) VGA="-vga none -device bochs-display" ;;
    *) echo "display: std|bochs" >&2; exit 2 ;;
esac
LOG=$Q/serial-$ENTRY.log
rm -f $LOG
set +e
# 13 GiB host; 4 GiB guest puts RAM above 4 GiB too (the MMIO hole displaces it).
case ${ACCEL:-kvm} in
    kvm) CPU="-enable-kvm -cpu host" ;;
    tcg) CPU="-cpu max" ;;
esac
timeout $TMO qemu-system-x86_64 $CPU -M q35 -m ${MEM:-6144} -smp 2 \
    -bios /usr/share/ovmf/OVMF.fd $VGA -display none \
    -drive file=$IMG,format=raw,if=none,id=esp -device ahci,id=ahci -device ide-hd,drive=esp,bus=ahci.0 \
    -serial file:$LOG -monitor none -no-reboot
RC=$?
set -e
echo "qemu rc=$RC ($( [ $RC = 124 ] && echo 'TIMEOUT: still running at the deadline' || echo 'exited: guest reset or powered off'))"
grep -a -E "multiboot2 entry|cmd:|  fb:|font:|kbd:|headless|\[herd\]|autoreboot|PANIC|panic|Fault|FAIL|halt" $LOG | head -40
