#!/bin/sh
# amd64_metal_j4_loop.sh FIRST LAST [jobs] — clean self-host kernel builds, ON the bare-metal
# trashcan, inside Akuma. Run it there as `sh` (execve does not understand `#!`), detached:
#
#   ssh akuma 'setsid nohup sh /root/j4loop.sh 1 10 > /root/j4runs/loop.log 2>&1 < /dev/null &'
#
# Each run wipes the WHOLE /root/ktarget (host units too — `kbuild -c` alone keeps 16 of the 95
# crates) then `kbuild -c -j $jobs`. Results: /root/j4runs/N/{build.out,signals,summary}.
# The console ring washes in ~3.5 min, so a sampler greps dmesg every 15 s while the build runs and
# `signals` is the de-duplicated union — the end-of-run dmesg alone would miss a mid-build fault.
# Not the ryzen rig (scripts/benchmarks/ryzen_fc/): that one boots a Firecracker guest per run.
. /etc/akuma-dev.env
D=/root/j4runs; J=${3:-4}
mkdir -p $D
PAT='Fault\]|SIGSEGV|#GP|#PF|#SS|#UD|BKL\] stuck|TLB\] stuck|TRAMP|MM\] |PANIC|panic|killed by|OOM|out of memory|xhci|usb|transfer|EIO|I/O err'
for n in $(seq $1 $2); do
  o=$D/$n; rm -rf $o; mkdir -p $o
  rm -rf /root/ktarget
  t0=$(cut -d. -f1 /proc/uptime)
  ( while :; do dmesg 2>/dev/null | grep -a -E "$PAT" >> $o/signals.raw; sleep 15; done ) &
  sp=$!
  kbuild -c -j $J > $o/build.out 2>&1; rc=$?
  kill $sp 2>/dev/null
  dmesg 2>/dev/null | grep -a -E "$PAT" >> $o/signals.raw
  sort -u $o/signals.raw > $o/signals 2>/dev/null; rm -f $o/signals.raw
  el=$(( $(cut -d. -f1 /proc/uptime) - t0 ))
  v=$([ $rc = 0 ] && echo PASS || echo FAIL)
  {
    echo "run=$n jobs=$J rc=$rc verdict=$v elapsed=${el}s uptime=$(cut -d. -f1 /proc/uptime)s"
    echo "compiling=$(grep -c ' Compiling ' $o/build.out) sig11=$(grep -c -E 'signal: 11|SIGSEGV' $o/build.out) fault_lines=$(grep -a -c 'Fault\]' $o/signals) bkl_stuck=$(grep -a -c 'BKL\] stuck' $o/signals) tlb_stuck=$(grep -a -c 'TLB\] stuck' $o/signals)"
    echo "--- build tail"; tail -8 $o/build.out
    echo "--- signals (first 20, BKL stuck folded out)"; grep -a -v 'BKL\] stuck' $o/signals | head -20
  } > $o/summary
  echo "$n $v rc=$rc ${el}s" >> $D/results.txt
done
echo done > $D/batch-$1-$2.done
