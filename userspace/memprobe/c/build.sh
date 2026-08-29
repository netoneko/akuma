#!/bin/bash
# Build `mem_op_cost` and optionally push it into a running guest.
#
# The sibling of `userspace/futexprobe/c/build.sh`, and it exists for the same
# two cases a disk-populating build does not cover: pushing a rebuilt probe into
# a VM that is already running (no reboot, no disk edit), and getting the SAME
# binary onto the Linux comparison VM. Building it once here and shipping it to
# both sides is the whole point — see the header of mem_op_cost.c.
#
# Usage:
#   userspace/memprobe/c/build.sh                    # just build
#   userspace/memprobe/c/build.sh --push-akuma 2322  # + push over SSH
#   userspace/memprobe/c/build.sh --push-lima  fc    # + push to a Lima VM
set -euo pipefail
cd "$(dirname "$0")"
# Two probes, and the split is deliberate: `mem_op_cost` arms never fault or
# allocate, `mem_fault_cost` arms do nothing else. Mixing them would put the
# PMM's variance on top of a decode measurement.
PROBES="mem_op_cost mem_fault_cost"

for OUT in $PROBES; do
  aarch64-linux-musl-gcc -static -O2 -Wall -Wextra -o "$OUT" "$OUT.c"
  echo "built $PWD/$OUT ($(wc -c < "$OUT") bytes)"
done

case "${1:-}" in
  --push-akuma)
    PORT="${2:-2222}"
    # base64 over SSH: the guest has no scp, and the disk image cannot be
    # written from the host while QEMU holds it open.
    for OUT in $PROBES; do
    base64 < "$OUT" | ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
      -o LogLevel=ERROR -p "$PORT" root@localhost \
      "base64 -d > /tmp/$OUT && chmod +x /tmp/$OUT && ls -l /tmp/$OUT"
    done
    ;;
  --push-lima)
    VM="${2:-fc}"
    for OUT in $PROBES; do
    limactl shell "$VM" -- sh -c "cat > /tmp/$OUT && chmod +x /tmp/$OUT && ls -l /tmp/$OUT" < "$OUT"
    done
    ;;
esac
