#!/bin/sh
# denver/probe-javaclasslib.sh - the PHASE-3-JAVACLASSLIB gate probe (G12): run the
# operator-staged REAL-JAVAC classes (Workload.class + Worker.class, /root/denver/javac/,
# md5s in that README.txt) through the four surfaces - exceptions, file I/O, G1 allocation
# pressure, threads - on the proven kernel. probe-javaworkload.sh's shape (stamps +
# free-the-slot first + stop at the end), and NO kernel code moves: the gate runs the
# proven GREEN kernel d4acb339 unchanged; only the disk differs (a FRESH COPY of
# denver-jdk.img with /work/{Workload,Worker}.class added by debugfs, D65/D70 - the
# stale attempt image's IoV* residue is NOT carried: the log shows exactly what ran).
#
#   sh denver/probe-javaclasslib.sh 2>&1 | tee denver/gates/PHASE-3-JAVACLASSLIB.log
#
# Design = denver/docs/research/PHASE-3-JAVACLASSLIB.md (D69-D71):
#   D69 the class files are real-javac output (operator's Mac, javac --release 21,
#   major 65) - the assembler premise ("no javac exists") was corrected by the operator;
#   D70 delivery = debugfs write into an image COPY + dump-back md5 verification;
#   D71 the reader is the pipe arm ONLY, and PASS needs ALL FOUR surface markers +
#   WORKLOAD-ALL-PASS + exit=0 (a marker-only pass could hide a silently failed surface).
#
# Pass conditions:
#   1. uname says denver AND carries d4acb339 AND NOT any older rig hash;
#   2. image staging: denver-classlib.img exists host-side and carries BOTH classes
#      with the README md5s (debugfs dump-back), and the guest can name them;
#   3. tokioprobe RESULT PASS (platform sanity);
#   4. GETCPU PASS cpu<4 node=0 (one PASS proves the whole feature set rides);
#   5. THE DATUM: RESULT PASS only on READABLE-RUN: JVM INFO exit=0 AND
#      WORKLOAD-EXC-PASS + WORKLOAD-IO-PASS + WORKLOAD-GC-PASS + WORKLOAD-THREAD-PASS
#      + WORKLOAD-ALL-PASS captured through the pipe arm.
#      A readable FAILURE is RESULT FAIL with the datum named. On any non-RUN outcome
#      the class-load diagnostic arm fires (same pipe arm).
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
SRC=/root/denver/javac
GREENHASH="d4acb339"                   # the proven kernel binary - REQUIRED
NULLFIXHASH="72be5c55"                 # older rigs - must NOT boot
NULLSTOREHASH="800e8c80"
PROCSHASH="f0dcacb6"
MERGEDHASH="3531cf69"
HEAPROHASH="c921d625"
MEMINFOHASH="24398b99"
WLMD5="477ed2ef861fb248cea250b2e118fb79"   # Workload.class, operator README
WKMD5="791d69b2a6653d9c8ecc2174e55a288e"   # Worker.class,  operator README
RIP=60000067cd7a                       # the old death rip - must stay absent
VSYSCALL=ffffffffff600800              # the closed vsyscall death - must NOT recur
MARKER="WORKLOAD-ALL-PASS"
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

step "source sanity: operator-staged classes present with the README md5s"
for f in Workload.class Worker.class; do
    [ -r "$SRC/$f" ] || verdict "RESULT FAIL: $SRC/$f missing (operator staging)" 1
done
for f in "$SRC/Workload.class:$WLMD5" "$SRC/Worker.class:$WKMD5"; do
    got=$(md5sum "${f%%:*}" | cut -d' ' -f1)
    [ "$got" = "${f##*:}" ] || verdict "RESULT FAIL: ${f%%:*} md5 $got != ${f##*:}" 1
done
echo "  sources OK (md5 match)"

step "host-side staging: FRESH copy of the proven image + debugfs write of both classes"
[ -r "$SPEC" ] || verdict "RESULT FAIL: no spec at $SPEC" 1
[ -r "$W/key" ] || verdict "RESULT FAIL: no key at $W/key" 1
[ -r "$VMLOG" ] || verdict "RESULT FAIL: no console log at $VMLOG" 1
[ -r "$W/denver-jdk.img" ] || verdict "RESULT FAIL: no proven image at $W/denver-jdk.img" 1
cp "$W/denver-jdk.img" "$W/denver-classlib.img" || verdict "RESULT FAIL: image copy failed" 1
debugfs -w -R "mkdir /work" "$W/denver-classlib.img" >/dev/null 2>&1
debugfs -w -R "write $SRC/Workload.class /work/Workload.class" "$W/denver-classlib.img" >/dev/null 2>&1
debugfs -w -R "write $SRC/Worker.class /work/Worker.class" "$W/denver-classlib.img" >/dev/null 2>&1
staged=$(debugfs -R "ls -l /work" "$W/denver-classlib.img" 2>/dev/null)
echo "$staged" | grep -E 'Workload.class|Worker.class' | sed 's/^/  in image: /'
case "$staged" in *Workload.class*Worker.class*) ;; *) verdict "RESULT FAIL: classes not in $W/denver-classlib.img - staging failed" 1 ;; esac
debugfs -R "dump /work/Workload.class /tmp/wl.img.class" "$W/denver-classlib.img" >/dev/null 2>&1
debugfs -R "dump /work/Worker.class /tmp/wk.img.class" "$W/denver-classlib.img" >/dev/null 2>&1
imgmd5=$(md5sum /tmp/wl.img.class | cut -d' ' -f1)
[ "$imgmd5" = "$WLMD5" ] || verdict "RESULT FAIL: Workload.class read-back md5 $imgmd5 != $WLMD5" 1
imgmd5=$(md5sum /tmp/wk.img.class | cut -d' ' -f1)
[ "$imgmd5" = "$WKMD5" ] || verdict "RESULT FAIL: Worker.class read-back md5 $imgmd5 != $WKMD5" 1
echo "  dump-back md5 match - the image carries the operator classes byte-exact"

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
    *) verdict "RESULT FAIL: the booted kernel is NOT the proven $GREENHASH - the gate requires it unchanged" 1 ;;
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

step "feature-set proof: the getcpu arm"
get=$(timeout 60 $SSH "/bin/jvmprobe getcpu" 2>&1)
echo "$get"
case "$get" in
    *"GETCPU PASS"*) ;;
    *) verdict "RESULT FAIL: the getcpu arm did not pass - the proven feature set is not riding" 1 ;;
esac
cpu=$(printf '%s\n' "$get" | grep 'GETCPU PASS' | sed -E 's/.*cpu=([0-9]+).*/\1/')
case "$cpu" in ''|*[!0-9]*) verdict "RESULT FAIL: could not parse cpu from the GETCPU PASS line" 1 ;; esac
[ "$cpu" -lt 4 ] || verdict "RESULT FAIL: cpu=$cpu out of range (spec vcpus=4)" 1

step "guest-side staging identity: the guest can NAME the classes before java is blamed"
lsout=$(timeout 30 $SSH "/bin/busybox ls -l /work" 2>&1 | grep -av connecting)
echo "$lsout"
case "$lsout" in
    *Workload.class*) WL_SEEN=1 ;;
esac
case "$lsout" in
    *Worker.class*) WK_SEEN=1 ;;
esac
if [ -n "$WL_SEEN" ] && [ -n "$WK_SEEN" ]; then
    echo "  guest sees both classes - image staging confirmed on the far side"
else
    verdict "RESULT FAIL: guest cannot see the classes in /work - the disk did not stage" 1
fi

step "B0: channel baseline (poke before any dispatch)"
b0=$(timeout 15 $SSH "echo ALIVE-B0" 2>&1)
echo "$b0"
case "$b0" in *ALIVE-B0*) CHANNEL_OK=1 ;; *) CHANNEL_OK= ;; esac

step "THE ARM (reader): the four-surface workload through the pipe capture (L2 rule; alarm bounds mean the exit report always returns)"
OFF=$(wc -c < "$VMLOG")                # console bytes before the dispatch
TARM=$(date +%s)
jv=$(timeout 300 $SSH "/bin/jvmprobe java /opt/jdk/bin/java -Xms64m -Xmx1536m -cp /work Workload" 2>&1)
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
case "$jv" in
    *"JVM PASS "*|*"JVM RED "*) JVLINE=$(printf '%s\n' "$jv" | grep -E '^JVM (PASS|RED)' | tail -1) ;;
esac
GOTMARKER=
case "$jv" in *"$MARKER"*) GOTMARKER=1 ;; esac
SURF=0
for m in WORKLOAD-EXC-PASS WORKLOAD-IO-PASS WORKLOAD-GC-PASS WORKLOAD-THREAD-PASS; do
    case "$jv" in *"$m"*) SURF=$((SURF + 1)) ;; esac
done
echo "  surface markers present: $SURF/4"

step "diagnostic arm (fires only on a non-RUN outcome): class-load trace names the failing resolution step"
if [ -z "$GOTMARKER" ] || [ "$RCODE" != "0" ] || [ "$SURF" != "4" ]; then
    diag=$(timeout 300 $SSH "/bin/jvmprobe java /opt/jdk/bin/java -Xlog:class+load=info -Xms64m -Xmx1536m -cp /work Workload" 2>&1)
    echo "$diag" | grep -E 'class,load|JVM INFO exit=|JVM PASS|JVM RED|JVM-INFO raw|Exception|Error' | head -40
    echo "  (full class-load capture above, trimmed to the load lines + verdict)"
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
head -25 /tmp/javaclasslib-window.txt 2>/dev/null
echo "-- console window: syscall census (trace-err buckets, top counts):"
printf '%s\n' "$win" | grep -ao 'nr=[0-9]*' | sort | uniq -c | sort -rn | head -15

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
if [ -n "$GOTMARKER" ] && [ -n "$EXITED" ] && [ "$RCODE" = "0" ] && [ "$SURF" = "4" ]; then
    echo "READABLE-RUN: Workload.class (real-javac, major 65) loaded from /work, ran all four surfaces to completion (exit=0, arm wall=${EAR:-?}s), all 4 surface markers + $MARKER captured: exceptions/unwind, java.io file I/O, G1 allocation pressure incl. humongous, thread start/join - served by kernel d4acb339 unchanged"
    echo "  the phase-3 ladder's third rung is GREEN - zero new kernel code"
    verdict "RESULT PASS" 0
fi
if [ -n "$faultline" ] || [ -n "$killline" ]; then
    echo "READABLE-DEATH (the rung FAILS, the datum is the fix pass's input): the console names a death (above)"
    verdict "RESULT FAIL: readable death while running the workload - datum above, next gate = fix pass for what it names" 1
fi
if [ -n "$EXITED" ]; then
    echo "READABLE-FAIL: java exited rc=${RCODE:-?} with ${JVLINE:-no verdict line}; class-load trace above names the failing step; surface markers $SURF/4"
    verdict "RESULT FAIL: the workload did not run clean (exit=$RCODE) - the datum is named; next gate = fix pass" 1
fi
verdict "RESULT FAIL: no exit report at all - arm_java's alarms should have bounded it; capture rig suspect" 1
