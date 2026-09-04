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
# DISK=<local path> attaches it as the guest's first virtio-blk drive.
#
# Firecracker passes no device tree, and **by default** presents virtio over
# MMIO with PCI switched off — it appends `pci=off` and a
# `virtio_mmio.device=<size>@<base>:<irq>` token to the kernel command line,
# which arrives through `hvm_start_info.cmdline_paddr`. Attaching a drive is
# therefore also what makes that token appear: with `"drives": []` the command
# line is empty and there is nothing to discover.
#
# "By default" is the operative phrase. v1.16.1 has `--enable-pci`, and it builds
# a real PCIe segment — measured, see `docs/reference/firecracker-amd64/README.md`.
# MMIO is a choice here, not a constraint the VMM imposes.
# Defaults to the ext2 root image, rebuilt from the just-compiled guest ELF.
# `DISK=none` boots with no drive, which is the pre-Stage-M shape and still valid.
DISK="${DISK:-}"
KERNEL=target/x86_64-unknown-none/release/akuma-amd64

SSH="ssh -o StrictHostKeyChecking=no -i $FC_KEY"

cargo build -p akuma-amd64 --target x86_64-unknown-none --release

$SSH "$FC_HOST" "mkdir -p ~/$FC_DIR"
scp -q -o StrictHostKeyChecking=no -i "$FC_KEY" "$KERNEL" "$FC_HOST:$FC_DIR/akuma-amd64"

# The drives array, built here so the JSON below stays a fixed template.
DRIVES_JSON="[]"
if [ -z "$DISK" ]; then
    DISK=target/x86_64-unknown-none/release/amd64-root.img
    sh "$HERE/mkdisk.sh" "$DISK" 8 >/dev/null
fi
[ "$DISK" = "none" ] && DISK=""
if [ -n "$DISK" ]; then
    [ -f "$DISK" ] || { echo "DISK=$DISK does not exist" >&2; exit 1; }
    scp -q -o StrictHostKeyChecking=no -i "$FC_KEY" "$DISK" "$FC_HOST:$FC_DIR/disk.img"
    DRIVES_JSON='[{"drive_id":"rootfs","path_on_host":"DISK_PATH","is_root_device":false,"is_read_only":false}]'
fi

# Stage the config and a standalone launcher, then run it. The launcher is
# written here rather than kept only on the host so the two cannot drift.
# Backticks below are escaped, every one of them. The outer heredoc is
# unquoted — it has to be, since $VCPUS and $FC_DIR are substituted here —
# so an unescaped `word` in a *comment* of the staged script runs as a
# command on the DEV machine and its output is what lands on the host. That
# is how `timeout --foreground` in the note below became three stray
# "Try 'timeout --help'" lines on stderr and an empty span in the staged file.
$SSH "$FC_HOST" "sh -s" <<EOSH
set -e
cd ~/$FC_DIR

cat > akuma-vm.json <<'EOJSON'
{
  "boot-source": { "kernel_image_path": "KERNEL_PATH", "boot_args": "" },
  "drives": $DRIVES_JSON,
  "network-interfaces": [],
  "machine-config": { "vcpu_count": $VCPUS, "mem_size_mib": $MEMORY }
}
EOJSON
sed -i "s|KERNEL_PATH|\$HOME/$FC_DIR/akuma-amd64|" akuma-vm.json
sed -i "s|DISK_PATH|\$HOME/$FC_DIR/disk.img|" akuma-vm.json

cat > run.sh <<'EORUN'
#!/bin/sh
# Boot Akuma/amd64 under Firecracker. Staged by amd64/run-firecracker.sh.
#
# Output goes to the terminal AND to ./boot.log (truncated each run).
#
# The kernel halts with \`cli; hlt\` rather than exiting, so Firecracker never
# returns on its own — hence the timeout. TIMEOUT=0 runs without one (Ctrl-C).
#
# \`timeout --foreground\` is load-bearing, not tidiness. Plain \`timeout\` puts the
# child in its OWN PROCESS GROUP, which stops it being the foreground group of
# the controlling terminal. Firecracker attaches guest serial input to stdin, so
# reading the TTY from a background process group raises SIGTTIN and the process
# stops dead right after printing its banner — the guest never runs and there is
# no error. The symptom is a single "Running Firecracker" line and nothing else,
# and it only reproduces on a real terminal: over a pipe (ssh with no -t) there
# is no controlling TTY and plain \`timeout\` works fine, which is a good way to
# lose an hour. \`--foreground\` leaves the child in the shell's process group.
set -e
cd "\$(dirname "\$0")"

FC=\$(command -v firecracker || echo "\$HOME/bin/firecracker")
[ -x "\$FC" ] || { echo "firecracker not found (looked in PATH and ~/bin)" >&2; exit 1; }

LOG=boot.log
TIMEOUT="\${TIMEOUT:-20}"

if [ "\$TIMEOUT" = "0" ]; then
    "\$FC" --no-api --config-file akuma-vm.json 2>&1 | tee "\$LOG"
else
    timeout --foreground "\$TIMEOUT" \
        "\$FC" --no-api --config-file akuma-vm.json 2>&1 | tee "\$LOG" || true
fi
echo "--- log written to \$(pwd)/\$LOG ---"
EORUN
chmod +x run.sh

TIMEOUT=$TIMEOUT ./run.sh
EOSH
