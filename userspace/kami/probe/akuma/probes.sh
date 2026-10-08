#!/bin/sh
# In-guest probe pass (run-fc.sh probes.sh <probes...>): the c_stress probes
# beside chromeprobe.c, then the kernel heap (`Slab:` in /proc/meminfo)
# sampled while Chromium's 250 MB binary is exec'd. With the streaming loader
# the peak should not move by the binary's size.
echo "== probes: $(uname -a)"
for p in bpprobe trapprobe spawnprobe singletonprobe snapprobe taskprobe credprobe capprobe fallocprobe jitprobe thrprobe exeprobe chromeprobe; do
  [ -x /$p ] || continue
  echo "== $p"
  /$p 2>&1 | tail -20
done
[ -x /spawnprobe ] && /spawnprobe /usr/lib/chromium/chrome_crashpad_handler --help 2>&1 | grep spawnprobe
slab() { grep Slab: /proc/meminfo | tr -s ' ' | cut -d' ' -f2; }
before=$(slab)
peak=$before
( while [ ! -f /tmp/stop ]; do s=$(slab); echo "$s" >> /tmp/slab.samples; done ) &
for i in 1 2 3; do /usr/lib/chromium/chromium --version; done
touch /tmp/stop; wait
for s in $(cat /tmp/slab.samples); do [ "$s" -gt "$peak" ] && peak=$s; done
echo "== slab kB: before=$before peak-during-execs=$peak after=$(slab) samples=$(wc -l < /tmp/slab.samples)"
echo "== probes done"
