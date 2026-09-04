#!/bin/sh
# Give the amd64 Firecracker host a tap device, DHCP and NAT — through Docker,
# with no sudo.
#
# The aarch64 equivalent is `overlays/devbox-firecracker/guest-setup.sh`, which
# does the same three things as root on a Lima VM or an AWS .metal instance. This
# is that recipe on a machine where we have Docker but not passwordless sudo, and
# the addresses are **deliberately identical** so a guest cannot tell the two
# hosts apart:
#
#   gateway 10.0.2.2, guest 10.0.2.15, subnet 10.0.2.0/24
#
# Those are QEMU's SLIRP addresses, which is why they were chosen there — Akuma's
# DHCP and no-DHCP paths agree on them.
#
# # Why Docker works here
#
# `--network host` puts the container in the **host's** network namespace, so an
# interface it creates belongs to the host and outlives the container.
# `--cap-add=NET_ADMIN` grants exactly the capability `ip tuntap` and `iptables`
# need, without a password and without granting anything else.
#
#   FC_HOST=user@host amd64/net-setup.sh
#   FC_HOST=... amd64/net-setup.sh --status
#   FC_HOST=... amd64/net-setup.sh --down
set -e

HERE=$(dirname "$0")
cd "$HERE/.."

FC_HOST="${FC_HOST:?set FC_HOST=user@host}"
FC_KEY="${FC_KEY:-$HOME/.ssh/id_ed25519}"
TAP="${FC_TAP:-tap0}"
GW="${FC_GATEWAY:-10.0.2.2}"
SUBNET="${FC_SUBNET:-10.0.2.0/24}"
RANGE_LO="${FC_RANGE_LO:-10.0.2.15}"
RANGE_HI="${FC_RANGE_HI:-10.0.2.30}"
# Must match the guest_mac in the VM config, or the fixed lease below never
# matches and the guest gets an address the port forward does not point at.
GUEST_MAC="${FC_GUEST_MAC:-02:FC:00:00:00:01}"
GUEST_IP="${FC_GUEST_IP:-10.0.2.15}"
# Small and stable. `alpine` needs `apk add iproute2`: busybox's `ip` has no
# `tuntap` subcommand, which fails as "ip: can't find device tap0" — a message
# that reads like the device is missing rather than like the applet is.
NETIMG="${FC_NETIMG:-alpine}"

SSH="ssh -o StrictHostKeyChecking=no -i $FC_KEY"
ACTION="${1:-up}"

$SSH "$FC_HOST" "sh -s" <<EOSH
set -e
TAP=$TAP; GW=$GW; SUBNET=$SUBNET
GUEST_MAC=$GUEST_MAC; GUEST_IP=$GUEST_IP
RANGE_LO=$RANGE_LO; RANGE_HI=$RANGE_HI
NETIMG=$NETIMG
UID_N=\$(id -u)

# One helper: a throwaway container in the host's netns with NET_ADMIN.
#
# \`--device /dev/net/tun\` is required and easy to miss: \`--network host\` shares
# the network *namespace*, not \`/dev\`. Without it \`ip tuntap add\` fails with
# "open: No such file or directory" — which reads like the tap is missing, and is
# actually the control device being absent inside the container.
netdo() {
    docker run --rm --network host --device /dev/net/tun \
        --cap-add=NET_ADMIN --cap-add=NET_RAW \
        "\$NETIMG" sh -c "apk add -q iproute2 iptables >/dev/null 2>&1; \$1"
}

case "$ACTION" in
--status)
    echo "--- tap ---";     ip -br addr show \$TAP 2>&1 || true
    echo "--- dnsmasq ---"; docker ps --filter name=akuma-dnsmasq --format '{{.Names}} {{.Status}}' || true
    echo "--- leases ---";  docker exec akuma-dnsmasq cat /var/lib/misc/dnsmasq.leases 2>/dev/null || echo "(none)"
    exit 0 ;;
--down)
    docker rm -f akuma-dnsmasq >/dev/null 2>&1 || true
    netdo "ip link del \$TAP 2>/dev/null; true"
    echo "torn down"
    exit 0 ;;
esac

# ---- tap ---------------------------------------------------------------------
# \`user \$UID_N\` is the numeric uid on purpose: the container has no account for
# the host's user, and \`ip tuntap ... user netoneko\` fails with
# 'invalid user "netoneko"'. The uid is what the kernel stores anyway.
#
# A tap stays DOWN until something opens it; Firecracker does that at boot.
# Seeing DOWN here is normal.
if ip link show \$TAP >/dev/null 2>&1; then
    echo "\$TAP exists"
else
    netdo "ip tuntap add \$TAP mode tap user \$UID_N"
    echo "created \$TAP owned by uid \$UID_N"
fi
ip addr show \$TAP 2>/dev/null | grep -q "\$GW/" || netdo "ip addr add \$GW/24 dev \$TAP"
netdo "ip link set \$TAP up"

# ---- DHCP --------------------------------------------------------------------
# Firecracker has no SLIRP, so nothing answers a guest's DHCP request unless we
# run a server. Detached and named, not \`--rm\` in the foreground: it has to
# outlive this script.
#
# --dhcp-host pins the lease so a port forward has a stable target; without it
# dnsmasq picks anywhere in the range and the forward silently points at nothing.
if docker ps --filter name=akuma-dnsmasq --format '{{.Names}}' | grep -q akuma-dnsmasq; then
    echo "dnsmasq already running"
else
    docker rm -f akuma-dnsmasq >/dev/null 2>&1 || true
    docker run -d --name akuma-dnsmasq --network host \
        --cap-add=NET_ADMIN --cap-add=NET_RAW --cap-add=NET_BIND_SERVICE \
        "\$NETIMG" sh -c "apk add -q dnsmasq >/dev/null 2>&1; \
            exec dnsmasq --no-daemon --interface=\$TAP --bind-dynamic \
              --except-interface=lo \
              --dhcp-range=\$RANGE_LO,\$RANGE_HI,12h \
              --dhcp-host=\$GUEST_MAC,\$GUEST_IP \
              --dhcp-option=3,\$GW --dhcp-option=6,\$GW \
              --dhcp-authoritative --log-dhcp" >/dev/null
    echo "dnsmasq started on \$TAP (\$RANGE_LO-\$RANGE_HI, router \$GW)"
fi
# \`--no-daemon\` here is the OPPOSITE of the aarch64 script's advice, and both are
# right: there it backgrounds itself from a shell that exits, so daemonizing is
# required; here the container IS the supervisor, and a dnsmasq that forks makes
# its own PID 1 exit and takes the container with it.

# ---- NAT ---------------------------------------------------------------------
netdo "sysctl -qw net.ipv4.ip_forward=1 2>/dev/null; \
    iptables -t nat -C POSTROUTING -s \$SUBNET -j MASQUERADE 2>/dev/null || \
    iptables -t nat -A POSTROUTING -s \$SUBNET -j MASQUERADE; \
    iptables -C FORWARD -i \$TAP -j ACCEPT 2>/dev/null || iptables -A FORWARD -i \$TAP -j ACCEPT; \
    iptables -C FORWARD -o \$TAP -j ACCEPT 2>/dev/null || iptables -A FORWARD -o \$TAP -j ACCEPT"
echo "NAT for \$SUBNET installed"

ip -br addr show \$TAP
EOSH
