#!/usr/bin/env bash
# Prepare the KVM host to run Akuma microVMs: Firecracker binary, tap device,
# DHCP server, NAT.
#
# Runs either through `limactl shell` from macOS (the default) or directly on an
# aarch64 host with /dev/kvm — pass --local for that, e.g. on AWS .metal.
set -euo pipefail

INSTANCE="${LIMA_INSTANCE:-fc}"
# Pinned deliberately: src/platform.rs's Firecracker addresses were read from
# this release. Changing it means re-reading arch/aarch64/layout.rs and
# gic/gicv3/mod.rs — the memory map has moved between releases before.
FC_VERSION="${FC_VERSION:-v1.16.1}"
TAP="${FC_TAP:-tap0}"
# Same addresses QEMU's SLIRP uses, so Akuma's DHCP and no-DHCP paths agree.
GW="${FC_GATEWAY:-10.0.2.2}"
SUBNET="${FC_SUBNET:-10.0.2.0/24}"
RANGE_LO="${FC_RANGE_LO:-10.0.2.15}"
RANGE_HI="${FC_RANGE_HI:-10.0.2.30}"

LOCAL=0
[ "${1:-}" = "--local" ] && LOCAL=1

say() { printf '\033[1;36m[guest-setup]\033[0m %s\n' "$*"; }

# Run one command as root on the KVM host. Deliberately one command per call:
# `limactl shell <inst> -- sudo bash -c '<many; commands>'` fails silently in a
# way that cost real debugging time.
run() {
  if [ "$LOCAL" = 1 ]; then sudo "$@"; else limactl shell "$INSTANCE" -- sudo "$@"; fi
}
runsh() {
  if [ "$LOCAL" = 1 ]; then sudo sh -c "$1"; else limactl shell "$INSTANCE" -- sudo sh -c "$1"; fi
}

# ---- Firecracker ----------------------------------------------------------------
if runsh "command -v firecracker >/dev/null 2>&1"; then
  say "firecracker present: $(runsh 'firecracker --version 2>/dev/null | head -1')"
else
  say "installing firecracker $FC_VERSION"
  runsh "curl -sL -o /tmp/fc.tgz https://github.com/firecracker-microvm/firecracker/releases/download/$FC_VERSION/firecracker-$FC_VERSION-aarch64.tgz"
  runsh "tar -xzf /tmp/fc.tgz -C /tmp"
  runsh "install -m0755 /tmp/release-$FC_VERSION-aarch64/firecracker-$FC_VERSION-aarch64 /usr/local/bin/firecracker"
  say "installed: $(runsh 'firecracker --version 2>/dev/null | head -1')"
fi

# ---- tap device -----------------------------------------------------------------
# A tap device stays DOWN until something opens it; Firecracker does that at boot.
# Seeing "DOWN" here is normal and not a failure.
if runsh "ip link show $TAP >/dev/null 2>&1"; then
  say "$TAP exists"
else
  say "creating $TAP"
  run ip tuntap add "$TAP" mode tap
fi
runsh "ip addr show $TAP | grep -q '${GW}/' " || run ip addr add "${GW}/24" dev "$TAP"
run ip link set "$TAP" up
run sysctl -qw net.ipv4.ip_forward=1
say "$TAP -> $(runsh "ip -br addr show $TAP")"

# ---- DHCP -----------------------------------------------------------------------
# Firecracker has no SLIRP, so nothing answers Akuma's DHCP request unless we run
# a server. Akuma boots with config::ENABLE_DHCP = true.
if runsh "pgrep -f 'dnsmasq.*$TAP' >/dev/null 2>&1"; then
  say "dnsmasq already serving $TAP"
else
  runsh "command -v dnsmasq >/dev/null 2>&1" || {
    say "installing dnsmasq"
    runsh "DEBIAN_FRONTEND=noninteractive apt-get install -y dnsmasq >/dev/null 2>&1 || true"
    # The distro unit would bind every interface; we only want our own instance.
    runsh "systemctl stop dnsmasq 2>/dev/null; systemctl disable dnsmasq 2>/dev/null; true"
  }
  say "starting dnsmasq on $TAP ($RANGE_LO-$RANGE_HI, router $GW)"
  # dnsmasq daemonizes itself. Do NOT add --no-daemon here: backgrounding it from
  # a shell that then exits kills it.
  runsh "dnsmasq --interface=$TAP --bind-dynamic --except-interface=lo \
    --dhcp-range=$RANGE_LO,$RANGE_HI,12h \
    --dhcp-option=3,$GW --dhcp-option=6,$GW \
    --dhcp-authoritative --log-dhcp \
    --log-facility=/var/tmp/dnsmasq.log --pid-file=/var/tmp/dnsmasq.pid"
fi
runsh "pgrep -f 'dnsmasq.*$TAP' >/dev/null" && say "dnsmasq running"

# ---- NAT ------------------------------------------------------------------------
say "adding NAT for $SUBNET"
runsh "iptables -t nat -C POSTROUTING -s $SUBNET -j MASQUERADE 2>/dev/null || iptables -t nat -A POSTROUTING -s $SUBNET -j MASQUERADE"
runsh "iptables -C FORWARD -i $TAP -j ACCEPT 2>/dev/null || iptables -A FORWARD -i $TAP -j ACCEPT"
runsh "iptables -C FORWARD -o $TAP -j ACCEPT 2>/dev/null || iptables -A FORWARD -o $TAP -j ACCEPT"

say "OK. Next: scripts/firecracker/build.sh && scripts/firecracker/run.sh"
