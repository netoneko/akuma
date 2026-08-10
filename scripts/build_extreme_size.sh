#!/usr/bin/env bash
set -e
# extreme-size: like `size` but with the non-essential syscall families gated
# out. There is no in-kernel TLS or cryptography at all any more (purged
# 2026-08-10 with the in-kernel shell that was its only consumer), so nothing to
# drop on that front — use a userspace HTTPS tool. `no-tests` excludes the
# boot test suite; `extreme` is the profile discriminator build.rs reads
# (CARGO_FEATURE_EXTREME) to emit kernel_profile_extreme, which trims the main.rs
# heap-reserve knobs (MIN_CODE_AND_STACK / STACK_GUARD). The boot-stack
# reservation itself is now derived from the linked image size in linker.ld, not
# a per-profile constant. Re-add any sc-* feature below to keep
# that family in the build (used to bisect which family tcc needs).
#
# `smoltcp` is listed explicitly: it is now an optional feature (so the devbox
# can drop the native stack), and extreme keeps its native stack + built-in SSH
# as before. Drop `smoltcp` here to reclaim its space if extreme goes netless.
cargo +nightly build \
    --profile extreme-size \
    --no-default-features \
    --features no-tests,smoltcp,extreme \
    -Z build-std=core,alloc \
    "$@"
ls -lh target/aarch64-unknown-none/extreme-size/akuma
