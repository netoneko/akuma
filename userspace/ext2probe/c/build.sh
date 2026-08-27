#!/bin/bash
# Build `read_syscall_cost` and optionally push it into a running guest.
#
# `userspace/build.sh` already builds this and drops it in `bootstrap/bin/`, so
# a freshly populated disk has it at `/bin/read_syscall_cost`. This script is
# for the two cases that does not cover: pushing a rebuilt probe into a VM that
# is already running (no reboot, no disk edit), and getting the SAME binary onto
# the Linux comparison VM.
#
# That "same binary" part is the whole point of the probe — see the header of
# read_syscall_cost.c. Building it once here and shipping it to both sides is
# what makes the Akuma and Linux columns differ by the kernel and nothing else;
# building it separately in each guest would put a different libc's `read`
# wrapper in front of each `svc`, which is worth ~1.5 us on the Linux side.
#
# Usage:
#   userspace/ext2probe/c/build.sh                    # just build
#   userspace/ext2probe/c/build.sh --push-akuma 2322  # + scp-less push over SSH
#   userspace/ext2probe/c/build.sh --push-lima  fc    # + push to a Lima VM
set -euo pipefail
cd "$(dirname "$0")"
OUT=read_syscall_cost

aarch64-linux-musl-gcc -static -O2 -Wall -Wextra -o "$OUT" read_syscall_cost.c
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
