#!/usr/bin/env bash
# Create the local aarch64 KVM host: a Lima VM with nested virtualization.
#
# macOS never has /dev/kvm itself, and Docker Desktop cannot provide one (its
# linuxkit kernel is built with CONFIG_VIRTUALIZATION unset AND its VM boots at
# EL1) — see docs/archive/AKUMA_FIRECRACKER_KVM.md §2. A Lima VM on the `vz`
# driver with nestedVirtualization can, on M3+ / macOS 15+.
set -euo pipefail

INSTANCE="${LIMA_INSTANCE:-fc}"
CPUS="${LIMA_CPUS:-4}"
MEM="${LIMA_MEM:-8}"

say() { printf '\033[1;36m[host-setup]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[host-setup] %s\033[0m\n' "$*" >&2; exit 1; }

[ "$(uname -s)" = "Darwin" ] || die "run this on macOS; on metal use guest-setup.sh directly"

# ---- 1. Is nested virt even possible on this machine? --------------------------
say "checking nested-virtualization support"
NV_SWIFT="$(mktemp -t nv).swift"
cat > "$NV_SWIFT" <<'EOF'
import Virtualization
if #available(macOS 15.0, *) {
    print(VZGenericPlatformConfiguration.isNestedVirtualizationSupported ? "yes" : "no")
} else { print("no-os") }
EOF
NV="$(swift "$NV_SWIFT" 2>/dev/null | tail -1 || echo unknown)"
rm -f "$NV_SWIFT"
case "$NV" in
  yes)    say "nested virtualization: supported" ;;
  no)     die "this Mac reports nested virtualization unsupported (needs Apple silicon M3+). Use AWS .metal — proposals/FIRECRACKER_PORT.md §7" ;;
  no-os)  die "needs macOS 15 or newer" ;;
  *)      say "WARNING: could not determine nested-virt support; continuing" ;;
esac

# ---- 2. Lima --------------------------------------------------------------------
command -v limactl >/dev/null || die "limactl not found — brew install lima"
say "lima $(limactl --version | awk '{print $3}')"

if limactl list -q 2>/dev/null | grep -qx "$INSTANCE"; then
  say "instance '$INSTANCE' exists"
else
  say "creating instance '$INSTANCE' (downloads a ~940 MB Ubuntu image; slow first time)"
  # NOTE: limactl buffers ALL output when stdout is not a TTY, so a backgrounded
  # run looks dead while working — and several concurrent runs will race on the
  # same instance and starve each other. Keep this in the foreground.
  limactl start -y --name="$INSTANCE" --vm-type=vz --nested-virt \
      --cpus="$CPUS" --memory="$MEM"
fi

# Both settings must be present; --nested-virt is meaningless without vz.
say "verifying instance config"
grep -qE '^vmType: vz' "$HOME/.lima/$INSTANCE/lima.yaml" \
  || die "instance is not on the vz driver; delete it and re-run: limactl delete $INSTANCE"
grep -qE '^nestedVirtualization: true' "$HOME/.lima/$INSTANCE/lima.yaml" \
  || die "nestedVirtualization not set; delete it and re-run: limactl delete $INSTANCE"

if [ "$(limactl list "$INSTANCE" --format '{{.Status}}' 2>/dev/null)" != "Running" ]; then
  say "starting '$INSTANCE'"
  limactl start "$INSTANCE"
fi

# ---- 3. The checks that actually matter -----------------------------------------
say "verifying /dev/kvm in the guest"
limactl shell "$INSTANCE" -- ls -l /dev/kvm \
  || die "/dev/kvm missing in the guest — nested virt did not engage"

# 'started at EL2' is the proof. A guest at EL1 has no exception level to run a
# hypervisor in, however present the device node is.
if limactl shell "$INSTANCE" -- sudo dmesg 2>/dev/null | grep -q "started at EL2"; then
  say "guest booted at EL2 — nested virt engaged"
else
  die "guest did not boot at EL2; /dev/kvm will not work. Recheck vmType/nestedVirtualization."
fi

limactl shell "$INSTANCE" -- sudo dmesg 2>/dev/null | grep -m1 "kvm .*nv:" || true

say "OK. Next: scripts/firecracker/guest-setup.sh"
