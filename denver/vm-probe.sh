#!/bin/sh
# denver/vm-probe.sh - boot the Denver guest under Akuma KVM, probe it over ssh, shut it down.
#
#   sh denver/vm-probe.sh [spec]        # run on the ryzen box, booted with `kvmctl kvmmem=4096`
#
# Prints the probe's own output and ends with a `RESULT PASS` or `RESULT FAIL: <why>` verdict
# line (tokioprobe prints its own `RESULT PASS` before it), the marker the autoresearch driver
# greps for in denver/gates/<gate>.log. Capture with:
#
#   sh denver/vm-probe.sh 2>&1 | tee denver/gates/<gate>.log
#
# Why this exists: the host runs ONE VM at a time (docs/reference/subsystems/kvm.md), and a
# previous run's guest, an old `vm = true` herd service, or a `vm-run` whose caller was killed
# leaves the slot taken. `herd vm-run` then answers `the kernel refused the spec (errno 16)`
# (EBUSY) and a session reads that as "KVM is broken". So this script frees the slot first
# (`stop` on /dev/kvm0, which is a no-op when nothing runs), boots, waits for an ssh round
# trip (never a log marker: docs/runbooks, "Waiting for a VM"), probes, and always stops the
# guest again so the next run starts clean.
#
# The key is a raw seed for Akuma's own ssh client (`ssh -y -i`), not an OpenSSH key; the
# guest listens on the host's loopback at the spec's `forward=` port.
#
# Not a `#!` script on the box: Akuma's execve cannot run one. Invoke it as `sh <file>`.

W=${DENVER_W:-/root/denver/w}
SPEC=${1:-$W/vm.spec}
PORT=${DENVER_PORT:-2247}
WAIT=${DENVER_BOOT_WAIT_TRIES:-30}     # x 5 s
SSH="ssh -y -i $W/key -p $PORT root@127.0.0.1"
VMLOG=$W/vmrun.log
VP=

verdict() { echo "$1"; stop_vm; exit "$2"; }
stop_vm() {
    echo stop > /dev/kvm0 2>/dev/null
    [ -n "$VP" ] && kill "$VP" 2>/dev/null
    VP=
}

[ -r "$SPEC" ] || verdict "RESULT FAIL: no spec at $SPEC" 1
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

echo "== uname"
echo "$out" | grep Linux
echo "== tokioprobe"
probe=$($SSH "/bin/tokioprobe" 2>&1)
echo "$probe"

case "$out" in *denver*) ;; *) verdict "RESULT FAIL: uname does not say denver" 1 ;; esac
case "$probe" in *"RESULT PASS"*) verdict "RESULT PASS" 0 ;; esac
verdict "RESULT FAIL: tokioprobe did not pass" 1
