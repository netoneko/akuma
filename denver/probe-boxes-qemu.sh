#!/bin/sh
# Experimental Denver box boundary probe. Builds an isolated throwaway ext2
# image; requires a previously built amd64 kernel with the containers feature.
set -eu
KERNEL=${DENVER_KERNEL:?Set DENVER_KERNEL to the experimental Denver kernel}
D=$(mktemp -d "${TMPDIR:-/tmp}/denver-boxes.XXXXXX")
trap 'rm -rf "$D"' EXIT HUP INT TERM
for name in parent child inside; do
    cc -nostdlib -static -fno-stack-protector -O2 "denver/probes/boxes/$name.c" -o "$D/$name"
done
printf 'BOX!' > "$D/boxsecret"
printf 'HOST' > "$D/hostsecret"
truncate -s 32M "$D/root.img"
mke2fs -q -t ext2 -F "$D/root.img"
for path in /bin /box /box/bin; do debugfs -w -R "mkdir $path" "$D/root.img" >/dev/null 2>&1; done
for pair in "parent:/bin/parent" "child:/box/bin/raw" "inside:/box/bin/inside" "boxsecret:/box/secret" "hostsecret:/secret" "hostsecret:/hostonly"; do
    src=${pair%%:*}; dest=${pair#*:}
    debugfs -w -R "write $D/$src $dest" "$D/root.img" >/dev/null 2>&1
done
for path in /bin/parent /box/bin/raw /box/bin/inside; do debugfs -w -R "set_inode_field $path mode 0100755" "$D/root.img" >/dev/null 2>&1; done
# Denver parks after init, so timeout is expected even after a complete probe.
timeout 30s qemu-system-x86_64 -M microvm -cpu max -global virtio-mmio.force-legacy=false -m 1024 -smp 1 \
    -kernel "$KERNEL" -drive "id=d0,file=$D/root.img,format=raw,if=none,snapshot=on" \
    -device virtio-blk-device,drive=d0,bus=virtio-mmio-bus.0 \
    -append 'virtio_mmio.device=512@0xfeb00000:5 smp=1 init=/bin/parent' \
    -serial mon:stdio -display none -no-reboot > "$D/log" 2>&1 || rc=$?
cat "$D/log"
for marker in 'REGISTER PASS' 'SPAWN PASS' 'BOX_READ_PASS' 'HOST_HIDDEN_PASS' 'FILE_MMAP_REFUSED_PASS' 'INNER_EXEC_PASS' 'KILL PASS' 'Denver: exit code 0'; do
    if ! grep -Fq "$marker" "$D/log"; then echo "FAIL: missing $marker" >&2; exit 1; fi
done
if grep -Eq '(^|[[:space:]])(REGISTER FAIL|SPAWN FAIL|BOX_READ_FAIL|HOST_HIDDEN_FAIL|FILE_MMAP_REFUSED_FAIL|EXEC_FAIL|KILL FAIL)' "$D/log"; then
    echo 'FAIL: guest reported a failed check' >&2; exit 1
fi
case ${rc:-0} in 0|124) ;; *) echo "FAIL: QEMU exited ${rc}" >&2; exit 1 ;; esac
echo 'RESULT PASS: experimental box boundary probe'
