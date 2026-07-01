#!/bin/bash
# Build the Akuma devbox image end-to-end: userspace binaries (incl. a rump-routed meow and
# neatvi) -> create ext2 image -> populate base (apk + musl-dev + Rust toolchain) -> prune
# native-stack services -> overlay devbox config + full busybox symlinks -> clone the full
# Akuma repo (with submodules) into /src. See overlays/devbox/README.md.
#
# Env knobs (all optional):
#   DEVBOX_DISK            output image (default: devbox.img)
#   DEVBOX_DISK_MB         image size in MB (default: 12288)
#   AKUMA_GIT_URL          repo to clone into /src (default: git@github.com:netoneko/akuma.git;
#                          set to a local path or file://... for an offline clone)
#   GITHUB_PAT             if set, injected into an https github clone URL for private repos
#   DEVBOX_ALL_SUBMODULES  clone every submodule incl. large src-netbsd (default: true;
#                          set false for a lighter image with just what builds meow)
#   DEVBOX_BUILD_USERSPACE rebuild the devbox userspace members (default: true)
#   NEATVI_URL             neatvi upstream (default: https://github.com/aligrudi/neatvi.git)
#   CROSS_CC / CROSS_STRIP aarch64 static-musl cross toolchain (default: aarch64-linux-musl-*)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$REPO_ROOT"

DEVBOX_DISK="${DEVBOX_DISK:-devbox.img}"
DEVBOX_DISK_MB="${DEVBOX_DISK_MB:-12288}"
AKUMA_GIT_URL="${AKUMA_GIT_URL:-git@github.com:netoneko/akuma.git}"
GITHUB_PAT="${GITHUB_PAT:-}"
DEVBOX_ALL_SUBMODULES="${DEVBOX_ALL_SUBMODULES:-true}"
DEVBOX_BUILD_USERSPACE="${DEVBOX_BUILD_USERSPACE:-true}"
NEATVI_URL="${NEATVI_URL:-https://github.com/aligrudi/neatvi.git}"
CROSS_CC="${CROSS_CC:-aarch64-linux-musl-gcc}"
CROSS_STRIP="${CROSS_STRIP:-aarch64-linux-musl-strip}"

STAGE="$SCRIPT_DIR/.stage"
mkdir -p "$STAGE"

hr() { echo; echo "=== $* ==="; }

# ---------------------------------------------------------------------------
# 1. Userspace binaries
# ---------------------------------------------------------------------------
if [ "$DEVBOX_BUILD_USERSPACE" = "true" ]; then
    hr "Building devbox userspace members (herd sshd wavplay scratch tcc)"
    for m in herd sshd wavplay scratch tcc; do
        ( cd userspace && ./build.sh --"${m}"-only )
    done

    hr "Building meow for the Linux ABI (rump-routed: DNS+TLS through the box)"
    # meow is a freestanding no_std binary with its own _start, so it links with rust-lld
    # directly (no crt/libc/gcc) and build-std supplies the mem intrinsics. --features
    # linux-net swaps the custom RESOLVE_HOST DNS for UDP-socket DNS that the kernel
    # intercepts into the rump box; its TLS sockets (libakuma-tls) already ride the same
    # intercepted linux socket syscalls. See overlays/devbox/README.md.
    (
        cd userspace/meow
        CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUSTFLAGS="-Clinker=rust-lld -Clinker-flavor=ld.lld -Clink-self-contained=no" \
        cargo build --release --target aarch64-unknown-linux-musl --features linux-net \
            -Z build-std=core,alloc,compiler_builtins -Z build-std-features=compiler-builtins-mem
    )
    cp userspace/meow/target/aarch64-unknown-linux-musl/release/meow bootstrap/bin/meow
    echo "Staged rump-routed meow -> bootstrap/bin/meow"
else
    echo "Skipping userspace rebuild (DEVBOX_BUILD_USERSPACE=false); using existing bootstrap/bin"
fi

# ---------------------------------------------------------------------------
# 2. neatvi (editor) — cross-compiled with the repo's static-musl C compiler
# ---------------------------------------------------------------------------
hr "Building neatvi with $CROSS_CC"
NEATVI_SRC="$STAGE/neatvi"
rm -rf "$NEATVI_SRC"
git clone --depth 1 "$NEATVI_URL" "$NEATVI_SRC"
NEATVI_COMMIT="$(git -C "$NEATVI_SRC" rev-parse HEAD)"
make -C "$NEATVI_SRC" CC="$CROSS_CC" CFLAGS="-O2 -static" LDFLAGS="-static"
"$CROSS_STRIP" "$NEATVI_SRC/vi" 2>/dev/null || true
mkdir -p bootstrap/bin
cp "$NEATVI_SRC/vi" bootstrap/bin/vi
echo "neatvi $NEATVI_COMMIT -> bootstrap/bin/vi"

# ---------------------------------------------------------------------------
# 3. Create + base-populate the image
# ---------------------------------------------------------------------------
hr "Creating ${DEVBOX_DISK_MB}MB image $DEVBOX_DISK"
DISK="$DEVBOX_DISK" scripts/create_disk.sh "$DEVBOX_DISK_MB"

hr "Populating base (bootstrap + apk + musl-dev + Rust toolchain)"
DISK="$DEVBOX_DISK" scripts/populate_disk.sh --with-apk --with-musl-dev --with-rust-toolchain

# ---------------------------------------------------------------------------
# 4. Prune native-stack services so rump is the only stack (single kernel)
# ---------------------------------------------------------------------------
hr "Pruning native httpd / core2 herd services from enabled set"
docker run --rm --privileged \
    -v "$REPO_ROOT/$DEVBOX_DISK:/disk.img" \
    alpine:latest \
    sh -c '
        set -e
        mkdir -p /mnt/disk
        mount -o loop /disk.img /mnt/disk
        rm -f /mnt/disk/etc/herd/enabled/httpd.conf \
              /mnt/disk/etc/herd/enabled/core2herd.conf
        echo "Enabled herd services now:"
        ls /mnt/disk/etc/herd/enabled/ 2>/dev/null || true
        sync
        umount /mnt/disk
    '

# ---------------------------------------------------------------------------
# 5. Overlay devbox config (rump-only herd, meow->z.ai) + full busybox symlinks
# ---------------------------------------------------------------------------
hr "Overlaying devbox rootfs + full busybox applet symlinks"
DISK="$DEVBOX_DISK" scripts/populate_disk.sh --overlay "$SCRIPT_DIR/rootfs" --full-busybox

# ---------------------------------------------------------------------------
# 6. Clone the full Akuma repo (with submodules) into /src
# ---------------------------------------------------------------------------
hr "Cloning $AKUMA_GIT_URL into the image at /src/github.com/netoneko/akuma"
SRC_STAGE="$STAGE/srctree"
DEST="$SRC_STAGE/src/github.com/netoneko/akuma"
rm -rf "$SRC_STAGE"
mkdir -p "$(dirname "$DEST")"

CLONE_URL="$AKUMA_GIT_URL"
if [ -n "$GITHUB_PAT" ]; then
    case "$CLONE_URL" in
        https://github.com/*)
            CLONE_URL="https://${GITHUB_PAT}@github.com/${CLONE_URL#https://github.com/}" ;;
    esac
fi

if [ "$DEVBOX_ALL_SUBMODULES" = "true" ]; then
    echo "NOTE: cloning ALL submodules (incl. large src-netbsd) — this is slow and sizable."
    echo "      Set DEVBOX_ALL_SUBMODULES=false for a lighter image (still builds meow)."
    git clone --depth 1 --recurse-submodules --shallow-submodules "$CLONE_URL" "$DEST"
else
    git clone --depth 1 "$CLONE_URL" "$DEST"
    # Minimum needed to build meow from source in-VM.
    git -C "$DEST" submodule update --init --depth 1 userspace/meow userspace/tcc/tinycc || \
        echo "WARN: submodule init failed (meow source may be missing in-VM)"
fi

hr "Overlaying /src tree into the image (large — copying via Docker loop mount)"
DISK="$DEVBOX_DISK" scripts/populate_disk.sh --overlay "$SRC_STAGE"

# ---------------------------------------------------------------------------
# Done
# ---------------------------------------------------------------------------
hr "Devbox image ready: $DEVBOX_DISK"
cat <<EOF

Next:
  overlays/devbox/run.sh                 # boot (single kernel, RUMP_NIC=1, 4GB RAM)
  # wait for the rump DHCP lease + "[SSH Server] Listening", then:
  ssh -o StrictHostKeyChecking=no -p 2223 root@localhost

Inside the VM, fill in secrets (see /root/DEVBOX.txt):
  - z.ai API key   -> /etc/meow/config (uncomment + set api_key)
  - GitHub PAT     -> scratch config credential.token ghp_...
EOF
