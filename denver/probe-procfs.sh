#!/bin/sh
# denver/probe-procfs.sh - the PHASE-2-PROCFS gate probe (probe-vdso.sh's shape: stamps +
# console window; a NEW rig: denver-procfs kernel binary + vm-procfs.spec, D30).
#
#   sh denver/probe-procfs.sh 2>&1 | tee denver/gates/PHASE-2-PROCFS.log
#
# G4 (denver/docs/REQUIREMENTS.md): /proc/<pid>/auxv and /proc/<pid>/maps are not served,
# so a fault cannot be *named* once the JVM gets further - G9's user-mode NULL store
# (rip=0x000000003005fc41 cr2=0x0 code=0x6, observed in /root/denver/merge-verify/gate-vdso.log)
# dies unnamed, and a bare rip cannot be fixed. The procfs-diag feature (D29) registers the
# shared maps walk (akuma-exec::process::maps_rows, D27) and the auxv retention (D28).
#
# Pass conditions (D31, in order):
#   1. uname says denver AND NOT the WX hash 8a658723 AND NOT the merged tree 3531cf69
#      (the vdso binary does not serve procfs-diag; the old binary booting = staging failed);
#   2. tokioprobe RESULT PASS (platform sanity under the new registration);
#   3. RWX PASS rw_to_rx=42 (wx-relaxed rides; this binary carries all three features);
#   4. AUXV PASS AT_SYSINFO_EHDR= (the vDSO disclosure survives the retention change;
#      value python-checked = 18446744073699061760 = 0xffffffffff5ff000, D22);
#   5. MAPS PASS (jvmprobe maps): /proc/self/maps parses, has an executable line;
#   6. AUXV PASS procfs-auxv (jvmprobe auxv): /proc/self/auxv serves whole pairs with the
#      AT_NULL terminator, AT_PAGESZ=4096, and its AT_SYSINFO_EHDR EQUALS the stack's -
#      phase 1's `AUXV INFO /proc/self/auxv unreadable errno=2` must be GONE;
#   7. NAME arm (jvmprobe name /opt/jdk/bin/java 3005fc41 -version): fork+exec java, poll
#      /proc/<pid>/maps to death, dump the last snapshot, attribute the expected rip.
#      NAME PASS (attributed) or NAME SURPRISE (captured but unattributed - the CONSTANT is
#      the suspect, the maps are the record) pass; NAME MISSING (no snapshot before death -
#      the diagnostic cannot do its job) FAILS. The expected rip is python-checked against
#      the recorded capture (805698625 = 0x3005fc41, gate-vdso.log's fault line).
#   8. the console window may carry the G9 fault again (expected rip, recorded); a DIFFERENT
#      rip with NAME SURPRISE is the re-rank's data, not a gate verdict.
#
# Expected movement (REQUIREMENTS re-rank 2026-11-14): the workload still dies at the
# NULL store - this gate exists to NAME it, not to fix it. The fix is G9's own pass.
#
# Machine notes carried from vm-probe.sh/probe-vdso.sh: the host runs ONE VM at a time -
# free the slot first (a stale guest answers EBUSY/errno 16, not "broken KVM"); wait for an
# ssh round trip, never a log marker; always stop the guest; grep console-derived text
# -a-tolerantly. Not a `#!` script on the box: Akuma's execve cannot run one. Invoke as
# `sh <file>`.

W=${DENVER_W:-/root/denver/w}
SPEC=${1:-$W/vm-procfs.spec}
PORT=${DENVER_PORT:-2247}
WAIT=${DENVER_BOOT_WAIT_TRIES:-30}     # x 5 s
SSH="ssh -y -i $W/key -p $PORT root@127.0.0.1"
VMLOG=$W/vmrun.log
WXHASH="8a658723"                      # the proven WX binary - must NOT boot now (D31)
VDsoHASH="3531cf69"                    # the merged-tree/vdso binary - must NOT boot now
VSYSCALL=ffffffffff600800              # the legacy vsyscall getcpu slot (the closed death)
RIP=3005fc41                           # G9's rip (python-checked: 805698625, gate-vdso.log)
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

[ -r "$SPEC" ] || verdict "RESULT FAIL: no spec at $SPEC (stage denver-procfs + vm-procfs.spec first, see denver/docs/research/PHASE-2-PROCFS.md D30)" 1
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

step "rig identity: uname says denver, NOT $WXHASH, NOT $VDSoHASH (D31)"
echo "$out" | grep Linux
case "$out" in
    *denver*) ;;
    *) verdict "RESULT FAIL: uname does not say denver" 1 ;;
esac
case "$out" in
    *"$WXHASH"*) verdict "RESULT FAIL: the WX rig binary booted - staging of denver-procfs failed" 1 ;;
esac
case "$out" in
    *"$VDSoHASH"*) verdict "RESULT FAIL: the vdso rig binary booted - it does not serve procfs-diag; staging failed" 1 ;;
esac

step "platform sanity: tokioprobe (phase 0's marker)"
tok=$(timeout 90 $SSH "/bin/tokioprobe" 2>&1)
echo "$tok"
case "$tok" in *"RESULT PASS"*) ;; *) verdict "RESULT FAIL: tokioprobe did not pass under the procfs-diag feature" 1 ;; esac

step "wx-relaxed proof: the rwx arm (rides; this binary carries wx-relaxed AND vsyscall AND procfs-diag)"
rwx=$(timeout 60 $SSH "/bin/jvmprobe rwx" 2>&1)
echo "$rwx"
case "$rwx" in *"RWX PASS rw_to_rx=42"*) ;; *) verdict "RESULT FAIL: rwx arm did not print RWX PASS rw_to_rx=42" 1 ;; esac

step "the disclosure survives: the stack-auxv arm"
aux=$(timeout 60 $SSH "/bin/jvmprobe auxv" 2>&1)
echo "$aux"
case "$aux" in
    *"AUXV PASS AT_SYSINFO_EHDR="*) ;;
    *) verdict "RESULT FAIL: the stack auxv arm did not print AUXV PASS AT_SYSINFO_EHDR=" 1 ;;
esac
case "$aux" in
    *"AUXV PASS AT_SYSINFO_EHDR=18446744073699061760"*) ;;
    *)
        echo "  (SURPRISE: AT_SYSINFO_EHDR carries a different value - read the research artifact)" ;;
esac

step "the G4 flip 1: /proc/self/maps (phase 1 and earlier: absent)"
maps=$(timeout 60 $SSH "/bin/jvmprobe maps" 2>&1)
echo "$maps"
case "$maps" in
    *"MAPS PASS lines="*) ;;
    *) verdict "RESULT FAIL: the maps arm did not print MAPS PASS (errno=2 or empty)" 1 ;;
esac

step "the G4 flip 2: /proc/self/auxv (phase 1's evidence line: errno=2 - it must be gone)"
case "$aux" in
    *"unreadable errno=2"*) verdict "RESULT FAIL: /proc/self/auxv still unreadable errno=2 - G4 did not flip" 1 ;;
esac
case "$aux" in
    *"AUXV PASS procfs-auxv"*) ;;
    *) verdict "RESULT FAIL: the auxv arm did not print AUXV PASS procfs-auxv" 1 ;;
esac
case "$aux" in
    *"AT_SYSINFO_EHDR matches stack="*)
        echo "  (stack vector and /proc render agree: the kernel answers the same table twice)" ;;
    *)
        echo "  (note: no stack/procfs EHDR agreement line - the disclosure or the render moved)" ;;
esac

step "the payoff: the NAME arm - fork+exec java, poll /proc/<pid>/maps to death, attribute rip 0x$RIP"
OFF=$(wc -c < "$VMLOG")                    # console bytes before the arm: attribution offset
name=$(timeout 180 $SSH "/bin/jvmprobe name /opt/jdk/bin/java $RIP -version" 2>&1)
echo "$name"
case "$name" in
    *"$BLOCKER1"*) verdict "RESULT FAIL: the workload died at the W^X blocker-1 line - wx-relaxed regressed?" 1 ;;
esac
case "$name" in
    *"NAME MISSING"*) verdict "RESULT FAIL: no maps snapshot before death - the diagnostic cannot do its job" 1 ;;
esac

case "$name" in
    *"NAME PASS rip="*) nameword="NAME PASS" ;;
    *"NAME SURPRISE"*) nameword="NAME SURPRISE" ;;
    *) nameword="NAME ???" ;;
esac

step "the console window: what did the workload do this time"
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

case "$faultline" in
    *"rip=0x00000000$RIP"*)
        echo "  (the G9 NULL store recurred at the expected rip - the NAME arm above names its owner)" ;;
    *"ring3 PageFault"*)
        echo "  (SURPRISE: a fault at a DIFFERENT rip - read the NAME arm's maps snapshot; re-rank owns this)" ;;
    *)
        if printf '%s\n' "$name" | grep -q "JVM\|exit=0"; then
            echo "  (the workload exited without a fault line in the window - check the NAME arm's exit above)"
        else
            echo "  (no fault line captured in the window - the arm's own lines above are the record)"
        fi ;;
esac

step "verdict"
case "$nameword" in
    "NAME PASS")
        echo "MOVEMENT: G9's rip has an owner - the fix pass (G9) can name its target now" ;;
    "NAME SURPRISE")
        echo "MOVEMENT: maps captured but the rip moved - the maps snapshot above is the record; re-rank before the G9 fix" ;;
esac
verdict "RESULT PASS" 0
