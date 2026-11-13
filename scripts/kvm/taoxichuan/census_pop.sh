#!/bin/bash
# census_pop.sh - B10's syscall census: unmodified tokio programs under `strace` on Linux (Pop on the ryzen).
#
# Run as root on Pop from /root/census, which must hold `tokioprobe`, `nettest-reqwest` and `goose` (static musl;
# goose from the guest image), `goose.env` and `home/.config/goose/{config,secrets}.yaml`, and /root/kot/bin/kot.
# Writes out/<run>.txt (`strace -f -c`) for every run, and out/<run>.full.gz (`strace -f`, every call with its pid)
# for the two runs that spawn a program, so `census_classify.py` can drop what the spawned program called.
# kot runs with a throwaway database, a dev seed and no peers, so it touches nothing of the real node.
set -u
cd /root/census
mkdir -p out
summary() { name=$1; shift; timeout 600 strace -f -qq -c -o "out/$name.txt" "$@" > "out/$name.stdout" 2>&1; echo "$name rc=$?"; }
full() { name=$1; shift; timeout 600 strace -f -qq -s 0 -o "out/$name.full" "$@" > /dev/null 2>&1; gzip -f "out/$name.full"; }

summary tokioprobe ./tokioprobe
full tokioprobe ./tokioprobe
summary reqwest-get ./nettest-reqwest get https://akuma.sh/
summary reqwest-post ./nettest-reqwest post https://httpbin.org/post 64

rm -rf kotrun && mkdir -p kotrun
( cd kotrun && MIOT_NAME=census MIOT_SEED=7 MIOT_ROSTER=census=7 MIOT_PORT=18080 MIOT_DB=/root/census/kotrun/census.db \
  MIOT_PEERS= timeout 25 strace -f -qq -c -o ../out/kot.txt /root/kot/bin/kot run > ../out/kot.stdout 2>&1 ) &
sleep 10
curl -sk -m 5 -o /dev/null https://127.0.0.1:18080/ # accept + a TLS handshake (refused: the mesh wants client certs)
wait
echo "kot: $(grep -c . out/kot.txt) lines"

set -a; . ./goose.env; set +a
export HOME=/root/census/home
summary goose-pong ./goose run --no-session -t 'Reply with exactly the word PONG and nothing else. Do not use tools.'
TOOL='Use your shell tool to run the command `echo census-ok` and reply with exactly its output.'
summary goose-tool ./goose run --no-session -t "$TOOL"
full goose-tool ./goose run --no-session -t "$TOOL"
grep -c census-ok out/goose-tool.stdout
