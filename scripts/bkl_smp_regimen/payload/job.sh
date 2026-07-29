#!/bin/sh
# SMP=N BKL stress + attribution regimen, run INSIDE the VM (fork-frugal edition).
#
# Each phase drives a different BKL consumer:
#   net4   4 concurrent 32 MiB downloads   -> net syscalls + ext2 WRITE (128 MiB)
#   read4  4 concurrent sha256sum          -> ext2 READ, working set >> block cache
#   cp2    2 concurrent cp + verify        -> ext2 read+write in one process
#   rm     remove the files                -> mutating fs syscalls (Phase 2c motivation)
#
# TWO Akuma bugs shape this script; do not "simplify" them back out
# (docs/archive/BKL_VFS_CARVE_OUT.md §11.3, §11.4):
#  1. The shell's `wait` builtin never returns (the kernel delivers no SIGCHLD),
#     so parallel phases join by polling for per-worker sentinel files.
#  2. Thread slots are reclaimed only tens of seconds after a process exits, so
#     under load each fork can stall for MINUTES and eventually fail with
#     "can't fork: Out of memory" while GBs of RAM are free. Every avoidable
#     fork is therefore removed: no `$(date)`, no per-file `wc -c` loop, no
#     command substitution. Results go to files, read out afterwards over ssh.
#
# There is deliberately NO fork-storm phase: it measures bug 2, not the BKL.
set -u
REF=275a7c3bc65538d242c1ceaa5cf74be059c63a1ed733ba62d3e92627c31f604d  # p32.bin
URL=http://10.0.2.2:8899/p32.bin
D=/tmp/bkl

join() { # join <n-workers> <max-polls>  — each poll costs one `sleep 20` fork
    n=$1; limit=$2; polls=0
    while [ $polls -lt $limit ]; do
        left=0
        i=0
        while [ $i -lt $n ]; do
            [ -f $D/w$i.done ] || left=$((left+1))
            i=$((i+1))
        done
        if [ $left -eq 0 ]; then echo "joined $n after ${polls} polls"; return 0; fi
        sleep 20; polls=$((polls+1))
    done
    echo "JOIN TIMEOUT ($left still running)"
    return 1
}

rm -rf $D
mkdir -p $D
echo "=== REGIMEN START"

echo "=== PHASE net4"
for i in 0 1 2 3; do
    ( curl -s -o $D/d$i.bin $URL; echo $? > $D/d$i.rc; echo done > $D/w$i.done ) &
done
join 4 60
ls -l $D > $D/sizes.txt

echo "=== PHASE read4"
rm -f $D/w0.done $D/w1.done $D/w2.done $D/w3.done
for i in 0 1 2 3; do
    ( sha256sum $D/d$i.bin > $D/d$i.sha; echo done > $D/w$i.done ) &
done
join 4 60
cat $D/d0.sha $D/d1.sha $D/d2.sha $D/d3.sha > $D/digests.txt
echo "reference $REF" >> $D/digests.txt

echo "=== PHASE cp2"
rm -f $D/w0.done $D/w1.done
for i in 0 1; do
    ( cp $D/d$i.bin $D/c$i.bin; sha256sum $D/c$i.bin > $D/c$i.sha; echo done > $D/w$i.done ) &
done
join 2 60
cat $D/c0.sha $D/c1.sha >> $D/digests.txt

echo "=== PHASE rm"
rm -f $D/d0.bin $D/d1.bin $D/d2.bin $D/d3.bin $D/c0.bin $D/c1.bin
echo "=== REGIMEN DONE"
echo done > /tmp/bkl.done
