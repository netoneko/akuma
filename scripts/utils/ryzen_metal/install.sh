#!/bin/sh
# ryzen bare metal, step 2 of 2 — run as root on ryzen, after build.sh.
#
# ryzen boots systemd-boot, not GRUB, and systemd-boot cannot load a multiboot2
# kernel. So: a standalone GRUB EFI binary (config embedded) on the Pop ESP,
# and one systemd-boot entry that chainloads it. Pop's own entries, kernelstub
# and Windows are not touched; undo = `rm -r $ESP/EFI/akuma $ESP/loader/entries/akuma.conf`.
#
# It does NOT arm a boot. To boot Akuma once and come back to Pop on the next
# reset:   bootctl set-oneshot akuma.conf && systemctl reboot
#
# Akuma's root is a RAM image (GRUB module) — writes are lost at reboot.
set -e
W=/home/netoneko/akuma-metal
OUT=$W/out
ESP=/boot/efi
D=$ESP/EFI/akuma

for f in akuma-amd64 akuma-amd64.tests root.img; do
    [ -s "$OUT/$f" ] || { echo "missing $OUT/$f — run build.sh first" >&2; exit 1; }
done

# grub-common (grub-mkstandalone, grub-file) + the x86_64-efi modules. Neither
# package runs grub-install or touches the ESP; that is `grub-efi-amd64`, not
# installed on purpose.
command -v grub-mkstandalone >/dev/null || apt-get install -y --no-install-recommends grub-efi-amd64-bin

for k in akuma-amd64 akuma-amd64.tests; do
    grub-file --is-x86-multiboot2 "$OUT/$k" || { echo "$k: no multiboot2 header" >&2; exit 1; }
done

mkdir -p $D
cp "$OUT/akuma-amd64" "$OUT/akuma-amd64.tests" "$OUT/root.img" $D/
sync

# Entry 0 is what a one-shot boots with nobody watching; the menu waits 5 s.
# `fbverbose`: keep kernel diagnostics on the screen — this machine has no
# serial port and (yet) no network, so the panel is the only output.
# `nosmp` until a 16-thread boot has been seen to work.
CFG=$(mktemp)
cat > $CFG <<'EOF'
insmod part_gpt
insmod fat
insmod multiboot2
insmod all_video
search --no-floppy --file --set=root /EFI/akuma/akuma-amd64
set timeout=5
set default=0
menuentry "Akuma/amd64 (nosmp, verbose)" {
    multiboot2 /EFI/akuma/akuma-amd64 init=/bin/herd nosmp fbverbose
    module2 /EFI/akuma/root.img
}
menuentry "Akuma/amd64 self-tests (nosmp)" {
    multiboot2 /EFI/akuma/akuma-amd64.tests init=/bin/herd nosmp fbverbose
    module2 /EFI/akuma/root.img
}
menuentry "Akuma/amd64 (SMP)" {
    multiboot2 /EFI/akuma/akuma-amd64 init=/bin/herd fbverbose
    module2 /EFI/akuma/root.img
}
menuentry "Back to firmware (reboot)" {
    reboot
}
EOF
grub-mkstandalone -O x86_64-efi -o $D/grubx64.efi "boot/grub/grub.cfg=$CFG"
cp $CFG $D/grub.cfg.embedded   # for reading only; the binary carries its own copy
rm -f $CFG

cat > $ESP/loader/entries/akuma.conf <<'EOF'
title   Akuma/amd64
efi     /EFI/akuma/grubx64.efi
EOF

ls -la $D $ESP/loader/entries
bootctl list --no-pager 2>/dev/null | grep -E "title|id:" || true
md5sum $D/akuma-amd64 $D/akuma-amd64.tests $D/root.img
df -h $ESP | tail -1
echo "== INSTALL DONE (nothing armed)"
