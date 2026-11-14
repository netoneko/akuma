#!/bin/sh
# denver/probe-heapro.sh - the PHASE-2-HEAPRO gate probe (probe-nullfix.sh's
# shape: stamps + free-the-slot first + stop at the end; NEW rig: the
# denver-heapro kernel binary + vm-heapro.spec, D46).
#
#   sh denver/probe-heapro.sh 2>&1 | tee denver/gates/PHASE-2-HEAPRO.log
#
# The HEAPRO fix pass (denver/docs/research/PHASE-2-HEAPRO.md): the NULLFIX java
# death was a user WRITE to a PRESENT read-only page (code=0x7,
# pte=0x20dc6025 - unmarked, refs=2) at a VA whose only mm event was a MAP_FIXED
# release (kind=2, 0x6000112e9000..0x600011559000). The release sweep is
# present-only: the P=0 lazy records `lazy::reserve` installs survive every
# release (the walk classifies them Missing), and the not-present fault arm
# (`lazy::fill`) decodes them with no region check - the released range came
# back, a stale file record read RO, and the next write died unmarked (D44).
# The fix is `lazy-release-sweep` (off by default, R6): the release sweep also
# clears the range's lazy records.
#
# Pass conditions (D47, in order):
#   1. uname says denver AND NOT the nullfix hash 72be5c55 (no sweep in it) AND
#      NOT nullstore 800e8c80 AND NOT procfs f0dcacb6 AND NOT vdso 3531cf69
#      (a stale binary booting = staging failed; empty-constant abort);
#   2. tokioprobe RESULT PASS (platform sanity);
#   3. RWX PASS rw_to_rx=42 (rides);
#   4. MAPS PASS + AUXV PASS procfs-auxv (rides);
#   5. MMARMS PASS (rides);
#   6. SYSINFO recorded (PASS or INFO errno=N - G7 owns that verdict);
#   7. REGPASS ALL: regpreserve >= 20 REGPASS, ZERO REGRED/CRASH (the NULLFIX
#      fix rides - its canary arm must stay green);
#   8. LAZYREL PASS anon-release-kills AND file-release-kills, ZERO LAZYREL
#      RED/CRASH - the fix's java-free proof: a released range must stay dead;
#   9. THE JAVA ARM MOVED (D37's honesty rule): the console window carries NO
#      fault at the old death rip 60000067cd7a. Progress past it is a different
#      rip (its attribution + trace-err ring are recorded as the next gate's
#      data), version output recovered from /tmp/java-out.log, or no kill at
#      all. A kill at the old rip = the fix didn't take = RESULT FAIL, honestly.
#   RESULT PASS = 1-6 AND 7 AND 8 AND 9.
#
# Machine notes carried: ONE VM at a time (free the slot; stale = EBUSY); wait
# for an ssh round trip, never a log marker; always stop the guest; grep
# console-derived text -a-tolerantly; invoke as `sh <file>` (not #! runnable).
# The java arm stalls sshd (session 8's data): its 180 s timeout and the
# console window are the designed path, not a failure of the probe. The arm's
# output is redirected to /tmp/java-out.log guest-side; if sshd ever answers
# after the arm, the file is recovered best-effort - never a pass condition.

W=${DENVER_W:-/root/denver/w}
SPEC=${1:-$W/vm-heapro.spec}
PORT=${DENVER_PORT:-2247}
WAIT=${DENVER_BOOT_WAIT_TRIES:-30}     # x 5 s
SSH="ssh -y -i $W/key -p $PORT root@127.0.0.1"
VMLOG=$W/vmrun.log
NULLFIXHASH="72be5c55"                 # the proven NULLFIX binary - must NOT boot (no sweep in it)
NULLSTOREHASH="800e8c80"               # the proven NULLSTORE binary - must NOT boot
PROCSHASH="f0dcacb6"                   # the proven PROCFS binary - must NOT boot
MERGEDHASH="3531cf69"                  # the merged-tree/vdso binary - must NOT boot
[ -n "$NULLFIXHASH" ] && [ -n "$NULLSTOREHASH" ] && [ -n "$PROCSHASH" ] && [ -n "$MERGEDHASH" ] ||
    { echo "RESULT FAIL: rig identity constant empty (NULLFIX/NULLSTORE/PROCS/MERGED hash) - the probe would match any uname" >&2; exit 1; }
RIP=60000067cd7a                       # the HEAPRO death rip - the death this gate must NOT reproduce
VSYSCALL=ffffffffff600800              # the closed vsyscall death - must NOT recur
T0=$(date +%s)

VP=
step() {
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

[ -r "$SPEC" ] || verdict "RESULT FAIL: no spec at $SPEC (stage denver-heapro + vm-heapro.spec first, denver/docs/research/PHASE-2-HEAPRO.md D46)" 1
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

step "rig identity: uname says denver, NOT $NULLFIXHASH, NOT $NULLSTOREHASH, NOT $PROCSHASH, NOT $MERGEDHASH"
echo "$out" | grep Linux
case "$out" in
    *denver*) ;;
    *) verdict "RESULT FAIL: uname does not say denver" 1 ;;
esac
for h in "$NULLFIXHASH" "$NULLSTOREHASH" "$PROCSHASH" "$MERGEDHASH"; do
    case "$out" in
        *"$h"*) verdict "RESULT FAIL: a stale rig binary booted (hash $h) - staging failed" 1 ;;
    esac
done

step "platform sanity: tokioprobe"
tok=$(timeout 90 $SSH "/bin/tokioprobe" 2>&1)
echo "$tok"
case "$tok" in *"RESULT PASS"*) ;; *) verdict "RESULT FAIL: tokioprobe did not pass" 1 ;; esac

step "wx-relaxed proof: the rwx arm (rides; this binary carries wx-relaxed AND vsyscall AND procfs-diag AND trace-err AND fold-regpreserve AND lazy-release-sweep)"
rwx=$(timeout 60 $SSH "/bin/jvmprobe rwx" 2>&1)
echo "$rwx"
case "$rwx" in *"RWX PASS rw_to_rx=42"*) ;; *) verdict "RESULT FAIL: rwx arm did not pass" 1 ;; esac

step "procfs-diag alive: the maps arm"
maps=$(timeout 60 $SSH "/bin/jvmprobe maps" 2>&1)
echo "$maps"
case "$maps" in
    *"MAPS PASS lines="*) ;;
    *) verdict "RESULT FAIL: the maps arm did not print MAPS PASS" 1 ;;
esac

step "procfs-diag alive: the auxv arm"
aux=$(timeout 60 $SSH "/bin/jvmprobe auxv" 2>&1)
echo "$aux"
case "$aux" in
    *"AUXV PASS procfs-auxv"*) ;;
    *) verdict "RESULT FAIL: the auxv arm did not print AUXV PASS procfs-auxv" 1 ;;
esac

step "the contract facts, java-free (D35): mmaparms"
mma=$(timeout 90 $SSH "/bin/jvmprobe mmaparms" 2>&1)
echo "$mma"
case "$mma" in
    *"MMARMS PASS len0"*"MMARMS PASS oversize"*) ;;
    *) verdict "RESULT FAIL: the mmap failure conventions did not hold" 1 ;;
esac
case "$mma" in
    *"MMARMS PASS commit-RW"*"MMARMS PASS commit-RX call=42"*) ;;
    *) verdict "RESULT FAIL: the code-cache commit shape did not hold" 1 ;;
esac
redline=$(printf '%s\n' "$mma" | grep 'MMARMS RED\|CRASH' | grep -v 'rwx-mapped-perm')
[ -n "$redline" ] && { echo "$redline"; verdict "RESULT FAIL: mmaparms printed a RED/CRASH line outside the recorded rwx-policy divergence" 1; }

step "sysinfo recorded (G7's feed; this gate does not judge it)"
sys=$(timeout 60 $SSH "/bin/jvmprobe sysinfo" 2>&1)
echo "$sys"
case "$sys" in
    *"SYSINFO PASS"*|*"SYSINFO INFO"*) echo "  (recorded for the G7 pass)" ;;
    *) verdict "RESULT FAIL: the sysinfo arm printed neither PASS nor INFO" 1 ;;
esac

step "the NULLFIX fix rides: regpreserve - every canary kept (D47 condition 7)"
reg=$(timeout 120 $SSH "/bin/jvmprobe regpreserve" 2>&1)
echo "$reg"
redcount=$(printf '%s\n' "$reg" | grep -c 'REGRED')
regpass=$(printf '%s\n' "$reg" | grep -c 'REGPASS')
if [ "$redcount" -ne 0 ]; then
    verdict "RESULT FAIL: regpreserve printed $redcount REGRED lines - the fold rewrite came back" 1
fi
if [ "$regpass" -lt 20 ]; then
    verdict "RESULT FAIL: regpreserve printed only $regpass REGPASS lines (< 20) - the arm did not complete" 1
fi

step "THE FIX, java-free: lazyrel - a released range must stay dead (D47 condition 8)"
laz=$(timeout 120 $SSH "/bin/jvmprobe lazyrel" 2>&1)
echo "$laz"
lred=$(printf '%s\n' "$laz" | grep -c 'LAZYREL RED\|LAZYREL CRASH')
if [ "$lred" -ne 0 ]; then
    verdict "RESULT FAIL: lazyrel printed $lred RED/CRASH lines - the released range came back (lazy records not swept)" 1
fi
printf '%s\n' "$laz" | grep 'LAZYREL PASS anon-release-kills' >/dev/null ||
    verdict "RESULT FAIL: no LAZYREL PASS anon-release-kills - the anon arm did not complete" 1
printf '%s\n' "$laz" | grep 'LAZYREL PASS file-release-kills' >/dev/null ||
    verdict "RESULT FAIL: no LAZYREL PASS file-release-kills - the file arm did not complete" 1

step "the payoff: the java arm - the death must MOVE (D37/D41 honesty); sshd may starve (session 8's data)"
OFF=$(wc -c < "$VMLOG")                    # console bytes before the arm
timeout 180 $SSH "/bin/jvmprobe name /opt/jdk/bin/java $RIP -version > /tmp/java-out.log 2>&1" 2>&1 | head -3
echo "  (arm dispatched; java's output goes to /tmp/java-out.log guest-side)"

step "the console window: the old death must be absent"
sleep 10
win=$(tail -c +$((OFF + 1)) "$VMLOG" 2>/dev/null | grep -a .)
faultline=$(printf '%s\n' "$win" | grep 'ring3 PageFault rip=' | tail -1)
killline=$(printf '%s\n' "$win" | grep -F 'killed (SIGSEGV)' | tail -1)
echo "-- console window: fault line:"
echo "${faultline:-<none>}"
echo "-- console window: kill line:"
echo "${killline:-<none>}"

printf '%s\n' "$win" | grep '\[ring\]\|\[hang\]\|\[fault-code\]\|\[mm\]\|\[Fault\]\|ring3 map \|ring3 regs \|ring3 stack ' > /tmp/heapro-window.txt 2>/dev/null
echo "-- console window: evidence lines:"
head -40 /tmp/heapro-window.txt 2>/dev/null

case "$faultline" in
    *"$VSYSCALL"*)
        verdict "RESULT FAIL: the workload fetched the vsyscall page again - the map did not take" 1 ;;
esac

case "$faultline" in
    *"rip=0x0000$RIP"*)
        verdict "RESULT FAIL: the java arm died at the OLD rip 0x$RIP - the release sweep did not take on the java path" 1 ;;
esac

case "$faultline" in
    *"ring3 PageFault"*)
        echo "MOVED: the death moved - the old HEAPRO rip never appeared"
        echo "-- a fault at a DIFFERENT rip is progress (D41): its attribution/ring above are the next gate's data" ;;
    "")
        echo "MOVED: no fault at all in the window - the old death is gone" ;;
    *)
        echo "MOVED: window state recorded above" ;;
esac

step "best-effort: recover java's output (never a pass condition)"
got=$(timeout 15 $SSH "cat /tmp/java-out.log 2>/dev/null; echo RC=\$?" 2>&1)
echo "$got" | head -10
case "$got" in
    *"version "*|*"OpenJDK"*) echo "JAVA COMPLETE: java -version produced output - the JVM survived the released-range death" ;;
    *) echo "  (sshd did not answer or the file has no version line - the console window above is the verdict's evidence)" ;;
esac

step "verdict"
echo "MOVEMENT: the HEAPRO present-RO write is FIXED behind lazy-release-sweep - lazyrel all-PASS (released ranges stay dead), regpreserve rides, and the java arm never returned to the old death"
verdict "RESULT PASS" 0
