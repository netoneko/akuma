#!/bin/sh
# Dump what an x86_64 Firecracker microVM tells its guest about itself.
#
# The counterpart of `docs/runbooks/dump-firecracker-fdt.md`, which boots a Linux
# guest on aarch64 and copies `/sys/firmware/fdt`. **There is no FDT here.** On
# x86_64 Firecracker passes a PVH `hvm_start_info` block (an E820 memory map, a
# command line) and a set of ACPI tables, and that is the whole machine
# description. So this dumps the ACPI inventory instead — and it does it by
# reading Linux's own boot log rather than a sysfs file, because Linux prints
# every table it finds, with address, length and OEM id, long before it needs a
# root filesystem. That means **no rootfs is required**: the guest panics on "no
# working init" a moment later, by which time the dump is already on the serial
# line.
#
# Run at several vCPU counts on purpose. On aarch64 that is how the GIC
# redistributor's vCPU-dependent base was found (`docs/reference/firecracker/fdt/`,
# and the bug in `docs/archive/GICD_IROUTER_ALIASING.md`); the same question —
# "what in this description moves when the machine changes?" — is the reason to
# capture more than one here.
#
#   FC_HOST=user@host amd64/dump-machine.sh
#   FC_HOST=... VCPU_LIST="1 2 4 8" OUT=docs/reference/firecracker-amd64 ...
set -e

HERE=$(dirname "$0")
cd "$HERE/.."

FC_HOST="${FC_HOST:?set FC_HOST=user@host}"
FC_KEY="${FC_KEY:-$HOME/.ssh/id_ed25519}"
FC_DIR="${FC_DIR:-akuma-dump}"
VCPU_LIST="${VCPU_LIST:-1 2 4 8}"
MEMORY="${MEMORY:-512}"
OUT="${OUT:-docs/reference/firecracker-amd64}"
# Firecracker's own published CI kernel. An uncompressed ELF `vmlinux`, which is
# what the PVH/Linux boot path needs — a distro `vmlinuz` is a compressed bzImage
# and Firecracker cannot load one.
KERNEL_URL="${KERNEL_URL:-https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.12/x86_64/vmlinux-6.1.128}"

SSH="ssh -o StrictHostKeyChecking=no -i $FC_KEY"

mkdir -p "$OUT"

$SSH "$FC_HOST" "sh -s" <<EOSH
set -e
mkdir -p ~/$FC_DIR && cd ~/$FC_DIR

# Download to a temporary name and rename only on success. An interrupted
# fetch otherwise leaves a partial \`vmlinux\` that the next run happily reuses,
# and Firecracker's diagnosis of a truncated ELF is "Unable to read kernel
# image" — which reads like a permissions or path problem, not a short file.
# (\`file\` gives it away: "missing section headers at <offset past EOF>".)
if [ ! -f vmlinux ]; then
    echo "fetching guest kernel..." >&2
    curl -fL --retry 3 -o vmlinux.part "$KERNEL_URL"
    mv vmlinux.part vmlinux
fi

for VCPUS in $VCPU_LIST; do
    cat > dump-\$VCPUS.json <<EOJSON
{
  "boot-source": {
    "kernel_image_path": "\$HOME/$FC_DIR/vmlinux",
    "boot_args": "console=ttyS0 reboot=k panic=1 pci=off"
  },
  "drives": [],
  "network-interfaces": [],
  "machine-config": { "vcpu_count": \$VCPUS, "mem_size_mib": $MEMORY }
}
EOJSON

    # \`--foreground\` for the SIGTTIN reason documented in run-firecracker.sh.
    # The guest panics with no init; that is expected and not an error here.
    timeout --foreground 25 \$HOME/bin/firecracker --no-api \
        --config-file dump-\$VCPUS.json > dump-\$VCPUS.log 2>&1 || true
    echo "vcpus=\$VCPUS -> \$(wc -l < dump-\$VCPUS.log) lines" >&2
done
EOSH

for VCPUS in $VCPU_LIST; do
    scp -q -o StrictHostKeyChecking=no -i "$FC_KEY" \
        "$FC_HOST:$FC_DIR/dump-$VCPUS.log" "$OUT/linux-boot-$VCPUS-vcpu.log"
    echo "  $OUT/linux-boot-$VCPUS-vcpu.log"
done
