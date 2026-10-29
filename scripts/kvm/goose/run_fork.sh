#!/bin/sh
# run_fork.sh - the stage-8 M4 slice-3 (REAL_FORK on) suite: F1, PK1, PK2, ONE boot.
# Companion to run_pipes.sh (the P-suite, unchanged); run_gates.sh is closed and is
# not extended. Run run_fork.sh FIRST on a fresh boot (the new-path verdicts spend
# the channel's clean-connection budget, pe's boots G-I lesson), then run_pipes.sh
# for P1-P5: 3 + 5 = 8 execs, the per-boot cap.
#
# Usage (guest booted by boot_dev.sh first):  run_fork.sh
# Verdicts are the console's "[SSH] Exit status: N"; output gates ALSO compare the
# raw client bytes ($OUT/<gate>.client; read with od -c, never strings).
#
# Gates (each ONE exec; no apk, so no mirror is needed):
#   F1 sh -c 'echo x > /tmp/f; cat /tmp/f; wc -c /tmp/f'   exit 0, client bytes "x\n2 /tmp/f\n"
#   PK1 sh -c 'yes | head -n 20000 | wc -l'                exit 0, "20000\n"  (pipe park/wake:
#        yes parks on the full 64 KiB buffer; head's exit close wakes it into BrokenPipe;
#        wc parks on the empty read until the last close delivers EOF)
#   PK2 /bin/grandfork                                     exit 0, "grandfork: ALL PASS" in the
#        client bytes (baked into the image by devroot-ensure's AKUMA_DEVROOT_BAKE_FORKPROBE)
set -u
export TERM=dumb HOME=/root GIT_PAGER=cat
. /etc/akuma-dev.env

OUT=/root/dev-cycle/taoxichuan-s7
LOG=/var/log/herd/vm-dev.log
SSHPORT=2224
PACE=15
VERD="$OUT/gates-F.txt"
: > "$VERD"
emit() { echo "$*"; echo "$*" >>"$VERD"; }
[ -f "$LOG" ] || { emit "ABORT no console log $LOG - boot_dev.sh first"; exit 1; }

# This boot's slice start (the log rotates ~250 KB and keeps older boots' lines:
# fault attribution and exec slicing must start HERE, never whole-log).
before_old=$(wc -c < "$LOG.old" 2>/dev/null || echo 0)
before=$(wc -c < "$LOG")
echo "$before" > "$OUT/logmark-pf.txt"
conslice() {
  if [ "$(wc -c < "$LOG")" -lt "$before" ]; then
    { tail -c +$((before_old+1)) "$LOG.old" 2>/dev/null; cat "$LOG"; }
  else
    tail -c +$((before+1)) "$LOG" 2>/dev/null
  fi
}

# gate <name> <cmd>: one exec; G_rc 0 pass / 1 nonzero exit / 2 no exit line. Same slice
# logic as run_pipes.sh (survives a log rotation mid-exec); the client is never killed
# before the console line appears (killing it early is how the NEXT exec wedges). No
# client-side retries: a stall is EVIDENCE (the channel budget is the finding), not a
# failure to paper over.
gate() {
  name=$1; cmd=$2
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
  # Strip the client's own two additions (stderr connect line, trailing newline) and
  # compare the remainder byte-exactly; the console exit line stays the primary verdict.
  sed '$d' "$OUT/$name.client" | tail -n +2 >"$OUT/$name.payload" 2>/dev/null
  if cmp -s "$OUT/$name.want" "$OUT/$name.payload"; then emit "PASS $name"
  else emit "FAIL $name (exit 0 but client bytes differ; od -c $name.payload vs $name.want)"; fi
}

# contains <name> <substring> <cmd>: exit 0 AND substring in the client payload (PK2's
# step lines are long; the probe's own final line is the verdict, host-A/B validated).
contains() {
  name=$1; want=$2; cmd=$3
  gate "$name" "$cmd"
  if [ "$G_rc" = 2 ]; then emit "UNKNOWN $name (no console exit line)"; return; fi
  if [ "$G_rc" = 1 ]; then emit "FAIL $name (nonzero exit; see $name.console)"; return; fi
  sed '$d' "$OUT/$name.client" | tail -n +2 >"$OUT/$name.payload" 2>/dev/null
  if grep -q "$want" "$OUT/$name.payload"; then emit "PASS $name"
  else emit "FAIL $name (exit 0 but '$want' not in client bytes; od -c $name.payload)"; fi
}

verdict F1 'x\n2 /tmp/f\n'  "sh -c 'echo x > /tmp/f; cat /tmp/f; wc -c /tmp/f'"
verdict PK1 '20000\n'       "sh -c 'yes | head -n 20000 | wc -l'"
contains PK2 'grandfork: ALL PASS' "/bin/grandfork"

# Faults since THIS boot's start line only (whole-log greps cross boots: the log keeps
# older boots' lines across ~250 KB rotations).
conslice | grep -a -E 'Fault|PANIC|sig!' >"$OUT/gates-F.faults" 2>/dev/null
if [ -s "$OUT/gates-F.faults" ]; then
  emit "FAIL faults ($(wc -l < "$OUT/gates-F.faults") lines since the boot mark; see gates-F.faults)"
else
  emit "PASS no-faults (since logmark-pf.txt)"
fi
# Ticks still advancing: the last [TICK]/tick-style console activity is per-boot evidence;
# cheap proxy — the guest answered 3 execs, and F1's console carries the w4ret/tick prints.
if conslice | grep -aq 'Exit status: 0'; then
  emit "PASS console-alive (boot answered execs; see F1.console for the tick prints)"
fi
echo "---- run_fork.sh done ----"; cat "$VERD"
