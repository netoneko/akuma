#!/bin/sh
# Boot gate for vfat + ext3/ext4 on the *ryzen* kernel: the default feature set
# (every driver: fbcon hda i8042 kvm nvme touchpad wifi xhci, console, boot suite)
# plus `vfat,ext4` -- the same features overlays/ryzen/build.sh builds.
# Three boots of one kernel under `-M microvm`: vfatprobe, then ext-ro-probe on
# an ext3 and an ext4 image. Needs e2fsprogs and dosfstools. Exit 0 only if all pass.
set -eu
cd "$(dirname "$0")/.."
TIMEOUT="${TIMEOUT:-300}"
KERNEL=target/x86_64-unknown-none/release/akuma
W=$(mktemp -d "${TMPDIR:-/tmp}/akuma-fs-full.XXXXXXXX")
trap 'rm -rf "$W"' EXIT HUP INT TERM

cargo build -p akuma --target x86_64-unknown-none --release --features vfat,ext4
(cd userspace && cargo build -q -p vfatprobe --target x86_64-unknown-none --release
 cargo build -q -p ext2probe --bin ext-ro-probe --target x86_64-unknown-none --release)
U=userspace/target/x86_64-unknown-none/release

sh amd64/mkdisk.sh "$W/root.img" 128 >/dev/null
debugfs -w -R "mkdir /mnt" "$W/root.img" >/dev/null 2>&1
debugfs -w -R "write $U/vfatprobe /bin/vfatprobe" "$W/root.img" >/dev/null 2>&1
debugfs -w -R "write $U/ext-ro-probe /bin/ext-ro-probe" "$W/root.img" >/dev/null 2>&1

boot() { # name image args
    st=0
    timeout "$TIMEOUT" qemu-system-x86_64 -M microvm -cpu max -global virtio-mmio.force-legacy=false \
        -kernel "$KERNEL" -m 2048 -smp 1 \
        -drive id=d0,file="$W/root.img",format=raw,if=none -device virtio-blk-device,drive=d0,bus=virtio-mmio-bus.0 \
        -drive id=d1,file="$2",format=raw,if=none -device virtio-blk-device,drive=d1,bus=virtio-mmio-bus.1 \
        -append "virtio_mmio.device=512@0xfeb00000:5 virtio_mmio.device=512@0xfeb00200:6 $3" \
        -serial "file:$W/$1.log" -display none -no-reboot || st=$?
    [ "$st" -eq 0 ] || [ "$st" -eq 124 ] || { cat "$W/$1.log"; exit "$st"; }
    if grep -qE 'STACK CANARY|SWITCH BADFRAME|\[EXCEPTION\]' "$W/$1.log"; then cat "$W/$1.log"; exit 1; fi
}

truncate -s 64M "$W/esp.img"; mkfs.vfat -F 16 -n AKUMAESP "$W/esp.img" >/dev/null
boot vfat "$W/esp.img" "init=/bin/vfatprobe initargs=/dev/vdb,/mnt"
grep -aq "VFATPROBE PASS" "$W/vfat.log" || { cat "$W/vfat.log"; echo "FAIL vfat"; exit 1; }
fsck.vfat -n "$W/esp.img" && echo "vfat OK"

printf 'QEMU_EXT4_READ_OK\n' > "$W/payload"
for kind in ext3 ext4; do
    truncate -s 32M "$W/$kind.img"
    mke2fs -q -F -t "$kind" -b 1024 "$W/$kind.img"
    debugfs -w -R "write $W/payload /payload" "$W/$kind.img" >/dev/null 2>&1
    before=$(sha256sum "$W/$kind.img" | cut -d' ' -f1)
    boot "$kind" "$W/$kind.img" "init=/bin/ext-ro-probe initargs=$kind"
    grep -aq 'ext-ro-probe: PASS' "$W/$kind.log" || { cat "$W/$kind.log"; echo "FAIL $kind"; exit 1; }
    grep -aq 'self-test: .*0 failed' "$W/$kind.log" || { echo "FAIL $kind: boot suite"; exit 1; }
    [ "$before" = "$(sha256sum "$W/$kind.img" | cut -d' ' -f1)" ] || { echo "FAIL $kind: image written"; exit 1; }
    e2fsck -fn "$W/$kind.img" >/dev/null && echo "$kind OK"
done
echo "OK: vfat, ext3, ext4 on the ryzen feature set"
