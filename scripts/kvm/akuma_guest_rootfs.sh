#!/bin/sh
# Build the ext2 root image the *Akuma* KVM guest boots from (`kvmtest kvmakuma`).
#
#   scripts/kvm/akuma_guest_rootfs.sh OUT.img BUSYBOX
#
# BUSYBOX is a static x86_64 busybox (the one `amd64/mkdisk.sh` fetches:
# target/x86_64-unknown-none/release/busybox-x86_64). The guest kernel is told
# `init=/bin/busybox initargs=sh,/etc/guest.sh`; the script checks a read (md5 of
# busybox) and a write (a 256 KiB pattern file, synced), then prints
# AKUMA-GUEST-DONE, which the hypervisor takes as the pass marker. Needs
# e2fsprogs. See docs/archive/AKUMA_KVM_AKUMA_GUEST.md.
set -eu
OUT=${1:?usage: akuma_guest_rootfs.sh OUT.img BUSYBOX}
BB=${2:?usage: akuma_guest_rootfs.sh OUT.img BUSYBOX}
E2=${E2FSPROGS:-$(dirname "$(command -v mke2fs 2>/dev/null || echo /opt/homebrew/opt/e2fsprogs/sbin/mke2fs)")}
T=$(mktemp -d)
trap 'rm -rf "$T"' EXIT
mkdir -p "$T/bin" "$T/sbin" "$T/etc" "$T/proc" "$T/tmp"
cp "$BB" "$T/bin/busybox"
chmod 755 "$T/bin/busybox"
ln -s busybox "$T/bin/sh"
cat > "$T/etc/guest.sh" <<'SH'
/bin/busybox echo AKUMA-GUEST-UP
/bin/busybox uname -a
/bin/busybox md5sum /bin/busybox
/bin/busybox yes | /bin/busybox head -c 262144 > /wtest
/bin/busybox sync
/bin/busybox md5sum /wtest
/bin/busybox echo AKUMA-GUEST-DONE
SH
rm -f "$OUT"
"$E2/mke2fs" -q -t ext2 -d "$T" -L akumaguest "$OUT" 16M
echo "wrote $OUT"
