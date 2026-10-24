#!/bin/sh
# The wait half of the supervisor loop, with no model in it. Kimi (goose on the Mac, session $KIMI_SID) ended
# its turn every time it was asked to "wait", so the waiting is done here: block while a metal driver
# (goose_acp_prompt.py sending a taoxichuan-* brief to the ryzen) runs, then prompt Kimi once with
# scripts/kvm/goose/kimi-stage-done.md. Kimi must launch the next stage's driver inside that turn; if no new
# driver appears within 15 minutes of its turn ending, or the turn reports a quota error, this script exits
# non-zero so a human (or the Sonnet watcher) takes over. Progress goes to stdout.
# Usage: KIMI_SID=20261024_3 scripts/kvm/goose/supervise-loop.sh [max_stages]
cd "$(dirname "$0")/../../.." || exit 1
PAT='goose_acp_prompt.py scripts/kvm/goose/taoxichuan-'
SID=${KIMI_SID:?set KIMI_SID to the supervisor session id}
MAX=${1:-11}
n=0
while [ "$n" -lt "$MAX" ]; do
    echo "$(date +%T) waiting for a metal driver"
    t=0
    until pgrep -f "$PAT" > /dev/null; do
        sleep 30; t=$((t + 30))
        if [ "$t" -ge 900 ]; then echo "$(date +%T) NO DRIVER for 15 min: Kimi did not launch the next stage"; exit 3; fi
    done
    while pgrep -f "$PAT" > /dev/null; do sleep 30; done
    echo "$(date +%T) metal driver exited; prompting Kimi (close #$((n + 1)))"
    out=$(GOOSE_ACP_KEY_FILE=${GOOSE_ACP_KEY_FILE:-$HOME/.akuma/goose/acp_secret_key} \
        python3 -I scripts/goose_acp_say.py "$SID" scripts/kvm/goose/kimi-stage-done.md "$PWD" 2>&1)
    echo "$out" | tail -n 3 | cut -c1-300
    if echo "$out" | grep -q -i -E '429|usage limit|quota|rate.?limit|"error"'; then
        echo "$(date +%T) KIMI QUOTA/ERROR: stop the loop, Sonnet takes over"; exit 4
    fi
    n=$((n + 1))
done
echo "$(date +%T) $MAX stage closes done"
