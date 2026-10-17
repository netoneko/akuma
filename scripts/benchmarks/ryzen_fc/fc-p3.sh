#!/bin/sh
# Boot a Firecracker guest on a THROWAWAY copy-on-write snapshot of p3 (Akuma's
# metal root on ryzen): the guest runs the real installed userspace (claude,
# herd services, config) while p3 itself is never written — dm-snapshot sends
# every guest write to a fresh sparse COW file, recreated on each boot.
#
#   fc-p3.sh <kernel.bin> [vcpu] [mem_mib]    XARGS='idlesleep wdt' for extra cmdline
#   fc-p3.sh stop                              (do this before mounting p3 rw)
#
# Run as root on ryzen, from Pop. Guest is 10.0.2.15 on tapsh; reach its sshd
# from the Mac with `ssh -L 2223:10.0.2.15:2222 root@ryzen`. The guest clock
# starts at 1970 (no RTC): `date -s` it before anything that does TLS. The
# MASQUERADE/FORWARD rules it adds are runtime-only (gone at the next Pop boot).
#
# This is the rig that reproduced and verified the `mmap_regions` shootdown
# deadlock (docs/archive/AMD64_TLB_STUCK_MMAP_REGIONS.md).
W=/home/netoneko/akuma-selfhost
stop() {
  for p in $(pgrep -f 'firecracker.*p3-vm.jso[n]'); do kill -9 $p; done
  for i in 1 2 3 4 5 6 7 8 9 10; do pgrep -f 'firecracker.*p3-vm.jso[n]' >/dev/null || break; sleep 1; done
  sleep 2
  dmsetup remove p3snap 2>/dev/null
  for l in $(losetup -j $W/p3cow.img | cut -d: -f1); do losetup -d $l; done
}
stop
[ "$1" = stop ] && { echo stopped; exit 0; }
K=$1; V=${2:-8}; MEM=${3:-4096}
mountpoint -q /mnt/p3 && umount /mnt/p3
rm -f $W/p3cow.img; truncate -s 8G $W/p3cow.img
L=$(losetup -f --show $W/p3cow.img)
echo "0 $(blockdev --getsz /dev/nvme0n1p3) snapshot /dev/nvme0n1p3 $L N 8" | dmsetup create p3snap || exit 1
ip link show tapsh >/dev/null 2>&1 || { ip tuntap add tapsh mode tap && ip addr add 10.0.2.2/24 dev tapsh && ip link set tapsh up; }
iptables -t nat -C POSTROUTING -s 10.0.2.0/24 -o wlp2s0 -j MASQUERADE 2>/dev/null || iptables -t nat -A POSTROUTING -s 10.0.2.0/24 -o wlp2s0 -j MASQUERADE
iptables -C FORWARD -i tapsh -j ACCEPT 2>/dev/null || iptables -I FORWARD -i tapsh -j ACCEPT
iptables -C FORWARD -o tapsh -j ACCEPT 2>/dev/null || iptables -I FORWARD -o tapsh -j ACCEPT
cp -f "$K" $W/p3-kernel.bin
cat > $W/p3-vm.json <<CFG
{"boot-source":{"kernel_image_path":"$W/p3-kernel.bin","boot_args":"init=/bin/herd initargs=daemon $XARGS"},
 "drives":[{"drive_id":"rootfs","path_on_host":"/dev/mapper/p3snap","is_root_device":false,"is_read_only":false}],
 "network-interfaces":[{"iface_id":"eth0","host_dev_name":"tapsh","guest_mac":"02:FC:00:00:00:02"}],
 "machine-config":{"vcpu_count":$V,"mem_size_mib":$MEM}}
CFG
[ -f $W/p3-fc.log ] && mv $W/p3-fc.log $W/p3-fc.log.prev
cd $W && setsid nohup /home/netoneko/bin/firecracker --no-api --config-file $W/p3-vm.json > $W/p3-fc.log 2>&1 < /dev/null &
sleep 1; echo "launched vcpu=$V mem=$MEM kernel=$(md5sum $W/p3-kernel.bin | cut -c1-12)"
