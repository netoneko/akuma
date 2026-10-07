#!/bin/sh
# One Chromium-on-Akuma run under Firecracker, on the trashcan's Ubuntu side.
# Run from this directory there (push.sh puts it at /root/cdp-probe/akuma):
#   sh run-fc.sh [script] [extra files...]
#
# Copies kami-root.img to kami-run.img (the original is never booted),
# writes the smoke script, exeprobe and any extra files into the copy's root,
# boots the kernel `hpbox.build()` produced, prints the interesting log
# lines, and dumps any /tmp/shot*.png out of the image into ./out/.
#
# The boot prints "self-test: N passed, 41 FAILED" on this image, which is
# expected: the kernel's fs/fd self-tests look for fixtures (/probe.txt, …)
# that only the standard amd64-root.img carries. Compare the count across
# runs, not against zero.
#
# Env: KERNEL (default the box's release build), MEM (MiB, default 10240 —
# a >= 8 GiB guest gets the 1 GiB kernel heap, which Chromium's whole-image
# execve needed before the streaming loader), VCPUS (2), TIMEOUT (400),
# KARGS (extra kernel command-line flags, e.g. KARGS=strace_err: one `[sc!]`
# line per failing syscall with pid, paths and errno — see usermode.rs).
set -e
cd "$(dirname "$0")"
SCRIPT=${1:-kami-smoke.sh}
[ $# -gt 0 ] && shift
KERNEL=${KERNEL:-/root/akuma/target/x86_64-unknown-none/release/akuma-amd64}
MEM=${MEM:-10240}
VCPUS=${VCPUS:-2}
TIMEOUT=${TIMEOUT:-400}
KARGS=${KARGS:-}
IMG=$PWD/kami-run.img
[ -f kami-root.img ] || { echo "no kami-root.img: run mkimg.sh first" >&2; exit 1; }
cp kami-root.img "$IMG"
for f in "$SCRIPT" exeprobe "$@"; do
  [ -f "$f" ] || continue
  n=$(basename "$f")
  debugfs -w -R "rm $n" "$IMG" >/dev/null 2>&1 || true
  debugfs -w -R "write $f $n" "$IMG" >/dev/null 2>&1
  debugfs -w -R "sif $n mode 0100755" "$IMG" >/dev/null 2>&1
done
sed -e "s#@KERNEL@#$KERNEL#" -e "s#@IMG@#$IMG#" -e "s#@SCRIPT@#$(basename "$SCRIPT")#" \
    -e "s#@VCPUS@#$VCPUS#" -e "s#@KARGS@#$KARGS#" -e "s#@MEM@#$MEM#" kami-fc.json.in > kami-fc.json
for p in $(pgrep -x firecracker); do kill -9 "$p"; done
rm -f /tmp/kami-fc.sock
timeout "$TIMEOUT" firecracker --no-api --config-file kami-fc.json --api-sock /tmp/kami-fc.sock > kami-fc.log 2>&1 || true
grep -a -E 'self-test|== |FATAL|ERROR|Fault\]|no row for|ALLOC FAIL|Segmentation|exeprobe|written' kami-fc.log \
  | grep -v '\[OK\]' | cut -c1-220 | tail -60
mkdir -p out
# `debugfs` exits 0 even for a missing file, so judge by what was dumped.
rm -f out/shot*.png out/dmesg.txt
debugfs -R "dump /tmp/dmesg.txt out/dmesg.txt" "$IMG" >/dev/null 2>&1 || true
[ -s out/dmesg.txt ] && echo "== got out/dmesg.txt ($(wc -l < out/dmesg.txt) lines)"
for shot in shot.png shot--no-zygote.png; do
  debugfs -R "dump /tmp/$shot out/$shot" "$IMG" >/dev/null 2>&1 || true
  if [ -s "out/$shot" ]; then echo "== got out/$shot"; else rm -f "out/$shot"; fi
done
echo "== full log: $PWD/kami-fc.log"
