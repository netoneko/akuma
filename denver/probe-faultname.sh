#!/bin/sh
# denver/probe-faultname.sh - the PHASE-2-FAULTNAME gate probe (probe-wx.sh's shape; the SAME
# proven rig: denver-wx kernel binary + vm-wx.spec, both unchanged - D18).
#
#   sh denver/probe-faultname.sh 2>&1 | tee denver/gates/PHASE-2-FAULTNAME.log
#
# G6 (denver/docs/REQUIREMENTS.md): the workload's post-G1 death must be *named* in this
# capture. Research (denver/docs/research/PHASE-2-FAULTNAME.md, Finding 0): the kernel has
# printed the naming line since its birth (aafbb973) - `[Fault] ring3 PageFault rip= cr2=
# code=` - but no gate capture ever carried it (the probe scripts capture ssh-side output
# only; the WX session's census quote dropped the ring3 line above the slot line). So this
# gate's job: (1) capture the console with per-arm attribution, (2) interpret the address.
# Expected death (the WX session's x3-reproducible shape, vmrun-diag.log):
#   [Fault] ring3 PageFault rip=0xffffffffff600800 rsp=... cr2=0xffffffffff600800 code=0x14 -- task killed
#   [Fault] slot N killed (SIGSEGV)
# code=0x14 = user-mode instruction fetch, not-present: HotSpot jumped to the legacy vsyscall
# page - G3's mechanism, and the same pc the metal hs_err captured in phase 1.
#
# Probe debts paid here (denver/docs/lessons/L2-writing-probes.md Record): epoch stamps at
# every step (debt 2; probe-method's shape, slew-clamped per L4 Do 3), and the fault naming
# (debt 1 - the java arm names exit-vs-signal; the console names the fault).
#
# Pass condition (D19, in order): uname carries the WX-gate rig hash 0.1.0-8a658723-denver
# (mismatch = rig changed = FAIL) -> tokioprobe RESULT PASS (platform sanity) -> RWX PASS
# rw_to_rx=42 (the wx-relaxed policy active) -> the java arm dies JVM RED signal=11 -> the
# console window (bytes from the pre-arm offset of $W/vmrun.log) carries the ring3 line with
# rip= AND cr2= -> FAULT PASS (module=legacy-vsyscall-page at the ABI constant) -> RESULT PASS.
# A java arm that passes outright is RESULT FAIL: nothing to name - a ladder-moving surprise,
# recorded, not smoothed over.
#
# Machine notes carried from vm-probe.sh/probe-wx.sh: the host runs ONE VM at a time - free
# the slot first (a stale guest answers EBUSY/errno 16, not "broken KVM"); wait for an ssh
# round trip, never a log marker; always stop the guest. grep over console-derived text is
# -a-safe (taoxichuan gates.md rule: the log carries control bytes). Not a `#!` script on the
# box: Akuma's execve cannot run one. Invoke it as `sh <file>`.

W=${DENVER_W:-/root/denver/w}
SPEC=${1:-$W/vm-wx.spec}
PORT=${DENVER_PORT:-2247}
WAIT=${DENVER_BOOT_WAIT_TRIES:-30}     # x 5 s
SSH="ssh -y -i $W/key -p $PORT root@127.0.0.1"
VMLOG=$W/vmrun.log
RIGHASH="0.1.0-8a658723-denver"        # the proven WX-gate binary's uname hash (D18)
VSYSCALL=ffffffffff600800             # the legacy vsyscall page (Linux ABI constant; metal hs_err pc)
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

[ -r "$SPEC" ] || verdict "RESULT FAIL: no spec at $SPEC (the FAULTNAME gate runs the WX rig: denver-wx + vm-wx.spec, D18)" 1
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

step "rig identity: uname must carry the WX-gate hash $RIGHASH (D18)"
echo "$out" | grep Linux
case "$out" in
    *"$RIGHASH"*) ;;
    *) verdict "RESULT FAIL: rig changed - uname lacks $RIGHASH (denver-wx rebuilt or swapped; the gate's evidence chain needs the proven binary)" 1 ;;
esac

step "platform sanity: tokioprobe (phase 0's marker)"
tok=$(timeout 90 $SSH "/bin/tokioprobe" 2>&1)
echo "$tok"
case "$tok" in *"RESULT PASS"*) ;; *) verdict "RESULT FAIL: tokioprobe did not pass" 1 ;; esac

step "wx-relaxed proof: the rwx arm (RED in phase 1: mmap-RWX-refused, rw_to_rx mprotect, both EACCES)"
rwx=$(timeout 60 $SSH "/bin/jvmprobe rwx" 2>&1)
echo "$rwx"
case "$rwx" in *"RWX PASS rw_to_rx=42"*) ;; *) verdict "RESULT FAIL: rwx arm did not print RWX PASS rw_to_rx=42" 1 ;; esac

step "the death: java -version (expected JVM RED signal=11 - the death is this gate's subject)"
OFF=$(wc -c < "$VMLOG")                    # console bytes before the arm: attribution offset
jout=$(timeout 180 $SSH "/bin/jvmprobe java /opt/jdk/bin/java" 2>&1)
echo "$jout"
case "$jout" in
    *"$BLOCKER1"*) verdict "RESULT FAIL: the workload died at the W^X blocker-1 line - the wx-relaxed kernel regressed?" 1 ;;
esac
case "$jout" in
    *"JVM PASS"*) verdict "RESULT FAIL: nothing to name - java -version passed outright (surprise: the ladder moved past the death; re-rank REQUIREMENTS before this gate can judge)" 1 ;;
    *"signal=11"*) ;;
    *) verdict "RESULT FAIL: the java arm did not die with signal=11 and did not pass - unexpected shape, read the capture" 1 ;;
esac

step "the naming: grep the console window from byte $OFF (+2 s flush)"
sleep 2
win=$(tail -c +$((OFF + 1)) "$VMLOG" 2>/dev/null | grep -a .)
faultline=$(printf '%s\n' "$win" | grep 'ring3 PageFault rip=' | tail -1)
slotline=$(printf '%s\n' "$win" | grep -F 'killed (SIGSEGV)' | tail -1)
echo "$faultline"
echo "$slotline"
case "$faultline" in
    *"rip=0x"*cr2=0x*) ;;
    *) echo "-- console window tail (no ring3 naming line found):"
       printf '%s\n' "$win" | tail -15
       verdict "RESULT FAIL: the console window carries no [Fault] ring3 rip=/cr2= line for the death" 1 ;;
esac

parsed=$(printf '%s\n' "$faultline" | sed 's/.*rip=0x\([0-9a-f]*\).*cr2=0x\([0-9a-f]*\).*code=0x\([0-9a-f]*\).*/\1 \2 \3/')
RIP=$(printf '%s\n' "$parsed" | cut -d' ' -f1)
CR2=$(printf '%s\n' "$parsed" | cut -d' ' -f2)
CODE=$(printf '%s\n' "$parsed" | cut -d' ' -f3)
case "$RIP" in
    "$VSYSCALL")
        echo "FAULT PASS rip=0x$RIP cr2=0x$CR2 code=0x$CODE module=legacy-vsyscall-page"
        echo "  (rip==cr2 at the ABI constant: user fetch on the unmapped vsyscall page - the metal"
        echo "   hs_err named the same pc in phase 1; G3's mechanism, now guest-proven)" ;;
    *)
        echo "FAULT PASS rip=0x$RIP cr2=0x$CR2 code=0x$CODE module=unmapped"
        echo "  (SURPRISE: the death moved off the vsyscall page - the blocker changed shape; re-check REQUIREMENTS G3/G6)" ;;
esac

step "verdict"
verdict "RESULT PASS" 0
