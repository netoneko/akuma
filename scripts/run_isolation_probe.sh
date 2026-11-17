#!/bin/sh
# Boot the amd64 kernel with `userspace/amd64/isoprobe` as init and report its verdict.
#
# The probe forks and execs real processes and checks what the kernel does when one tries to read another's memory
# (see its header). Exit status: 0 = `ISOPROBE PASS`, 1 = `ISOPROBE FAIL`, 2 = the probe did not report at all.
#
#   scripts/run_isolation_probe.sh                 # KVM if /dev/kvm is usable, otherwise TCG
#   ACCEL=tcg scripts/run_isolation_probe.sh       # force software emulation
#   TIMEOUT=120 scripts/run_isolation_probe.sh
#
# Needs what `amd64/run.sh` and `amd64/mkdisk.sh` need: mkfs.ext2 and debugfs (e2fsprogs), qemu-system-x86_64, and
# `rust-readobj` on PATH (rustup's llvm-tools component).
set -e
cd "$(dirname "$0")/.."

TIMEOUT="${TIMEOUT:-150}"
if [ -z "$ACCEL" ]; then
    if [ -r /dev/kvm ] && [ -w /dev/kvm ]; then ACCEL=kvm; else ACCEL=tcg; fi
fi
OUT="$(mktemp -d)"
IMG="$OUT/isoprobe.img"
LOG="$OUT/boot.log"
trap 'rm -rf "$OUT"' EXIT

cargo build -p akuma --target x86_64-unknown-none --release >/dev/null 2>&1
sh amd64/mkdisk.sh "$IMG" 64 >/dev/null 2>&1

case "$ACCEL" in
    kvm) EXTRA="-accel kvm -cpu host" ;;
    *)   EXTRA="" ;;
esac
# shellcheck disable=SC2086
DISK="$IMG" INIT=/bin/isoprobe HTTP_PORT="${HTTP_PORT:-18090}" SSH_PORT="${SSH_PORT:-12290}" \
    timeout "$TIMEOUT" sh amd64/run.sh $EXTRA >"$LOG" 2>&1 || true

grep -a "ISOPROBE\|PEEK-SURVIVED\|KERNEL-READ-SURVIVED\|status 0x" "$LOG" || true
if grep -aq "ISOPROBE PASS" "$LOG"; then
    echo "isolation probe: PASS ($ACCEL)"
    exit 0
elif grep -aq "ISOPROBE FAIL" "$LOG"; then
    echo "isolation probe: FAIL ($ACCEL)"
    exit 1
fi
echo "isolation probe: no verdict in $TIMEOUT s ($ACCEL); tail of the boot log:"
tail -15 "$LOG"
exit 2
