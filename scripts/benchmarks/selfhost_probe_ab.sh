#!/bin/sh
# A/B the selfhost_repro probes between the aarch64 and amd64 guests.
#
# Runs jobserver_stress (rustc's jobserver shape: MPMC Mutex+Condvar over
# futexes) with identical knobs in each guest and prints one timing line per
# phase. The point is a same-binary, same-knobs comparison of the kernel's
# futex/sched/mmap paths — the primitives behind a self-host cargo build.
#
#   scripts/benchmarks/selfhost_probe_ab.sh aarch64   # local devbox, :2222
#   scripts/benchmarks/selfhost_probe_ab.sh amd64     # FC guest via HP box
#
# Binaries: userspace/forktest/c_stress/jobserver_stress.{aarch64,x86_64}
# (build: rustc --target <triple>-unknown-linux-musl -C linker=<triple>-linux-musl-gcc
#  -O [-C relocation-model=static for x86_64, else it is static-pie and the
#  amd64 loader SIGSEGVs on it] userspace/forktest/selfhost_repro/jobserver_stress.rs -o ...)
#
# Knobs stay under the aarch64 devbox's thread-spawn cap (~1000 spawns in one
# process die EAGAIN and exhaust fork for ~45 s — see
# docs/archive/AKUMA_AMD64_SELFHOST_BUILD_SLOWNESS.md), hence JS_SPAWN_ITERS.
#
# Quoting note: the amd64 path is a *nested* ssh (laptop -> HP box -> FC
# guest). ssh joins all trailing arguments with spaces and the shell layers
# re-split them, so any multi-word guest command must travel as a FILE: stage
# it with `cat > file` (one quoted word per hop), then `sh file`. Never pass
# a multi-line script as remote-command text through both hops.
set -e
HERE=$(dirname "$0")
ARCH="${1:?aarch64|amd64}"
[ "$ARCH" = amd64 ] && TRIPLE=x86_64 || TRIPLE=$ARCH
BIN="$HERE/../../userspace/forktest/c_stress/jobserver_stress.$TRIPLE"
[ -f "$BIN" ] || { echo "missing $BIN (see header)"; exit 1; }

# One ssh hop, taking the guest command as a single argument.
case "$ARCH" in
  aarch64)
    G() { ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
              -o LogLevel=ERROR -p 2222 root@localhost "$@"; }
    ;;
  amd64)
    KEY=/root/akuma/target/x86_64-unknown-none/release/amd64-ssh-test-key
    G() { ssh -F /dev/null -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
              -o LogLevel=ERROR -p 22 root@192.168.1.123 \
              "ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -i $KEY -p 2222 root@10.0.2.15 '$*'"; }
    ;;
esac

echo "== staging probe script ($ARCH) =="
cat > /tmp/probe_ab_guest.sh <<'GUEST'
export JS_SPAWN_ITERS=150 JS_TIMEOUT_SECS=600
t0=$(date +%s); /tmp/js > /tmp/js.out 2>&1; t1=$(date +%s)
echo ALL_WALL=$((t1-t0))s
grep -c " ok" /tmp/js.out | xargs echo phases_ok
export JS_PHASE=condvar JS_REQUESTS=5000000
t0=$(date +%s); /tmp/js > /dev/null 2>&1; t1=$(date +%s)
echo CONDVAR_5M_WALL=$((t1-t0))s
export JS_PHASE=park JS_PARK_ITERS=3000000
t0=$(date +%s); /tmp/js > /dev/null 2>&1; t1=$(date +%s)
echo PARK_3M_WALL=$((t1-t0))s
GUEST
cat /tmp/probe_ab_guest.sh | G 'cat > /_probe_ab.sh'

echo "== pushing probe ($ARCH) =="
# The amd64 rootfs has busybox but no `base64` applet link; the devbox has both.
base64 < "$BIN" | G 'busybox base64 -d > /tmp/js 2>/dev/null || base64 -d > /tmp/js'
G 'chmod +x /tmp/js; [ -s /tmp/js ]' || { echo "push failed"; exit 1; }

echo "== phase timings ($ARCH) =="
G 'sh /_probe_ab.sh'

echo "== reference (aarch64 devbox, kernel 0.0.8, 4 cores HVF) =="
echo "ALL_WALL=1s phases_ok=4 CONDVAR_5M_WALL=4s PARK_3M_WALL=1s"
