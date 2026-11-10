#!/bin/sh
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
# can drop the native stack). Drop it here to reclaim its space if extreme goes
# netless.
#
# `userspace-sshd` is the DEFAULT for extreme since 2026-08-10: it turns herd off
# (config::AUTO_START_HERD) and has the kernel spawn /bin/sshd directly with
# /bin/paws as the login shell (config::AUTO_START_SSHD / USERSPACE_SSHD_SHELL).
# That is what makes 4.0 MB work: herd + its service tree costs ~1.4 MB, and
# busybox as the login shell is 265 mapped pages against a 128-page dedup cache
# at 4 MB, which collapses text sharing and kills the box on fork.
# Measured at MEMORY=4096K: 1804 KB free idle, acceptance/08 passes.
cargo +nightly build \
    --profile extreme-size \
    --no-default-features \
    --features no-tests,smoltcp,extreme,userspace-sshd \
    -Z build-std=core,alloc \
    "$@"
ls -lh target/aarch64-unknown-none/extreme-size/akuma
