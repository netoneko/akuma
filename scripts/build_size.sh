#!/usr/bin/env bash
set -e
cargo +nightly build \
    --profile size \
    --no-default-features \
    --features no-tests \
    -Z build-std=core,alloc \
    "$@"
ls -lh target/aarch64-unknown-none/size/akuma
