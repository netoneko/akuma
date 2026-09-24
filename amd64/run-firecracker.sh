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
#   FC_HOST=user@host amd64/run-firecracker.sh                  # kernel + a fresh disk
#   FC_HOST=user@host FC_KEEP_DISK=1 amd64/run-firecracker.sh   # kernel only
#
# Everything it writes lives under one directory on the host (FC_DIR, default
# ~/akuma), including a standalone `run.sh` so the VM can be re-launched there
# without this script or the dev machine.
#
# **The first form replaces the host's `disk.img`, `akuma-vm.json` and
# `run.sh`.** That is right for a scratch host and wrong for one somebody has
# configured. The second form writes the kernel ELF and nothing else. See the
# note on `FC_KEEP_DISK` below.
set -e

# Which of the tunables the caller actually set, captured before the `${X:-…}`
# defaults below make every one of them non-empty. `FC_KEEP_DISK` reports the
# ones it is about to ignore, and reporting a *default* as ignored would be
# three lines of noise on every run — which is how a real warning stops being
# read. `${X+set}` is "was it set", including to the empty string.
for _v in VCPUS MEMORY INIT FC_NET DISK; do
    eval "_SET_$_v=\${$_v+set}"
done

HERE=$(dirname "$0")
cd "$HERE/.."

FC_HOST="${FC_HOST:?set FC_HOST=user@host}"
FC_KEY="${FC_KEY:-$HOME/.ssh/id_ed25519}"
FC_DIR="${FC_DIR:-akuma}"        # relative to the remote $HOME
MEMORY="${MEMORY:-2048}"
VCPUS="${VCPUS:-1}"
TIMEOUT="${TIMEOUT:-20}"
# FC_NET=1 attaches a virtio-net device on the host tap `FC_TAP` (default tap0).
# Run `amd64/net-setup.sh` first — it creates the tap, a dnsmasq DHCP server and
# NAT, all on the same 10.0.2.0/24 SLIRP addresses QEMU uses. The guest MAC has
# to match net-setup.sh's `--dhcp-host` or the pinned lease never applies.
FC_NET="${FC_NET:-}"
FC_TAP="${FC_TAP:-tap0}"
FC_GUEST_MAC="${FC_GUEST_MAC:-02:FC:00:00:00:01}"
# INIT= picks the program the kernel runs after the self-tests (init= on the
# kernel command line). Default paws; `INIT=/bin/httpd` for the server.
INIT="${INIT:-/bin/paws}"
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
# FC_KEEP_DISK=1 stages the KERNEL ONLY and touches nothing else the host owns:
# no image is built, `disk.img` is left alone, and **`akuma-vm.json` and
# `run.sh` are left alone too** — the host's own launcher is used as it stands.
#
# **Use this against any host whose setup is configured.** The default path
# rebuilds a 128 MB rootfs and scps it over `$FC_DIR/disk.img`, *and* regenerates
# the VM config from this script's templates — so vcpus, memory, networking and
# `init=` all revert to this script's defaults (`INIT` is `/bin/paws`, which is
# not what a herd-supervised host wants). On a box someone staged, both halves
# are destructive.
#
# `DISK=none` is NOT the alternative — that means "no drive at all", so the
# guest comes up with no rootfs. Cost the Ryzen laptop's disk on 2026-09-20;
# see `docs/runbooks/amd64-bare-metal-loop.md` § "Rules that cost time to learn".
#
# Because the host's config is used verbatim, `VCPUS`/`MEMORY`/`INIT`/`FC_NET`
# are **ignored** in this mode — the script says so rather than appearing to
# honour them. Change those on the host's own `akuma-vm.json`, or drop
# `FC_KEEP_DISK` and pass the full set.
FC_KEEP_DISK="${FC_KEEP_DISK:-}"
# FC_RESTART=1 (implies FC_KEEP_DISK) is for a host whose guest *runs*, not one
# that gets test boots: the ryzen laptop, where the guest is a herd-supervised
# kot member launched as `TIMEOUT=0 ./run.sh` and left up. There the default
# ending — `./run.sh` under a 20 s timeout, in the foreground — would stop a
# service after 20 s. Instead:
#
#   1. the kernel it replaces is kept as `akuma-amd64.prev`,
#   2. the guest running from `$FC_DIR` is stopped — a hard power-off, since
#      `run.sh` launches with `--no-api` and Firecracker has no other way to
#      stop it, so sync the guest first if what it's writing matters,
#   3. `run.sh` is relaunched detached with `TIMEOUT=0`, and the script waits
#      for herd's first "Started" line in `boot.log`, then prints the tail.
#
#   FC_HOST=netoneko@ryzen FC_RESTART=1 amd64/run-firecracker.sh
FC_RESTART="${FC_RESTART:-}"
[ -n "$FC_RESTART" ] && FC_KEEP_DISK=1
KERNEL=target/x86_64-unknown-none/release/akuma-amd64

SSH="ssh -o StrictHostKeyChecking=no -i $FC_KEY"

cargo build -p akuma-amd64 --target x86_64-unknown-none --release

$SSH "$FC_HOST" "mkdir -p ~/$FC_DIR"
if [ -n "$FC_RESTART" ]; then
    # `cp`, not `mv`: the running guest already has its kernel in memory, but
    # a copy leaves the file in place if the scp below fails.
    $SSH "$FC_HOST" "cd ~/$FC_DIR && { [ ! -f akuma-amd64 ] || cp -p akuma-amd64 akuma-amd64.prev; }"
fi
scp -q -o StrictHostKeyChecking=no -i "$FC_KEY" "$KERNEL" "$FC_HOST:$FC_DIR/akuma-amd64"

# ── FC_KEEP_DISK: the kernel is the only thing this script is allowed to write ──
#
# An early exit rather than a branch threaded through the staging below, because
# the point is that none of that staging runs. Everything past this block writes
# something the host owns.
if [ -n "$FC_KEEP_DISK" ]; then
    # Refuse rather than boot something subtly different. "Keep the host's
    # setup" is only meaningful if the host has one, and the three files below
    # are what that means; a missing `disk.img` would otherwise boot diskless
    # and a missing `akuma-vm.json` would have nothing to boot from at all.
    for f in disk.img akuma-vm.json run.sh; do
        if ! $SSH "$FC_HOST" "test -f \$HOME/$FC_DIR/$f"; then
            echo "FC_KEEP_DISK=1 but $FC_HOST:$FC_DIR/$f does not exist" >&2
            echo "  (that host has no staged setup to keep — run without" >&2
            echo "   FC_KEEP_DISK once to create one, on a host you may overwrite)" >&2
            exit 1
        fi
    done
    # Say what is being dropped. An env var that looks honoured and is not is
    # how you conclude a change did nothing when it was never applied.
    # `if`, not `[ … ] && echo`: under `set -e` the false branch of an AND-list
    # is the exit status of the loop body, and that is not worth reasoning about.
    for v in VCPUS MEMORY INIT FC_NET DISK; do
        eval "was=\${_SET_$v:-}"
        eval "val=\${$v:-}"
        if [ "$was" = set ]; then
            echo "note: $v=$val ignored — FC_KEEP_DISK=1 uses the host's akuma-vm.json"
        fi
    done
    echo "keeping $FC_HOST:$FC_DIR/{disk.img,akuma-vm.json,run.sh} as they are"
    if [ -n "$FC_RESTART" ]; then
        # Only the Firecracker whose cwd is this $FC_DIR — a host can run
        # others. Its `run.sh` parent exits on its own once the pipe closes.
        # The relaunch is `setsid nohup … < /dev/null`: detached from this
        # ssh session, and with no terminal for serial input to stop it
        # (see the SIGTTIN note in run.sh).
        $SSH "$FC_HOST" "sh -s" <<EOSH
set -e
cd ~/$FC_DIR
for p in \$(pgrep -u "\$(id -un)" -x firecracker); do
    if [ "\$(readlink /proc/\$p/cwd)" = "\$(pwd)" ]; then
        echo "stopping guest (firecracker pid \$p)"
        kill "\$p"
        for _ in 1 2 3 4 5 6 7 8 9 10; do kill -0 "\$p" 2>/dev/null || break; sleep 1; done
        kill -9 "\$p" 2>/dev/null || true
    fi
done
# Aside, not left for tee to truncate: the wait below greps boot.log, and
# the old boot's "Started" lines would end it before the new one began.
[ ! -f boot.log ] || mv boot.log boot.log.prev
TIMEOUT=0 setsid nohup ./run.sh >/dev/null 2>&1 < /dev/null &
echo "relaunched: TIMEOUT=0 ./run.sh (log: ~/$FC_DIR/boot.log)"
for _ in \$(seq 1 60); do
    grep -a -q '\[herd\] Started' boot.log 2>/dev/null && break
    sleep 2
done
tail -20 boot.log
EOSH
        exit 0
    fi
    # `|| rc=$?` so a non-zero from the guest's launcher is reported rather than
    # tripping `set -e` on the way to reporting it.
    rc=0
    $SSH "$FC_HOST" "cd ~/$FC_DIR && TIMEOUT=$TIMEOUT ./run.sh" || rc=$?
    exit "$rc"
fi

# The drives array, built here so the JSON below stays a fixed template.
DRIVES_JSON="[]"
if [ -z "$DISK" ]; then
    DISK=target/x86_64-unknown-none/release/amd64-root.img
    # 128, matching `run.sh` and `mkdisk.sh`'s own default. This said 8 until
    # 2026-09-05, from before the image grew to hold busybox/apk/sshd: the
    # build silently ran out of space and the guest failed 13 self-tests with
    # "bad ELF identification" and "persist failed: no space" — none of which
    # named the disk. The QEMU path never saw it because `run.sh` was updated.
    sh "$HERE/mkdisk.sh" "$DISK" 128 >/dev/null
fi
[ "$DISK" = "none" ] && DISK=""
if [ -n "$DISK" ]; then
    [ -f "$DISK" ] || { echo "DISK=$DISK does not exist" >&2; exit 1; }
    scp -q -o StrictHostKeyChecking=no -i "$FC_KEY" "$DISK" "$FC_HOST:$FC_DIR/disk.img"
    DRIVES_JSON='[{"drive_id":"rootfs","path_on_host":"DISK_PATH","is_root_device":false,"is_read_only":false}]'
fi

# The network-interfaces array. Firecracker auto-appends a
# `virtio_mmio.device=<size>@<base>:<irq>` token to the kernel command line for
# every configured MMIO device, drive and NIC alike, in creation order — so the
# drive lands on slot 0 and the NIC on slot 1, which is what the probe expects.
NET_JSON="[]"
if [ -n "$FC_NET" ]; then
    NET_JSON="[{\"iface_id\":\"eth0\",\"host_dev_name\":\"$FC_TAP\",\"guest_mac\":\"$FC_GUEST_MAC\"}]"
fi

# `init=` goes in boot_args; Firecracker appends its device tokens after it.
# Passed unconditionally, as `run.sh` does, so the two stands-in agree on which
# program gets the console.
BOOT_ARGS="init=$INIT"

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
  "boot-source": { "kernel_image_path": "KERNEL_PATH", "boot_args": "BOOT_ARGS_VAL" },
  "drives": $DRIVES_JSON,
  "network-interfaces": $NET_JSON,
  "machine-config": { "vcpu_count": $VCPUS, "mem_size_mib": $MEMORY }
}
EOJSON
sed -i "s|KERNEL_PATH|\$HOME/$FC_DIR/akuma-amd64|" akuma-vm.json
sed -i "s|DISK_PATH|\$HOME/$FC_DIR/disk.img|" akuma-vm.json
sed -i "s|BOOT_ARGS_VAL|$BOOT_ARGS|" akuma-vm.json

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
