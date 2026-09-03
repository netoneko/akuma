#!/bin/sh
# Build the amd64 kernel, stage it on a remote x86_64 KVM host, and boot it under
# Firecracker.
#
# Firecracker cannot run on the Apple Silicon dev machine (it needs KVM and an
# x86 host), so this pushes the ELF to a box that has both. QEMU via
# `amd64/run.sh` is the local stand-in and takes the *same* PVH entry path; the
# observable difference is where the boot block lands — QEMU 0x1580,
# Firecracker 0x6000 — which is why kmain prints that address.
#
#   FC_HOST=user@host amd64/run-firecracker.sh
#
# Everything it writes lives under one directory on the host (FC_DIR, default
# ~/akuma), including a standalone `run.sh` so the VM can be re-launched there
# without this script or the dev machine.
set -e

HERE=$(dirname "$0")
cd "$HERE/.."

FC_HOST="${FC_HOST:?set FC_HOST=user@host}"
FC_KEY="${FC_KEY:-$HOME/.ssh/id_ed25519}"
FC_DIR="${FC_DIR:-akuma}"        # relative to the remote $HOME
MEMORY="${MEMORY:-512}"
VCPUS="${VCPUS:-1}"
TIMEOUT="${TIMEOUT:-20}"
KERNEL=target/x86_64-unknown-none/release/akuma-amd64

SSH="ssh -o StrictHostKeyChecking=no -i $FC_KEY"

cargo build -p akuma-amd64 --target x86_64-unknown-none --release

$SSH "$FC_HOST" "mkdir -p ~/$FC_DIR"
scp -q -o StrictHostKeyChecking=no -i "$FC_KEY" "$KERNEL" "$FC_HOST:$FC_DIR/akuma-amd64"

# Stage the config and a standalone launcher, then run it. The launcher is
# written here rather than kept only on the host so the two cannot drift.
$SSH "$FC_HOST" "sh -s" <<EOSH
set -e
cd ~/$FC_DIR

cat > akuma-vm.json <<'EOJSON'
{
  "boot-source": { "kernel_image_path": "KERNEL_PATH", "boot_args": "" },
  "drives": [],
  "network-interfaces": [],
  "machine-config": { "vcpu_count": $VCPUS, "mem_size_mib": $MEMORY }
}
EOJSON
sed -i "s|KERNEL_PATH|\$HOME/$FC_DIR/akuma-amd64|" akuma-vm.json

cat > run.sh <<'EORUN'
#!/bin/sh
# Boot Akuma/amd64 under Firecracker. Staged by amd64/run-firecracker.sh.
#
# The kernel halts rather than exiting, so Firecracker never returns on its own —
# hence the timeout. TIMEOUT=0 runs without one (Ctrl-C to stop).
set -e
cd "\$(dirname "\$0")"

FC=\$(command -v firecracker || echo "\$HOME/bin/firecracker")
[ -x "\$FC" ] || { echo "firecracker not found (looked in PATH and ~/bin)" >&2; exit 1; }

TIMEOUT="\${TIMEOUT:-20}"
if [ "\$TIMEOUT" = "0" ]; then
    exec "\$FC" --no-api --config-file akuma-vm.json
else
    timeout "\$TIMEOUT" "\$FC" --no-api --config-file akuma-vm.json || true
fi
EORUN
chmod +x run.sh

TIMEOUT=$TIMEOUT ./run.sh
EOSH
