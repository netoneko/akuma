#!/usr/bin/env bash
set -e
# Build the devbox kernel: the `devbox` profile (inherits release codegen) plus a
# feature set that makes the NetBSD rump stack the DEFAULT network stack for box 0
# (`rump-default`) and drops the built-in smoltcp SSH server (`userspace-sshd`).
#
# `--no-default-features` compiles the native smoltcp stack (and the smoltcp-coupled
# built-in SSH) OUT entirely — rump is the only networking. We re-add the non-smoltcp
# defaults the image wants (mirrors scripts/build_size.sh's explicit style): `sound`
# (virtio-sound for wavplay), `neko` (in-kernel editor), all `sc-*` syscall families,
# and `no-tests` (this is a runtime target; the smoltcp-coupled boot suites are off).
# Omitted vs. the default set: `smoltcp`, `kernel-tls`, `tls-rsa` (no in-kernel HTTPS
# — use a userspace tool). Boot with RUMP_NIC=1 (overlays/devbox/run.sh does that).
#
# Extra args are forwarded (e.g. `scripts/build_devbox.sh --quiet`).
DEVBOX_FEATURES="devbox,neko,sound,no-tests,sc-aio,sc-sysv-ipc,sc-framebuffer,sc-containers,sc-timerfd,sc-eventfd,sc-pidfd,sc-epoll"
cargo build \
    --profile devbox \
    --no-default-features \
    --features "$DEVBOX_FEATURES" \
    "$@"
ls -lh target/aarch64-unknown-none/devbox/akuma
