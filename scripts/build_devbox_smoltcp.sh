#!/bin/sh
set -e
# Build the devbox-smoltcp kernel: the DEFAULT devbox going forward.
#
# The INVERSE of scripts/build_devbox.sh (which is rump-only). This image keeps the
# NATIVE smoltcp stack for box 0, DROPS the built-in in-kernel (smoltcp) SSH server
# (`userspace-sshd` → the only sshd is the userspace /bin/sshd from herd, which routes
# over smoltcp like any other process), and compiles in real shared-kernel SMP
# (`smp-shared`). rump_server work is DEFERRED — the rump path stays in-tree/buildable
# via scripts/build_devbox.sh, but this smoltcp image is the recommended devbox.
#
# Unlike build_devbox.sh we do NOT pass --no-default-features: smoltcp must stay IN
# (the userspace sshd routes over it). The devbox is the default set plus `no-tests`
# (a runtime target; skip the boot self-test suite), built with plain `--release`.
# There is no devbox feature of its own: `devbox-smoltcp` was an empty alias and was
# removed 2026-11-11.
#
# Run with overlays/devbox/run-smoltcp.sh (SMP=N, no RUMP_NIC, host :2222 -> :22).
# Extra args are forwarded (e.g. scripts/build_devbox_smoltcp.sh --quiet).
#
# The kernel has no sshd-specific feature: the listener backlog is 8 on every
# build. How sshd serves sessions is sshd's own build/config
# (SSHD_FORK_SESSIONS=0 userspace/build.sh --sshd-only for the cooperative one;
# docs/reference/build-profiles.md § Listener backlog).
DEVBOX_SMOLTCP_FEATURES="no-tests"
cargo build \
    --release \
    --features "$DEVBOX_SMOLTCP_FEATURES" \
    "$@"
ls -lh target/aarch64-unknown-none/release/akuma
