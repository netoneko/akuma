#!/usr/bin/env bash
# Build the Akuma kernel for Firecracker and flatten it to a bootable Image.
#
# Firecracker's loader wants a raw binary carrying an ARM64 Image header, not the
# ELF `cargo build` emits, so the objcopy step is mandatory rather than cosmetic.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$REPO_ROOT"

OUT="${FC_KERNEL:-$REPO_ROOT/akuma-fc.bin}"
ELF="target/aarch64-unknown-none/release/akuma"

say() { printf '\033[1;36m[fc-build]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[fc-build] %s\033[0m\n' "$*" >&2; exit 1; }

say "building --release --features platform-firecracker"
cargo build --release --features platform-firecracker

# The load address is the single most likely thing to be wrong, and it fails as a
# silent hang rather than an error, so check it here rather than at boot.
#
# Firecracker loads at get_kernel_start() = SYSTEM_MEM_START + SYSTEM_MEM_SIZE =
# 0x8020_0000, then adds the Image header's text_offset (1 MiB) — so the kernel
# must be linked at exactly 0x8030_0000. A `_boot` at 0x4010_0000 means the QEMU
# target got built by mistake.
BOOT_ADDR="$(nm "$ELF" | awk '$3=="_boot"{print $1}')"
[ "$BOOT_ADDR" = "0000000080300000" ] \
  || die "_boot is at 0x$BOOT_ADDR, expected 0x80300000 (did the platform-firecracker feature apply?)"
say "_boot at 0x$BOOT_ADDR (correct)"

say "objcopy -> $OUT"
rust-objcopy -O binary "$ELF" "$OUT"

python3 - "$OUT" <<'EOF'
import struct, sys
raw = open(sys.argv[1], 'rb').read()
to, isz = struct.unpack_from('<QQ', raw, 8)
magic = struct.unpack_from('<I', raw, 56)[0]
assert magic == 0x644d5241, f'missing ARM64 Image magic (got {magic:#x})'
# image_size == 0 makes linux-loader assume text_offset = 0x80000 instead of
# reading the header's value, which would load the kernel 512 KiB too low.
assert isz != 0, 'image_size is 0; the loader would ignore text_offset'
print(f'[fc-build] header ok: text_offset={to:#x} image_size={isz:#x} '
      f'-> loads at {0x80200000 + to:#x}')
EOF

say "$(ls -la "$OUT" | awk '{print $5" bytes"}')"
say "OK. Next: overlays/devbox-firecracker/run.sh"
