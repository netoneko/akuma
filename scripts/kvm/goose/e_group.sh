#!/bin/sh
# e_group.sh - session O's E-group probes, one exec per connection, 15 s pace.
# Reuses run_gates.sh's console-slice rule (rotation-safe delta) and verdict
# rule (console 'Exit status: 0' is the only truth; client capture lies).
# Usage: e_group.sh <name=cmd> ...   (from the repo scripts dir)
set -u
export TERM=dumb HOME=/root GIT_PAGER=cat
. /etc/akuma-dev.env
REPO=/src/github.com/netoneko/akuma
OUT=/root/dev-cycle/taoxichuan-s7
LOG=/var/log/herd/vm-dev.log
SSHPORT=2224
PACE=15
run_ssh() { /bin/ssh -p "$SSHPORT" root@localhost "$@"; }

for pair in "$@"; do
  name=${pair%%=*}
  cmd=${pair#*=}
  before_old=$(wc -c < "$LOG.old" 2>/dev/null || echo 0)
  before=$(wc -c < "$LOG")
  run_ssh "$cmd" >"$OUT/$name.client" 2>&1 </dev/null
  sleep "$PACE"
  if [ "$(wc -c < "$LOG")" -lt "$before" ]; then
    { tail -c +$((before_old+1)) "$LOG.old" 2>/dev/null; cat "$LOG"; } \
      >"$OUT/$name.console" 2>/dev/null
  else
    tail -c +$((before+1)) "$LOG" >"$OUT/$name.console" 2>/dev/null
  fi
  if grep -q 'Exit status: 0' "$OUT/$name.console"; then v=PASS
  elif grep -q 'Exit status:' "$OUT/$name.console"; then v=FAIL
  else v=UNKNOWN; fi
  echo "$v $name"
done
