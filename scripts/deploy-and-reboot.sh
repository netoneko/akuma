#!/bin/sh
# Push a freshly HOST-built kernel onto an ALREADY-RUNNING KERNEL_DROPOFF
# guest's drop-off drive, then trigger the reboot that picks it up.
#
# Pair with scripts/build_devbox_smoltcp.sh:
#   scripts/build_devbox_smoltcp.sh
#   scripts/deploy-and-reboot.sh
#
# The guest must already be booted with KERNEL_DROPOFF=1 (see
# overlays/devbox/run-smoltcp.sh) — this does not start a VM, only reloads
# the kernel of one that's already up. For the in-guest equivalent (build
# and drop off from inside the VM instead), see scripts/dropoff_kernel.sh.
#
# docs/runbooks/selfhost-kernel-build.md § "Swap the running kernel in place"
# docs/archive/RAW_BLOCK_DEVICE_FD.md
#
# Usage:
#   scripts/deploy-and-reboot.sh [elf-path]
#
# Env:
#   INSTANCE   - must match the INSTANCE the target VM was booted with
#                (default 0 -> ssh :2222). Same port math as cargo_runner.sh.
#   SSH_PORT   - overrides the derived port directly.
#   SSH_USER   - default root.
#   NO_VERIFY  - "1" skips waiting for the guest to come back over ssh.
set -euo pipefail

ELF="${1:-target/aarch64-unknown-none/release/akuma}"
if [ ! -f "$ELF" ]; then
    echo "deploy-and-reboot.sh: no ELF at $ELF -- build it first (scripts/build_devbox_smoltcp.sh)" >&2
    exit 1
fi

INSTANCE="${INSTANCE:-0}"
SSH_PORT="${SSH_PORT:-$((2222 + 100 * INSTANCE))}"
SSH_USER="${SSH_USER:-root}"
SSH_OPTS=(-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=10 -p "$SSH_PORT")

BIN="${ELF}.bin"
BIN_TMP="${BIN}.$$.tmp"

# Same tmp-then-atomic-mv shape scripts/cargo_runner.sh uses for its own
# unconditional objcopy — never leaves a half-written file on the path a
# relaunch reads, and an already-running QEMU's open fd on the old inode is
# unaffected either way (KERNEL_DROPOFF mounts the file, not the directory
# entry, so this is safe to do while the target VM is up).
echo "deploy-and-reboot.sh: rust-objcopy -O binary $ELF -> $BIN"
rust-objcopy -O binary "$ELF" "$BIN_TMP"
mv -f "$BIN_TMP" "$BIN"
echo "deploy-and-reboot.sh: wrote $(wc -c < "$BIN" | tr -d ' ') bytes to $BIN"

echo "deploy-and-reboot.sh: ssh :$SSH_PORT reboot -f"
# reboot -f drops the ssh session as the guest goes down (busybox's plain
# `reboot` fails EPERM here -- it tries to signal an init process this
# kernel doesn't have). A nonzero/broken-pipe exit from ssh here is the
# expected outcome, not a failure -- don't let it trip the script.
ssh "${SSH_OPTS[@]}" "$SSH_USER@localhost" 'reboot -f' || true

if [ "${NO_VERIFY:-0}" = "1" ]; then
    echo "deploy-and-reboot.sh: NO_VERIFY=1, not waiting for the relaunch"
    exit 0
fi

echo "deploy-and-reboot.sh: waiting for the guest to come back on :$SSH_PORT..."
for _ in $(seq 1 60); do
    if UNAME_OUT=$(ssh "${SSH_OPTS[@]}" "$SSH_USER@localhost" 'uname -a' 2>/dev/null); then
        echo "deploy-and-reboot.sh: back up -- $UNAME_OUT"
        exit 0
    fi
    sleep 2
done

echo "deploy-and-reboot.sh: guest did not come back on :$SSH_PORT within 120s" >&2
exit 1
