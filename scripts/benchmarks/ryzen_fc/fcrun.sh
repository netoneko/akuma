#!/bin/sh
# boot the self-host guest: 4 vCPU. usage: fcrun.sh [vcpu] [mem_mib]
W=/home/netoneko/akuma-selfhost
V=${1:-4}; MEM=${2:-4096}
for p in $(pgrep -f 'firecracker.*selfhost-vm.jso[n]'); do kill -9 $p; done
for i in 1 2 3 4 5 6 7 8 9 10; do pgrep -f 'firecracker.*selfhost-vm.jso[n]' >/dev/null || break; sleep 1; done; sleep 3
ip link show tapsh >/dev/null 2>&1 || { ip tuntap add tapsh mode tap && ip addr add 10.0.2.2/24 dev tapsh && ip link set tapsh up; }
cp -f $W/akuma/target/x86_64-unknown-none/release/akuma-amd64 $W/kernel.bin
cat > $W/selfhost-vm.json <<CFG
{"boot-source":{"kernel_image_path":"$W/kernel.bin","boot_args":"init=/bin/herd"},
 "drives":[{"drive_id":"rootfs","path_on_host":"$W/selfhost.img","is_root_device":false,"is_read_only":false}],
 "network-interfaces":[{"iface_id":"eth0","host_dev_name":"tapsh","guest_mac":"02:FC:00:00:00:02"}],
 "machine-config":{"vcpu_count":$V,"mem_size_mib":$MEM}}
CFG
[ -f $W/fc.log ] && mv $W/fc.log $W/fc.log.prev
cd $W && setsid nohup /home/netoneko/bin/firecracker --no-api --config-file $W/selfhost-vm.json > $W/fc.log 2>&1 < /dev/null &
sleep 1; echo "launched vcpu=$V mem=$MEM kernel md5=$(md5sum $W/kernel.bin | cut -c1-12)"
