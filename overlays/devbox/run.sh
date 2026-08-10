#!/bin/bash
# Boot the Akuma devbox: single kernel (no SMP), rump networking as the only stack.
# RUMP_NIC=1 adds /dev/net/tap0 (virtio-mmio-bus.4) and forwards host :2223 -> box :22.
# Built with the `devbox` profile + `devbox` feature (scripts/build_devbox.sh):
# rump is the DEFAULT stack for box 0 (rump-default) and there is no built-in SSH
# (userspace-sshd) — the kernel brings up box 0's rump_server at boot and the only
# sshd is the userspace /bin/sshd (herd), running unboxed on that rump stack.
# See overlays/devbox/README.md.
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
echo "Booting devbox: DISK=$DISK MEMORY=$MEMORY RUMP_NIC=1 (single kernel, profile=devbox)"
echo "Once you see the rump DHCP lease + userspace sshd listening, connect with:"
echo "  ssh -o StrictHostKeyChecking=no -p $SSH_PORT root@localhost"
echo

# Same feature set as scripts/build_devbox.sh: --no-default-features drops smoltcp
# (and the smoltcp-coupled built-in SSH) entirely; rump is the only stack.
DEVBOX_FEATURES="devbox,neko,sound,no-tests,rump-tests,sc-aio,sc-sysv-ipc,sc-framebuffer,sc-containers,sc-timerfd,sc-eventfd,sc-pidfd,sc-epoll"
exec cargo run --release --no-default-features --features "$DEVBOX_FEATURES"
