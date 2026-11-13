#!/bin/sh
# gdev.sh [INIT [INITARG,INITARG...]] - the Taoxichuan guest dev cycle on the ryzen (Akuma host).
#   rebuild the guest kernel from $AKUMA_SRC, install it as /boot/akuma-guest-dev, boot it on a FRESH copy of the goose image
#   (/root/tx/rs.goose.orig, left untouched), print the console from the first user instruction.
# Default: INIT=/bin/busybox ARGS=sh,/etc/goose-test.sh  (runs `goose --version` as init, prints GOOSE-TEST-RC=...).
# One VM at a time: this kills the previous `herd vm-run` (by pid) first. Console log: /root/tx/vm-dev.log.
# NEVER write /boot/akuma-amd64*; never pkill -f (it matches your own shell).
. /etc/akuma-dev.env
INIT=${1:-/bin/busybox}; ARGS=${2:-sh,/etc/goose-test.sh}
cd "$AKUMA_SRC" || exit 1
CARGO_TARGET_DIR=/root/gtarget kbuild -j 4 -p taoxichuan --features no-tests,trace-err || { echo BUILD-FAILED; exit 1; }
K=/root/gtarget/x86_64-unknown-none/release/taoxichuan
cp $K /boot/akuma-guest-dev.new && chmod +x /boot/akuma-guest-dev.new && mv /boot/akuma-guest-dev.new /boot/akuma-guest-dev || exit 1
md5sum /boot/akuma-guest-dev
pid=$(ps | grep "[v]m-run" | awk "{print \$1}"); [ -n "$pid" ] && kill $pid && sleep 2
herd stop vm-serve-mc >/dev/null 2>&1; herd stop vm-dev >/dev/null 2>&1
cp /root/tx/rs.goose.orig /root/tx/rs.dev.img.new && mv /root/tx/rs.dev.img.new /root/tx/rs.dev.img
cat > /root/tx/vm-dev.spec <<SPEC
type=kvm
kernel=/boot/akuma-guest-dev
cmdline=init=$INIT initargs=$ARGS epoch=$(date +%s)
ram_mib=3072
vcpus=4
disk=/root/tx/rs.dev.img
disk_ram=0
outbound=1
forward=2224:22
SPEC
setsid /root/tx/herd-new vm-run /root/tx/vm-dev.spec </dev/null > /root/tx/vm-dev.log 2>&1 &
n=0; while [ $n -lt 60 ]; do sleep 2; n=$((n+1)); grep -a -q "GOOSE-TEST-RC\|GOOSE-RUN-RC\|PANIC\|Fault\|init exited\|all programs done" /root/tx/vm-dev.log && break; done
grep -a -A60 "entering ring 3" /root/tx/vm-dev.log | grep -a -v "relay:\|\[KVM\] cpu" | cut -c1-200 | head -80
