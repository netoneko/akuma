#!/bin/sh
# guest-up.sh [KERNEL_ELF] - install a kernel as the guest kernel and boot the dev guest on PART.
# Prints one line: `UP ready_in=<s> disk=/dev/nvme0n1pN` or `FAIL <reason>`. Detail: $OUT/guest-up.log
set -u
. "$(dirname "$0")/env.sh"
need_part
K=${1:-$KBUILD_ELF}
L=$OUT/guest-up.log
fail() { echo "FAIL $1"; exit 1; }
[ -f "$K" ] || fail "no kernel ELF $K"
case "$GUEST_KERNEL" in /boot/akuma-amd64*) fail "refusing to overwrite the host kernel" ;; esac

T0=$(date +%s)
for s in $OTHER_VMS $VM; do herd stop "$s" >>"$L" 2>&1 || true; done
# Disabled while the files change: herd starts an enabled-but-stopped service on its next
# reload tick (~20 s) and would boot a guest against a half-written conf.
herd disable "$VM" >>"$L" 2>&1 || true
"$AKUMA_SRC/scripts/install_kernel_amd64.sh" "$K" "$GUEST_KERNEL" >>"$L" 2>&1 || fail "kernel install"

EPOCH=$(date +%s)
sed -e "s|@PART@|$PART|g" -e "s|@EPOCH@|$EPOCH|g" -e "s|@KERNEL@|$GUEST_KERNEL|g" \
    -e "s|@SSH_PORT@|$SSH_PORT|g" "$OV/vm-dev-part.conf.tpl" > /etc/herd/available/$VM.conf || fail "render conf"
cp /etc/herd/available/$VM.conf /etc/herd/enabled/$VM.conf || fail "enable conf"
herd enable "$VM" >>"$L" 2>&1 || fail "herd enable"
herd start "$VM" >>"$L" 2>&1 || fail "herd start"

# Readiness is a completed ssh command, never a console marker (CLAUDE.md "Waiting for a VM").
ready=""; i=0
while [ $i -lt 6 ]; do
    [ $i -gt 0 ] && sleep 30
    gssh 'echo __UP__' >"$OUT/probe.out" 2>>"$L"
    grep -q __UP__ "$OUT/probe.out" && { ready=1; break; }
    i=$((i+1))
done
[ -n "$ready" ] || fail "no ssh round trip on port $SSH_PORT (see $L and /var/log/herd/$VM.log)"
grep -q "^disk=/dev/nvme0n1p$PART" /var/run/herd/$VM.vm 2>/dev/null || fail "runfile disk is not p$PART"
echo "UP ready_in=$(( $(date +%s) - T0 ))s disk=/dev/nvme0n1p$PART"
