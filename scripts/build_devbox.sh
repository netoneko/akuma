#!/bin/sh
set -e
# Build the devbox kernel: plain `--release` codegen plus a
# feature set that makes the NetBSD rump stack the DEFAULT network stack for box 0
# (`rump-default`). The only sshd is the userspace /bin/sshd that herd starts.
#
# `--no-default-features` compiles the native smoltcp stack OUT entirely — rump is the only networking. We re-add the non-smoltcp
# defaults the image wants (mirrors scripts/build_size.sh's explicit style): `sound`
# (virtio-sound for wavplay), all `sc-*` syscall families,
# and `no-tests` (this is a runtime target; the smoltcp-coupled boot suites are off).
# Omitted vs. the default set: `smoltcp` (rump is the only stack). Boot with RUMP_NIC=1 (overlays/devbox/run.sh does that).
#
# Extra args are forwarded (e.g. `scripts/build_devbox.sh --quiet`).
DEVBOX_FEATURES="rump-default,sc-reboot,sound,no-tests,linux-extra-syscalls"
cargo build \
    --release \
    --no-default-features \
    --features "$DEVBOX_FEATURES" \
    "$@"
ls -lh target/aarch64-unknown-none/release/akuma
