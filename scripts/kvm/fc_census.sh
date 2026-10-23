#!/bin/sh
# One census iteration: rebuild the kernel, boot the phase-0 rig with the /dev/kvm stub, print the
# [kvmapi] lines and firecracker's own error. Usage: scripts/kvm/fc_census.sh [LOG]
set -eu
cd "$(dirname "$0")/../.."
D=${CENSUS_DIR:?set CENSUS_DIR to the dir holding fc/ gcc/ musl/ (extracted apks)}
LOG=${1:-$D/census.log}
cargo build -p akuma-amd64 --target x86_64-unknown-none --release --features no-tests 2>&1 | grep -E "^error|^  -->" || true
rm -f "$LOG"
P0="echo P0-BEGIN
for i in ${REPEATS:-1}; do /usr/bin/firecracker ${FCARGS:---no-api --no-seccomp --config-file /vm.json}; echo \"run \$i rc=\$?\"; done
echo P0-END" FCDIR=$D/fc GCCDIR=$D/gcc MUSLDIR=$D/musl KCMDLINE="kvmctl kvmmem=${KVMMEM:-1024} kvmapi ${EXTRA:-}" scripts/kvm/firecracker_phase0.sh "$LOG" "${SECS:-90}" >/dev/null 2>&1 &
timeout "${SECS:-90}" sh -c "until grep -aq 'P0-END' $LOG 2>/dev/null; do sleep 2; done" || true
grep -a "kvmapi\|rror\|panic" "$LOG" | cut -c1-300
