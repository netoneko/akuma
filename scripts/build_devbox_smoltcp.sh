#!/usr/bin/env bash
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
# (the userspace sshd routes over it). We layer the
# `devbox-smoltcp` meta-feature (userspace-sshd + smp-shared) plus `no-tests` (this is
# a runtime target; skip the boot self-test suite) on top of the default set. Built
# with plain `--release`: the `smp-shared` FEATURE gates the code, and the profile
# that used to pair with it added no codegen of its own (removed 2026-08-10).
#
# Run with overlays/devbox/run-smoltcp.sh (SMP=N, no RUMP_NIC, host :2222 -> :22).
# Extra args are forwarded (e.g. scripts/build_devbox_smoltcp.sh --quiet).
DEVBOX_SMOLTCP_FEATURES="devbox-smoltcp,no-tests"
cargo build \
    --release \
    --features "$DEVBOX_SMOLTCP_FEATURES" \
    "$@"
ls -lh target/aarch64-unknown-none/release/akuma
