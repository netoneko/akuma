#!/usr/bin/env bash
set -e
# extreme-size: like `size` but with the non-essential syscall families gated
# out. --no-default-features drops `neko`; `no-tests` excludes the boot test
# suite; `extreme` is the profile discriminator build.rs reads (CARGO_FEATURE_EXTREME)
# to pick a tighter IMAGE_SIZE / STACK_BOTTOM. Re-add any sc-* feature below to
# keep that family in the build (used to bisect which family tcc needs).
cargo +nightly build \
    --profile extreme-size \
    --no-default-features \
    --features no-tests,extreme \
    -Z build-std=core,alloc \
    "$@"
ls -lh target/aarch64-unknown-none/extreme-size/akuma
