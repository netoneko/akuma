#!/usr/bin/env bash
set -e
# Re-add every sc-* gate so the `size` profile keeps every syscall family.
# Dropped vs the default build (via --no-default-features): `neko`, tests, and
# `tls-rsa` (RSA cert verification — saves ~300 KB; ECDSA/Ed25519 HTTPS still
# work, SSH is Ed25519-only and unaffected). extreme-size omits the sc-* too.
cargo +nightly build \
    --profile size \
    --no-default-features \
    --features no-tests,sc-aio,sc-sysv-ipc,sc-framebuffer,sc-containers,sc-timerfd,sc-eventfd,sc-pidfd,sc-epoll \
    -Z build-std=core,alloc \
    "$@"
ls -lh target/aarch64-unknown-none/size/akuma
