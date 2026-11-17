#!/bin/sh
# QEMU guest-stop fault injection; all images and output are disposable.
set -eu
cd "$(dirname "$0")/../.."
work=$(mktemp -d "${TMPDIR:-/tmp}/akuma-ext3-crash.XXXXXXXX")
trap 'rm -rf "$work"' EXIT HUP INT TERM
cargo build -p akuma --target x86_64-unknown-none --release --features ext3-qemu-crash-test,no-tests
cargo build --manifest-path userspace/Cargo.toml -p ext2probe --bin ext-crash-probe --target x86_64-unknown-none --release
sh amd64/mkdisk.sh "$work/root.img" 128
debugfs -w -R 'mkdir /mnt' "$work/root.img" >/dev/null 2>&1
debugfs -w -R 'write userspace/target/x86_64-unknown-none/release/ext-crash-probe /bin/ext-crash-probe' "$work/root.img" >/dev/null 2>&1
truncate -s 32M "$work/base.img"
mke2fs -q -F -t ext3 -b 1024 "$work/base.img"
printf 'CRASH_TEST_OLD!!!\n' > "$work/payload"
debugfs -w -R "write $work/payload /payload" "$work/base.img" >/dev/null 2>&1
python3 scripts/probes/ext3_qemu_crash_matrix.py "$work/root.img" target/x86_64-unknown-none/release/akuma "$work/base.img" "$work"
