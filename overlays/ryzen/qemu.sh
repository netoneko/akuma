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
#   env: DISK=nvme — the ESP and an ext2 root on an NVMe drive with ryzen's
#        exact GPT layout (a sparse 477 GiB image: same LBAs, so p3 is the
#        same 279 GiB window), booted by OVMF from NVMe. Afterwards the root
#        is fsck'd read-only and /var/log/ryzen shown, from Linux.
#        P3_SIZE=<resize2fs size, e.g. 32G> (default: the whole partition)
#        P3=blank leaves p3 unformatted (no ext2): the fallback-to-RAM path,
#        i.e. ryzen before its p3 is formatted. Nothing may be written to it.
#        KEEP=1 boots the existing nvme.img again as it is (persistence check)
#        KERNEL=<path> boots that kernel in place of $OUT/akuma-amd64 (bisect.sh)
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
if [ "${DISK:-ahci}" = nvme ] && [ "${KEEP:-0}" = 1 ] && [ -f $Q/nvme.img ]; then
    IMG=$Q/nvme.img
    echo "KEEP=1: booting the existing $IMG unchanged"
elif [ "${DISK:-ahci}" = nvme ]; then
    # ryzen's table, sector for sector (sgdisk -p /dev/nvme0n1 on the host).
    IMG=$Q/nvme.img
    rm -f $IMG
    truncate -s 512110190592 $IMG
    sgdisk -n1:2048:534527 -t1:ef00 -c1:"EFI system partition" \
              -n2:534528:567295 -t2:0c01 -c2:"Microsoft reserved partition" \
              -n3:567296:586518527 -t3:0700 -c3:"Basic data partition" \
              -n4:996118528:1000214527 -t4:2700 -c4:"Basic data partition" \
              -n5:586520576:987928574 -t5:8300 \
              -n6:987928576:996118525 -t6:ef00 $IMG >/dev/null
    sgdisk -p $IMG | grep -q "^   6 " || { echo "sgdisk did not create the partitions" >&2; exit 1; }
    LOOP=$(losetup -P -f --show $IMG)
    trap 'umount $Q/mnt 2>/dev/null; losetup -d $LOOP 2>/dev/null || true' EXIT
    # udev creates ${LOOP}pN asynchronously; wait for the ones used.
    udevadm settle 2>/dev/null || true
    for _ in $(seq 50); do [ -b ${LOOP}p6 ] && [ -b ${LOOP}p3 ] && break; sleep 0.1; done
    mkfs.vfat -F 32 -n AKUMAESP ${LOOP}p6 >/dev/null
    # p3: the root image itself, grown to the partition (or P3_SIZE).
    if [ "${P3:-ext2}" != blank ]; then
        dd if=$OUT/root.img of=${LOOP}p3 bs=4M conv=sparse status=none
        e2fsck -fy ${LOOP}p3 >/dev/null 2>&1 || true
        resize2fs ${LOOP}p3 ${P3_SIZE:-} 2>&1 | tail -1
    fi
    MNT=$Q/mnt
    mkdir -p $MNT
    mount ${LOOP}p6 $MNT
else
    # The ESP image: big enough for two kernels and the 512 MiB root.
    IMG=$Q/esp.img
    rm -f $IMG
    truncate -s 640M $IMG
    mkfs.vfat -F 32 -n AKUMAESP $IMG >/dev/null
    MNT=$Q/mnt
    mkdir -p $MNT
    mount -o loop $IMG $MNT
    trap 'umount $MNT 2>/dev/null || true' EXIT
fi
if [ "${KEEP:-0}" != 1 ] || [ "${DISK:-ahci}" != nvme ]; then
mkdir -p $MNT/EFI/BOOT $MNT/EFI/akuma
cp $OUT/akuma-amd64.tests $OUT/root.img $MNT/EFI/akuma/
cp ${KERNEL:-$OUT/akuma-amd64} $MNT/EFI/akuma/akuma-amd64
# The real menu with only `default`/`timeout` changed, so the rehearsal boots
# exactly the command line the metal will.
sed -e "s/^set default=.*/set default=$ENTRY/" -e "s/^set timeout=.*/set timeout=0/" \
    $W/akuma/overlays/ryzen/grub.cfg > $Q/grub.cfg
grep -A1 "^menuentry" $Q/grub.cfg | sed -n "$((ENTRY * 3 + 1)),$((ENTRY * 3 + 2))p"
grub-mkstandalone -O x86_64-efi -o $MNT/EFI/BOOT/BOOTX64.EFI "boot/grub/grub.cfg=$Q/grub.cfg"
sync; umount $MNT
[ "${DISK:-ahci}" = nvme ] && { losetup -d $LOOP; trap - EXIT; } || trap - EXIT
fi

case $DISP in
    std)   VGA="-vga std" ;;
    bochs) VGA="-vga none -device bochs-display" ;;
    *) echo "display: std|bochs" >&2; exit 2 ;;
esac
if [ "${DISK:-ahci}" = nvme ]; then
    DRIVE="-drive file=$IMG,format=raw,if=none,id=nv -device nvme,drive=nv,serial=AKUMANVME"
else
    DRIVE="-drive file=$IMG,format=raw,if=none,id=esp -device ahci,id=ahci -device ide-hd,drive=esp,bus=ahci.0"
fi
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
    $DRIVE \
    -serial file:$LOG -monitor none -no-reboot
RC=$?
set -e
echo "qemu rc=$RC ($( [ $RC = 124 ] && echo 'TIMEOUT: still running at the deadline' || echo 'exited: guest reset or powered off'))"
grep -a -E "multiboot2 entry|cmd:|  fb:|font:|kbd:|headless|\[herd\]|autoreboot|PANIC|panic|Fault|FAIL|halt" $LOG | head -40
if [ "${DISK:-ahci}" = nvme ]; then
    LOOP=$(losetup -P -f --show $IMG)
    udevadm settle 2>/dev/null || true
    for _ in $(seq 50); do [ -b ${LOOP}p3 ] && break; sleep 0.1; done
    if [ "${P3:-ext2}" = blank ]; then
        # Blank going in; must be blank coming out. The first 256 MiB is where
        # any ext2 write (superblock, group descriptors, bitmaps) would land.
        if cmp -s -n 268435456 ${LOOP}p3 /dev/zero; then
            echo "== p3 untouched: first 256 MiB still all zeros"
        else
            echo "== p3 WRITTEN: $(cmp -n 268435456 ${LOOP}p3 /dev/zero 2>&1 | head -1)"
        fi
    fi
    echo "== e2fsck -fn p3 (read-only) after the guest ran"
    e2fsck -fn ${LOOP}p3 > $Q/e2fsck.log 2>&1
    echo "e2fsck rc=$? (0 = clean)"; tail -2 $Q/e2fsck.log
    mount -o ro ${LOOP}p3 $Q/mnt && {
        echo "== /var/log/ryzen on p3"
        ls -la $Q/mnt/var/log/ryzen 2>&1
        L=$(ls -t $Q/mnt/var/log/ryzen/boot-*.dmesg 2>/dev/null | head -1)
        [ -n "$L" ] && grep -a -E "nvme|fs: " $L | head -12
        umount $Q/mnt
    }
    losetup -d $LOOP
fi
