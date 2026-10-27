#!/bin/sh
# run_pipes.sh - the stage-8 M3 (pipes) suite: five gates, ONE boot, well under the host
# forward's ~30-connection ceiling. NEW suite; run_gates.sh is closed (agent log, "Stage 8,
# sessions T-Y") and is not extended. Closed stages' gates run once as a spot check before a
# merge to origin, not here.
#
# Usage (guest booted by boot_dev.sh first):  run_pipes.sh [KERNEL_ELF]
# Verdicts are the console's "[SSH] Exit status: N" (the ssh client capture loses output);
# output gates ALSO compare the raw client bytes, written to $OUT/<gate>.client. Read them with
# `od -c`, never `strings` (hides lines shorter than 4 characters).
#
# Gates (each ONE exec; no apk, so no mirror is needed):
#   P1 sh -c 'echo hi | cat'              exit 0, client bytes "hi\n"
#   P2 sh -c 'echo hi | cat | wc -c'      exit 0, client bytes "3\n"       (three members)
#   P3 sh -c 'echo a | wc -c; echo b | wc -c'   exit 0, "2\n2\n"           (pipes are reusable)
#   P4 sh -c 'echo hi | cat > /tmp/p; cat /tmp/p'  exit 0, "hi\n"          (pipe + redirect + F1 path)
#   P5 sh -c 'echo x | true; echo done'   exit 0, "done\n"                 (reader exits without reading:
#                                          the writer must not die of SIGPIPE/hang; EPIPE tolerated)
# Out of scope (needs M4's scheduler, a pump with a live parent): any pipe carrying more than the
# 64 KiB cap, `yes | head`, a pipe whose writer outlives its reader's read.
set -u
export TERM=dumb HOME=/root GIT_PAGER=cat
. /etc/akuma-dev.env

OUT=/root/dev-cycle/taoxichuan-s7
LOG=/var/log/herd/vm-dev.log
SSHPORT=2224
PACE=15
VERD="$OUT/gates-P.txt"
: > "$VERD"
emit() { echo "$*"; echo "$*" >>"$VERD"; }
[ -f "$LOG" ] || { emit "ABORT no console log $LOG - boot_dev.sh first"; exit 1; }

# gate <name> <cmd>: one exec; G_rc 0 pass / 1 nonzero exit / 2 no exit line. Same slice logic as
# run_gates.sh (survives a log rotation mid-exec); the client is never killed before the console
# line appears (killing it early is how the NEXT exec wedges).
gate() {
  name=$1; cmd=$2
  before_old=$(wc -c < "$LOG.old" 2>/dev/null || echo 0)
  before=$(wc -c < "$LOG")
  conslice() {
    if [ "$(wc -c < "$LOG")" -lt "$before" ]; then
      { tail -c +$((before_old+1)) "$LOG.old" 2>/dev/null; cat "$LOG"; }
    else
      tail -c +$((before+1)) "$LOG" 2>/dev/null
    fi
  }
  /bin/ssh -p "$SSHPORT" root@localhost "$cmd" >"$OUT/$name.client" 2>&1 </dev/null &
  spid=$!
  n=0; saw=""
  while kill -0 "$spid" 2>/dev/null && [ "$n" -lt 90 ]; do
    if conslice 2>/dev/null | grep -q 'Exit status:'; then saw=1; break; fi
    sleep 1; n=$((n+1))
  done
  if [ -n "$saw" ]; then
    g=0
    while kill -0 "$spid" 2>/dev/null && [ "$g" -lt 30 ]; do sleep 1; g=$((g+1)); done
  fi
  if kill -0 "$spid" 2>/dev/null; then
    kill "$spid" 2>/dev/null
    emit "NOTE $name: client killed by pid (console exit line seen: ${saw:-no})"
  fi
  wait "$spid" 2>/dev/null
  sleep "$PACE"
  conslice >"$OUT/$name.console" 2>/dev/null
  if grep -q 'Exit status: 0' "$OUT/$name.console"; then G_rc=0
  elif grep -q 'Exit status:' "$OUT/$name.console"; then G_rc=1
  else G_rc=2; fi
}

# verdict <name> <expected bytes as printf format> <cmd>: exit 0 AND client bytes equal.
verdict() {
  name=$1; want=$2; cmd=$3
  gate "$name" "$cmd"
  printf "$want" >"$OUT/$name.want"
  if [ "$G_rc" = 2 ]; then emit "UNKNOWN $name (no console exit line)"; return; fi
  if [ "$G_rc" = 1 ]; then emit "FAIL $name (nonzero exit; see $name.console)"; return; fi
  # The client's own bytes pollute a raw cmp: it always prints the stderr
  # connect line ("[ssh] connecting to ...") and appends one newline at EOF
  # (proof: pa-probe.client and the old green F1.client carry exactly those
  # two additions behind correct guest bytes). The verdict below strips
  # exactly those — first line and last (empty) line — and compares the
  # remainder byte-exactly against the guest's expected output. The console
  # exit line above stays the primary verdict, as in the closed run_gates.sh.
  sed '$d' "$OUT/$name.client" | tail -n +2 >"$OUT/$name.payload" 2>/dev/null
  if cmp -s "$OUT/$name.want" "$OUT/$name.payload"; then emit "PASS $name"
  else emit "FAIL $name (exit 0 but client bytes differ; od -c $name.payload vs $name.want)"; fi
}

verdict P1 'hi\n'     "sh -c 'echo hi | cat'"
verdict P2 '3\n'      "sh -c 'echo hi | cat | wc -c'"
verdict P3 '2\n2\n'   "sh -c 'echo a | wc -c; echo b | wc -c'"
verdict P4 'hi\n'     "sh -c 'echo hi | cat > /tmp/p; cat /tmp/p'"
verdict P5 'done\n'   "sh -c 'echo x | true; echo done'"

if grep -a -E 'Fault|PANIC|sig!' "$LOG" >"$OUT/gates-P.faults" 2>/dev/null && [ -s "$OUT/gates-P.faults" ]; then
  emit "FAIL faults ($(wc -l < "$OUT/gates-P.faults") lines in the console; see gates-P.faults)"
else
  emit "PASS no-faults"
fi
