#!/bin/sh
# Boot Akuma once, then come back to Pop — run as root on ryzen.
#
#   sh arm.sh          # menu entry 0 (unattended, NVMe root, autoreboot)
#   sh arm.sh 6        # any other entry of grub.cfg, by index, for this boot only
#
# Two one-shots stacked: systemd-boot's (`bootctl set-oneshot`) picks Akuma
# over Pop, GRUB's (`next_entry` in /EFI/akuma/grubenv) picks the menu entry.
# Both are consumed by the boot they select.
set -e
D=/boot/efi/EFI/akuma
if [ -n "$1" ]; then
    grub-editenv $D/grubenv set next_entry="$1"
    grub-editenv $D/grubenv list
fi
bootctl set-oneshot akuma.conf
sync
date +%T
(sleep 2; systemctl reboot) >/dev/null 2>&1 &
