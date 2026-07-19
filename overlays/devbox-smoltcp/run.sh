#!/bin/bash
# Boot the Akuma devbox-smoltcp: the smoltcp (native, in-kernel) counterpart to the
# rump devbox, for A/B latency comparisons on the SAME disk image.
#
# This build is RUMP-FREE by construction: --no-default-features drops `rump` (which
# is in the default set), and no `rump`/`rump-default`/`userspace-sshd`/`rump-tests`
# feature is selected. Result: no rump_server, no /dev/net/tap0, no sysproxy, no
# RUMP_NIC — box 0 runs the native smoltcp stack directly and the built-in in-kernel
# SSH server. Verify rump-freeness: `nm target/aarch64-unknown-none/release/akuma |
# grep -c rump` == 0 (the rump devbox build has ~76).
#
# The ONLY difference from overlays/devbox is the network stack — same devbox.img,
# same userspace/tooling/paths — which is exactly what makes it a clean A/B control
# for the rump tax (see docs/reference/subsystems/rump-stack.md "Rump tax vs native
# smoltcp"). See overlays/devbox-smoltcp/README.md.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$REPO_ROOT"

export DISK="${DEVBOX_DISK:-devbox.img}"
export MEMORY="${DEVBOX_MEMORY:-4096}"
export RUMP_NIC=0   # smoltcp only: no second NIC, no /dev/net/tap0
unset SMP           # single kernel; do NOT build/run the multikernel

if [ ! -f "$DISK" ]; then
    echo "Error: $DISK not found. Build it first: overlays/devbox/bootstrap.sh"
    exit 1
fi

SSH_PORT=2222   # built-in SSH on NIC0/smoltcp (QEMU net0 hostfwd :2222 -> box :22)
echo "Booting devbox-smoltcp: DISK=$DISK MEMORY=$MEMORY (single kernel, RUMP-FREE, native smoltcp)"
echo "The built-in SSH is publickey-only — stage your key once (image is shared with devbox):"
echo "  cp ~/.ssh/id_ed25519.pub bootstrap/etc/sshd/authorized_keys && DISK=$DISK scripts/populate_disk.sh --etc-only"
echo "Then connect with:"
echo "  ssh -o StrictHostKeyChecking=no -i ~/.ssh/id_ed25519 -p $SSH_PORT root@localhost"
echo

# Rump-free feature set = the devbox set MINUS `devbox` (=rump-default+userspace-sshd)
# and `rump-tests`, PLUS `smoltcp`. `--no-default-features` drops rump entirely; the
# built-in in-kernel SSH is present because `userspace-sshd` is NOT selected. This
# lands at ~2.2 MB, under the release profile's 3 MB size guard (no override needed).
SMOLTCP_FEATURES="smoltcp,neko,sound,no-tests,sc-aio,sc-sysv-ipc,sc-framebuffer,sc-containers,sc-timerfd,sc-eventfd,sc-pidfd,sc-epoll"
exec cargo run --release --no-default-features --features "$SMOLTCP_FEATURES"
