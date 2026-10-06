#!/bin/sh
# Sample the embedded controller's memory mirror from Linux, next to what
# Linux's battery driver reports at the same moment, and stage it on p3 at
# /root/acpi/linux/ec-eram-sample.txt for the Akuma-side battery reader to
# validate its field map against (docs/handoff-battery-status.md). Read-only.
#
#   sh overlays/ryzen/ec-sample.sh        (root, in Pop)
#
# The window is the ERAM base the DSDT's EC region names (0xFEEC2380 on this
# laptop) widened to 0x200 bytes: the battery fields are at +0x80..+0x93 of the
# EC register space, and Linux's own readings (BAT0 uevent) are the ground
# truth. Needs /dev/mem readable (CONFIG_STRICT_DEVMEM may refuse; then the
# file says so and nothing else is claimed).
set -u
P=/mnt/p3
mountpoint -q $P || { mkdir -p $P; mount /dev/nvme0n1p3 $P; }
OUT=$P/root/acpi/linux/ec-eram-sample.txt
BASE=$((0xFEEC2300))
{
echo "# base 0xFEEC2300, 0x200 bytes; offset = byte index; one block per sample"
for n in 1 2 3; do
  echo "## sample $n  $(date +%T)"
  grep -E "STATUS|VOLTAGE_NOW|POWER_NOW|ENERGY_(FULL|NOW)|CAPACITY=|CYCLE" /sys/class/power_supply/BAT0/uevent
  dd if=/dev/mem bs=1 skip=$BASE count=512 2>/dev/null | od -A x -t x1 -v | sed 's/^/ec /'
  sleep 3
done
} > $OUT 2>&1
sync
echo "ec sample -> p3:/root/acpi/linux/ec-eram-sample.txt ($(wc -l < $OUT) lines)"
head -12 $OUT
umount $P
