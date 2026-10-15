#!/bin/sh
# Touchpad bring-up constants for the ryzen laptop (IdeaPad 5 2-in-1 16AHP9).
#
# Run under Pop!/Linux on the box, with sudo for the ACPI/debugfs parts. It
# prints every number the kernel glue needs, ready to paste into the boot
# command line or `amd64/src/touchpad.rs`:
#
#   - the AMDI0010 DesignWare I2C controller's MMIO base + length
#   - the touchpad's I2C slave address, bus number, ACPI _HID
#   - the DSDT's FMCN/SSCN timing packages (hcnt, lcnt, sda_hold)
#   - the raw HID report descriptor (for pinning as an akuma-hid fixture)
#
# Everything it runs is read-only. Background:
#   docs/archive/AKUMA_AMD64_TOUCHPAD_I2C_HID.md
#   proposals/AMD64_TOUCHPAD_I2C_HID.md (M0)
set -eu

say() { printf '\n== %s ==\n' "$1"; }

say "ACPI devices: i2c-hid touchpads (PNP0C50 / vendor HIDs)"
# The touchpad device: its _HID, its bus, its slave address.
for d in /sys/bus/acpi/devices/*; do
    hid=$(basename "$d")
    case "$hid" in
        PNP0C50:*|ACPI0C50:*|SYNA*:*:*|GDX*|GXTP*|ELAN*:*)
            [ -e "$d/physical_node" ] || continue
            printf '%s -> %s\n' "$hid" "$(readlink -f "$d/physical_node")"
            ;;
    esac
done

say "Which i2c bus owns the touchpad (i2c_designware lines)"
dmesg | grep -E "i2c_designware|i2c-(SYNA|GDX|GXTP|ELAN)" | tail -30 || true

say "Touchpad i2c devices and addresses"
# The bus the touchpad lives on, from the i2c-hid device name:
for b in /sys/bus/i2c/devices/i2c-*; do
    name=$(basename "$b")
    case "$(readlink -f "$b")" in
        *SYNA*|*GDX*|*GXTP*|*ELAN*|*PNP0C50*|*ACPI0C50*)
            printf '%s: %s\n' "$name" "$(ls "$b" 2>/dev/null | grep -E '^[0-9]-[0-9a-f]+$' || true)"
            ;;
    esac
done

say "The touchpad's HID identity (vendor:product the kernel saw)"
for r in /sys/bus/i2c/devices/*/modalias; do
    grep -q acpi0c50 "$r" 2>/dev/null && { printf '%s: %s\n' "$r" "$(cat "$r")"; }
done || true

say "AMDI0010 controllers and their MMIO windows (from /proc, no root needed)"
grep -E "AMDI0010|Synopsys DesignWare" /proc/iomem || true

say "I2C controller MMIO bases (dmesg, needs the designware driver's lines)"
dmesg | grep -E "i2c_designware.*\[[0-9a-fx]+\]" | tail -12 || true

say "FMCN / SSCN timing packages in the DSDT (if acpidump+iasl are installed)"
if command -v acpidump >/dev/null 2>&1 && command -v iasl >/dev/null 2>&1; then
    T=$(mktemp -d)
    sudo acpidump > "$T/acpi.out"
    acpixtract -a "$T/acpi.out" > /dev/null 2>&1
    for tbl in "$T"/dsdt.dat "$T"/ssdt*.dat; do
        [ -f "$tbl" ] || continue
        iasl -d "$tbl" >/dev/null 2>&1 || true
    done
    grep -nE "Method \(.*(FMCN|SSCN)" "$T"/dsdt.dsl 2>/dev/null || true
    grep -nE -A8 "Name \(FMCN|Method \(FMCN|Method \(SSCN" "$T"/dsdt.dsl 2>/dev/null | head -40 || true
    say "The touchpad device node in the DSDT (I2cSerialBusV2: address, speed)"
    grep -nE -B2 -A12 "PNP0C50|SYNA[0-9A-F]{4}|GDX[0-9A-F]{4}|GXTP[0-9]{4}" "$T"/dsdt.dsl 2>/dev/null | head -80 || true
    echo "DSDT DSL kept at: $T/dsdt.dsl"
else
    echo "acpidump/iasl not installed — sudo apt install acpica-tools, then rerun"
fi

say "Raw HID report descriptor(s) — pin these as akuma-hid test fixtures"
for r in /sys/kernel/debug/hid/*/rdesc; do
    [ -f "$r" ] || continue
    printf -- '--- %s (%s bytes) ---\n' "$r" "$(wc -c < "$r")"
    sudo xxd "$r" || sudo od -A x -t x1z "$r"
done

say "Input devices the touchpad presents (names + caps, for the policy layer)"
for d in /sys/class/input/input*; do
    name=$(cat "$d/name" 2>/dev/null || true)
    case "$name" in
        *Touchpad*|*TOUCHPAD*|*Synaptics*|*Goodix*|*SYNA*)
            printf '%s: %s\n' "$(basename "$d")" "$name"
            cat "$d/id/"{bustype,vendor,product,version} 2>/dev/null | tr '\n' ' '; echo
            ;;
    esac
done

say "Done — paste the numbers into the boot cmdline / touchpad.rs"
