#!/bin/bash
# bis_run.sh COMMIT INIT : boot kb-COMMIT at smp=1, run the cargo step ~100 s, report whether cargo progressed
P="$(cd "$(dirname "$0")" && pwd)"
cd /tmp && python3 -I $P/exp_cargo.py $P/kb-$1 1 5 $2 > /tmp/bis-$1.out 2>&1
python3 -I $P/rzput.py -c 'strings /root/tx-run5/console.log > /root/c5.txt; echo RESULT '"$1"' rustc-execves: $(grep -ac "execve.*rustc" /root/c5.txt) polls: $(grep -ac "poll#" /root/c5.txt) lines: $(wc -l < /root/c5.txt)'
