#!/bin/sh
# Phase 0 of docs/archive/AKUMA_FIRECRACKER_KVM.md: can Alpine's unmodified `firecracker` run on Akuma?
#
#   scripts/kvm/firecracker_phase0.sh LOG [SECONDS]
#
# Builds an ext2 root (busybox, Alpine musl + libgcc, firecracker from FCDIR/GCCDIR/MUSLDIR = extracted
# apks) and boots the amd64 kernel under QEMU microvm (TCG) with `strace_err`, running /etc/p0.sh.
# The console log is LOG. Env: FCDIR GCCDIR MUSLDIR (extracted apks), KERNEL, BUSYBOX, P0 (script body).
set -eu
LOG=${1:?usage: firecracker_phase0.sh LOG [SECONDS]}
SECS=${2:-240}
cd "$(dirname "$0")/../.."
E2=${E2FSPROGS:-/opt/homebrew/opt/e2fsprogs/sbin}
KERNEL=${KERNEL:-target/x86_64-unknown-none/release/akuma-amd64}
BB=${BUSYBOX:-target/x86_64-unknown-none/release/busybox-x86_64}
T=$(mktemp -d); trap 'rm -rf "$T"' EXIT
mkdir -p "$T/bin" "$T/etc" "$T/proc" "$T/tmp" "$T/lib" "$T/usr/bin" "$T/usr/lib" "$T/boot" "$T/dev"
cp "$BB" "$T/bin/busybox"; chmod 755 "$T/bin/busybox"; ln -s busybox "$T/bin/sh"
for a in ls cat grep sed head tail wc echo sleep ps env; do ln -sf busybox "$T/bin/$a"; done
cp "$MUSLDIR/lib/ld-musl-x86_64.so.1" "$T/lib/"; cp "$MUSLDIR/lib/libc.musl-x86_64.so.1" "$T/lib/"
cp "$GCCDIR/usr/lib/libgcc_s.so.1" "$T/usr/lib/"; cp "$GCCDIR/usr/lib/libgcc_s.so.1" "$T/lib/"
cp "$FCDIR/usr/bin/firecracker" "$T/usr/bin/"; chmod 755 "$T/usr/bin/firecracker"
cp "$KERNEL" "$T/boot/akuma-guest"
cat > "$T/vm.json" <<'J'
{"boot-source":{"kernel_image_path":"/boot/akuma-guest","boot_args":"console=ttyS0"},
 "machine-config":{"vcpu_count":1,"mem_size_mib":128},
 "drives":[{"drive_id":"rootfs","path_on_host":"/boot/akuma-guest","is_root_device":true,"is_read_only":true}]}
J
if [ -n "${P0:-}" ]; then printf '%s\n' "$P0" > "$T/etc/p0.sh"; else cat > "$T/etc/p0.sh" <<'S'
echo P0-BEGIN
/usr/bin/firecracker --version
echo "version rc=$?"
/usr/bin/firecracker --no-api --config-file /vm.json
echo "run rc=$?"
echo P0-END
S
fi
"$E2/mke2fs" -q -t ext2 -d "$T" -L p0 /tmp/p0root.$$.img 64M 2>/dev/null || "$E2/mke2fs" -q -t ext2 -d "$T" /tmp/p0root.$$.img 64M
IMG=/tmp/p0root.$$.img
CL="virtio_mmio.device=512@0xfeb00000:5 init=/bin/busybox initargs=sh,/etc/p0.sh strace_err ${KCMDLINE:-}"
timeout "$SECS" qemu-system-x86_64 -M microvm -cpu EPYC-Milan,svm=on -global virtio-mmio.force-legacy=false \
  -kernel "$KERNEL" -m 2048 -smp 1 \
  -drive id=d0,file=$IMG,format=raw,if=none -device virtio-blk-device,drive=d0,bus=virtio-mmio-bus.0 \
  -append "$CL" -serial mon:stdio -display none -no-reboot > "$LOG" 2>&1 || true
rm -f "$IMG"
grep -a -c . "$LOG" | sed 's/^/log lines: /'
