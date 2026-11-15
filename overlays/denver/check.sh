#!/bin/sh
# Boot Denver under QEMU, wait for the Clojure server to answer, print the verdict, stop QEMU.
#
#   overlays/denver/check.sh        env: WAIT (seconds to wait, default 600), HTTP_PORT (default 8080)
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
PORT=${HTTP_PORT:-8080}
LOG=$HERE/out/check-${ARCH:-amd64}.log
ARCH=${ARCH:-amd64} "$HERE/run.sh" > "$LOG" 2>&1 < /dev/null &
QP=$!
trap 'kill $QP 2>/dev/null || true' EXIT
t=0
while [ "$t" -lt "${WAIT:-600}" ]; do
    body=$(curl -fsS --max-time 5 "http://localhost:$PORT/" 2>/dev/null || true)
    case "$body" in *"Hello from Clojure"*) echo "$body"; echo "RESULT PASS"; exit 0 ;; esac
    kill -0 $QP 2>/dev/null || { echo "QEMU exited early; console tail:"; tail -c 800 "$LOG"; echo "RESULT FAIL"; exit 1; }
    sleep 5; t=$((t + 5))
done
echo "console tail:"; tail -c 800 "$LOG"; echo "RESULT FAIL: no answer in ${WAIT:-600}s"; exit 1
