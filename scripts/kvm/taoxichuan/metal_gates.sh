#!/bin/sh
# metal_gates.sh KERNEL VCPUS - on the metal host (Akuma, entry 29): boot KERNEL as vm-dev with VCPUS vCPUs
# (smp=VCPUS on its cmdline when > 1) through scripts/kvm/goose/boot_dev.sh, then run the gate battery.
# GATES=0 boots only (then run spawn_loop.sh). Prints the boot line and every verdict line. Run detached; output is long-lived.
set -u
K=$1; N=$2
R=/src/github.com/netoneko/akuma
T=$R/scripts/kvm/goose/vm-dev.stage7.conf
cd $R && git checkout -q -- scripts/kvm/goose/vm-dev.stage7.conf
sed -i "s/^vm_vcpus = .*/vm_vcpus = $N/" $T
[ "$N" -gt 1 ] && sed -i "s/^\(vm_cmdline = .*\)$/\1 smp=$N/" $T
cp "$K" /root/tx-k && chmod +x /root/tx-k
sh scripts/kvm/goose/boot_dev.sh /root/tx-k
[ "${GATES:-1}" = 1 ] && sh scripts/kvm/goose/run_gates.sh /root/tx-k
git checkout -q -- scripts/kvm/goose/vm-dev.stage7.conf
echo METAL-GATES-DONE
