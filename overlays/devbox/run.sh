#!/bin/bash
# Boot the Akuma devbox: single kernel (no SMP), rump networking as the only stack.
# RUMP_NIC=1 adds /dev/net/tap0 (virtio-mmio-bus.4) and forwards host :2223 -> box :22.
# The release profile compiles in rump + sound. See overlays/devbox/README.md.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$REPO_ROOT"

export DISK="${DEVBOX_DISK:-devbox.img}"
export MEMORY="${DEVBOX_MEMORY:-4096}"
export RUMP_NIC=1
unset SMP   # single kernel; do NOT build/run the multikernel

if [ ! -f "$DISK" ]; then
    echo "Error: $DISK not found. Build it first: overlays/devbox/bootstrap.sh"
    exit 1
fi

SSH_PORT="${RUMP_SSH_PORT:-2223}"
echo "Booting devbox: DISK=$DISK MEMORY=$MEMORY RUMP_NIC=1 (single kernel)"
echo "Once you see the rump DHCP lease + '[SSH Server] Listening', connect with:"
echo "  ssh -o StrictHostKeyChecking=no -p $SSH_PORT root@localhost"
echo

exec cargo run --release
