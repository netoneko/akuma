#!/usr/bin/env bash
set -e
# Build the devbox kernel: the `devbox` profile (inherits release codegen, so it
# carries `rump` + `sound`) plus the `devbox` FEATURE set, which makes the NetBSD
# rump stack the DEFAULT network stack for box 0 (`rump-default`) and drops the
# built-in smoltcp SSH server (`userspace-sshd`). Boot it with RUMP_NIC=1 (there
# is no rump stack without /dev/net/tap0) — overlays/devbox/run.sh does that.
#
# Phase 1 (now): layered on top of the default feature set, so smoltcp is still
# compiled in — box 0 simply never routes to it. Phase 2 will add
# `--no-default-features` here to compile smoltcp (and the smoltcp-coupled
# built-in SSH) out entirely.
#
# Extra args are forwarded (e.g. `scripts/build_devbox.sh --quiet`).
cargo build \
    --profile devbox \
    --features devbox \
    "$@"
ls -lh target/aarch64-unknown-none/devbox/akuma
