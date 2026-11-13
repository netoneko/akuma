#!/bin/sh
# denver/probe-method.sh - the PHASE-1-METHOD gate probe (vm-probe.sh's shape, new arms).
#
#   sh denver/probe-method.sh 2>&1 | tee denver/gates/PHASE-1-METHOD.log
#
# Phase 1 = the method (lessons 1, 2, 4). This script proves, in one captured run:
#   - lesson 2 (RED): every probe arm runs and fails *cleanly* — a NAME-prefixed verdict
#     line, never a hang or garbage (the RED suite: getcpu, auxv, rwx, java; cycles is
#     the diagnostics arm, R8, and PASSes);
#   - lesson 1 (requirements evidence): the workload (`java -version`, the attempt doc's
#     own command) runs on BOTH kernels this session can reach — the denver guest and the
#     host metal — and the failure evidence lands in the capture (D11's A/B);
#   - lesson 4 (measured loop): a hypothesis is stated with a timestamp and its verdict
#     lands inside the capture; the wall-clock seconds are printed (done-when: < 3600);
#   - the platform did not regress under the bigger image: /bin/tokioprobe must still
#     print RESULT PASS (phase 0's own marker) before any method evidence counts.
#
# The gate passes on METHOD evidence only: clean verdict lines + captured failure + a
# measured cycle. RED is the expected phase-1 outcome; `RESULT PASS` at the end is the
# marker the autoresearch driver greps (phase-0 D12 convention).
#
# Machine notes carried from vm-probe.sh: the host runs ONE VM at a time — free the slot
# first (a stale guest answers EBUSY/errno 16, not "broken KVM"); wait for an ssh round
# trip, never a log marker; always stop the guest. grep -a over the capture is mandatory
# downstream (taoxichuan gates.md rule: the log carries control bytes).

W=${DENVER_W:-/root/denver/w}
SPEC=${1:-$W/vm-jdk.spec}
PORT=${DENVER_PORT:-2247}
WAIT=${DENVER_BOOT_WAIT_TRIES:-30}     # x 5 s
SSH="ssh -y -i $W/key -p $PORT root@127.0.0.1"
VMLOG=$W/vmrun.log
JM=${DENVER_JVMPROBE:-denver/probes/jvmprobe}
JDK=/opt/jdk/bin/java                  # the workload in the guest image
METAL_JAVA=${DENVER_METAL_JAVA:-/usr/bin/java}
T0=$(date +%s)

VP=
verdict() { echo "$1"; stop_vm; echo "== wall clock: method seconds=$(( $(date +%s) - T0 ))"; exit "$2"; }
stop_vm() {
    echo stop > /dev/kvm0 2>/dev/null
    [ -n "$VP" ] && kill "$VP" 2>/dev/null
    VP=
}

[ -r "$SPEC" ] || verdict "RESULT FAIL: no spec at $SPEC" 1
[ -r "$W/key" ] || verdict "RESULT FAIL: no key at $W/key" 1
[ -x "$JM" ] || gcc -static -O2 -o "$JM" denver/probes/jvmprobe.c
[ -x "$JM" ] || verdict "RESULT FAIL: cannot build $JM" 1

echo "== free the VM slot (a stale guest answers EBUSY)"
echo stop > /dev/kvm0 2>/dev/null
sleep 3

echo "== boot: herd vm-run $SPEC"
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

echo "== uname"
echo "$out" | grep Linux
case "$out" in *denver*) ;; *) verdict "RESULT FAIL: uname does not say denver" 1 ;; esac

echo "== lesson 1, metal leg: the workload on the host kernel (the A/B of D11)"
for arm in getcpu auxv rwx cycles; do
    echo "-- metal: $arm"
    timeout 60 "$JM" "$arm" 2>&1
done
echo "-- metal: java -version"
timeout 150 "$JM" java "$METAL_JAVA" 2>&1

echo "== lesson 1/2, guest leg: the workload + the RED suite on the denver kernel"
echo "== platform sanity first: the bigger image must not regress tokio (phase 0's marker)"
tok=$(timeout 90 $SSH "/bin/tokioprobe" 2>&1)
echo "$tok"
case "$tok" in *"RESULT PASS"*) ;; *) verdict "RESULT FAIL: tokioprobe did not pass on the jdk image" 1 ;; esac

for arm in getcpu auxv rwx cycles; do
    echo "-- guest: $arm"
    out_arm=$(timeout 60 $SSH "/bin/jvmprobe $arm" 2>&1)
    echo "$out_arm"
    case "$arm" in
        getcpu) g_getcpu=$out_arm ;;
        auxv)   g_auxv=$out_arm ;;
        rwx)    g_rwx=$out_arm ;;
        cycles) g_cycles=$out_arm ;;
    esac
done

echo "-- guest: java -version (H1 is timed from the hypothesis line to the verdict line)"
echo "HYPOTHESIS H1 ($(date +%s)): getcpu(2) is served nowhere on denver -> the getcpu arm answers ENOSYS; and the workload dies inside HotSpot init (the attempt's blocker shape) on BOTH kernels"
H_T0=$(date +%s)
hline=$(timeout 60 $SSH "/bin/jvmprobe getcpu" 2>&1 | grep -E "^GETCPU" | head -1)
echo "H1 getcpu verdict: $hline"
jout=$(timeout 180 $SSH "/bin/jvmprobe java $JDK" 2>&1)
echo "$jout"
H_T1=$(date +%s)
H_S=$((H_T1 - H_T0))
[ "$H_S" -le 3600 ] && cyv=PASS || cyv=FAIL
echo "CYCLE $cyv H1 seconds=$H_S (hypothesis stated -> verdict captured; done-when: under an hour)"

echo "== hs_err evidence (the plan: 'a better failure is progress - hs_err is a milestone')"
hfile=$($SSH "ls /root/hs_err_pid*.log 2>/dev/null | head -1" 2>&1 | grep "^/root" | head -1)
if [ -n "$hfile" ]; then
    echo "-- guest $hfile (head)"
    timeout 30 $SSH "head -25 $hfile" 2>&1
else
    echo "-- guest hs_err: none (the failure left no report - the JVM INFO lines above are the evidence)"
fi
mh=$(ls -t hs_err_pid*.log 2>/dev/null | head -1)
if [ -n "$mh" ]; then
    echo "-- metal $mh (head)"
    head -12 "$mh" 2>/dev/null
fi

echo "== verdict: every arm must have produced a clean NAME VERDICT line (RED is expected)"
ok=1
# judge each captured body by its UPPERCASE prefix (the arm output lines are UPPERCASE)
for up in GETCPU AUXV RWX; do
    body=$(eval echo \${g_$(echo $up | tr A-Z a-z)})
    echo "$body" | grep -qE "^$up (PASS|RED)" || { echo "MISSING: clean verdict line for $up"; ok=0; }
done
echo "$g_cycles" | grep -qE "^CYCLES PASS" || { echo "MISSING: CYCLES PASS"; ok=0; }
echo "$jout" | grep -qE "^JVM (PASS|RED)" || { echo "MISSING: clean JVM verdict line"; ok=0; }
[ "$ok" = 1 ] && verdict "RESULT PASS" 0
verdict "RESULT FAIL: not every arm produced a clean verdict (see MISSING lines)" 1
