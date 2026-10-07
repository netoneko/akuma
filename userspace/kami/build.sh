#!/bin/sh
# Cross-build kami as a static x86_64 musl binary (same toolchain as
# userspace/rio/build.sh: rustup target x86_64-unknown-linux-musl plus the
# musl-cross linker). Output: target/x86_64-unknown-linux-musl/release/kami
set -e
cd "$(dirname "$0")"
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=x86_64-linux-musl-gcc
cargo build --release --target x86_64-unknown-linux-musl "$@"
ls -l target/x86_64-unknown-linux-musl/release/kami
