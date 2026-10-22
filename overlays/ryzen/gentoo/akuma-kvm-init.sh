#!/bin/sh
# Gentoo as the KVM guest of the ryzen's Akuma (`kvmgentoo`, /boot/kvm-guest = akuma-gentoo):
# the guest kernel runs this as init (init=/usr/bin/bash initargs=/akuma-kvm-init.sh).
#
# Not OpenRC and not the box-0 script (akuma-init.sh): the guest has no radio and no
# framebuffer. Its NIC is the hypervisor's gateway (10.0.2.15 by DHCP, the host reaches it
# through the forward on host port 2223), and its console is the host's `dmesg`.
export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
export HOME=/root TERM=xterm
hostname akuma-gentoo
mkdir -p /run/sshd /var/empty /var/log
echo "=== gentoo guest: $(uname -a)"
ifconfig
# OpenSSH as configured on p7 (Port 4444, keys only); the host's relay connects to that port.
/usr/sbin/sshd -D -e &
echo AKUMA-GUEST-SSHD-STARTED
while :; do sleep 3600; done
