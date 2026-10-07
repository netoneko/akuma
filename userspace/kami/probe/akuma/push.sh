#!/bin/sh
# Laptop side: build the static probes and copy this directory (plus the
# Dockerfile) to the trashcan's Ubuntu at /root/cdp-probe/akuma.
# HPBOX_IP defaults to 192.168.1.120. Same ssh options as hpbox.py's Ubuntu
# helper: -F /dev/null so the `akuma` alias (port 2222, the Akuma
# personality) cannot hijack the connection, and no host-key pinning because
# the two personalities answer on one IP with different keys.
set -e
cd "$(dirname "$0")"
IP=${HPBOX_IP:-192.168.1.120}
C=../../../forktest/c_stress
for p in exeprobe shmvar chromeprobe; do
  x86_64-linux-musl-gcc -O2 -static -o "$p" "$C/$p.c"
done
COPYFILE_DISABLE=1 tar --no-xattrs -cf - mkimg.sh kami-smoke.sh kami-fc.json.in run-fc.sh \
    exeprobe shmvar chromeprobe -C .. Dockerfile \
  | ssh -F /dev/null -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
      -o LogLevel=ERROR -o BatchMode=yes -p 22 "root@$IP" \
      'mkdir -p /root/cdp-probe/akuma && tar -C /root/cdp-probe/akuma -xf - && mv -f /root/cdp-probe/akuma/Dockerfile /root/cdp-probe/Dockerfile && ls /root/cdp-probe/akuma'
rm -f exeprobe shmvar chromeprobe
