#!/usr/bin/env bash
set -e
# extreme-size: like `size` but with the non-essential syscall families gated
# out. --no-default-features drops `neko` and `tls-rsa` (RSA cert verification,
# ~300 KB — ECDSA/Ed25519 HTTPS still work, SSH is Ed25519-only); `no-tests`
# excludes the boot test suite; `extreme` is the profile discriminator build.rs
# reads (CARGO_FEATURE_EXTREME) to emit kernel_profile_extreme, which trims the
# main.rs heap-reserve knobs (MIN_CODE_AND_STACK / STACK_GUARD). The boot-stack
# reservation itself is now derived from the linked image size in linker.ld, not
# a per-profile constant. Re-add any sc-* feature below to keep that family in the
# build (used to bisect which family tcc needs).
cargo +nightly build \
    --profile extreme-size \
    --no-default-features \
    --features no-tests,extreme \
    -Z build-std=core,alloc \
    "$@"
ls -lh target/aarch64-unknown-none/extreme-size/akuma
