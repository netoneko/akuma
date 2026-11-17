#!/bin/sh
# Disposable host-generated ext3/ext4 images through the actual guest mount path.
# Requires cargo, qemu-system-x86_64, mke2fs, debugfs, e2fsck, and timeout.
set -eu
cd "$(dirname "$0")/../.."
work=$(mktemp -d "${TMPDIR:-/tmp}/akuma-ext-ro.XXXXXXXX")
trap 'rm -rf "$work"' EXIT HUP INT TERM
cargo build -p akuma --target x86_64-unknown-none --release --features ext4
cargo build --manifest-path userspace/Cargo.toml -p ext2probe --bin ext-ro-probe --target x86_64-unknown-none --release
sh amd64/mkdisk.sh "$work/root.img" 128
# Existing images and live virtual machines are never touched.
debugfs -w -R 'mkdir /mnt' "$work/root.img" >/dev/null 2>&1
debugfs -w -R "write userspace/target/x86_64-unknown-none/release/ext-ro-probe /bin/ext-ro-probe" "$work/root.img" >/dev/null 2>&1
printf 'QEMU_EXT4_READ_OK\n' > "$work/payload"
for kind in ext3 ext4 ext3recover; do
    image="$work/$kind.img"
    truncate -s 32M "$image"
    if [ "$kind" = ext3recover ]; then
        python3 scripts/probes/create_dirty_ext3.py "$image"
    elif [ "$kind" = ext4 ]; then
        mke2fs -q -F -t ext4 -b 1024 "$image"
    else
        mke2fs -q -F -t ext3 -b 1024 "$image"
    fi
    if [ "$kind" != ext3recover ]; then
        debugfs -w -R "write $work/payload /payload" "$image" >/dev/null 2>&1
    fi
    before=$(sha256sum "$image" | cut -d' ' -f1)
    # The guest parks after init exits, so timeout is expected. Inspect the
    # serial log and require both the probe verdict and a clean boot suite.
    status=0
    timeout 30s qemu-system-x86_64 -M microvm -cpu max \
        -global virtio-mmio.force-legacy=false \
        -kernel target/x86_64-unknown-none/release/akuma -m 2048 -smp 1 \
        -drive "id=d0,file=$work/root.img,format=raw,if=none" \
        -device virtio-blk-device,drive=d0,bus=virtio-mmio-bus.0 \
        -drive "id=d1,file=$image,format=raw,if=none,readonly=on" \
        -device virtio-blk-device,drive=d1,bus=virtio-mmio-bus.1 \
        -append "virtio_mmio.device=512@0xfeb00000:5 virtio_mmio.device=512@0xfeb00200:6 init=/bin/ext-ro-probe initargs=$kind" \
        -serial "file:$work/$kind.log" -display none -no-reboot || status=$?
    if [ "$status" -ne 0 ] && [ "$status" -ne 124 ]; then cat "$work/$kind.log"; exit "$status"; fi
    if ! grep -q 'ext-ro-probe: PASS' "$work/$kind.log" ||
       ! grep -q 'self-test: .*0 failed' "$work/$kind.log" ||
       grep -qE 'STACK CANARY|SWITCH BADFRAME|\[EXCEPTION\]' "$work/$kind.log"; then
        cat "$work/$kind.log"
        exit 1
    fi
    after=$(sha256sum "$image" | cut -d' ' -f1)
    test "$before" = "$after" || { echo "$kind image changed during read-only mount" >&2; exit 1; }
    if [ "$kind" = ext3recover ]; then
        cp "$image" "$work/oracle.img"
        e2fsck -fy -E journal_only "$work/oracle.img" || [ "$?" -eq 1 ]
        test "$(debugfs -R 'cat /payload' "$work/oracle.img" 2>/dev/null)" = 'QEMU_EXT3_REPLAY!'
        e2fsck -fn "$work/oracle.img"
    else
        e2fsck -fn "$image"
    fi
    echo "$kind QEMU read-only probe passed"
done
