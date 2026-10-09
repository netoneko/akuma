#!/bin/sh
# Pop side (/root/cdp-probe/new): one Firecracker run of an in-guest script
# with the tap NIC (DHCP from the akuma-dnsmasq container, 192.168.1.50).
# Env: SCRIPT (tumblr-once.sh | kami-once.sh), KERNEL, VCPUS (4), MEM (6144),
# TIMEOUT (240), KARGS (strace_err), and the guest's URL / DUR / KEYS.
set -e
cd /root/cdp-probe/new
SCRIPT=${SCRIPT:-tumblr-once.sh}
KERNEL=${KERNEL:-/root/cdp-probe/akuma-amd64.head}
VCPUS=${VCPUS:-4}; MEM=${MEM:-6144}; TIMEOUT=${TIMEOUT:-240}; KARGS=${KARGS:-strace_err}
IMG=$PWD/kami-run.img
cp kami-root.img "$IMG"
for f in tumblr-once.sh tumblr.py cdp.py kami-once.sh kamitry.py kami; do
  [ -f "$f" ] || continue
  debugfs -w -R "rm $f" "$IMG" >/dev/null 2>&1 || true
  debugfs -w -R "write $f $f" "$IMG" >/dev/null 2>&1
  debugfs -w -R "sif $f mode 0100755" "$IMG" >/dev/null 2>&1
done
debugfs -w -R "rm /etc/resolv.conf" "$IMG" >/dev/null 2>&1 || true
debugfs -w -R "write resolv.conf /etc/resolv.conf" "$IMG" >/dev/null 2>&1
# The guest's knobs: a tiny env file both in-guest scripts source.
printf 'URL=%s\nDUR=%s\nKEYS=%s\n' "${URL:-https://www.tumblr.com/}" "${DUR:-60}" "${KEYS:-j,j,j,j}" > guest.env
debugfs -w -R "rm /guest.env" "$IMG" >/dev/null 2>&1 || true
debugfs -w -R "write guest.env /guest.env" "$IMG" >/dev/null 2>&1
cat > tumblr-fc.json <<JSON
{
  "boot-source": { "kernel_image_path": "$KERNEL", "boot_args": "init=/bin/busybox initargs=sh,/$SCRIPT $KARGS" },
  "drives": [{"drive_id":"rootfs","path_on_host":"$IMG","is_root_device":false,"is_read_only":false}],
  "network-interfaces": [{"iface_id":"eth0","host_dev_name":"tap0","guest_mac":"02:FC:00:00:00:01"}],
  "machine-config": { "vcpu_count": $VCPUS, "mem_size_mib": $MEM }
}
JSON
for p in $(pgrep -x firecracker); do kill -9 "$p"; done
rm -f /tmp/tumblr-fc.sock
timeout "$TIMEOUT" /home/netoneko/bin/firecracker --no-api --config-file tumblr-fc.json --api-sock /tmp/tumblr-fc.sock > tumblr-fc.log 2>&1 || true
mkdir -p out; rm -f out/shot.png out/dmesg.txt out/chromium.stderr out/kami-input.log out/kami.log
for f in dmesg.txt shot.png chromium.stderr kami-input.log kami.log; do
  debugfs -R "dump /tmp/$f out/$f" "$IMG" >/dev/null 2>&1 || true
  [ -s "out/$f" ] || rm -f "out/$f"
done
echo "== guest lines"; grep -a -E '^== |IP:' tumblr-fc.log | grep -v targetCreated | cut -c1-200 | tail -80
echo "== kernel: faults / signals / crash lines"; grep -a -E 'Fault\]|sig!\]|signal\]|\[kill|BKL\] stuck|panic|PANIC|TRAMP|stale tid' tumblr-fc.log | cut -c1-200 | tail -30
echo "== EMFILE (socket/pipe/open -> -24) count: $(grep -a -c -E '\[sc!\].*-> -24$' tumblr-fc.log)"
ls -la out/
