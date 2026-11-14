#!/bin/sh
# denver/probe-javac.sh - the PHASE-3-JAVAC gate probe (D75 + run deltas D77-D80,
# denver/docs/research/PHASE-3-JAVAC.md): does a SYNCHRONOUS ring-3 fault reach the
# JVM's own SIGSEGV handler on the fault-signal kernel, so that javac - the trap-and-
# handle consumer - works inside the guest?
#
#   sh denver/probe-javac.sh 2>&1 | tee denver/gates/PHASE-3-JAVAC.log
#
# Gate kernel = the EXISTING operator-proven binary /root/denver/w/denver-faultsig
# (stamp 38f16b2d - the operator's javac/Clojure/Scala/Groovy runs, zero [Fault] lines).
# No kernel code moves in this probe. Disk = a fresh COPY of the operator-staged
# denver-faultsig.img (javac writes /work/out; the staged original stays pristine),
# spec = vm-javac.spec (the operator's proven vm-faultsig.spec except disk + port 2249).
#
# Arms in order (D75, verdict rules D78):
#   1. SIGPROBE  - /work/sigprobe: SA_SIGINFO handler, write through NULL, patch saved
#      rip (skip the dead mov) + rax->99, rt_sigreturn. REQUIRED: delivery (the
#      SIGPROBE-DELIVERED line). The rax edit is KNOWN-OPEN (operator-measured rc=0
#      want 99, unexplained): recorded verbatim, NOT a gate blocker (D78).
#   2. JAVAC -Xint - recorded observation only (JIT-specificity, alarm-bounded).
#   3. THE DATUM - `java -m jdk.compiler/com.sun.tools.javac.Main -version`: exit=0,
#      console window clean of any kill/fault, the old death rip 0x6000118848e3 absent.
#   4. STAGED-PRECOMPILED (D79) - the operator's exact command line runs the staged
#      real-javac classes on the GATE kernel: four surfaces + WORKLOAD-ALL-PASS.
#   5. DONE (the gate's bar) - javac compiles /work/Workload.java + Worker.java INSIDE
#      the guest (-d /work/out), then the produced class runs: same markers.
#
# PASS = uname identity + tokioprobe + getcpu arm + arm 1 delivered + arms 3,4,5.
# Every failure names its datum; REDs land with the class-load diagnostic trace.

W=${DENVER_W:-/root/denver/w}
GATEKERNEL=$W/denver-faultsig
SRCIMG=$W/denver-faultsig.img          # operator-staged; NEVER modified in place (D80)
IMG=$W/denver-javac.img                # this probe's own copy
SPEC=$W/vm-javac.spec
PORT=${DENVER_PORT:-2249}
WAIT=${DENVER_BOOT_WAIT_TRIES:-30}     # x 5 s
SSH="ssh -y -i $W/key -p $PORT root@127.0.0.1"
VMLOG=$W/vm-javac.log                  # own console log: vmrun.log keeps the 16:23 JAVACLASSLIB console (D80)
GATEHASH="38f16b2d"                    # the fault-signal kernel - REQUIRED
OLDHASHES="d4acb339 72be5c55 800e8c80 f0dcacb6 3531cf69 c921d625 24398b99"
OLDRIP="6000118848e3"                  # the datum's death rip - must stay absent
WL_MD5="477ed2ef861fb248cea250b2e118fb79"   # staged README
WK_MD5="791d69b2a6653d9c8ecc2174e55a288e"
SIGPROBE_MD5=$(md5sum denver/probes/sigprobe | cut -d' ' -f1)
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
markers_ok() { m=1
    for k in WORKLOAD-EXC-PASS WORKLOAD-IO-PASS WORKLOAD-GC-PASS WORKLOAD-THREAD-PASS WORKLOAD-ALL-PASS; do
        case "$1" in *"$k"*) ;; *) echo "  missing marker: $k"; m=0 ;; esac
    done
    return $((1 - m))
}

step "host-side staging sanity: kernel + image + key + staged contents (md5, D80)"
[ -r "$GATEKERNEL" ] || verdict "RESULT FAIL: no gate kernel at $GATEKERNEL" 1
[ -r "$SRCIMG" ] || verdict "RESULT FAIL: no staged image at $SRCIMG" 1
[ -r "$W/key" ] || verdict "RESULT FAIL: no key at $W/key" 1
cp "$SRCIMG" "$IMG" || verdict "RESULT FAIL: could not copy $SRCIMG to $IMG" 1
dump_md5() { debugfs -R "dump $1 /tmp/denver-javac-dump" "$IMG" 2>/dev/null >/dev/null && md5sum /tmp/denver-javac-dump | cut -d' ' -f1; }
wl=$(dump_md5 /work/Workload.class);  echo "  Workload.class md5=$wl (want $WL_MD5)"
wk=$(dump_md5 /work/Worker.class);    echo "  Worker.class   md5=$wk (want $WK_MD5)"
sp=$(dump_md5 /work/sigprobe);        echo "  sigprobe       md5=$sp (tree build $SIGPROBE_MD5)"
wj=$(debugfs -R "ls -l /work" "$IMG" 2>/dev/null | grep -c 'Workload.java')
[ "$wl" = "$WL_MD5" ] || verdict "RESULT FAIL: staged Workload.class md5 mismatch" 1
[ "$wk" = "$WK_MD5" ] || verdict "RESULT FAIL: staged Worker.class md5 mismatch" 1
[ "$sp" = "$SIGPROBE_MD5" ] || verdict "RESULT FAIL: staged sigprobe does not match the tree build" 1
[ "$wj" -ge 1 ] || verdict "RESULT FAIL: /work/Workload.java not staged in $IMG" 1

cat > "$SPEC" <<EOF2
type=kvm
kernel=$GATEKERNEL
cmdline=init=/bin/sshd initargs=--port,22,--idle-timeout-secs,0,--shell-arg,+m smp=4 epoch=1794671763
ram_mib=3072
vcpus=4
disk=$IMG
disk_ram=0
outbound=1
forward=$PORT:22
EOF2

step "free the VM slot (a stale guest answers EBUSY; the gate stops whatever holds /dev/kvm0 - expected)"
echo stop > /dev/kvm0 2>/dev/null
sleep 3

step "boot: herd vm-run $SPEC (gate kernel $GATEHASH + the javac disk copy)"
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

step "rig identity: uname carries the gate kernel $GATEHASH, no older rig"
echo "$out" | grep Linux
case "$out" in
    *denver*) ;;
    *) verdict "RESULT FAIL: uname does not say denver" 1 ;;
esac
case "$out" in
    *"$GATEHASH"*) ;;
    *) verdict "RESULT FAIL: the booted kernel is NOT the fault-signal gate kernel $GATEHASH" 1 ;;
esac
for h in $OLDHASHES; do
    case "$out" in
        *"$h"*) verdict "RESULT FAIL: an older rig kernel booted (hash $h) - staging failed" 1 ;;
    esac
done

step "platform sanity: tokioprobe"
tok=$(timeout 90 $SSH "/bin/tokioprobe" 2>&1)
echo "$tok"
case "$tok" in *"RESULT PASS"*) ;; *) verdict "RESULT FAIL: tokioprobe did not pass" 1 ;; esac

step "feature-set proof: the getcpu arm (one PASS proves the underlying feature set rides)"
get=$(timeout 60 $SSH "/bin/jvmprobe getcpu" 2>&1)
echo "$get"
case "$get" in
    *"GETCPU PASS"*) ;;
    *) verdict "RESULT FAIL: the getcpu arm did not pass - the feature set is not riding" 1 ;;
esac

step "ARM 1 (D75/D78): sigprobe - synchronous fault -> own SIGSEGV handler, edited ucontext, sigreturn"
sig=$(timeout 30 $SSH "/work/sigprobe" 2>&1)
echo "$sig"
SIGDEL=
case "$sig" in *"SIGPROBE-DELIVERED"*) SIGDEL=1 ;; esac
[ -n "$SIGDEL" ] || verdict "RESULT FAIL: SIGSEGV was NOT delivered to the user handler (kill or silence) - the fault-signal path is broken" 1
case "$sig" in
    *"SIGPROBE PASS"*)
        echo "  sigprobe FULL PASS (delivery + rip edit + rax edit)" ;;
    *)
        echo "  KNOWN-OPEN (D78): delivery + rip edit work, the rax edit did not take (recorded verbatim above); carried to REQUIREMENTS as its own open row - NOT a gate blocker" ;;
esac

step "guest-side staging identity: the guest can NAME the source and the probe binary"
lsout=$(timeout 30 $SSH "/bin/busybox ls -l /work" 2>&1 | grep -av connecting)
echo "$lsout"
case "$lsout" in
    *Workload.java*) echo "  guest sees /work/Workload.java - staging confirmed on the far side" ;;
    *) verdict "RESULT FAIL: guest cannot see /work/Workload.java" 1 ;;
esac

step "B0: channel baseline (poke before any dispatch)"
b0=$(timeout 15 $SSH "echo ALIVE-B0" 2>&1)
echo "$b0"
case "$b0" in *ALIVE-B0*) CHANNEL_OK=1 ;; *) CHANNEL_OK= ;; esac

OFF=$(wc -c < "$VMLOG")                # console bytes before the java dispatches

step "ARM 2 (recorded observation): javac -version under -Xint (JIT-specificity, alarm-bounded)"
xi=$(timeout 180 $SSH "/bin/jvmprobe java /opt/jdk/bin/java -Xint -m jdk.compiler/com.sun.tools.javac.Main -version" 2>&1)
echo "$xi"

step "ARM 3 = THE DATUM: javac -version on the JIT (the old death must not recur)"
OFFDATUM=$(wc -c < "$VMLOG")
dt=$(timeout 240 $SSH "/bin/jvmprobe java /opt/jdk/bin/java -m jdk.compiler/com.sun.tools.javac.Main -version" 2>&1)
echo "$dt"
DTEXIT=
DTRC=
case "$dt" in
    *"JVM INFO exit="*)
        DTEXIT=1
        DTRC=$(printf '%s\n' "$dt" | grep 'JVM INFO exit=' | sed -E 's/.*exit=(-?[0-9]+).*/\1/') ;;
esac
JAVACLINE=
case "$dt" in
    *javac\ *) JAVACLINE=$(printf '%s\n' "$dt" | grep 'javac ' | head -1) ;;
esac
[ -n "$DTEXIT" ] || verdict "RESULT FAIL: no exit report for the datum run - capture rig suspect" 1
[ "$DTRC" = "0" ] || verdict "RESULT FAIL: javac -version exited rc=$DTRC (datum: $JAVACLINE)" 1
dwin=$(tail -c +$((OFFDATUM + 1)) "$VMLOG" 2>/dev/null | grep -a .)
if printf '%s\n' "$dwin" | grep -aq 'task killed\|ring3 PageFault'; then
    printf '%s\n' "$dwin" | grep -a 'task killed\|ring3 PageFault' | head -5
    verdict "RESULT FAIL: the datum window carries a fault/kill - the JIT died again" 1
fi
case "$dwin" in
    *"$OLDRIP"*) verdict "RESULT FAIL: the OLD death rip $OLDRIP appeared in the datum window" 1 ;;
esac
echo "  DATUM PASS: exit=0, javac banner ${JAVACLINE:-<captured above>}, window clean of fault/kill, old rip absent"

step "ARM 4 (D79): the staged real-javac classes run on the GATE kernel (the operator's exact command line)"
w1=$(timeout 240 $SSH "/bin/jvmprobe java /opt/jdk/bin/java -Xms64m -Xmx1536m -cp /work Workload" 2>&1)
echo "$w1"
W1EXIT=$(printf '%s\n' "$w1" | grep 'JVM INFO exit=' | sed -E 's/.*exit=(-?[0-9]+).*/\1/')
[ "$W1EXIT" = "0" ] || verdict "RESULT FAIL: staged Workload exited rc=${W1EXIT:-?} on the gate kernel" 1
markers_ok "$w1" || { echo "$w1" | tail -8; verdict "RESULT FAIL: staged Workload markers incomplete on the gate kernel" 1; }
echo "  STAGED PASS: four surfaces + WORKLOAD-ALL-PASS on kernel $GATEHASH (was proven on d4acb339 at 16:23)"

step "ARM 5a = DONE compile: javac compiles /work/Workload.java + Worker.java INSIDE the guest"
cp=$(timeout 240 $SSH "/bin/jvmprobe java /opt/jdk/bin/java -m jdk.compiler/com.sun.tools.javac.Main -d /work/out /work/Workload.java /work/Worker.java" 2>&1)
echo "$cp"
CPEXIT=$(printf '%s\n' "$cp" | grep 'JVM INFO exit=' | sed -E 's/.*exit=(-?[0-9]+).*/\1/')
[ "$CPEXIT" = "0" ] || { echo "$cp" | tail -12
    diag=$(timeout 240 $SSH "/bin/jvmprobe java /opt/jdk/bin/java -Xlog:class+load=info -m jdk.compiler/com.sun.tools.javac.Main -d /work/out /work/Workload.java" 2>&1)
    echo "$diag" | grep -E 'class,load|JVM INFO exit=|JVM PASS|JVM RED|error' | head -30
    verdict "RESULT FAIL: in-guest javac exited rc=${CPEXIT:-?} (diagnostics above)" 1; }
echo "  COMPILE PASS: exit=0, classes written to /work/out"

step "ARM 5b = DONE run: the guest-compiled class runs (the gate's bar)"
w2=$(timeout 240 $SSH "/bin/jvmprobe java /opt/jdk/bin/java -cp /work/out Workload" 2>&1)
echo "$w2"
W2EXIT=$(printf '%s\n' "$w2" | grep 'JVM INFO exit=' | sed -E 's/.*exit=(-?[0-9]+).*/\1/')
[ "$W2EXIT" = "0" ] || verdict "RESULT FAIL: guest-compiled Workload exited rc=${W2EXIT:-?}" 1
markers_ok "$w2" || { echo "$w2" | tail -8; verdict "RESULT FAIL: guest-compiled Workload markers incomplete" 1; }
echo "  DONE PASS: javac-compiled-in-the-guest classes run all four surfaces + WORKLOAD-ALL-PASS"

step "console window: the whole java session clean + census"
win=$(tail -c +$((OFF + 1)) "$VMLOG" 2>/dev/null | grep -a .)
faultline=$(printf '%s\n' "$win" | grep 'ring3 PageFault rip=' | tail -1)
killline=$(printf '%s\n' "$win" | grep -F 'task killed' | tail -1)
echo "-- console window: fault line: ${faultline:-<none>}"
echo "-- console window: kill line: ${killline:-<none>}"
printf '%s\n' "$win" | grep '\[ring\]\|\[hang\]\|\[fault-code\]\|\[mm\]\|\[nosys\]\|\[err\]\|\[Fault\]' > /tmp/javac-window.txt 2>/dev/null
echo "-- console window: evidence lines:"
head -20 /tmp/javac-window.txt 2>/dev/null
echo "-- console window: syscall census (trace-err buckets, top counts):"
printf '%s\n' "$win" | grep -ao 'nr=[0-9]*' | sort | uniq -c | sort -rn | head -12

step "verdict"
E=$(( $(date +%s) - T0 )); [ "$E" -lt 0 ] && E=0
[ -z "$CHANNEL_OK" ] && verdict "RESULT FAIL: even the B0 baseline poke went unanswered - the rig itself is broken" 1
if [ -n "$faultline" ] || [ -n "$killline" ]; then
    echo "READABLE-DEATH: the console names a death somewhere after the dispatch (above)"
    verdict "RESULT FAIL: readable death in the java session - datum above" 1
fi
echo "GATE GREEN: kernel $GATEHASH (fault-signal riding) delivers a synchronous fault to the JVM's own SIGSEGV handler -"
echo "  javac -version exits 0, the staged real-javac workload passes on this kernel, javac compiled Workload.java INSIDE"
echo "  the guest and the produced class ran all four surfaces to WORKLOAD-ALL-PASS. G13's bar is met."
verdict "RESULT PASS" 0
