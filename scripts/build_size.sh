#!/usr/bin/env bash
set -e
# Re-add every sc-* gate so the `size` profile is functionally identical to
# before the gates landed (only `neko` and tests are dropped via
# --no-default-features + no-tests). extreme-size is the build that omits them.
cargo +nightly build \
    --profile size \
    --no-default-features \
    --features no-tests,sc-aio,sc-sysv-ipc,sc-framebuffer,sc-containers,sc-timerfd,sc-eventfd,sc-pidfd,sc-epoll \
    -Z build-std=core,alloc \
    "$@"
ls -lh target/aarch64-unknown-none/size/akuma
