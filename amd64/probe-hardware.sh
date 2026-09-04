#!/bin/sh
# Boot Linux under Firecracker with a matrix of device configurations and report
# what the guest actually sees.
#
# Discovery, not verification. `amd64/dump-machine.sh` captures one machine at
# several vCPU counts; this asks a different question — **what can this VMM be
# asked for, and where does it put it?** — by trying configurations and reading
# the guest's own inventory. A config the VMM rejects is as informative as one it
# accepts, so a failure is recorded rather than aborting the run.
#
# Linux first, deliberately: it enumerates everything, names it, and prints where
# it found it. Akuma finding nothing tells you nothing about whether the device
# is there.
#
#   FC_HOST=user@host amd64/probe-hardware.sh
#   FC_HOST=... CASES="baseline pci" amd64/probe-hardware.sh
set -e

HERE=$(dirname "$0")
cd "$HERE/.."

FC_HOST="${FC_HOST:?set FC_HOST=user@host}"
FC_KEY="${FC_KEY:-$HOME/.ssh/id_ed25519}"
FC_DIR="${FC_DIR:-akuma-dump}"
MEMORY="${MEMORY:-512}"
VCPUS="${VCPUS:-2}"
OUT="${OUT:-docs/reference/firecracker-amd64}"
# Cases to try. Each is a JSON fragment plus optional firecracker flags.
CASES="${CASES:-baseline pci block block-pci two-block rng rng-pci vsock balloon pmem hotplug everything}"

SSH="ssh -o StrictHostKeyChecking=no -i $FC_KEY"
mkdir -p "$OUT"

$SSH "$FC_HOST" "sh -s" <<EOSH
set -e
cd ~/$FC_DIR
[ -f vmlinux ] || { echo "no vmlinux; run amd64/dump-machine.sh first" >&2; exit 1; }
[ -f disk.img ] || dd if=/dev/zero of=disk.img bs=1M count=4 status=none
[ -f pmem.img ] || dd if=/dev/zero of=pmem.img bs=1M count=16 status=none

emit() {
    # \$1 = case name, \$2 = extra JSON members, \$3 = firecracker flags,
    # \$4 = extra kernel command-line words
    #
    # \`drives\` and \`network-interfaces\` are MANDATORY in the single-JSON path
    # even when empty (the API path defaults them; this one does not), so they
    # are in the template — and a case that supplies its own must therefore
    # OVERRIDE rather than append. serde rejects a duplicate key outright:
    # "duplicate field \`drives\`". That is what the first run of this script did,
    # and every block case failed in four lines.
    _drives="\${DRIVES:-[]}"
    _nics="\${NICS:-[]}"
    cat > probe-\$1.json <<EOJSON
{
  "boot-source": {
    "kernel_image_path": "\$HOME/$FC_DIR/vmlinux",
    "boot_args": "console=ttyS0 reboot=k panic=1 \$4"
  },
  "drives": \$_drives,
  "network-interfaces": \$_nics,
  "machine-config": { "vcpu_count": $VCPUS, "mem_size_mib": $MEMORY }\$2
}
EOJSON
    # A rejected config exits non-zero with its reason on stderr; that IS the
    # result, so it is captured rather than treated as a failure of the probe.
    timeout --foreground 25 \$HOME/bin/firecracker --no-api \
        \$3 --config-file probe-\$1.json > probe-\$1.log 2>&1 || true
    echo "  \$1: \$(wc -l < probe-\$1.log) lines"
}

ONE_DISK='[{"drive_id":"d0","path_on_host":"'\$HOME'/$FC_DIR/disk.img","is_root_device":false,"is_read_only":false}]'
TWO_DISK='[{"drive_id":"d0","path_on_host":"'\$HOME'/$FC_DIR/disk.img","is_root_device":false,"is_read_only":false},{"drive_id":"d1","path_on_host":"'\$HOME'/$FC_DIR/pmem.img","is_root_device":false,"is_read_only":false}]'

for c in $CASES; do
    DRIVES=""; NICS=""
    case \$c in
      baseline)   emit \$c "" "" ;;
      pci)        emit \$c "" "--enable-pci" ;;
      block)      DRIVES="\$ONE_DISK" emit \$c "" "" ;;
      block-pci)  DRIVES="\$ONE_DISK" emit \$c "" "--enable-pci" ;;
      two-block)  DRIVES="\$TWO_DISK" emit \$c "" "" ;;
      # The MAC must match \`amd64/net-setup.sh\`'s \`--dhcp-host\`, or the pinned
      # lease never matches and the guest lands somewhere unpredictable.
      net)        NICS='[{"iface_id":"eth0","host_dev_name":"tap0","guest_mac":"02:FC:00:00:00:01"}]' emit \$c "" "" ;;
      net-pci)    NICS='[{"iface_id":"eth0","host_dev_name":"tap0","guest_mac":"02:FC:00:00:00:01"}]' emit \$c "" "--enable-pci" ;;
      # In-kernel DHCP (\`ip=dhcp\`, CONFIG_IP_PNP_DHCP) so the whole path — NIC,
      # tap, dnsmasq, lease — is proved without a root filesystem.
      net-dhcp)   NICS='[{"iface_id":"eth0","host_dev_name":"tap0","guest_mac":"02:FC:00:00:00:01"}]' emit \$c "" "" "ip=dhcp" ;;
      rng)        emit \$c ',"entropy":{}' "" ;;
      rng-pci)    emit \$c ',"entropy":{}' "--enable-pci" ;;
      # The socket file is removed first: Firecracker binds it and does not
      # clean up, so a second run fails with "Address in use" — which reads like
      # a capability limit and is a leftover file.
      vsock)      rm -f \$HOME/$FC_DIR/v.sock; emit \$c ',"vsock":{"guest_cid":3,"uds_path":"'\$HOME'/$FC_DIR/v.sock"}' "" ;;
      balloon)    emit \$c ',"balloon":{"amount_mib":16,"deflate_on_oom":true}' "" ;;
      pmem)       emit \$c ',"pmem":[{"id":"p0","path_on_host":"'\$HOME'/$FC_DIR/pmem.img","root_device":false,"read_only":false}]' "" ;;
      hotplug)    emit \$c ',"memory-hotplug":{"total_size_mib":1024,"slot_size_mib":128,"block_size_mib":2}' "" ;;
      everything) rm -f \$HOME/$FC_DIR/v.sock; DRIVES="\$ONE_DISK" emit \$c ',"entropy":{},"balloon":{"amount_mib":16,"deflate_on_oom":true},"vsock":{"guest_cid":3,"uds_path":"'\$HOME'/$FC_DIR/v.sock"}' "--enable-pci" ;;
    esac
done
EOSH

for c in $CASES; do
    scp -q -o StrictHostKeyChecking=no -i "$FC_KEY" \
        "$FC_HOST:$FC_DIR/probe-$c.log" "$OUT/probe-$c.log" 2>/dev/null \
        || echo "  (no log for $c)"
done
echo "logs in $OUT/"
