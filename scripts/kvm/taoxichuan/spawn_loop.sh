#!/bin/sh
# spawn_loop.sh [N] - on the metal host: N ssh round trips into vm-dev (port 2224) each running `sh -c "echo hi"`;
# prints how many answered `hi`. A spawn that fails shows as the guest sshd's "failed to spawn" console line.
N=${1:-15}; ok=0; i=0
while [ $i -lt $N ]; do
  o=$(/bin/ssh -p 2224 root@localhost 'sh -c "echo hi"' 2>/dev/null)
  r=0; [ "$o" = hi ] && { ok=$((ok+1)); r=1; }
  printf '%s' "$r"
  i=$((i+1)); sleep 1
done
echo; echo "spawn_loop: $ok/$N"
tr -d '\000-\010' < /var/log/herd/vm-dev.log | grep -a -c 'failed to spawn'
