#!/bin/sh
# retry_goose2.sh PROMPT TAG MAXTRIES - rerun a goose session while its stream keeps dying.
# Successor to retry_goose.sh. Differences:
#  - the silence watchdog is not a flat 9 minutes: while the LAST tool event in the log is an unfinished
#    `tool_call` (a boot battery or kbuild is running on the box and produces no stream traffic) the limit is
#    TOOL_SILENCE (default 1800 s); otherwise MODEL_SILENCE (default 540 s).
#  - goose_acp_prompt.py writes an `alive:` heartbeat (at most every 30 s) while the model streams thinking
#    chunks, so a long GLM think is log activity and does not trip the watchdog.
#  - a try counts as died only when the driver log says "Stream decode error" or this script killed it.
cd /Users/netoneko/github.com/netoneko/akuma
PROMPT=$1; TAG=$2; MAX=${3:-6}; SP=$(dirname "$PROMPT")
MODEL_SILENCE=${MODEL_SILENCE:-540}; TOOL_SILENCE=${TOOL_SILENCE:-1800}
# The driver gives up on one turn after BUDGET seconds (its own default is 3 h, which a long session outlives).
BUDGET=${BUDGET:-43200}
n=1
while [ $n -le $MAX ]; do
  LOG=/tmp/goose-$TAG-$n.log
  P=$SP/$TAG-try$n.txt
  cp "$PROMPT" "$P"
  if [ $n -gt 1 ]; then
    printf '\nATTEMPT %s: earlier attempts died of network stream errors with no memory. FIRST read /root/dev-cycle/taoxichuan-s7/RESULT-%s.md if it exists, run git status/log in the checkout, check `herd status` and /var/run/herd/vm-dev.vm for a boot left running, and continue from there; do not redo finished work. Write RESULT-%s.md after every boot AND after every commit.\n' $n $TAG $TAG >> "$P"
  fi
  scripts/kvm/goose/ak.sh 'herd stop goose-glm; sleep 1; for i in 1 2; do for p in $(ps | grep goose | grep -v herd | grep -v grep | awk "{print \$1}"); do kill -9 $p; done; sleep 1; done; herd start goose-glm; sleep 12' >/dev/null 2>&1
  python3 scripts/goose_acp_probe.py >/dev/null 2>&1
  python3 scripts/goose_acp_prompt.py "$P" --out $LOG --name $TAG$n --budget $BUDGET >/dev/null 2>&1 &
  pid=$!
  echo "$TAG try $n started pid=$pid log=$LOG"
  killed=0
  while kill -0 $pid 2>/dev/null; do
    sleep 30
    now=$(date +%s); m=$(stat -f %m $LOG 2>/dev/null || echo $now)
    last=$(grep -a -E 'tool_call:|tool completed:|tool failed:' $LOG 2>/dev/null | tail -1)
    case "$last" in
      *"tool_call:"*) limit=$TOOL_SILENCE; why="tool running" ;;
      *) limit=$MODEL_SILENCE; why="model" ;;
    esac
    if [ $((now-m)) -gt $limit ]; then
      echo "$TAG try $n SILENT $((now-m))s (limit $limit, $why): killing"; kill $pid 2>/dev/null; killed=1; break
    fi
  done
  wait $pid 2>/dev/null
  if grep -aq "Stream decode error\|BUDGET EXHAUSTED" $LOG 2>/dev/null || [ $killed -eq 1 ]; then
    echo "$TAG try $n died; retrying"; n=$((n+1)); continue
  fi
  echo "$TAG try $n FINISHED (no stream error)"; tail -c 600 $LOG; exit 0
done
echo "$TAG gave up after $MAX tries"
