#!/bin/sh
# Reproduce the "UDP reply then nc -u -w 2" guest stall on PLAIN QEMU — no SVM, no hypervisor.
#
#   scripts/kvm/udp_stall_repro.sh LOG [SECONDS]
#
# A UDP echo server on this machine (127.0.0.1:7777) is reached from the guest as slirp's
# 10.0.2.2:7777. The guest runs `echo akuma-probe | nc -u -w 2 10.0.2.2 7777`. Expect
# "udp host echo: akuma-probe" and "after-udp"; the bug is that neither ever appears: `nc`'s
# 2 s poll times out and the guest then spins (utime climbs, no syscalls, idle-loop timer storm,
# and QEMU's own monitor stops answering). A silent port (nothing answering) and a TCP echo both
# finish normally. See docs/archive/AKUMA_KVM_AKUMA_GUEST.md § A NIC, "Item 4".
# Exits 1 on the stall.
set -eu
LOG=${1:?usage: udp_stall_repro.sh LOG [SECONDS]}
MAX=${2:-120}
KERNEL=${KERNEL:-target/x86_64-unknown-none/release/akuma-amd64}
BUSYBOX=${BUSYBOX:-target/x86_64-unknown-none/release/busybox-x86_64}
HERE=$(dirname "$0")
W=$(mktemp -d)
trap 'rm -rf "$W"; [ -n "${ECHO_PID:-}" ] && kill "$ECHO_PID" 2>/dev/null || true' EXIT
cat > "$W/echo.py" <<'PY'
import socket
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
s.bind(("127.0.0.1", 7777))
s.settimeout(600)
while True:
    d, a = s.recvfrom(2048)
    s.sendto(d, a)
PY
python3 -I "$W/echo.py" &
ECHO_PID=$!
GUEST_EXTRA='/bin/busybox echo before-udp
/bin/busybox echo "udp host echo: $(/bin/busybox echo akuma-probe | /bin/busybox nc -u -w 2 10.0.2.2 7777)"
/bin/busybox echo after-udp' NET=1 sh "$HERE/akuma_guest_rootfs.sh" "$W/guest.img" "$BUSYBOX" >/dev/null
qemu-system-x86_64 -M microvm -cpu max -global virtio-mmio.force-legacy=false -m 1024 -smp 1 \
    -kernel "$KERNEL" \
    -drive id=d0,file="$W/guest.img",format=raw,if=none -device virtio-blk-device,drive=d0,bus=virtio-mmio-bus.0 \
    -netdev user,id=n0 -device virtio-net-device,netdev=n0,bus=virtio-mmio-bus.1 \
    -append "virtio_mmio.device=512@0xfeb00000:5 virtio_mmio.device=512@0xfeb00200:6 init=/bin/busybox initargs=sh,/etc/guest.sh" \
    -serial file:"$LOG" -display none -no-reboot &
Q=$!
i=0
while [ "$i" -lt "$MAX" ] && kill -0 "$Q" 2>/dev/null && ! grep -aq "after-udp" "$LOG" 2>/dev/null; do
    sleep 1
    i=$((i + 1))
done
kill -9 "$Q" 2>/dev/null || true
if grep -aq "after-udp" "$LOG"; then
    echo "no stall: $(grep -a 'udp host echo' "$LOG")"
else
    echo "STALL: before-udp seen=$(grep -ac before-udp "$LOG"), after-udp never"
    exit 1
fi
