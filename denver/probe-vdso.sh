#!/bin/sh
# denver/probe-vdso.sh - the PHASE-2-VDSO gate probe (probe-faultname.sh's shape: stamps +
# console window; a NEW rig: denver-vdso kernel binary + vm-vdso.spec, D25/D26).
#
#   sh denver/probe-vdso.sh 2>&1 | tee denver/gates/PHASE-2-VDSO.log
#
# G3 (denver/docs/REQUIREMENTS.md): the ABI gap the FAULTNAME gate proved first-hand - HotSpot
# loads 0xffffffffff600800 as a sign-extended imm32 and calls the legacy vsyscall getcpu slot
# as a C function (libjvm.so+0xaef710; denver/docs/research/PHASE-2-VDSO.md, Finding 0), and
# the page did not exist, so the JVM died there twice (metal hs_err, guest console, one pc).
# The vsyscall feature serves the page user-RX from the kernel root and discloses it in auxv
# as AT_SYSINFO_EHDR (D21).
#
# Pass condition (D26, in order):
#   1. uname says denver AND NOT the WX rig hash 8a658723 (the rig DID change; the old binary
#      booting would mean staging failed);
#   2. tokioprobe RESULT PASS (platform sanity under the new mapping);
#   3. RWX PASS rw_to_rx=42 (wx-relaxed still active - this binary carries both features);
#   4. AUXV PASS AT_SYSINFO_EHDR= (phase 1's RED arm flips; the value is the page's real
#      address, 0xffffffffff600000 = 18446744073709493248);
#   5. the java arm runs, and the console window carries NO `rip=0xffffffffff600800` - the
#      closed death must not recur. The workload's next shape (a new fault elsewhere, a clean
#      nonzero exit, or outright JVM PASS) is recorded as movement/surprise, not gated.
#
# Expected movement (D24): the +0x800 stub executes syscall 168, still ENOSYS (G2); HotSpot's
# wrapper (`cmp $-1`) does not treat -38 as failure and reads the pre-zeroed cpu, so the JVM
# should get PAST the SIGSEGV. Whatever comes next is evidence for the re-rank.
#
# Machine notes carried from vm-probe.sh/probe-wx.sh/probe-faultname.sh: the host runs ONE VM
# at a time - free the slot first (a stale guest answers EBUSY/errno 16, not "broken KVM");
# wait for an ssh round trip, never a log marker; always stop the guest; grep console-derived
# text -a-tolerantly. Not a `#!` script on the box: Akuma's execve cannot run one. Invoke as
# `sh <file>`.

W=${DENVER_W:-/root/denver/w}
SPEC=${1:-$W/vm-vdso.spec}
PORT=${DENVER_PORT:-2247}
WAIT=${DENVER_BOOT_WAIT_TRIES:-30}     # x 5 s
SSH="ssh -y -i $W/key -p $PORT root@127.0.0.1"
VMLOG=$W/vmrun.log
WXHASH="8a658723"                      # the proven WX/FAULTNAME binary - must NOT boot now (D26)
VSYSCALL=ffffffffff600800             # the legacy vsyscall getcpu slot (Linux ABI constant)
BLOCKER1="Failed to mark memory page as executable"
T0=$(date +%s)

VP=
step() {                                   # epoch stamp per step (L2 debt 2)
    N=$(date +%s); E=$((N - T0)); CL=""
    if [ "$E" -lt 0 ]; then E=0; CL=" -- clock slewed, elapsed clamped (t0=$T0 epoch=$N)"; fi
    echo "== $1 (elapsed=${E}s$CL)"
}
verdict() { E=$(( $(date +%s) - T0 )); [ "$E" -lt 0 ] && E=0
    echo "$1"; stop_vm; echo "== wall clock: seconds=$E (t0=$T0 end=$(date +%s))"; exit "$2"; }
stop_vm() {
    echo stop > /dev/kvm0 2>/dev/null
    [ -n "$VP" ] && kill "$VP" 2>/dev/null
    VP=
}

[ -r "$SPEC" ] || verdict "RESULT FAIL: no spec at $SPEC (stage denver-vdso + vm-vdso.spec first, see denver/docs/research/PHASE-2-VDSO.md D25)" 1
[ -r "$W/key" ] || verdict "RESULT FAIL: no key at $W/key" 1
[ -r "$VMLOG" ] || verdict "RESULT FAIL: no console log at $VMLOG - the console capture is this gate's evidence" 1

step "free the VM slot (a stale guest answers EBUSY)"
echo stop > /dev/kvm0 2>/dev/null
sleep 3

step "boot: herd vm-run $SPEC"
herd vm-run "$SPEC" > "$VMLOG" 2>&1 &
VP=$!

i=0
while :; do
    out=$($SSH "uname -a" 2>&1)
    case "$out" in *Linux*) break ;; esac
    i=$((i + 1))
    if [ "$i" -gt "$WAIT" ]; then
        echo "-- vm-run log tail:"; tail -c 600 "$VMLOG"
        verdict "RESULT FAIL: guest never answered ssh in $((WAIT * 5)) s" 1
    fi
    case "$(tail -c 300 "$VMLOG")" in
        *"no /dev/kvm0"*) verdict "RESULT FAIL: no /dev/kvm0 - host not booted with kvmctl" 1 ;;
        *"errno 16"*) verdict "RESULT FAIL: VM slot still busy (EBUSY) after stop" 1 ;;
    esac
    sleep 5
done

step "rig identity: uname says denver, and NOT the WX rig hash $WXHASH (D26)"
echo "$out" | grep Linux
case "$out" in
    *denver*) ;;
    *) verdict "RESULT FAIL: uname does not say denver" 1 ;;
esac
case "$out" in
    *"$WXHASH"*) verdict "RESULT FAIL: the WX rig binary booted - staging of denver-vdso failed" 1 ;;
esac

step "the new mapping's boot line (console side)"
grep -a "vsyscall:" "$VMLOG" | tail -2

step "platform sanity: tokioprobe (phase 0's marker)"
tok=$(timeout 90 $SSH "/bin/tokioprobe" 2>&1)
echo "$tok"
case "$tok" in *"RESULT PASS"*) ;; *) verdict "RESULT FAIL: tokioprobe did not pass under the vsyscall feature" 1 ;; esac

step "wx-relaxed proof: the rwx arm (this binary carries wx-relaxed AND vsyscall)"
rwx=$(timeout 60 $SSH "/bin/jvmprobe rwx" 2>&1)
echo "$rwx"
case "$rwx" in *"RWX PASS rw_to_rx=42"*) ;; *) verdict "RESULT FAIL: rwx arm did not print RWX PASS rw_to_rx=42" 1 ;; esac

step "the G3 flip: the auxv arm (RED in phase 1: AT_SYSINFO_EHDR missing)"
aux=$(timeout 60 $SSH "/bin/jvmprobe auxv" 2>&1)
echo "$aux"
case "$aux" in
    *"AUXV PASS AT_SYSINFO_EHDR="*) ;;
    *) verdict "RESULT FAIL: the auxv arm did not print AUXV PASS AT_SYSINFO_EHDR=" 1 ;;
esac
case "$aux" in
    *"AUXV PASS AT_SYSINFO_EHDR=18446744073709493248"*)
        echo "  (value = 0xffffffffff600000: the disclosed page is the ABI-fixed base, D21)" ;;
    *)
        echo "  (SURPRISE: AT_SYSINFO_EHDR carries a different value - the disclosure and the map disagree; read the research artifact)" ;;
esac

step "the death must not recur: java -version, then the console window"
OFF=$(wc -c < "$VMLOG")                    # console bytes before the arm: attribution offset
jout=$(timeout 180 $SSH "/bin/jvmprobe java /opt/jdk/bin/java" 2>&1)
echo "$jout"
case "$jout" in
    *"$BLOCKER1"*) verdict "RESULT FAIL: the workload died at the W^X blocker-1 line - wx-relaxed regressed?" 1 ;;
esac

sleep 2
win=$(tail -c +$((OFF + 1)) "$VMLOG" 2>/dev/null | grep -a .)
faultline=$(printf '%s\n' "$win" | grep 'ring3 PageFault rip=' | tail -1)
slotline=$(printf '%s\n' "$win" | grep -F 'killed (SIGSEGV)' | tail -1)
echo "-- console window: fault line:"
echo "${faultline:-<none>}"
echo "-- console window: slot line:"
echo "${slotline:-<none>}"

case "$faultline" in
    *"$VSYSCALL"*)
        echo "-- console window tail (the vsyscall fetch recurred):"
        printf '%s\n' "$win" | tail -15
        verdict "RESULT FAIL: the workload fetched the vsyscall page again - the map did not take (rip=0x$VSYSCALL in the window)" 1 ;;
esac

case "$jout" in
    *"JVM PASS"*)
        echo "MOVEMENT PASS: java -version passed outright (surprise: the ladder moved past the whole blocker list; re-check REQUIREMENTS G2/G4 before celebrating)" ;;
    *"signal=11"*)
        echo "MOVEMENT PASS: past the vsyscall death - a NEW fault elsewhere, named by the console above (re-rank: the next blocker is that one)" ;;
    *"getcpu"*)
        echo "MOVEMENT PASS: past the vsyscall death - the workload died at getcpu (the attempt's blocker 2; G2)" ;;
    *)
        echo "MOVEMENT PASS: the workload exited without the vsyscall death and without a signal - read the arm's own lines above" ;;
esac

step "verdict"
verdict "RESULT PASS" 0
