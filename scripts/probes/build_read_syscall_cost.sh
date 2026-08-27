#!/bin/bash
# Build `read_syscall_cost` for BOTH kernels from one source.
#
# The point of the probe is that the Akuma number and the Linux number differ by
# the kernel and nothing else, so both halves are static musl aarch64 binaries
# built here — not one built here and one built inside the Linux VM with its own
# libc, which would put a different `read` wrapper in front of each `svc`.
#
# Usage:
#   scripts/probes/build_read_syscall_cost.sh              # -> both binaries
#   scripts/probes/build_read_syscall_cost.sh --push-akuma 2322
#   scripts/probes/build_read_syscall_cost.sh --push-lima  fc
set -euo pipefail
cd "$(dirname "$0")"
OUT=read_syscall_cost

aarch64-linux-musl-gcc -static -O2 -Wall -Wextra -o "$OUT" read_syscall_cost.c
echo "built $PWD/$OUT ($(wc -c < "$OUT") bytes)"

case "${1:-}" in
  --push-akuma)
    PORT="${2:-2322}"
    # base64 over SSH: no scp in the guest, and the disk image is not writable
    # from the host while QEMU has it open.
    base64 < "$OUT" | ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
      -o LogLevel=ERROR -p "$PORT" root@localhost \
      "base64 -d > /tmp/$OUT && chmod +x /tmp/$OUT && ls -l /tmp/$OUT"
    ;;
  --push-lima)
    VM="${2:-fc}"
    limactl shell "$VM" -- sh -c "cat > /tmp/$OUT && chmod +x /tmp/$OUT && ls -l /tmp/$OUT" < "$OUT"
    ;;
esac
