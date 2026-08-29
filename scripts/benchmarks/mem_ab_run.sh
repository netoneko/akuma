#!/bin/bash
# One A/B/A arm for the memory family: build the tree as it stands, boot it, gate
# on the memory correctness probe, then take timing rounds. Written as a script
# because an A/B arm that is typed by hand twice is two different experiments.
#
#   scripts/benchmarks/mem_ab_run.sh <label> <outdir> [smp] [rounds]
#
# The arms cannot be interleaved (each needs a reboot), and this host's syscall
# floor drifts between boots, so the protocol is A/B/A: run this for the changed
# tree, then for the stashed baseline, then for the changed tree again. A real
# code effect reproduces in both A runs; boot drift does not.
#
# The aggregator is `futex_op_ab.py`, unchanged and on purpose: it is
# arm-agnostic (it builds its table from whatever arm names the probe prints),
# and `mem_op_cost` emits the same line format for exactly this reason. A
# second copy of its 146 lines under an epoll name would be a second thing to
# keep right — including the ratio-not-ns rule, which is the part everyone gets
# wrong first.
set -euo pipefail
LABEL="$1"; OUT="$2"; SMP="${3:-4}"; ROUNDS="${4:-12}"
PORT=2322
cd "$(dirname "$0")/../.."

echo "[arm $LABEL] building"
cargo build --release 2>&1 | tail -1
cp target/aarch64-unknown-none/release/akuma "$OUT/akuma.$LABEL"

# Kill only this instance, matched on its own forward — other VMs on this host
# belong to someone else (CLAUDE.md § VM Access).
pkill -f 'hostfwd=tcp::2322-:22' 2>/dev/null || true
sleep 2

echo "[arm $LABEL] booting SMP=$SMP"
INSTANCE=1 MEMORY=2048 SMP="$SMP" nohup scripts/cargo_runner.sh \
    target/aarch64-unknown-none/release/akuma > "$OUT/boot.$LABEL.log" 2>&1 &
python3 scripts/vm_ready.py "$PORT"

echo "[arm $LABEL] correctness probe"
scripts/mem_suite.py --port "$PORT" > "$OUT/suite.$LABEL.log" 2>&1 && SUITE=PASS || SUITE=FAIL
grep -E '^=====' "$OUT/suite.$LABEL.log" || true
echo "[arm $LABEL] suite: $SUITE"

echo "[arm $LABEL] timing"
userspace/memprobe/c/build.sh > /dev/null
python3 - "$PORT" <<'PY'
import base64, subprocess, sys
# Both probes: `mem_op_cost` (no arm faults or allocates) and `mem_fault_cost`
# (every arm does). They answer different questions and an arm run against the
# wrong build is worse than no number.
for probe in ("mem_op_cost", "mem_fault_cost"):
    b = open(f"userspace/memprobe/c/{probe}", "rb").read()
    subprocess.run(["ssh", "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null",
                    "-o", "LogLevel=ERROR", "-p", sys.argv[1], "root@localhost",
                    f"base64 -d > /tmp/{probe} && chmod +x /tmp/{probe}"],
                   input=base64.b64encode(b), capture_output=True)
PY
scripts/benchmarks/futex_op_ab.py --port "$PORT" --exe /tmp/mem_op_cost \
    --rounds "$ROUNDS" --label "$LABEL" --save "$OUT/mem.$LABEL.json"

# The fault/allocation half. `futex_op_ab.py` cannot drive this one — its arms
# report per-unit BRACKETS (a subtraction of two page counts), not one line per
# call — so it is run directly and its output kept for a side-by-side diff.
# Three repeats, because a fork-heavy arm is noisier than a decode arm.
echo "[arm $LABEL] fault-path timing"
for i in 1 2 3; do
  ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR \
      -p "$PORT" root@localhost "/tmp/mem_fault_cost 20 2>&1"
done > "$OUT/memfault.$LABEL.txt"
grep -E "per_|eager_extra" "$OUT/memfault.$LABEL.txt" || true
echo "[arm $LABEL] done (suite: $SUITE)"
