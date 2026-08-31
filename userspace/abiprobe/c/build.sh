#!/bin/bash
# Build `abi_write_probe` and optionally push it into a running guest.
#
# The sibling of `userspace/memprobe/c/build.sh`, for the same two cases a
# disk-populating build does not cover: pushing a rebuilt probe into a VM that
# is already running (no reboot, no disk edit), and getting the SAME binary onto
# the Linux comparison VM. This probe is a differential one — its whole point is
# that Akuma's bytes and Linux's bytes come from one binary.
#
# Usage:
#   userspace/abiprobe/c/build.sh                    # just build
#   userspace/abiprobe/c/build.sh --push-akuma 2222  # + push over SSH
#   userspace/abiprobe/c/build.sh --push-lima  fc    # + push to a Lima VM
set -euo pipefail
cd "$(dirname "$0")"
OUT=abi_write_probe

aarch64-linux-musl-gcc -static -O2 -Wall -Wextra -o "$OUT" "$OUT.c"
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
