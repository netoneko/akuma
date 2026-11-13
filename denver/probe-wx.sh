#!/bin/sh
# denver/probe-wx.sh - the PHASE-2-WX gate probe (vm-probe.sh's shape; the phase-1 jdk image;
# the wx-relaxed kernel binary denver-wx).
#
#   sh denver/probe-wx.sh 2>&1 | tee denver/gates/PHASE-2-WX.log
#
# G1 (denver/docs/REQUIREMENTS.md): the W^X policy must let HotSpot's executable-page marking
# through. Pass condition (the PHASE-1-METHOD transition line, D16 of the research artifact):
#   - platform sanity: /bin/tokioprobe prints RESULT PASS (the policy flip must not regress tokio);
#   - the rwx arm: RWX PASS rw_to_rx=42 - phase 1 captured the RED this gate closes on the same
#     image: `RWX RED mmap-RWX-refused errno=13 (Permission denied)` + `RWX RED rw_to_rx mprotect
#     errno=13 (Permission denied)`;
#   - the java arm: /opt/jdk/bin/java -version gets past the blocker-1 line ("Failed to mark
#     memory page as executable") - expected now to die at getcpu (the attempt's blocker 2, G2's
#     gate); a full `JVM PASS` would contradict phase 1's G2 evidence and is recorded as a
#     surprise, not assumed.
#
# The rig: /root/denver/w/denver-wx (built `cargo build -p denver --features wx-relaxed`,
# staged by hand, recorded in RESULT.md) + vm-wx.spec (copy of vm-jdk.spec with kernel= swapped).
# The phase-0/1 rig binaries stay untouched (D4/D15): the RED gate stays reproducible.
# Machine notes carried from vm-probe.sh: the host runs ONE VM at a time - free the slot first
# (a stale guest answers EBUSY/errno 16, not "broken KVM"); wait for an ssh round trip, never a
# log marker; always stop the guest.
# Not a `#!` script on the box: Akuma's execve cannot run one. Invoke it as `sh <file>`.

W=${DENVER_W:-/root/denver/w}
SPEC=${1:-$W/vm-wx.spec}
PORT=${DENVER_PORT:-2247}
WAIT=${DENVER_BOOT_WAIT_TRIES:-30}     # x 5 s
SSH="ssh -y -i $W/key -p $PORT root@127.0.0.1"
VMLOG=$W/vmrun.log
BLOCKER1="Failed to mark memory page as executable"

VP=
verdict() { echo "$1"; stop_vm; exit "$2"; }
stop_vm() {
    echo stop > /dev/kvm0 2>/dev/null
    [ -n "$VP" ] && kill "$VP" 2>/dev/null
    VP=
}

[ -r "$SPEC" ] || verdict "RESULT FAIL: no spec at $SPEC (stage denver-wx + vm-wx.spec first, see denver/docs/research/PHASE-2-WX.md D15)" 1
[ -r "$W/key" ] || verdict "RESULT FAIL: no key at $W/key" 1

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

echo "== uname (the hash names the wx-relaxed build; the phase-1 RED kernel was 8dc2dbcc)"
echo "$out" | grep Linux
case "$out" in *denver*) ;; *) verdict "RESULT FAIL: uname does not say denver" 1 ;; esac

echo "== platform sanity: the policy flip must not regress tokio (phase 0's marker)"
tok=$(timeout 90 $SSH "/bin/tokioprobe" 2>&1)
echo "$tok"
case "$tok" in *"RESULT PASS"*) ;; *) verdict "RESULT FAIL: tokioprobe did not pass under wx-relaxed" 1 ;; esac

echo "== G1 verdict arm: the rwx suite (RED in phase 1: mmap-RWX-refused, rw_to_rx mprotect, both EACCES)"
rwx=$(timeout 60 $SSH "/bin/jvmprobe rwx" 2>&1)
echo "$rwx"
case "$rwx" in *"RWX PASS rw_to_rx=42"*) ;; *) verdict "RESULT FAIL: rwx arm did not print RWX PASS rw_to_rx=42" 1 ;; esac

echo "== G1 movement arm: java -version past the blocker-1 line (expected next blocker: getcpu, G2)"
jout=$(timeout 180 $SSH "/bin/jvmprobe java /opt/jdk/bin/java" 2>&1)
echo "$jout"
case "$jout" in
    *"$BLOCKER1"*) verdict "RESULT FAIL: the workload still dies at the W^X blocker-1 line" 1 ;;
esac
case "$jout" in
    *"JVM PASS"*)
        echo "== surprise: java -version passed outright - G2's getcpu did not fire; record it, re-check REQUIREMENTS" ;;
    *"getcpu"*)
        echo "== movement: past W^X; the workload died at getcpu (the attempt's blocker 2) - G2 is the next gate" ;;
esac

verdict "RESULT PASS" 0
