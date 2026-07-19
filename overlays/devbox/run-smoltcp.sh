#!/bin/bash
# Boot the Akuma devbox-smoltcp: the DEFAULT devbox. Native smoltcp networking (box 0),
# built-in in-kernel SSH dropped (userspace-sshd → the only sshd is the userspace
# /bin/sshd from herd, over smoltcp), and REAL shared-kernel SMP (SMP=N, one shared
# kernel across cores). The inverse of run.sh (rump-only, single kernel).
# rump_server work is deferred; use run.sh for the rump path.
#
# Built with the `release-smp-shared` profile + `devbox-smoltcp` feature
# (scripts/build_devbox_smoltcp.sh). QEMU forwards host :2222 -> box :22 (the smoltcp
# runner default). See overlays/devbox/README.md.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$REPO_ROOT"

export DISK="${DEVBOX_DISK:-devbox.img}"
export MEMORY="${DEVBOX_MEMORY:-4096}"
export SMP="${SMP:-2}"     # real shared-kernel SMP: N cores share one kernel
# No RUMP_NIC: box 0 is on the native smoltcp stack, not rump.

if [ ! -f "$DISK" ]; then
    echo "Error: $DISK not found. Build it first: overlays/devbox/bootstrap.sh"
    exit 1
fi

SSH_PORT="${DEVBOX_SSH_PORT:-2222}"
echo "Booting devbox-smoltcp: DISK=$DISK MEMORY=$MEMORY SMP=$SMP (shared kernel, smoltcp stack)"
echo "Once you see the userspace sshd listening, connect with:"
echo "  ssh -o StrictHostKeyChecking=no -p $SSH_PORT root@localhost"
echo

# Keep the DEFAULT feature set (smoltcp/kernel-tls stay in); layer the devbox-smoltcp
# meta-feature (userspace-sshd + smp-shared) + no-tests. Matches build_devbox_smoltcp.sh.
DEVBOX_SMOLTCP_FEATURES="devbox-smoltcp,no-tests"
exec cargo run --profile release-smp-shared --features "$DEVBOX_SMOLTCP_FEATURES"
