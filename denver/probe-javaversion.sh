#!/bin/sh
# denver/probe-javaversion.sh - the PHASE-3-JAVAVERSION gate probe (the capture
# gate): make `java -version`'s fate a READABLE datum. probe-getcpu.sh's shape
# (stamps + free-the-slot first + stop at the end), but NO kernel code moves:
# the gate runs the proven GREEN getcpu rig (d4acb339) unchanged, and the
# capture ladder replaces the 2 ms-poller name arm (D59: the starvation datum
# was confounded with the probe's own poller).
#
#   sh denver/probe-javaversion.sh 2>&1 | tee denver/gates/PHASE-3-JAVAVERSION.log
#
# Design = denver/docs/research/PHASE-3-JAVAVERSION.md (D59-D61):
#   B0 channel baseline, B1 concurrent-session baseline (kills H2), then the
#   ARM: java FOREGROUND on its own session, host-side backgrounded, 300 s
#   window (10x the old 30 s), output guest-side + JAVARC=$? exit marker,
#   pokes every 45 s DURING the run (the poller-less starvation datum), two
#   recoveries after, then the console window (old rip absent, census of
#   [nosys]/[err] nr= lines - the next gate's data), and the /dev/console
#   probe (the D61 datum).
#
# Pass conditions (D60 - PASS means the fate is readable, not that java works):
#   1. uname says denver AND carries d4acb339 AND NOT any older rig hash
#      (the capture gate REQUIRES the proven binary; a different hash booted
#      = staging failed);
#   2. tokioprobe RESULT PASS (platform sanity);
#   3. GETCPU PASS cpu<4 node=0 (one PASS proves the whole feature set rides:
#      getcpu sysinfo wx-relaxed vsyscall procfs-diag fold lazyrel);
#   4. THE DATUM, one of:
#      READABLE-EXIT  - the arm returned with JAVARC + version output;
#      READABLE-DEATH - a console fault line names a rip (old rip also flags);
#      READABLE-HANG  - no fault, no exit, channel proven alive (B0/B1 + >=1
#                       poke ANSWERED), census + window dumped;
#   RESULT PASS = 1 AND 2 AND 3 AND one readable datum from 4.
#   FAIL when the capture rig itself could not observe (case 4 of D60): every
#   poke unanswered WITH B1 proving concurrency works -> the next pass is the
#   console-dev capture (D61), this gate re-runs under it.
#
# Machine notes carried: ONE VM at a time (free the slot); wait for an ssh
# round trip, never a log marker; always stop the guest; grep console-derived
# text -a-tolerantly; invoke as `sh <file>`. No maps poller anywhere in this
# probe (D59) - jvmprobe name is NOT dispatched.

W=${DENVER_W:-/root/denver/w}
SPEC=${1:-$W/vm-getcpu.spec}
PORT=${DENVER_PORT:-2247}
WAIT=${DENVER_BOOT_WAIT_TRIES:-30}     # x 5 s
SSH="ssh -y -i $W/key -p $PORT root@127.0.0.1"
VMLOG=$W/vmrun.log
GREENHASH="d4acb339"                   # the proven getcpu GREEN binary - REQUIRED
NULLFIXHASH="72be5c55"                 # older rigs - must NOT boot
NULLSTOREHASH="800e8c80"
PROCSHASH="f0dcacb6"
MERGEDHASH="3531cf69"
HEAPROHASH="c921d625"
MEMINFOHASH="24398b99"
[ -n "$GREENHASH" ] || { echo "RESULT FAIL: rig identity constant empty" >&2; exit 1; }
RIP=60000067cd7a                       # the old death rip - must stay absent
VSYSCALL=ffffffffff600800              # the closed vsyscall death - must NOT recur
JAVA_WINDOW=${DENVER_JAVA_WINDOW:-300} # the LONGER window (was 30 s)
POKE_EVERY=${DENVER_POKE_EVERY:-45}
POKE_MAX=${DENVER_POKE_MAX:-7}
T0=$(date +%s)

VP=
JP=
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
    [ -n "$JP" ] && kill "$JP" 2>/dev/null
    VP=; JP=
}

[ -r "$SPEC" ] || verdict "RESULT FAIL: no spec at $SPEC (the capture gate reuses the getcpu rig, research D59)" 1
[ -r "$W/key" ] || verdict "RESULT FAIL: no key at $W/key" 1
[ -r "$VMLOG" ] || verdict "RESULT FAIL: no console log at $VMLOG" 1

step "free the VM slot (a stale guest answers EBUSY)"
echo stop > /dev/kvm0 2>/dev/null
sleep 3

step "boot: herd vm-run $SPEC (the proven getcpu GREEN rig, unchanged)"
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

step "rig identity: uname carries the GREEN binary $GREENHASH, no older rig"
echo "$out" | grep Linux
case "$out" in
    *denver*) ;;
    *) verdict "RESULT FAIL: uname does not say denver" 1 ;;
esac
case "$out" in
    *"$GREENHASH"*) ;;
    *) verdict "RESULT FAIL: the booted binary is NOT the proven $GREENHASH - the capture gate requires it unchanged" 1 ;;
esac
for h in "$NULLFIXHASH" "$NULLSTOREHASH" "$PROCSHASH" "$MERGEDHASH" "$HEAPROHASH" "$MEMINFOHASH"; do
    case "$out" in
        *"$h"*) verdict "RESULT FAIL: a stale rig binary booted (hash $h) - staging failed" 1 ;;
    esac
done

step "platform sanity: tokioprobe"
tok=$(timeout 90 $SSH "/bin/tokioprobe" 2>&1)
echo "$tok"
case "$tok" in *"RESULT PASS"*) ;; *) verdict "RESULT FAIL: tokioprobe did not pass" 1 ;; esac

step "feature-set proof: the getcpu arm (one PASS proves getcpu/sysinfo/wx/vsyscall/procfs-diag/fold/lazyrel all ride)"
get=$(timeout 60 $SSH "/bin/jvmprobe getcpu" 2>&1)
echo "$get"
case "$get" in
    *"GETCPU PASS"*) ;;
    *) verdict "RESULT FAIL: the getcpu arm did not pass - the proven feature set is not riding" 1 ;;
esac
cpu=$(printf '%s\n' "$get" | grep 'GETCPU PASS' | sed -E 's/.*cpu=([0-9]+).*/\1/')
case "$cpu" in ''|*[!0-9]*) verdict "RESULT FAIL: could not parse cpu from the GETCPU PASS line" 1 ;; esac
[ "$cpu" -lt 4 ] || verdict "RESULT FAIL: cpu=$cpu out of range (spec vcpus=4)" 1

step "B0: channel baseline (poke before any dispatch)"
b0=$(timeout 15 $SSH "echo ALIVE-B0" 2>&1)
echo "$b0"
case "$b0" in *ALIVE-B0*) CHANNEL_OK=1 ;; *) CHANNEL_OK= ;; esac

step "B1: concurrent-session baseline (a background sleep while a second connection pokes - kills H2)"
sl=$(timeout 15 $SSH "command -v sleep" 2>&1 | head -1)
echo "  sleep binary: ${sl:-<none>}"
B1=
case "$sl" in */*) 
    timeout 70 $SSH "$sl 45" > /dev/null 2>&1 &
    BP=$!
    sleep 8
    b1=$(timeout 15 $SSH "echo ALIVE-B1" 2>&1)
    echo "$b1"
    case "$b1" in *ALIVE-B1*) B1=1 ;; *) B1= ;; esac
    kill "$BP" 2>/dev/null; wait "$BP" 2>/dev/null
    ;;
esac
[ -n "$B1" ] && echo "  B1 PASS: concurrent sessions work - H2 (bridge serialization) dead"
[ -z "$B1" ] && [ -n "$sl" ] && echo "  B1 RED: a second connection went unanswered during a benign sleep"
[ -z "$sl" ] && echo "  B1 SKIPPED: no sleep in the image (datum recorded)"

step "THE ARM: java -version foreground, NO maps poller, window ${JAVA_WINDOW}s, pokes every ${POKE_EVERY}s (D59/D60)"
timeout 15 $SSH "rm -f /tmp/java-out.log" > /dev/null 2>&1
OFF=$(wc -c < "$VMLOG")                # console bytes before the dispatch
timeout "$JAVA_WINDOW" $SSH "/opt/jdk/bin/java -version > /tmp/java-out.log 2>&1; echo JAVARC=\$? >> /tmp/java-out.log" > /tmp/java-ssh.log 2>&1 &
JP=$!

ANSWERED=0
n=0
while kill -0 "$JP" 2>/dev/null; do
    sleep "$POKE_EVERY"
    n=$((n + 1))
    [ "$n" -gt "$POKE_MAX" ] && break
    poke=$(timeout 12 $SSH "echo ALIVE-POKE$n; cat /tmp/java-out.log 2>/dev/null | head -5" 2>&1)
    echo "-- poke $n: $poke"
    case "$poke" in *ALIVE-POKE*) ANSWERED=$((ANSWERED + 1)) ;; esac
done
wait "$JP" 2>/dev/null
JP=

step "the arm's own channel return (foreground session; JAVARC present = java exited)"
cat /tmp/java-ssh.log 2>/dev/null | head -5
final=$(timeout 15 $SSH "cat /tmp/java-out.log 2>/dev/null; echo ENDRC=\$?" 2>&1)
echo "-- final recovery:"
echo "$final"
case "$final" in *"JAVARC="*) EXITED=1 ;; *) EXITED= ;; esac
case "$final" in *"version "*|*"OpenJDK"*) GOTOUT=1 ;; *) GOTOUT= ;; esac

step "recovery tries 2-3 (post-arm, +${POKE_EVERY}s apart)"
if [ -z "$EXITED" ]; then
    sleep "$POKE_EVERY"
    r2=$(timeout 15 $SSH "cat /tmp/java-out.log 2>/dev/null; echo ENDRC=\$?" 2>&1)
    echo "-- recovery 2: $r2"
    case "$r2" in *"JAVARC="*) EXITED=1 ;; esac
    case "$r2" in *"version "*|*"OpenJDK"*) GOTOUT=1 ;; esac
fi
if [ -z "$EXITED" ]; then
    sleep "$POKE_EVERY"
    r3=$(timeout 15 $SSH "cat /tmp/java-out.log 2>/dev/null; echo ENDRC=\$?" 2>&1)
    echo "-- recovery 3: $r3"
    case "$r3" in *"JAVARC="*) EXITED=1 ;; esac
    case "$r3" in *"version "*|*"OpenJDK"*) GOTOUT=1 ;; esac
fi

step "console window: old death absent + census (the next gate's data)"
win=$(tail -c +$((OFF + 1)) "$VMLOG" 2>/dev/null | grep -a .)
faultline=$(printf '%s\n' "$win" | grep 'ring3 PageFault rip=' | tail -1)
killline=$(printf '%s\n' "$win" | grep -F 'killed (SIGSEGV)' | tail -1)
echo "-- console window: fault line:"
echo "${faultline:-<none>}"
echo "-- console window: kill line:"
echo "${killline:-<none>}"
printf '%s\n' "$win" | grep '\[ring\]\|\[hang\]\|\[fault-code\]\|\[mm\]\|\[nosys\]\|\[err\]\|\[Fault\]\|ring3 map \|ring3 regs \|ring3 stack ' > /tmp/javaver-window.txt 2>/dev/null
echo "-- console window: evidence lines:"
head -25 /tmp/javaver-window.txt 2>/dev/null
echo "-- console window: syscall census (trace-err buckets, top counts):"
printf '%s\n' "$win" | grep -ao 'nr=[0-9]*' | sort | uniq -c | sort -rn | head -15

case "$faultline" in
    *"rip=0x0000$RIP"*)
        echo "DATUM: the OLD rip appeared - the same binary cannot regress; staging is the suspect" ;;
esac
case "$faultline" in
    *"$VSYSCALL"*)
        echo "DATUM: the vsyscall death reappeared - staging suspect" ;;
esac

step "the D61 datum: does a user->console path exist?"
con=$(timeout 15 $SSH "echo PROBE > /dev/console 2>&1; echo CONSOLEWRC=\$?" 2>&1)
echo "$con"
case "$con" in
    *CONSOLEWRC=0*) echo "  /dev/console WRITABLE - the console-side capture needs no kernel work" ;;
    *) echo "  /dev/console not writable (expected; D61 records the console-dev shape if case 4 lands)" ;;
esac

step "verdict (D60)"
E=$(( $(date +%s) - T0 )); [ "$E" -lt 0 ] && E=0
[ -z "$CHANNEL_OK" ] && verdict "RESULT FAIL: even the B0 baseline poke went unanswered - the capture rig itself is broken" 1
if [ -n "$EXITED" ] && [ -n "$GOTOUT" ]; then
    echo "READABLE-EXIT: java -version exits and its output is recovered - the phase-3 ladder's first rung"
    verdict "RESULT PASS" 0
fi
if [ -n "$faultline" ]; then
    echo "READABLE-DEATH: the console names a rip (above) - the death is the datum; procfs attribution rides"
    verdict "RESULT PASS" 0
fi
if [ -n "$EXITED" ]; then
    echo "READABLE-EXIT(partial): java exited (JAVARC) but the version line never surfaced - output buffering is the suspect; the exit code is the datum"
    verdict "RESULT PASS" 0
fi
if [ "$ANSWERED" -gt 0 ]; then
    echo "READABLE-HANG: no fault, no exit in ${JAVA_WINDOW}s, channel proven alive (B0 + $ANSWERED pokes answered) - java runs indefinitely without dying; census above is the next gate's datum"
    verdict "RESULT PASS" 0
fi
# ANSWERED == 0: no poke ever answered during or after the run - attribute before concluding
if [ -n "$B1" ]; then
    verdict "RESULT FAIL: every poke went unanswered while java ran and B1 proved concurrent sessions work - the workload starves the channel (H3); verdict case 4: the next pass is the console-dev capture (D61), this gate re-runs under it" 1
fi
if [ -z "$sl" ]; then
    verdict "RESULT FAIL: no poke answered and B1 was skipped (no sleep in the image) - serialization vs starvation unresolved; fix B1 and re-run" 1
fi
verdict "RESULT FAIL: no poke answered and B1 RED - the bridge itself does not serve concurrent sessions (H2); the pokes prove nothing about java; fix the bridge baseline and re-run" 1
