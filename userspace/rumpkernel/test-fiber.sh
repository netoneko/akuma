#!/bin/bash
#
# test-fiber.sh — Rust unit tests for the cooperative fiber rumpuser backend
# (src/fiber.rs, `--features threads_fiber`).
#
# The backend's context switch is hand-rolled ELF-style aarch64 asm, so the tests
# can't link/run on the macOS host directly. We CROSS-BUILD the test binary for
# musl on the host using rust's bundled lld (the macOS system linker rejects GNU
# ld flags), then EXECUTE the static binary in a Docker linux/arm64 container.
# The scheduler uses global state on a single OS thread → --test-threads=1.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
cd "${HERE}/rumpuser"
TARGET=aarch64-unknown-linux-musl

echo "=== cross-building fiber tests (lld, --no-run, ${TARGET}) ==="
BIN=$(RUSTFLAGS="-C linker=rust-lld -C linker-flavor=ld.lld -C link-self-contained=yes" \
      cargo test --no-run --features threads_fiber --target "${TARGET}" \
        --message-format=json 2>/dev/null \
      | python3 -c '
import sys, json
exe = ""
for line in sys.stdin:
    try:
        o = json.loads(line)
    except ValueError:
        continue
    if o.get("profile", {}).get("test") and o.get("executable"):
        exe = o["executable"]
print(exe)')

[ -n "${BIN}" ] || { echo "ERROR: no test executable produced" >&2; exit 1; }
echo "test binary: ${BIN}"
REL="${BIN#"${HERE}/rumpuser/"}"

echo "=== running in docker linux/arm64 (alpine, no toolchain) ==="
exec docker run --rm --platform linux/arm64 \
    -v "${HERE}/rumpuser:/w" -w /w \
    alpine:3.20 \
    "/w/${REL}" --test-threads=1 --nocapture
