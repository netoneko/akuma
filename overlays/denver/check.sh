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
if [ "${ARCH:-amd64}" = aarch64 ]; then
    # The JVM does not complete on aarch64 yet (README), so the verdict is the boot: Denver reaches
    # its init and starts it.
    t=0
    while [ "$t" -lt "${WAIT:-120}" ]; do
        if grep -aq "init spawn -- init=" "$LOG"; then
            grep -a "Denver 0.1.0\|init spawn" "$LOG" | tr -c '[:print:]\n' '?'; echo "RESULT BOOT-PASS (aarch64: kernel boots and starts init; the JVM is amd64-only for now)"; exit 0
        fi
        sleep 2; t=$((t + 2))
    done
    echo "RESULT FAIL: aarch64 kernel never reached init"; tail -c 600 "$LOG"; exit 1
fi
t=0
while [ "$t" -lt "${WAIT:-600}" ]; do
    body=$(curl -fsS --max-time 5 "http://localhost:$PORT/" 2>/dev/null || true)
    case "$body" in *"Hello from Clojure"*) echo "$body"; echo "RESULT PASS"; exit 0 ;; esac
    kill -0 $QP 2>/dev/null || { echo "QEMU exited early; console tail:"; tail -c 800 "$LOG"; echo "RESULT FAIL"; exit 1; }
    sleep 5; t=$((t + 5))
done
echo "console tail:"; tail -c 800 "$LOG"; echo "RESULT FAIL: no answer in ${WAIT:-600}s"; exit 1
