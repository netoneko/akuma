#!/bin/sh
# Build the Akuma/amd64 kernel for the metal, locally, as the current user.
#
#   scripts/build_kernel_amd64_host.sh [out-elf]
#
# The kernel that boots on ryzen: the default feature set (every driver, so
# `kvm` included) plus `no-tests` — what overlays/ryzen/build.sh builds. NOT
# `headless`: that drops the hypervisor (/dev/kvm0). The toolchain is the one
# rust-toolchain.toml pins. The ELF is verified: multiboot2 header present and
# the hypervisor linked in (a build that lost `kvm` is refused).
set -eu
SRC=$(cd "$(dirname "$0")/.." && pwd)
OUT=${1:-$SRC/target/docker-kernel/akuma-amd64}
FEATURES=${FEATURES:-no-tests}
cd "$SRC"
[ -f crates/akuma-fbcon/vendor/spleen/spleen-8x16.bdf ] || {
    echo "spleen submodule missing: git submodule update --init crates/akuma-fbcon/vendor/spleen" >&2; exit 1; }
rustup target add x86_64-unknown-none >/dev/null
cargo build -p akuma --target x86_64-unknown-none --release --features "$FEATURES"
mkdir -p "$(dirname "$OUT")"
cp target/x86_64-unknown-none/release/akuma "$OUT"
od -A d -t x4 -N 32768 "$OUT" | grep -qi e85250d6 || { echo "NO multiboot2 header in $OUT" >&2; exit 1; }
strings -a "$OUT" | grep -q '/dev/kvm0' || { echo "hypervisor not linked into $OUT (no /dev/kvm0) — refusing" >&2; exit 1; }
echo "built $OUT ($(wc -c <"$OUT") bytes, md5 $(md5sum <"$OUT" | cut -d' ' -f1)): multiboot2 ok, kvm linked, default + $FEATURES"
