#!/bin/sh
# denver/probe-getcpu.sh - the PHASE-2-GETCPU gate probe (probe-meminfo.sh's
# shape: stamps + free-the-slot first + stop at the end; NEW rig: the
# denver-getcpu kernel binary + vm-getcpu.spec; the jdk image already carries
# the jvmprobe getcpu arm - no re-injection).
#
#   sh denver/probe-getcpu.sh 2>&1 | tee denver/gates/PHASE-2-GETCPU.log
#
# The GETCPU pass (denver/docs/research/PHASE-2-GETCPU.md, D55/D56): G2's
# getcpu(2) - musl's sched_getcpu and any direct caller, raw nr 168 - was
# served nowhere (`GETCPU RED ENOSYS` since PHASE-1; the vsyscall stub's -38
# only softened HotSpot's own path, which reads a pre-zeroed cpu). The fix is
# one thin arm (the caller's core from ring3::cpu_index(), node 0, copies out
# in glue, EFAULT on failure) behind the off-by-default `getcpu` feature; the
# decision is pure, host-tested (akuma-syscalls-linux::sched).
#
# Pass conditions (D56, in order):
#   1. uname says denver AND NOT any prior rig hash (nullfix 72be5c55,
#      nullstore 800e8c80, procfs f0dcacb6, vdso/merged 3531cf69,
#      heapro c921d625, meminfo 24398b99 - a stale binary booting = staging
#      failed; empty-constant abort);
#   2. tokioprobe RESULT PASS (platform sanity);
#   3. RWX PASS rw_to_rx=42 (rides);
#   4. MAPS PASS + AUXV PASS procfs-auxv (rides);
#   5. MMARMS PASS (rides);
#   6. THE GATE: GETCPU PASS with cpu in 0..3 (the spec's vcpus=4 - the cpu
#      is the CALLER's core, never a constant, the BUG_FIX_LIST lesson) and
#      node=0 (the pure crate's decision). GETCPU RED ENOSYS (the arm did
#      not take) or any other RED = RESULT FAIL;
#   7. REGPASS ALL: regpreserve >= 20 REGPASS, ZERO REGRED/CRASH (rides);
#   8. LAZYREL PASS anon-release-kills AND file-release-kills, ZERO
#      RED/CRASH (rides);
#   9. THE JAVA ARM MOVED (D37's honesty rule, riding datum - not judged
#      further): the console window carries NO fault at the old rip
#      60000067cd7a. sysinfo's feed rides on this binary, so a recovered
#      `Too small maximum heap` is still the next gate's datum; a NEW fault
#      rip is progress, recorded. The [nosys] census line for nr=168 should
#      be ABSENT now (musl's raw path is the thing this gate changed).
#   RESULT PASS = 1-6 AND 7 AND 8 AND 9-not-failed.
#
# Machine notes carried: ONE VM at a time (free the slot; stale = EBUSY); wait
# for an ssh round trip, never a log marker; always stop the guest; grep
# console-derived text -a-tolerantly; invoke as `sh <file>` (not #! runnable).
# The java arm stalls sshd (session 8's data): its 180 s timeout and the
# console window are the designed path, not a failure of the probe.

W=${DENVER_W:-/root/denver/w}
SPEC=${1:-$W/vm-getcpu.spec}
PORT=${DENVER_PORT:-2247}
WAIT=${DENVER_BOOT_WAIT_TRIES:-30}     # x 5 s
SSH="ssh -y -i $W/key -p $PORT root@127.0.0.1"
VMLOG=$W/vmrun.log
NULLFIXHASH="72be5c55"                 # the proven NULLFIX binary - must NOT boot
NULLSTOREHASH="800e8c80"               # the proven NULLSTORE binary - must NOT boot
PROCSHASH="f0dcacb6"                   # the proven PROCFS binary - must NOT boot
MERGEDHASH="3531cf69"                  # the merged-tree/vdso binary - must NOT boot
HEAPROHASH="c921d625"                  # the proven HEAPRO binary - must NOT boot
MEMINFOHASH="24398b99"                 # the proven MEMINFO binary - must NOT boot (no getcpu arm in it)
GETCPU1HASH="50d64942"                 # the FAILED getcpu attempt-1 binary (abi-row hijack) - must NOT boot
[ -n "$NULLFIXHASH" ] && [ -n "$NULLSTOREHASH" ] && [ -n "$PROCSHASH" ] &&
[ -n "$MERGEDHASH" ] && [ -n "$HEAPROHASH" ] && [ -n "$MEMINFOHASH" ] && [ -n "$GETCPU1HASH" ] ||
    { echo "RESULT FAIL: rig identity constant empty - the probe would match any uname" >&2; exit 1; }
RIP=60000067cd7a                       # the old death rip - the death this gate must NOT reproduce
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

[ -r "$SPEC" ] || verdict "RESULT FAIL: no spec at $SPEC (stage denver-getcpu + vm-getcpu.spec first, denver/docs/research/PHASE-2-GETCPU.md D55)" 1
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

step "rig identity: uname says denver, NOT $NULLFIXHASH/$NULLSTOREHASH/$PROCSHASH/$MERGEDHASH/$HEAPROHASH/$MEMINFOHASH/$GETCPU1HASH"
echo "$out" | grep Linux
case "$out" in
    *denver*) ;;
    *) verdict "RESULT FAIL: uname does not say denver" 1 ;;
esac
for h in "$NULLFIXHASH" "$NULLSTOREHASH" "$PROCSHASH" "$MERGEDHASH" "$HEAPROHASH" "$MEMINFOHASH" "$GETCPU1HASH"; do
    case "$out" in
        *"$h"*) verdict "RESULT FAIL: a stale rig binary booted (hash $h) - staging failed" 1 ;;
    esac
done

step "platform sanity: tokioprobe"
tok=$(timeout 90 $SSH "/bin/tokioprobe" 2>&1)
echo "$tok"
case "$tok" in *"RESULT PASS"*) ;; *) verdict "RESULT FAIL: tokioprobe did not pass" 1 ;; esac

step "wx-relaxed proof: the rwx arm (rides; this binary carries wx-relaxed AND vsyscall AND procfs-diag AND trace-err AND fold-regpreserve AND lazy-release-sweep AND sysinfo AND getcpu)"
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
case "$mma" in
    *"MMARMS PASS rwx call=42"*) ;;
    *) verdict "RESULT FAIL: no MMARMS PASS rwx call=42 - the W+X mmap was not writable" 1 ;;
esac
redline=$(printf '%s\n' "$mma" | grep 'MMARMS RED\|CRASH' | grep -v 'rwx-mapped-perm')
[ -n "$redline" ] && { echo "$redline"; verdict "RESULT FAIL: mmaparms printed a RED/CRASH line outside the recorded rwx-policy divergence" 1; }

step "THE GATE: getcpu served with the caller's core (D56 condition 6 - was GETCPU RED ENOSYS)"
get=$(timeout 60 $SSH "/bin/jvmprobe getcpu" 2>&1)
echo "$get"
case "$get" in
    *"GETCPU PASS"*) ;;
    *"GETCPU RED ENOSYS"*) verdict "RESULT FAIL: getcpu still not served (GETCPU RED ENOSYS) - the getcpu kernel did not take" 1 ;;
    *) verdict "RESULT FAIL: the getcpu arm printed neither PASS nor RED ENOSYS" 1 ;;
esac
case "$get" in
    *"GETCPU RED"*) verdict "RESULT FAIL: GETCPU RED (an errno other than ENOSYS - the arm's copies failed)" 1 ;;
esac
# D42's rule, probe half: extract from the grep'd PASS line, never the raw capture.
pair=$(printf '%s\n' "$get" | grep 'GETCPU PASS' | sed -E 's/.*cpu=([0-9]+) node=([0-9]+)/\1 \2/')
cpu=${pair%% *}; node=${pair##* }
[ -n "$cpu" ] && [ -n "$node" ] 2>/dev/null ||
    verdict "RESULT FAIL: could not parse cpu/node from the GETCPU PASS line" 1
[ "$cpu" -lt 4 ] 2>/dev/null ||
    verdict "RESULT FAIL: cpu=$cpu out of range (spec vcpus=4) - the caller's core is not what the arm served" 1
[ "$node" -eq 0 ] 2>/dev/null ||
    verdict "RESULT FAIL: node=$node - the decision is node 0 (single-NUMA guest)" 1
echo "  (G2's gap SERVED: GETCPU PASS cpu=$cpu node=$node - the gate's named line)"

step "the NULLFIX fix rides: regpreserve - every canary kept (D56 condition 7)"
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

step "the HEAPRO fix rides: lazyrel - a released range must stay dead (D56 condition 8)"
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

step "the payoff datum: the java arm (riding, D37/D41 honesty; window 30 s, double recovery)"
OFF=$(wc -c < "$VMLOG")                    # console bytes before the arm
timeout 180 $SSH "/bin/jvmprobe name /opt/jdk/bin/java $RIP -version > /tmp/java-out.log 2>&1" 2>&1 | head -3
echo "  (arm dispatched; java's output goes to /tmp/java-out.log guest-side)"

step "the console window: the old death must be absent"
sleep 30
win=$(tail -c +$((OFF + 1)) "$VMLOG" 2>/dev/null | grep -a .)
faultline=$(printf '%s\n' "$win" | grep 'ring3 PageFault rip=' | tail -1)
killline=$(printf '%s\n' "$win" | grep -F 'killed (SIGSEGV)' | tail -1)
echo "-- console window: fault line:"
echo "${faultline:-<none>}"
echo "-- console window: kill line:"
echo "${killline:-<none>}"

printf '%s\n' "$win" | grep '\[ring\]\|\[hang\]\|\[fault-code\]\|\[mm\]\|\[nosys\]\|\[err\]\|\[Fault\]\|ring3 map \|ring3 regs \|ring3 stack ' > /tmp/getcpu-window.txt 2>/dev/null
echo "-- console window: evidence lines (incl. the census [nosys]/[err] - the next gates' data):"
head -40 /tmp/getcpu-window.txt 2>/dev/null
if printf '%s\n' "$win" | grep -q '\[nosys\] nr=168'; then
    echo "CENSUS DATUM: [nosys] nr=168 still in the java window - a caller hit the raw number unserved"
else
    echo "CENSUS DATUM: no [nosys] nr=168 in the window - the raw getcpu path is served (was: present in VDSO's census)"
fi

case "$faultline" in
    *"$VSYSCALL"*)
        verdict "RESULT FAIL: the workload fetched the vsyscall page again - the map did not take" 1 ;;
esac

case "$faultline" in
    *"rip=0x0000$RIP"*)
        verdict "RESULT FAIL: the java arm died at the OLD rip 0x$RIP - an earlier fix regressed" 1 ;;
esac

case "$faultline" in
    *"ring3 PageFault"*)
        echo "MOVED: the death moved - the old rip never appeared"
        echo "-- a fault at a DIFFERENT rip is progress (D41): its attribution/ring above are the next gate's data" ;;
    "")
        echo "MOVED: no fault at all in the window - the old death is gone" ;;
    *)
        echo "MOVED: window state recorded above" ;;
esac

RECOVERED=
step "best-effort: recover java's output, attempt 1 (+30 s window; never a pass condition)"
got=$(timeout 20 $SSH "cat /tmp/java-out.log 2>/dev/null; echo RC=\$?" 2>&1)
echo "$got" | head -10
case "$got" in
    *"version "*|*"OpenJDK"*) RECOVERED=1 ;;
esac
if [ -z "$RECOVERED" ]; then
    step "best-effort: recover java's output, attempt 2 (another 30 s)"
    sleep 30
    got=$(timeout 20 $SSH "cat /tmp/java-out.log 2>/dev/null; echo RC=\$?" 2>&1)
    echo "$got" | head -10
    case "$got" in
        *"version "*|*"OpenJDK"*) RECOVERED=1 ;;
    esac
fi
case "$got" in
    *"Too small maximum heap"*)
        echo "HEAP-SIZING DATUM: 'Too small maximum heap' with SYSINFO riding - a sizing consumer reads something OTHER than sysinfo (the next gate's datum)" ;;
esac
if [ -n "$RECOVERED" ]; then
    echo "JAVA COMPLETE: java -version produced output - the phase-3 ladder's first rung"
else
    echo "  (sshd did not answer or the file has no version line - the console window above is the verdict's evidence)"
fi

step "verdict"
echo "MOVEMENT: getcpu(2) is served with the caller's core behind the getcpu feature - GETCPU PASS cpu=$cpu node=$node (was GETCPU RED ENOSYS since PHASE-1), every riding arm green, and the java arm never returned to the old death"
verdict "RESULT PASS" 0
