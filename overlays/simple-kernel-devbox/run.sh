#!/bin/bash
# Boot the simple-kernel-devbox: the kernel built from THIS checkout with the
# devbox-smoltcp feature set (userspace sshd via herd + real shared-kernel SMP,
# native smoltcp stack — no rump), mounting the minimal rootfs built by
# overlays/simple-kernel-devbox/bootstrap.sh (sshd, busybox, nightly rust+cargo).
#
# Companion of bootstrap/public/kernel-bootstrap-with-local-ai/index.md.
#
# QEMU is invoked by cargo via scripts/cargo_runner.sh (the [target.
# aarch64-unknown-none] runner in .cargo/config.toml), which:
#   - forwards host :2222 -> guest :22 (INSTANCE>0 shifts ports by 100*N),
#   - attaches DISK as the first virtio-blk drive (busybox mount format=raw),
#   - on Apple Silicon uses HVF; on an x86_64 host it emulates (TCG, slow).
#
# Env knobs:
#   SIMPLE_KERNEL_DISK    image to boot (default: simple-kernel-devbox.img)
#   MEMORY                guest RAM (default 4096; in-guest rustc builds want this)
#   SMP                   cores (default 2; real shared-kernel SMP)
#   INSTANCE              0..98 — shifts host ports (ssh 2222+100*N) and
#                         snapshots the disk so parallel boots can't corrupt it
#   GDB / GDB_WAIT        expose a gdbstub on :1234 (+INSTANCE), optionally halted
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$REPO_ROOT"

export DISK="${SIMPLE_KERNEL_DISK:-$REPO_ROOT/simple-kernel-devbox.img}"
export MEMORY="${MEMORY:-4096}"
export SMP="${SMP:-2}"
# No RUMP_NIC: box 0 is on the native smoltcp stack.

if [ ! -f "$DISK" ]; then
    echo "Error: $DISK not found. Build it first: overlays/simple-kernel-devbox/bootstrap.sh"
    exit 1
fi

INSTANCE="${INSTANCE:-0}"
SSH_PORT=$((2222 + 100 * INSTANCE))

echo "Booting simple-kernel-devbox: DISK=$DISK MEMORY=$MEMORY SMP=$SMP INSTANCE=$INSTANCE"
echo "Once herd starts sshd ('[SSHD] Listening on ...' in the log), connect with:"
echo "  ssh -o StrictHostKeyChecking=no -p $SSH_PORT root@localhost"
echo

# MUST stay in sync with scripts/build_devbox_smoltcp.sh (this cargo run will
# otherwise silently rebuild a kernel without the feature set that script built).
DEVBOX_SMOLTCP_FEATURES="devbox-smoltcp,no-tests"
exec cargo run --release --features "$DEVBOX_SMOLTCP_FEATURES"
