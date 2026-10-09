#!/bin/sh
# In-guest: kami itself (daemon + screencast client, null display) against
# URL under Firecracker, driven by kamitry.py on a pty. Pair with
# KARGS=strace_err. Leaves /tmp/dmesg.txt, /tmp/kami-input.log, /tmp/kami.log.
echo "== kami-once: $(uname -a)"
[ -f /guest.env ] && . /guest.env
mkdir -p /tmp; rm -f /tmp/kami-input.log /tmp/kami.log /tmp/kami.sock
sleep ${NETWAIT:-6}
URL=${URL:-https://www.tumblr.com/}
python3 /kamitry.py "$URL" ${DUR:-60} "${KEYS:-j,j,j,j}" 2>&1
echo "== kami-once done"
dmesg > /tmp/dmesg.txt 2>&1
sync
/bin/busybox poweroff -f
