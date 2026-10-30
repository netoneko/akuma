#!/bin/sh
# ak.sh '<cmd>' - run a command on the ryzen box (Akuma, ssh :2222). The box's shell prints
# "unknown terminal type" without a TERM, so a command is prefixed with TERM=dumb GIT_PAGER=cat.
if [ $# -gt 0 ]; then
  set -- "export TERM=dumb GIT_PAGER=cat; $*"
fi
exec ssh -o BatchMode=yes -o ConnectTimeout=10 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -p 2222 root@192.168.1.159 "$@"
