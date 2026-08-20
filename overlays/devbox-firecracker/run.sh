#!/usr/bin/env bash
# Boot Akuma as a Firecracker microVM.
#
# Two topologies, same script:
#
#   macOS  -> --via-lima (default): the KVM host is a Lima VM, so the kernel and
#             disk have to be copied INTO it. Lima's virtiofs mount of the host is
#             read-only, which is why the disk is copied rather than used in place.
#   metal  -> --local: the machine running this IS the KVM host (AWS .metal, an
#             ARM SBC, a Linux box with /dev/kvm). Nothing is copied; paths are
#             used directly and are natively read-write.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

INSTANCE="${LIMA_INSTANCE:-fc}"
KERNEL="${FC_KERNEL:-$REPO_ROOT/akuma-fc.bin}"
DISK="${FC_DISK:-$REPO_ROOT/disk.img}"
TAP="${FC_TAP:-tap0}"
VCPUS="${FC_VCPUS:-1}"
MEM="${FC_MEM:-1024}"
# Sync (blocking syscalls) is Firecracker's default and what everything here was
# tested against. "Async" uses io_uring and Firecracker documents up to 30x total
# IOPS on NVMe — a real lever for Akuma's metadata-heavy, mmap-driven ext2
# workload on AWS — but it is still a developer preview and needs host kernel
# >= 5.10.51. Opt in with FC_IO_ENGINE=Async once the basics are solid.
IO_ENGINE="${FC_IO_ENGINE:-Sync}"
MODE=via-lima
WITH_DISK=1
WITH_NET=1
INTERACTIVE=0
TIMEOUT="${FC_TIMEOUT:-120}"

usage() {
  sed -n '2,14p' "$0" | sed 's/^# \{0,1\}//'
  cat <<EOF

Options:
  --local           this machine is the KVM host (AWS metal); do not use limactl
  --via-lima        KVM host is the Lima instance '\$LIMA_INSTANCE' (default)
  --no-disk         boot without a drive (console-only smoke test)
  --no-net          boot without a NIC
  --vcpus N         vCPU count (default 1; >1 is known broken, see README)
  --mem MiB         guest RAM (default 1024)
  --interactive     run in the foreground, console attached (Ctrl-C to stop)
  --timeout SECS    non-interactive run length (default 120)
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --local) MODE=local ;;
    --via-lima) MODE=via-lima ;;
    --no-disk) WITH_DISK=0 ;;
    --no-net) WITH_NET=0 ;;
    --vcpus) VCPUS="$2"; shift ;;
    --mem) MEM="$2"; shift ;;
    --interactive) INTERACTIVE=1 ;;
    --timeout) TIMEOUT="$2"; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage; exit 1 ;;
  esac
  shift
done

say() { printf '\033[1;36m[fc-run]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[fc-run] %s\033[0m\n' "$*" >&2; exit 1; }

[ -f "$KERNEL" ] || die "$KERNEL not found — run overlays/devbox-firecracker/build.sh"

if [ "$VCPUS" != "1" ]; then
  say "WARNING: vcpu_count=$VCPUS. Firecracker places the GIC redistributors at"
  say "         0x3FFF_0000 - vcpu_count * 0x2_0000, and Akuma's bootstrap device"
  say "         map assumes one vCPU, so CPU0 will drive another core's frames and"
  say "         lose its timer interrupt. See docs/reference/firecracker/."
fi

# --- where do the guest-visible paths live? -------------------------------------
if [ "$MODE" = local ]; then
  [ -e /dev/kvm ] || die "/dev/kvm not present — this is not a KVM host (try --via-lima)"
  G_KERNEL="$KERNEL"
  G_DISK="$DISK"
  run()  { sudo "$@"; }
  runsh() { sudo sh -c "$1"; }
  put()  { :; }                       # nothing to copy; same filesystem
else
  command -v limactl >/dev/null || die "limactl not found (use --local on a real KVM host)"
  G_KERNEL=/tmp/akuma-fc.bin
  # Deliberately NOT the virtiofs path: Lima mounts the host read-only, so
  # Firecracker cannot open a disk there read-write. On --local this copy vanishes.
  G_DISK=/var/tmp/akuma-disk.img
  run()  { limactl shell "$INSTANCE" -- sudo "$@"; }
  runsh() { limactl shell "$INSTANCE" -- sudo sh -c "$1"; }
  put() {
    say "copying kernel into '$INSTANCE'"
    limactl copy "$KERNEL" "$INSTANCE:$G_KERNEL"
    if [ "$WITH_DISK" = 1 ]; then
      [ -f "$DISK" ] || die "$DISK not found — scripts/create_disk.sh"
      # ~2 GB; only re-copy when the host image is newer.
      if limactl shell "$INSTANCE" -- test -f "$G_DISK" \
         && [ "$(limactl shell "$INSTANCE" -- stat -c%s "$G_DISK" 2>/dev/null)" = "$(stat -f%z "$DISK")" ]; then
        say "disk already staged in guest ($G_DISK)"
      else
        say "staging disk into guest (~$(( $(stat -f%z "$DISK") / 1048576 )) MiB, slow)"
        limactl copy "$DISK" "$INSTANCE:$G_DISK"
      fi
    fi
  }
fi

put

# --- config ---------------------------------------------------------------------
# `drives` and `network-interfaces` are REQUIRED keys in the single-JSON form,
# even when empty — Firecracker v1.16 errors with "missing field `drives`".
DRIVES="[]"
[ "$WITH_DISK" = 1 ] && DRIVES="[{\"drive_id\":\"rootfs\",\"path_on_host\":\"$G_DISK\",\"is_root_device\":true,\"is_read_only\":false,\"io_engine\":\"$IO_ENGINE\"}]"
NICS="[]"
[ "$WITH_NET" = 1 ] && NICS="[{\"iface_id\":\"eth0\",\"host_dev_name\":\"$TAP\",\"guest_mac\":\"02:FC:00:00:00:01\"}]"

CFG=/tmp/akuma-fc.json
runsh "cat > $CFG <<EOF
{
  \"boot-source\": { \"kernel_image_path\": \"$G_KERNEL\", \"boot_args\": \"console=ttyS0\" },
  \"drives\": $DRIVES,
  \"network-interfaces\": $NICS,
  \"machine-config\": { \"vcpu_count\": $VCPUS, \"mem_size_mib\": $MEM }
}
EOF"

say "disk=$([ "$WITH_DISK" = 1 ] && echo "$G_DISK ($IO_ENGINE)" || echo none) nic=$([ "$WITH_NET" = 1 ] && echo "$TAP" || echo none) vcpus=$VCPUS mem=${MEM}MiB"

# A stale API socket is a hard error, and it is owned by root.
runsh "rm -f /tmp/fc.sock"

# Firecracker logs an "Invalid MMIO" line for every probe of a virtio slot with no
# device behind it. That is Akuma correctly walking all 8 slots, not a fault.
if [ "$INTERACTIVE" = 1 ]; then
  say "foreground; Ctrl-C to stop"
  runsh "firecracker --api-sock /tmp/fc.sock --config-file $CFG 2>&1 | grep -v 'Invalid MMIO'"
else
  LOG=/tmp/akuma-fc-boot.log
  say "running ${TIMEOUT}s, log -> guest:$LOG"
  runsh "timeout $TIMEOUT firecracker --api-sock /tmp/fc.sock --config-file $CFG 2>&1 | grep -v 'Invalid MMIO' > $LOG" || true
  runsh "echo \"lines=\$(wc -l < $LOG) PASSED=\$(grep -ac PASSED $LOG) FAILED=\$(grep -ac FAILED $LOG) POISON=\$(grep -ac POISON $LOG)\""
  say "last 20 lines:"
  runsh "tail -20 $LOG"
  say "full log: $([ "$MODE" = local ] && echo "$LOG" || echo "limactl shell $INSTANCE -- cat $LOG")"
fi
