#!/bin/sh
# Dump ryzen's ACPI tables, and what Linux reads from the battery, onto p3 so
# Akuma (and Kimi inside it) can read them: /root/acpi/ on p3.
#
#   sh overlays/ryzen/acpi-dump.sh        (root, on ryzen, in Pop; p3 not in use by Akuma)
#
# Raw tables (binary, as firmware hands them over; DSDT and every SSDT hold the
# AML that defines the battery and the embedded controller), decompiled ASL when
# `iasl` exists, the power_supply readings (the ground truth to compare an Akuma
# reading against), and the EC register file via debugfs. Nothing identifying
# beyond what ACPI itself carries (serial numbers are dropped from the uevent).
set -eu
P=/mnt/p3
mountpoint -q $P || { mkdir -p $P; mount /dev/nvme0n1p3 $P; }
D=$P/root/acpi
mkdir -p $D/tables $D/linux
cp /sys/firmware/acpi/tables/* $D/tables/ 2>/dev/null || true
cp -r /sys/firmware/acpi/tables/data $D/tables/data 2>/dev/null || true
if command -v iasl >/dev/null; then
    mkdir -p $D/asl
    (cd $D/tables && for t in DSDT SSDT* ECDT FACP; do [ -f "$t" ] && iasl -d -p $D/asl/$t $t >/dev/null 2>&1; done) || true
fi
for b in /sys/class/power_supply/*; do
    n=$(basename $b)
    grep -v -i 'SERIAL' $b/uevent > $D/linux/power_supply-$n.uevent 2>/dev/null || true
done
mount -t debugfs none /sys/kernel/debug 2>/dev/null || true
[ -r /sys/kernel/debug/ec/ec0/io ] && cp /sys/kernel/debug/ec/ec0/io $D/linux/ec0-io.bin 2>/dev/null || true
dmesg 2>/dev/null | grep -i -E 'acpi|battery|embedded|ec:' | head -80 > $D/linux/dmesg-acpi.txt || true
cat /sys/class/dmi/id/product_name /sys/class/dmi/id/bios_version > $D/linux/dmi.txt 2>/dev/null || true
ls -l $D/tables | head -60 > $D/MANIFEST.txt
sync
echo "acpi dump -> p3:/root/acpi ($(ls $D/tables | wc -l) tables)"; umount $P 2>/dev/null || true
