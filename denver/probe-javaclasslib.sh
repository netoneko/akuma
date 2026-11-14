#!/bin/sh
# denver/probe-javaclasslib.sh - the PHASE-3-JAVACLASSLIB gate probe (G12): a compiled
# workload that reaches FURTHER into the runtime than Hello.class did - exceptions
# (throw/catch/unwind through a real exception_table), user threads (Thread start/join
# over clone + futex), java.io file I/O (FileOutputStream/FileInputStream over
# openat/read/write/close), and G1 allocation pressure (young-gen churn + humongous
# 16 MiB churn with heap expansion, sized by -Xms64m -Xmx1536m). probe-javaworkload.sh's
# shape (stamps + free-the-slot first + stop at the end), and NO kernel code moves: the
# gate runs the proven GREEN kernel d4acb339 unchanged; only the disk differs (a COPY
# of denver-jdk.img with /work/Workload.class + /work/Worker.class added by debugfs,
# D65's discipline).
#
#   sh denver/probe-javaclasslib.sh 2>&1 | tee denver/gates/PHASE-3-JAVACLASSLIB.log
#
# Design = denver/docs/research/PHASE-3-JAVACLASSLIB.md (D69-D71):
#   D69 the classes are hand-assembled by denver/probes/mkworkload.py (general CP
#   allocator, no javac exists anywhere in the lineage - D64 stands) as major-49 class
#   files (no StackMapTable needed below major 50; named risk + flip path in D69).
#   Verified pre-boot by THREE independent tools (D67's lesson generalized):
#   verifyworkload.py (structure + pool closure + decode + branch boundaries +
#   depth dataflow + intent), dumpworkload.py (raw disassembly), and determinism
#   (build twice, cmp). The receiver is still the guest's HotSpot.
#   D70 delivery = debugfs write into an image COPY (denver-classlib.img), fsck-checked;
#   the proven images are never modified in place.
#   D71 the reader is the pipe arm ONLY (L2 rule from D63/D66: no maps poller anywhere;
#   alarm bounds mean the exit report ALWAYS returns). The pipe captures stdout AND
#   stderr, so an escaping exception's stack trace is a readable datum.
#
# Pass conditions (G12: READABLE-RUN means the workload ran its four surfaces):
#   1. uname says denver AND carries d4acb339 AND NOT any older rig hash;
#   2. image staging: denver-classlib.img exists, /work/{Workload,Worker}.class in it
#      (debugfs), and the guest can name them (/bin/busybox ls -l /work);
#   3. tokioprobe RESULT PASS (platform sanity);
#   4. GETCPU PASS cpu<4 (one PASS proves the whole feature set rides);
#   5. THE DATUM: JVM INFO exit=0 AND ALL SEVEN markers captured:
#      WORKLOAD-EXC-PASS WORKLOAD-IO-PASS WORKLOAD-GC-PASS WORKLOAD-THREAD-RAN
#      WORKLOAD-THREAD-RESULT-FOLLOWS "42" WORKLOAD-THREAD-PASS WORKLOAD-ALL-PASS
#      (the bare "42" is the thread datum: Worker.run computes 21*2 by imul and
#      putstatics it; a thread that never ran leaves the default 0).
#      A readable FAILURE (exit!=0 / signal / console fault rip) is RESULT FAIL with
#      the datum named - the next gate is the fix pass for whatever it names. On any
#      non-RUN outcome the class-load diagnostic arm fires (-Xlog:class+load=info,
#      same pipe arm) so a RED lands with an actionable row, not a rerun.
#
# Machine notes carried: ONE VM at a time (free the slot); wait for an ssh round trip,
# never a log marker; always stop the guest; grep console-derived text -a-tolerantly;
# invoke as `sh <file>`. No maps poller anywhere in this probe (D59/L2).

W=${DENVER_W:-/root/denver/w}
SPEC=${1:-$W/vm-classlib.spec}
PORT=${DENVER_PORT:-2247}
WAIT=${DENVER_BOOT_WAIT_TRIES:-30}     # x 5 s
SSH="ssh -y -i $W/key -p $PORT root@127.0.0.1"
VMLOG=$W/vmrun.log
GREENHASH="d4acb339"                   # the proven kernel binary - REQUIRED
NULLFIXHASH="72be5c55"                 # older rigs - must NOT boot
NULLSTOREHASH="800e8c80"
PROCSHASH="f0dcacb6"
MERGEDHASH="3531cf69"
HEAPROHASH="c921d625"
MEMINFOHASH="24398b99"
[ -n "$GREENHASH" ] || { echo "RESULT FAIL: rig identity constant empty" >&2; exit 1; }
RIP=60000067cd7a                       # the old death rip - must stay absent
VSYSCALL=ffffffffff600800              # the closed vsyscall death - must NOT recur
MARKER_EXC="WORKLOAD-EXC-PASS"
MARKER_IO="WORKLOAD-IO-PASS"
MARKER_GC="WORKLOAD-GC-PASS"
MARKER_RAN="WORKLOAD-THREAD-RAN"
MARKER_FOLLOWS="WORKLOAD-THREAD-RESULT-FOLLOWS"
MARKER_THREAD="WORKLOAD-THREAD-PASS"
MARKER_ALL="WORKLOAD-ALL-PASS"
JAVA_CMD="/opt/jdk/bin/java -Xms64m -Xmx1536m -cp /work Workload"
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

step "host-side staging sanity: the classlib image exists and carries both classes"
[ -r "$SPEC" ] || verdict "RESULT FAIL: no spec at $SPEC (staging: cp denver-jdk.img denver-classlib.img; debugfs mkdir /work; debugfs write Workload.class Worker.class; sed the disk= line into vm-classlib.spec)" 1
[ -r "$W/key" ] || verdict "RESULT FAIL: no key at $W/key" 1
[ -r "$VMLOG" ] || verdict "RESULT FAIL: no console log at $VMLOG" 1
[ -r "$W/denver-classlib.img" ] || verdict "RESULT FAIL: no image at $W/denver-classlib.img - staging incomplete" 1
staged=$(debugfs -R "ls -l /work" "$W/denver-classlib.img" 2>/dev/null | grep -a 'class')
echo "  in image: ${staged:-<MISSING>}"
case "$staged" in
    *Workload.class*) ;;
    *) verdict "RESULT FAIL: /work/Workload.class not in $W/denver-classlib.img - staging incomplete" 1 ;;
esac
case "$staged" in
    *Worker.class*) ;;
    *) verdict "RESULT FAIL: /work/Worker.class not in $W/denver-classlib.img - staging incomplete" 1 ;;
esac

step "free the VM slot (a stale guest answers EBUSY)"
echo stop > /dev/kvm0 2>/dev/null
sleep 3

step "boot: herd vm-run $SPEC (proven kernel d4acb339 + staged classlib disk)"
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

step "rig identity: uname carries the GREEN kernel $GREENHASH, no older rig"
echo "$out" | grep Linux
case "$out" in
    *denver*) ;;
    *) verdict "RESULT FAIL: uname does not say denver" 1 ;;
esac
case "$out" in
    *"$GREENHASH"*) ;;
    *) verdict "RESULT FAIL: the booted kernel is NOT the proven $GREENHASH - the workload gate requires it unchanged" 1 ;;
esac
for h in "$NULLFIXHASH" "$NULLSTOREHASH" "$PROCSHASH" "$MERGEDHASH" "$HEAPROHASH" "$MEMINFOHASH"; do
    case "$out" in
        *"$h"*) verdict "RESULT FAIL: a stale rig kernel booted (hash $h) - staging failed" 1 ;;
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

step "guest-side staging identity: the guest can NAME both classes before java is blamed"
lsout=$(timeout 30 $SSH "/bin/busybox ls -l /work" 2>&1 | grep -av connecting)
echo "$lsout"
case "$lsout" in
    *Workload.class*) echo "  guest sees /work/Workload.class" ;;
    *) verdict "RESULT FAIL: guest cannot see /work/Workload.class - the disk did not stage (ext2 driver or image identity)" 1 ;;
esac
case "$lsout" in
    *Worker.class*) echo "  guest sees /work/Worker.class" ;;
    *) verdict "RESULT FAIL: guest cannot see /work/Worker.class - the disk did not stage" 1 ;;
esac

step "B0: channel baseline (poke before any dispatch)"
b0=$(timeout 15 $SSH "echo ALIVE-B0" 2>&1)
echo "$b0"
case "$b0" in *ALIVE-B0*) CHANNEL_OK=1 ;; *) CHANNEL_OK= ;; esac

step "THE ARM (reader): jvmprobe java - pipe capture, blocking read, ZERO polling (L2 rule, D63/D66/D71; alarm bounds mean the exit report always returns)"
OFF=$(wc -c < "$VMLOG")                # console bytes before the dispatch
TARM=$(date +%s)
jv=$(timeout 240 $SSH "/bin/jvmprobe java $JAVA_CMD" 2>&1)
EAR=$(( $(date +%s) - TARM ))
echo "$jv"
echo "  (reader arm wall=${EAR}s; child alarm=90s, parent alarm=120s inside jvmprobe)"
EXITED=
RCODE=
case "$jv" in
    *"JVM INFO exit="*)
        EXITED=1
        RCODE=$(printf '%s\n' "$jv" | grep 'JVM INFO exit=' | sed -E 's/.*exit=(-?[0-9]+).*/\1/') ;;
esac

step "marker tally (all seven required for READABLE-RUN)"
TM=
for m in "$MARKER_EXC" "$MARKER_IO" "$MARKER_GC" "$MARKER_RAN" "$MARKER_FOLLOWS" "$MARKER_THREAD" "$MARKER_ALL"; do
    if printf '%s\n' "$jv" | grep -aq "$m"; then echo "  HAVE $m"; else echo "  MISS $m"; TM="$TM $m"; fi
done
if printf '%s\n' "$jv" | grep -aq '^42[[:space:]]*$'; then
    echo "  HAVE the raw 42 line (Worker.run's imul result crossed the thread boundary)"
else
    echo "  MISS the raw 42 line (thread datum: RESULT was not 42 after join)"
    TM="$TM RESULT!=42"
fi

step "diagnostic arm (fires only on a non-RUN outcome): class-load trace names the failing resolution step"
if [ -n "$TM" ] || [ "$RCODE" != "0" ]; then
    diag=$(timeout 240 $SSH "/bin/jvmprobe java /opt/jdk/bin/java -Xlog:class+load=info -Xms64m -Xmx1536m -cp /work Workload" 2>&1)
    echo "$diag" | grep -aE 'class,load|JVM INFO exit=|JVM PASS|JVM RED|Error|Exception at|at Workload|at Worker' | head -40
    echo "  (diagnostic capture above, trimmed)"
else
    echo "  skipped: the run already READABLE-RUN"
fi

step "console window: old death absent + census"
win=$(tail -c +$((OFF + 1)) "$VMLOG" 2>/dev/null | grep -a .)
faultline=$(printf '%s\n' "$win" | grep 'ring3 PageFault rip=' | tail -1)
killline=$(printf '%s\n' "$win" | grep -F 'killed (SIGSEGV)' | tail -1)
echo "-- console window: fault line:"
echo "${faultline:-<none>}"
echo "-- console window: kill line:"
echo "${killline:-<none>}"
printf '%s\n' "$win" | grep '\[ring\]\|\[hang\]\|\[fault-code\]\|\[mm\]\|\[nosys\]\|\[err\]\|\[Fault\]\|ring3 map \|ring3 regs \|ring3 stack ' > /tmp/javaclasslib-window.txt 2>/dev/null
echo "-- console window: evidence lines:"
head -30 /tmp/javaclasslib-window.txt 2>/dev/null
echo "-- console window: syscall census (trace-err buckets, top counts):"
printf '%s\n' "$win" | grep -ao 'nr=[0-9]*' | sort | uniq -c | sort -rn | head -18

case "$faultline" in
    *"rip=0x0000$RIP"*)
        echo "DATUM: the OLD rip appeared - the same kernel cannot regress; staging is the suspect" ;;
esac
case "$faultline" in
    *"$VSYSCALL"*)
        echo "DATUM: the vsyscall death reappeared - staging suspect" ;;
esac

step "verdict"
E=$(( $(date +%s) - T0 )); [ "$E" -lt 0 ] && E=0
[ -z "$CHANNEL_OK" ] && verdict "RESULT FAIL: even the B0 baseline poke went unanswered - the rig itself is broken" 1
if [ -z "$TM" ] && [ -n "$EXITED" ] && [ "$RCODE" = "0" ]; then
    echo "READABLE-RUN: Workload ran all four surfaces on kernel d4acb339 unchanged (exit=0, arm wall=${EAR:-?}s) - exceptions threw+unwound+were caught; /work/out.bin round-tripped through openat/write/read/close; young+humongous churn expanded the heap under -Xms64m -Xmx1536m; Worker ran in a real thread (RESULT=42 crossed join), start/join served"
    echo "  the phase-3 ladder's third rung is GREEN - zero new kernel code"
    verdict "RESULT PASS" 0
fi
if [ -n "$faultline" ] || [ -n "$killline" ]; then
    echo "READABLE-DEATH (workload gate: the rung FAILS, the datum is the fix pass's input): the console names a death (above)"
    verdict "RESULT FAIL: readable death while running the workload - datum above, next gate = fix pass for what it names" 1
fi
if [ -n "$EXITED" ]; then
    echo "READABLE-FAIL: java exited rc=${RCODE:-?} with missing markers:${TM:- none}; class-load trace above names the failing step"
    verdict "RESULT FAIL: the workload did not run clean (exit=$RCODE) - the datum is named; next gate = fix pass" 1
fi
verdict "RESULT FAIL: no exit report at all - arm_java's alarms should have bounded it; capture rig suspect" 1
