#!/bin/sh
# cycle.sh [kbuild args...] - one turn of the loop: build the kernel, boot it as a guest of this
# kernel on PART, check it, report. Exits 0 on PASS. Never touches /boot/akuma-amd64*.
set -u
. "$(dirname "$0")/env.sh"
need_part
R=$OUT/cycle-$(date +%s).txt
{
echo "== build"; (cd "$AKUMA_SRC" && kbuild -j 4 --features no-tests "$@") || { echo "VERDICT FAIL build"; exit 1; }
echo "== boot guest"; U=$("$OV/guest-up.sh") ; echo "$U"
case "$U" in UP*) ;; *) echo "VERDICT FAIL guest-up"; exit 1 ;; esac
echo "== guest"; gssh 'uname -a; echo AKUMA-GUEST-OK' || { echo "VERDICT FAIL guest ssh"; exit 1; }
echo "== host dmesg: faults"; dmesg 2>/dev/null | tr -d '\033' | grep -a -E 'EXCEPTION|panic|\[KVM\].*(refused|fault)' | tail -5
echo "== guest console tail"; tail -n 15 /var/log/herd/$VM.log
echo "VERDICT PASS"
} 2>&1 | tee "$R"
grep -q '^VERDICT PASS' "$R"
