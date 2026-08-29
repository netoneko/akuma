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
OUT=mem_op_cost

aarch64-linux-musl-gcc -static -O2 -Wall -Wextra -o "$OUT" mem_op_cost.c
echo "built $PWD/$OUT ($(wc -c < "$OUT") bytes)"

case "${1:-}" in
  --push-akuma)
    PORT="${2:-2222}"
    # base64 over SSH: the guest has no scp, and the disk image cannot be
    # written from the host while QEMU holds it open.
    base64 < "$OUT" | ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
      -o LogLevel=ERROR -p "$PORT" root@localhost \
      "base64 -d > /tmp/$OUT && chmod +x /tmp/$OUT && ls -l /tmp/$OUT"
    ;;
  --push-lima)
    VM="${2:-fc}"
    limactl shell "$VM" -- sh -c "cat > /tmp/$OUT && chmod +x /tmp/$OUT && ls -l /tmp/$OUT" < "$OUT"
    ;;
esac
