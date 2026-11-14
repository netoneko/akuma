#!/bin/sh
# denver/probe-javaworkload.sh - the PHASE-3-JAVAWORKLOAD gate probe: run a COMPILED
# class (Hello.class) on the guest - the first java rung that exercises class loading,
# GC threads, and the file/classpath paths -version never touches. probe-javaversion.sh's
# shape (stamps + free-the-slot first + stop at the end), and NO kernel code moves: the
# gate runs the proven GREEN kernel d4acb339 unchanged; only the disk differs (a COPY of
# denver-jdk.img with /work/Hello.class added by debugfs, D65).
#
#   sh denver/probe-javaworkload.sh 2>&1 | tee denver/gates/PHASE-3-JAVAWORKLOAD.log
#
# Design = denver/docs/research/PHASE-3-JAVAWORKLOAD.md (D64-D66):
#   D64 the class is hand-assembled (no javac exists anywhere in the lineage) by
#   denver/probes/mkhello.py and verified structurally by denver/probes/verifyclass.py
#   (the host JRE cannot run-verify: it dies at the vsyscall address in VM init - the
#   L1 datum applies to this host too). D65 delivery = debugfs write into an image COPY
#   (jvmprobe reached /bin the same way; the proven image is never modified in place).
#   D66 the reader is the pipe arm ONLY (L2 rule from D63: no maps poller anywhere;
#   arm_java's alarm(90)/alarm(120) bounds mean the exit report ALWAYS returns - a hang
#   names itself as signal 14, so JAVAVERSION's foreground-fallback arm and the
#   /dev/console probe are dropped: their questions are closed).
#
# Pass conditions:
#   1. uname says denver AND carries d4acb339 AND NOT any older rig hash
#      (the workload gate REQUIRES the proven kernel binary; a different hash booted
#      = staging failed);
#   2. image staging: denver-hello.img exists host-side, /work/Hello.class is in it
#      (debugfs), and the guest can name it (/bin/busybox ls -l /work);
#   3. tokioprobe RESULT PASS (platform sanity);
#   4. GETCPU PASS cpu<4 node=0 (one PASS proves the whole feature set rides);
#   5. THE DATUM (stricter than D60 - this is a workload gate, not a capture gate):
#      RESULT PASS only on READABLE-RUN: JVM INFO exit=0 AND the marker line
#      HELLO-DENVER-WORKLOAD-PASS captured through the pipe arm.
#      A readable FAILURE (exit!=0 / signal / console fault rip) is RESULT FAIL with
#      the datum named - the next gate is the fix pass for whatever it names. On any
#      non-RUN outcome the class-load diagnostic arm fires (java -Xlog:class+load=info,
#      same pipe arm) so a RED lands with an actionable row, not a rerun.
#
# Machine notes carried: ONE VM at a time (free the slot); wait for an ssh round trip,
# never a log marker; always stop the guest; grep console-derived text -a-tolerantly;
# invoke as `sh <file>`. No maps poller anywhere in this probe (D59/L2).

W=${DENVER_W:-/root/denver/w}
SPEC=${1:-$W/vm-hello.spec}
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
MARKER="HELLO-DENVER-WORKLOAD-PASS"
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

step "host-side staging sanity: the hello image exists and carries /work/Hello.class"
[ -r "$SPEC" ] || verdict "RESULT FAIL: no spec at $SPEC (run the staging steps: cp denver-jdk.img denver-hello.img; debugfs mkdir /work; debugfs write .../Hello.class; vm-hello.spec)" 1
[ -r "$W/key" ] || verdict "RESULT FAIL: no key at $W/key" 1
[ -r "$VMLOG" ] || verdict "RESULT FAIL: no console log at $VMLOG" 1
[ -r "$W/denver-hello.img" ] || verdict "RESULT FAIL: no image at $W/denver-hello.img - staging incomplete" 1
staged=$(debugfs -R "ls -l /work" "$W/denver-hello.img" 2>/dev/null | grep 'Hello.class')
echo "  in image: ${staged:-<MISSING>}"
case "$staged" in *Hello.class*) ;; *) verdict "RESULT FAIL: /work/Hello.class not in $W/denver-hello.img - staging incomplete" 1 ;; esac

step "free the VM slot (a stale guest answers EBUSY)"
echo stop > /dev/kvm0 2>/dev/null
sleep 3

step "boot: herd vm-run $SPEC (proven kernel d4acb339 + staged hello disk)"
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
cpu=$(printf '%s\n' "$get" | grep 'GETCPU PASS' | sed -E 's/.*cpu=([0-9]+).*/\1/')
case "$cpu" in ''|*[!0-9]*) verdict "RESULT FAIL: could not parse cpu from the GETCPU PASS line" 1 ;; esac
[ "$cpu" -lt 4 ] || verdict "RESULT FAIL: cpu=$cpu out of range (spec vcpus=4)" 1

step "guest-side staging identity: the guest can NAME the class before java is blamed"
lsout=$(timeout 30 $SSH "/bin/busybox ls -l /work" 2>&1 | grep -av connecting)
echo "$lsout"
case "$lsout" in
    *Hello.class*) echo "  guest sees /work/Hello.class - image staging confirmed on the far side" ;;
    *) verdict "RESULT FAIL: guest cannot see /work/Hello.class - the disk did not stage (ext2 driver or image identity)" 1 ;;
esac

step "B0: channel baseline (poke before any dispatch)"
b0=$(timeout 15 $SSH "echo ALIVE-B0" 2>&1)
echo "$b0"
case "$b0" in *ALIVE-B0*) CHANNEL_OK=1 ;; *) CHANNEL_OK= ;; esac

step "THE ARM (reader): jvmprobe java - pipe capture, blocking read, ZERO polling (L2 rule, D63/D66; alarm bounds mean the exit report always returns)"
OFF=$(wc -c < "$VMLOG")                # console bytes before the dispatch
TARM=$(date +%s)
jv=$(timeout 240 $SSH "/bin/jvmprobe java /opt/jdk/bin/java -cp /work Hello" 2>&1)
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
case "$jv" in
    *"$MARKER"*) GOTMARKER=1 ;;
esac

step "diagnostic arm (fires only on a non-RUN outcome): class-load trace names the failing resolution step"
if [ -z "$GOTMARKER" ] || [ "$RCODE" != "0" ]; then
    diag=$(timeout 240 $SSH "/bin/jvmprobe java /opt/jdk/bin/java -Xlog:class+load=info -cp /work Hello" 2>&1)
    echo "$diag" | grep -E 'class,load|JVM INFO exit=|JVM PASS|JVM RED|JVM-INFO raw' | head -40
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
printf '%s\n' "$win" | grep '\[ring\]\|\[hang\]\|\[fault-code\]\|\[mm\]\|\[nosys\]\|\[err\]\|\[Fault\]\|ring3 map \|ring3 regs \|ring3 stack ' > /tmp/javaworkload-window.txt 2>/dev/null
echo "-- console window: evidence lines:"
head -25 /tmp/javaworkload-window.txt 2>/dev/null
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
if [ -n "$GOTMARKER" ] && [ -n "$EXITED" ] && [ "$RCODE" = "0" ]; then
    echo "READABLE-RUN: Hello.class loaded from /work through the classpath, ran to completion (exit=0, arm wall=${EAR:-?}s), marker captured: class loading + GC threads + classpath file paths + println all served by kernel d4acb339 unchanged"
    echo "  the phase-3 ladder's second rung is GREEN - zero new kernel code"
    verdict "RESULT PASS" 0
fi
if [ -n "$faultline" ] || [ -n "$killline" ]; then
    echo "READABLE-DEATH (workload gate: the rung FAILS, the datum is the fix pass's input): the console names a death (above)"
    verdict "RESULT FAIL: readable death while running the class - datum above, next gate = fix pass for what it names" 1
fi
if [ -n "$EXITED" ]; then
    echo "READABLE-FAIL: java exited rc=${RCODE:-?} with ${JVLINE:-no verdict line}; class-load trace above names the failing step"
    verdict "RESULT FAIL: the class did not run clean (exit=$RCODE) - the datum is named; next gate = fix pass" 1
fi
verdict "RESULT FAIL: no exit report at all - arm_java's alarms should have bounded it; capture rig suspect" 1
