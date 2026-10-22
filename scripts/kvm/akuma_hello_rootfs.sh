#!/bin/sh
# Build the smallest root the Akuma KVM guest can boot (`kvmtest kvmhello`): a 2 MiB
# ext2 whose only file is /bin/hello, a static no_std program that prints one line.
#
#   scripts/kvm/akuma_hello_rootfs.sh OUT.img
#
# Needs rustc with the x86_64-unknown-none target, and e2fsprogs (mke2fs). The program is
# built with the same flags as amd64/build.rs's guest programs. See
# docs/archive/AKUMA_KVM_AKUMA_GUEST.md.
set -eu
OUT=${1:?usage: akuma_hello_rootfs.sh OUT.img}
HERE=$(cd "$(dirname "$0")/../.." && pwd)
E2=${E2FSPROGS:-$(dirname "$(command -v mke2fs 2>/dev/null || echo /opt/homebrew/opt/e2fsprogs/sbin/mke2fs)")}
T=$(mktemp -d)
trap 'rm -rf "$T"' EXIT
mkdir -p "$T/root/bin"
rustc --edition 2024 --target x86_64-unknown-none -C opt-level=2 -C relocation-model=static \
    -C code-model=small -C panic=abort -C "link-arg=-T$HERE/userspace/amd64/user.ld" \
    -C link-arg=--no-pie -C link-arg=-nostdlib \
    -o "$T/root/bin/hello" "$HERE/userspace/amd64/helloworld/helloworld.rs"
chmod 755 "$T/root/bin/hello"
rm -f "$OUT"
"$E2/mke2fs" -q -t ext2 -d "$T/root" -L akumahello "$OUT" 2M
echo "wrote $OUT ($(wc -c < "$T/root/bin/hello" | tr -d ' ') byte /bin/hello)"
