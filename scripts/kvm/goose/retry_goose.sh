#!/bin/sh
# retry_goose.sh PROMPT TAG MAXTRIES - rerun a goose session while its stream keeps dying
# (a driver exit whose log ends in "Stream decode error", or 9 min of log silence).
cd /Users/netoneko/github.com/netoneko/akuma
PROMPT=$1; TAG=$2; MAX=${3:-6}; SP=$(dirname "$PROMPT")
n=1
while [ $n -le $MAX ]; do
  LOG=/tmp/goose-$TAG-$n.log
  P=$SP/$TAG-try$n.txt
  cp "$PROMPT" "$P"
  if [ $n -gt 1 ]; then
    printf '\nATTEMPT %s: earlier attempts died of network stream errors with no memory. FIRST read /root/dev-cycle/taoxichuan-s7/RESULT-%s.md if it exists, run git status/log in the checkout, and continue from there; do not redo finished work. Write RESULT-%s.md after every boot AND after every commit.\n' $n $TAG $TAG >> "$P"
  fi
  scripts/kvm/goose/ak.sh 'herd stop goose-glm; sleep 1; for i in 1 2; do for p in $(ps | grep goose | grep -v herd | grep -v grep | awk "{print \$1}"); do kill -9 $p; done; sleep 1; done; herd start goose-glm; sleep 12' >/dev/null 2>&1
  python3 scripts/goose_acp_probe.py >/dev/null 2>&1
  python3 scripts/goose_acp_prompt.py "$P" --out $LOG --name $TAG$n >/dev/null 2>&1 &
  pid=$!
  echo "$TAG try $n started pid=$pid log=$LOG"
  while kill -0 $pid 2>/dev/null; do
    sleep 30
    now=$(date +%s); m=$(stat -f %m $LOG 2>/dev/null || echo $now)
    if [ $((now-m)) -gt 540 ]; then echo "$TAG try $n SILENT 9min: killing"; kill $pid 2>/dev/null; break; fi
  done
  wait $pid 2>/dev/null
  if grep -aq "Stream decode error" $LOG 2>/dev/null || [ $((now-m)) -gt 540 ]; then
    echo "$TAG try $n died of stream error; retrying"; n=$((n+1)); continue
  fi
  echo "$TAG try $n FINISHED (no stream error)"; tail -c 600 $LOG; exit 0
done
echo "$TAG gave up after $MAX tries"
