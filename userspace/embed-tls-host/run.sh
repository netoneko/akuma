#!/bin/bash
# Build and run embed-tls-host on THIS machine.
#
# The wrapper exists for one reason: cargo walks UP from the crate directory
# collecting config, and `userspace/.cargo/config.toml` pins
# `aarch64-unknown-none` (the no_std kernel). Without an explicit `--target`
# this host program is cross-compiled for a bare-metal target and dies in
# `getrandom` with "can't find crate for `std`".
#
# `--target` from `rustc -vV` rather than a triple in a `.cargo/config.toml`:
# the triple would be this laptop's, and the whole value of a host control arm
# is that it runs wherever the developer is.
#
#   ./run.sh                      # the three default hosts
#   ./run.sh api.z.ai             # one host
#   RUST_LOG=trace ./run.sh api.z.ai
set -euo pipefail
HOST="$(rustc -vV | grep '^host:' | cut -d' ' -f2)"
cd "$(dirname "${BASH_SOURCE[0]}")"
cargo run --release --target "$HOST" --quiet -- "$@"
