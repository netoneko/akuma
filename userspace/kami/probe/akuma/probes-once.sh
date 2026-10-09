#!/bin/sh
# In-guest: the wake-latency and timer-precision probes (userspace/forktest/
# c_stress/wakelat.c, timerlat.c) on Akuma under Firecracker. Run on Linux
# first for the expectation (the same static binaries run on the host).
echo "== probes-once: $(uname -a)"
/wakelat.bin 200 3000
/timerlat.bin 100
echo "== probes-once done"
dmesg > /tmp/dmesg.txt 2>&1; sync
/bin/busybox poweroff -f
