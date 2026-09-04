#!/bin/bash
# Boot the amd64 target as a "devbox": the ext2 image `amd64/mkdisk.sh` builds
# (paws, httpd, sshd, and a stock static musl busybox) under QEMU `microvm`.
#
# This is a thin wrapper over `amd64/run.sh` — it exists so the amd64 bring-up
# target has a home next to `overlays/devbox/` (the aarch64 Alpine distro), not
# because it needs its own rootfs yet. When busybox `sh` runs interactively
# (Stage T: `fork`/`fstatat`), this is where its rootfs assembly will live.
#
#   overlays/devbox-amd64/run.sh                 # sshd, paws shell, wifi-reachable via hostfwd
#   INIT=/bin/busybox INITARGS=uname,-a  ...     # run a busybox applet directly
#   SSHD_SHELL=/bin/sh                  ...       # sshd starts busybox instead of paws
#
# The image carries busybox with `sh`/`uname`/`ls`/`cat`/… hard-linked to it, so
# `busybox <applet>` and (for applets that need no `fork`) `sh -c "<applet>"`
# both work. See docs/archive/AKUMA_FIRECRACKER_AMD64.md §3.25.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$REPO_ROOT"

export INIT="${INIT:-/bin/sshd}"
export SSH_PORT="${SSH_PORT:-2223}"

echo "Booting amd64 devbox: INIT=$INIT  (ssh on host :$SSH_PORT once sshd listens)"
echo "  ssh -i target/x86_64-unknown-none/release/amd64-ssh-test-key \\"
echo "      -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \\"
echo "      -p $SSH_PORT root@localhost"
echo

exec "$REPO_ROOT/amd64/run.sh" "$@"
