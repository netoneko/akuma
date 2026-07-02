#!/bin/bash
# Build a MINIMAL Akuma devbox image: enough to SSH in over the rump network stack,
# nothing more (the toolchains / neatvi / rump-routed meow / repo clone are layered
# back on later — "start with ssh via rumpnet only, then add the rest").
#
# Design for this step:
#   - The kernel (built by overlays/devbox/run.sh with the `devbox` profile+feature)
#     makes the rump stack the DEFAULT for box 0 and runs no built-in SSH. So the
#     image just needs: /bin/rump_server (box 0's stack), /bin/herd (supervisor),
#     /bin/sshd (userspace SSH), and a busybox shell.
#   - /etc comes from the OVERLAY ONLY. The base populate copies bootstrap/*
#     (binaries, libs), but we then WIPE /etc entirely and let overlays/devbox/rootfs
#     be the sole source of /etc/herd, /etc/sshd, etc. Nothing from bootstrap/etc/ is
#     inherited unreviewed.
#
# Env knobs (all optional):
#   DEVBOX_DISK            output image (default: devbox.img)
#   DEVBOX_DISK_MB         image size in MB (default: 1024; bumped when the toolchain
#                          + /src tree are added back)
#   DEVBOX_BUILD_USERSPACE rebuild herd + sshd from source (default: true; false uses
#                          the existing bootstrap/bin binaries)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$REPO_ROOT"

DEVBOX_DISK="${DEVBOX_DISK:-devbox.img}"
DEVBOX_DISK_MB="${DEVBOX_DISK_MB:-1024}"
DEVBOX_BUILD_USERSPACE="${DEVBOX_BUILD_USERSPACE:-true}"

hr() { echo; echo "=== $* ==="; }

# ---------------------------------------------------------------------------
# 1. Userspace binaries needed for SSH-over-rump. rump_server + busybox ship
#    prebuilt in bootstrap/bin; herd + sshd are workspace members we rebuild.
# ---------------------------------------------------------------------------
if [ "$DEVBOX_BUILD_USERSPACE" = "true" ]; then
    hr "Building devbox userspace members (herd sshd)"
    for m in herd sshd; do
        ( cd userspace && ./build.sh --"${m}"-only )
    done
else
    echo "Skipping userspace rebuild (DEVBOX_BUILD_USERSPACE=false); using existing bootstrap/bin"
fi

for b in rump_server herd sshd busybox sh; do
    [ -e "bootstrap/bin/$b" ] || { echo "ERROR: bootstrap/bin/$b missing (needed for SSH-over-rump)"; exit 1; }
done

# ---------------------------------------------------------------------------
# 2. Create + base-populate the image (binaries, libs). This copies bootstrap/*,
#    including bootstrap/etc — which step 3 then wipes so /etc is overlay-only.
# ---------------------------------------------------------------------------
hr "Creating ${DEVBOX_DISK_MB}MB image $DEVBOX_DISK"
DISK="$DEVBOX_DISK" scripts/create_disk.sh "$DEVBOX_DISK_MB"

hr "Populating base binaries (bootstrap/*)"
DISK="$DEVBOX_DISK" scripts/populate_disk.sh

# ---------------------------------------------------------------------------
# 3. Wipe the base /etc so the devbox overlay is the SOLE source of /etc — no
#    stale bootstrap/etc herd/sshd/meow configs survive.
# ---------------------------------------------------------------------------
hr "Wiping base /etc (devbox /etc comes from the overlay only)"
docker run --rm --privileged \
    -v "$REPO_ROOT/$DEVBOX_DISK:/disk.img" \
    alpine:latest \
    sh -c '
        set -e
        mkdir -p /mnt/disk
        mount -o loop /disk.img /mnt/disk
        rm -rf /mnt/disk/etc
        sync
        umount /mnt/disk
    '

# ---------------------------------------------------------------------------
# 4. Overlay the devbox rootfs (the ONLY source of /etc) + full busybox applets.
# ---------------------------------------------------------------------------
hr "Overlaying devbox rootfs + full busybox applet symlinks"
DISK="$DEVBOX_DISK" scripts/populate_disk.sh --overlay "$SCRIPT_DIR/rootfs" --full-busybox

# ---------------------------------------------------------------------------
# Done
# ---------------------------------------------------------------------------
hr "Minimal devbox image ready: $DEVBOX_DISK"
cat <<EOF

Next:
  overlays/devbox/run.sh                 # boot (devbox profile: rump default stack, no built-in ssh, RUMP_NIC=1)
  # wait for box 0's rump DHCP lease + herd starting sshd, then:
  ssh -o StrictHostKeyChecking=no -p 2223 root@localhost
EOF
