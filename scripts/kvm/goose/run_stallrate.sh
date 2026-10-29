#!/bin/sh
# run_stallrate.sh N [apk] - the stage-8 pl stall-rate harness.
#
# N boots; per boot the 3 cheapest gates (F1, PK1, P1 — no apk, no mirror) with
# `run_stallrate.sh N`, or the heavy probe (ONE `apk add git` per boot; mirror.sh
# start/stop around the run) with `run_stallrate.sh N apk`.
#
# Classification is PER GATE WINDOW: the logmark is taken at that gate's own
# start, never at the boot's (a whole-boot window sees gate 1's "Exit status"
# line and misreads every later gate — pj's premise 4 note). CLEAN gate = this
# window's "[SSH] Exit status: 0" within 90 s. STALL = no exit line in this
# window in 90 s (the client is never killed before the console line appears —
# killing early is how the NEXT exec wedges; after the STALL verdict the client
# is killed because the boot's channel is done as evidence). The boot is
# classified at its FIRST stall. Every stall's console window is saved to
# $OUT/STALLrate-boot<K>-gate<G>.console. Prints one line:
#   STALLRATE clean=<C>/total=<T>   (plus a per-boot line)
set -u
export TERM=dumb HOME=/root GIT_PAGER=cat
. /etc/akuma-dev.env

REPO=/src/github.com/netoneko/akuma
OUT=/root/dev-cycle/taoxichuan-s7
LOG=/var/log/herd/vm-dev.log
SSHPORT=2224
PACE=15
N=${1:-4}
MODE=${2:-gates}

[ -f "$LOG" ] || { echo "ABORT no console log $LOG"; exit 1; }

conslice() { # <old_bytes> <mark> - this window's console text, rotation-safe
  if [ "$(wc -c < "$LOG")" -lt "$2" ]; then
    { tail -c +$(( $1 + 1 )) "$LOG.old" 2>/dev/null; cat "$LOG"; }
  else
    tail -c +$(( $2 + 1 )) "$LOG" 2>/dev/null
  fi
}

if [ "$MODE" = apk ]; then
  sh "$REPO/scripts/kvm/goose/mirror.sh" start >/dev/null 2>&1
  trap 'sh "$REPO/scripts/kvm/goose/mirror.sh" stop >/dev/null 2>&1' INT TERM EXIT
fi

clean=0; total=0; k=0
while [ "$k" -lt "$N" ]; do
  k=$((k+1))
  echo "== boot $k/$N"
  sh "$REPO/scripts/kvm/goose/boot_dev.sh" >"$OUT/stallrate-boot$k.boot" 2>&1
  if ! grep -q '^BOOT OK' "$OUT/stallrate-boot$k.boot"; then
    echo "boot $k: BOOTFAIL ($(tail -1 "$OUT/stallrate-boot$k.boot"))"; total=$((total+1)); continue
  fi
  if [ "$MODE" = apk ]; then
    set -- "apk:apk add git"
  else
    set -- "F1:echo x > /tmp/f; cat /tmp/f; wc -c /tmp/f" \
           "PK1:yes | head -n 20000 | wc -l" \
           "P1:echo hi | cat"
  fi
  verdict=""
  for spec in "$@"; do
    g=${spec%%:*}; cmd=${spec#*:}
    g_old=$(wc -c < "$LOG.old" 2>/dev/null || echo 0)
    g_mark=$(wc -c < "$LOG")
    /bin/ssh -p "$SSHPORT" root@localhost "$cmd" >"$OUT/$g-stallrate$k.client" 2>&1 </dev/null &
    spid=$!
    n=0; saw=""
    # Wait for the console line, not for the client: the guest prints the exit
    # line a beat before the client finishes draining (run_fork's lesson).
    while [ "$n" -lt 90 ]; do
      if conslice "$g_old" "$g_mark" 2>/dev/null | grep -q 'Exit status:'; then saw=1; break; fi
      sleep 1; n=$((n+1))
    done
    if [ -n "$saw" ]; then
      g2=0
      while kill -0 "$spid" 2>/dev/null && [ "$g2" -lt 30 ]; do sleep 1; g2=$((g2+1)); done
    fi
    win=$(conslice "$g_old" "$g_mark" 2>/dev/null)
    if [ -n "$saw" ] && echo "$win" | grep -q 'Exit status: 0'; then
      echo "  $g CLEAN"
    else
      echo "$win" >"$OUT/STALLrate-boot$k-gate$g.console"
      echo "  $g STALL (last console lines: $(echo "$win" | grep -E 'SSH|TICK' | tail -3 | tr '\n' '|'))"
      verdict="STALL@$g"
    fi
    if kill -0 "$spid" 2>/dev/null; then kill "$spid" 2>/dev/null; fi
    wait "$spid" 2>/dev/null
    [ -n "$verdict" ] && break
    sleep "$PACE"
  done
  total=$((total+1))
  if [ -z "$verdict" ]; then clean=$((clean+1)); echo "boot $k: CLEAN"
  else echo "boot $k: $verdict"; fi
done

echo "STALLRATE clean=$clean/total=$total mode=$MODE boots=$N"
exit 0
