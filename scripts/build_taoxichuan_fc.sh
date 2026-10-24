#!/bin/sh
set -e
# Taoxichuan (the minimal self-hosting kernel), host track: aarch64 for Firecracker.
# Stage 1 = the `taoxichuan` feature set + platform-firecracker. Flattens the ELF to the
# Image Firecracker loads (overlays/devbox-firecracker/build.sh explains why).
# Output: $OUT (default taoxichuan-fc.bin). Boot: FC_KERNEL=$OUT overlays/devbox-firecracker/run.sh --no-net
# docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md
cd "$(dirname "$0")/.."
OUT=${OUT:-taoxichuan-fc.bin}
ELF=target/aarch64-unknown-none/extreme-size/akuma
cargo +nightly build --profile extreme-size --no-default-features \
    --features taoxichuan,platform-firecracker -Z build-std=core,alloc "$@"
rust-objcopy -O binary "$ELF" "$OUT"
ls -l "$OUT"
