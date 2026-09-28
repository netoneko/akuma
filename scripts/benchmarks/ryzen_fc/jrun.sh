#!/bin/bash
# One clean -j4 self-host build in a freshly booted 4-vCPU guest. usage: jrun.sh N [jobs] [stall_s] [budget_s]
W=/home/netoneko/akuma-selfhost; N=$1; J=${2:-4}; STALL=${3:-300}; BUDGET=${4:-2400}
OUT=$W/runs/$N; mkdir -p $OUT
$W/fcrun.sh ${VCPU:-4} ${MEM:-4096} > $OUT/boot.txt 2>&1
for i in $(seq 1 40); do $W/gssh.sh 'echo up' >/dev/null 2>&1 && break; sleep 3; done
$W/gssh.sh 'echo up' >/dev/null 2>&1 || { echo "NOBOOT" > $OUT/summary; exit 2; }
t0=$(date +%s)
# CLEAN_ALL=1 wipes the whole target dir (host units too); plain kbuild -c leaves target/release
PRE=""; [ -n "$CLEAN_ALL" ] && PRE="rm -rf /root/ktarget; "
( $W/gssh.sh "${PRE}kbuild -c -j $J" > $OUT/build.out 2>&1; echo "RC=$?" >> $OUT/build.out ) &
last=$(date +%s); lastsz=0; verdict=""
while :; do
  sleep 10; now=$(date +%s); el=$((now-t0))
  grep -q '^RC=' $OUT/build.out 2>/dev/null && break
  sz=$(stat -c %s $OUT/build.out 2>/dev/null || echo 0); csz=$(stat -c %s $W/fc.log)
  key="$sz-$csz"; [ "$key" != "$lastsz" ] && { lastsz=$key; last=$now; }
  [ $((now-last)) -ge $STALL ] && { verdict=WEDGE; break; }
  [ $el -ge $BUDGET ] && { verdict=TIMEOUT; break; }
done
el=$(( $(date +%s) - t0 ))
if [ -n "$verdict" ]; then
  $W/gssh.sh 'ps' > $OUT/ps-at-wedge.txt 2>&1
  top -b -n1 -p $(pgrep -f 'firecracker.*selfhost-vm.json' | head -1) | tail -2 > $OUT/host-cpu.txt
else
  rc=$(grep '^RC=' $OUT/build.out | tail -1 | cut -d= -f2); verdict=$([ "$rc" = 0 ] && echo PASS || echo FAIL)
fi
cp $W/fc.log $OUT/console.log
{ echo "run=$N jobs=$J vcpu=${VCPU:-4} mem=${MEM:-4096} clean_all=${CLEAN_ALL:-0} verdict=$verdict elapsed=${el}s crates=$(grep -c ' Compiling ' $OUT/build.out) kernel=$(md5sum $W/kernel.bin | cut -c1-12)"
  echo "counts: bkl_stuck=$(grep -a -c 'BKL\] stuck' $OUT/console.log) tlb_stuck=$(grep -a -c 'TLB\] stuck' $OUT/console.log) fault_lines=$(grep -a -c 'Fault\]' $OUT/console.log) mm_race=$(grep -a -c 'MM\] fault race' $OUT/console.log) tramp=$(grep -a -c TRAMP-MISMATCH $OUT/console.log) sig11=$(grep -a -c -E 'signal: 11|SIGSEGV' $OUT/build.out)"
  echo "--- build tail"; tail -12 $OUT/build.out
  echo "--- console signals"; grep -a -E "Fault\]|SIGSEGV|#GP|#PF|#SS|#UD|BKL\] stuck|TLB\] stuck|TRAMP|MM\] |FILL-SHORT|ISIG-MISS|SWITCH BADFRAME|PANIC|panic|killed by|PIPES\]" $OUT/console.log | tr -d '\000' | grep -v -E 'BKL\] stuck' | head -40
} > $OUT/summary
echo "DONE $verdict" >> $OUT/summary
