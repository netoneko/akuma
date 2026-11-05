#!/bin/bash
# Build the MINIMAL Akuma devbox image for local-AI kernel bring-up:
# the smallest ext2 rootfs that is immediately useful after `ssh` in —
# sshd (userspace, via herd) + busybox + a nightly rust/cargo toolchain,
# and the C toolchain cargo needs to link. Nothing else.
#
# Companion of bootstrap/public/kernel-bootstrap-with-local-ai/index.md —
# the guide a local AI agent follows to build a kernel and boot it; this
# image is what that kernel mounts as its rootfs.
#
# Design (deliberately a strict subset of overlays/devbox/bootstrap.sh):
#   - NO rump: the kernel runs the native smoltcp stack (box 0), so
#     /bin/rump_server is not shipped.
#   - NO demo content, NO /src repo clone, NO meow config: /bin gets exactly
#     {herd, sshd, busybox, apk, sh} plus busybox applet symlinks, and /etc
#     comes SOLELY from this overlay's rootfs/ (the base populate copies
#     nothing else; /etc is wiped then overlaid, same rule as the devbox).
#   - nightly rust+cargo from static.rust-lang.org -> /usr/local (the image's
#     Rust, first on PATH — the kernel's DEFAULT_ENV puts /usr/local ahead of
#     /usr). rustc + cargo + rust-std (musl host + aarch64-unknown-none) +
#     rust-src, so in-guest `cargo build -Z build-std` for the kernel target
#     works — that is what makes this a KERNEL devbox, not just a Rust one.
#   - C toolchain (clang lld gcc binutils make musl-dev) via apk into the
#     image: nightly cargo cannot link without a linker/cc. Not optional if
#     the toolchain should actually build anything.
#   - CA bundle via apk ca-certificates-bundle: cargo's bundled libcurl/OpenSSL
#     reads /etc/ssl/certs/ca-certificates.crt, and without it `cargo fetch`
#     against crates.io fails TLS verification.
#   - Publickey auth ONLY (a default userspace sshd build ignores
#     disable_key_verification). bootstrap.sh stages your pubkey into
#     /etc/sshd/authorized_keys automatically — see SIMPLE_KERNEL_SSH_PUBKEY.
#
# Env knobs (all optional):
#   SIMPLE_KERNEL_DISK          output image (default: simple-kernel-devbox.img)
#   SIMPLE_KERNEL_DISK_MB       image size in MB (default: 2560 — nightly
#                               toolchain + C toolchain + headroom for one
#                               in-guest cargo build; ~1.6 GB installs)
#   SIMPLE_KERNEL_BUILD_USERSPACE  rebuild herd + sshd from source (default:
#                               true; false reuses bootstrap/bin binaries)
#   SIMPLE_KERNEL_SSH_PUBKEY    path to the ed25519 pubkey staged into
#                               /etc/sshd/authorized_keys (default:
#                               ~/.ssh/id_ed25519.pub). MISSING key is not
#                               fatal: the image builds, the script tells you
#                               how to stage one afterwards.
#   SIMPLE_KERNEL_CA_CERTS      stage the TLS CA bundle (default true)
#   SIMPLE_KERNEL_RUST_TOOLCHAIN   C toolchain via apk (default true; false
#                               leaves cargo unable to link)
#   SIMPLE_KERNEL_NIGHTLY_RUST  nightly toolchain -> /usr/local (default true;
#                               false leaves NO Rust at all)
#
# Needs: Docker (loop-mount work happens in a throwaway alpine container),
# and the userspace build needs the nightly toolchain from userspace/.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$REPO_ROOT"

SIMPLE_KERNEL_DISK="${SIMPLE_KERNEL_DISK:-simple-kernel-devbox.img}"
SIMPLE_KERNEL_DISK_MB="${SIMPLE_KERNEL_DISK_MB:-2560}"
SIMPLE_KERNEL_BUILD_USERSPACE="${SIMPLE_KERNEL_BUILD_USERSPACE:-true}"
SIMPLE_KERNEL_SSH_PUBKEY="${SIMPLE_KERNEL_SSH_PUBKEY:-$HOME/.ssh/id_ed25519.pub}"

hr() { echo; echo "=== $* ==="; }

# ---------------------------------------------------------------------------
# 1. Userspace binaries: herd (supervisor) + sshd. busybox/apk ship prebuilt
#    in bootstrap/bin. rump_server is NOT needed — the smoltcp kernel never
#    spawns it.
# ---------------------------------------------------------------------------
if [ "$SIMPLE_KERNEL_BUILD_USERSPACE" = "true" ]; then
    hr "Building userspace members (herd sshd)"
    for m in herd sshd; do
        ( cd userspace && ./build.sh --"${m}"-only )
    done
else
    echo "Skipping userspace rebuild (SIMPLE_KERNEL_BUILD_USERSPACE=false); using existing bootstrap/bin"
fi

for b in herd sshd busybox apk; do
    [ -e "bootstrap/bin/$b" ] || { echo "ERROR: bootstrap/bin/$b missing (needed for the sshd boot path)"; exit 1; }
done

# ---------------------------------------------------------------------------
# 2. Create the image and populate /bin with EXACTLY the minimal set (not the
#    whole bootstrap/bin — most of those are acceptance-test probes) plus a
#    couple of busybox symlinks so the image can exec before step 4 lays the
#    full applet set. NO /usr from bootstrap/ (curl, tcc, llama, ... stay out
#    — minimal means minimal), and NO /etc from bootstrap/ (step 3 wipes it).
# ---------------------------------------------------------------------------
hr "Creating ${SIMPLE_KERNEL_DISK_MB}MB image $SIMPLE_KERNEL_DISK"
DISK="$SIMPLE_KERNEL_DISK" scripts/create_disk.sh "$SIMPLE_KERNEL_DISK_MB"

hr "Populating minimal /bin (herd sshd busybox apk + initial sh symlinks)"
docker run --rm --privileged \
    -v "$REPO_ROOT/$SIMPLE_KERNEL_DISK:/disk.img" \
    -v "$REPO_ROOT/bootstrap/bin:/bootstrap-bin:ro" \
    alpine:latest \
    sh -c '
        set -e
        mkdir -p /mnt/disk
        mount -o loop /disk.img /mnt/disk
        for b in herd sshd busybox apk; do
            cp -v /bootstrap-bin/$b /mnt/disk/bin/$b
            chmod 755 /mnt/disk/bin/$b
        done
        # busybox may be shipped as busybox.static; normalise the name.
        if [ ! -e /mnt/disk/bin/busybox ] && [ -e /mnt/disk/bin/busybox.static ]; then
            mv /mnt/disk/bin/busybox.static /mnt/disk/bin/busybox
        fi
        BB=busybox.static
        [ -e /mnt/disk/bin/$BB ] || BB=busybox
        # Enough applets for mount(8)-free boot + the overlay step itself.
        for cmd in sh cat echo ls mkdir chmod mount umount sync true false; do
            ln -sf $BB /mnt/disk/bin/$cmd 2>/dev/null || true
        done
        sync
        umount /mnt/disk
    '

# ---------------------------------------------------------------------------
# 3. Wipe the base /etc (create_disk/populate leave nothing, but stay
#    explicit — the overlay rootfs is the SOLE source of /etc, mirroring
#    overlays/devbox/bootstrap.sh step 3).
# ---------------------------------------------------------------------------
hr "Ensuring /etc comes only from this overlay (wipe + verify empty)"
docker run --rm --privileged \
    -v "$REPO_ROOT/$SIMPLE_KERNEL_DISK:/disk.img" \
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
# 4. Overlay the rootfs (the ONLY source of /etc) + full busybox applet set.
#    Never clobber a real binary with an applet symlink.
# ---------------------------------------------------------------------------
hr "Overlaying rootfs + full busybox applet symlinks"
docker run --rm --privileged \
    -v "$REPO_ROOT/$SIMPLE_KERNEL_DISK:/disk.img" \
    -v "$SCRIPT_DIR/rootfs:/overlay:ro" \
    alpine:latest \
    sh -c '
        set -e
        mkdir -p /mnt/disk
        mount -o loop /disk.img /mnt/disk
        cp -a /overlay/. /mnt/disk/

        BB=busybox
        [ -e /mnt/disk/bin/$BB ] || BB=busybox.static
        APPLETS="$(/mnt/disk/bin/$BB --list 2>/dev/null || true)"
        if [ -z "$APPLETS" ]; then
            echo "WARNING: could not enumerate busybox applets; laying a known-good subset" >&2
            APPLETS="awk sed grep egrep fgrep find xargs tar gzip gunzip zcat \
                less more head tail sort uniq wc cut tr nl \
                cat tac printf cp mv rm rmdir ln touch stat readlink realpath \
                basename dirname pwd env printenv date sleep sync truncate \
                chmod chown chgrp du df mount umount mknod mkfifo \
                ps top kill killall pgrep pkill nice free uptime \
                id whoami hostname uname clear tty stty \
                test true false yes seq expr dd \
                sha1sum sha256sum sha512sum md5sum cksum base64 \
                tee od hexdump cmp diff patch strings split \
                which mktemp getopt vi wget nc ping nslookup ifconfig route netstat ash"
        fi
        for app in $APPLETS; do
            if [ -e /mnt/disk/bin/$app ] && [ ! -L /mnt/disk/bin/$app ]; then
                continue
            fi
            ln -sf $BB /mnt/disk/bin/$app 2>/dev/null || true
        done
        sync
        umount /mnt/disk
    '

# ---------------------------------------------------------------------------
# 5. CA bundle — cargo (its bundled libcurl/OpenSSL) and any https fetch
#    verify peers against /etc/ssl/certs/ca-certificates.crt. Same apk
#    subpackage trick as the devbox: arch-independent, zero deps.
# ---------------------------------------------------------------------------
if [ "${SIMPLE_KERNEL_CA_CERTS:-true}" = "true" ]; then
    hr "Staging CA bundle (apk ca-certificates-bundle)"
    docker run --rm --privileged \
        -v "$REPO_ROOT/$SIMPLE_KERNEL_DISK:/disk.img" \
        alpine:latest \
        sh -c '
            set -e
            mkdir -p /mnt/disk
            mount -o loop /disk.img /mnt/disk
            apk add --root /mnt/disk --initdb --no-cache --allow-untrusted \
                --repository http://dl-cdn.alpinelinux.org/alpine/latest-stable/main \
                ca-certificates-bundle
            ls -la /mnt/disk/etc/ssl/certs/ca-certificates.crt
            sync
            umount /mnt/disk
        '
else
    echo "Skipping CA bundle (SIMPLE_KERNEL_CA_CERTS=false)"
fi

# ---------------------------------------------------------------------------
# 6. C toolchain via apk: clang lld gcc binutils make musl-dev. Without a
#    linker + libc headers, nightly cargo cannot link ANY binary. One apk
#    transaction (the devbox war story: separate apk add calls once reset
#    apk's wanted set and purged earlier packages — keep it one call).
#    NOTE: installs into the aarch64 rootfs from the container's apk —
#    /etc/apk/arch (aarch64) inside the mounted image decides the ABI, the
#    container's own arch is irrelevant.
# ---------------------------------------------------------------------------
if [ "${SIMPLE_KERNEL_RUST_TOOLCHAIN:-true}" = "true" ]; then
    hr "Installing C toolchain (clang lld gcc binutils make musl-dev)"
    docker run --rm --privileged \
        -v "$REPO_ROOT/$SIMPLE_KERNEL_DISK:/disk.img" \
        alpine:latest \
        sh -c '
            set -e
            mkdir -p /mnt/disk
            mount -o loop /disk.img /mnt/disk
            apk add --root /mnt/disk --no-cache --no-scripts --allow-untrusted \
                --repository https://dl-cdn.alpinelinux.org/alpine/latest-stable/main \
                clang lld gcc binutils make musl-dev
            for tool in clang ld.lld gcc make; do
                [ -x "/mnt/disk/usr/bin/$tool" ] || { echo "ERROR: /usr/bin/$tool missing after apk add" >&2; umount /mnt/disk; exit 1; }
            done
            sync
            umount /mnt/disk
        '
else
    echo "Skipping C toolchain (SIMPLE_KERNEL_RUST_TOOLCHAIN=false) — cargo will NOT be able to link"
fi

# ---------------------------------------------------------------------------
# 7. Nightly Rust toolchain (aarch64-unknown-linux-musl host) from
#    static.rust-lang.org -> /usr/local. Components: rustc, cargo,
#    rust-std for the musl host AND for aarch64-unknown-none (the kernel
#    target — `-Z build-std=core,alloc,compiler_builtins` for in-guest kernel
#    builds needs it + rust-src). This is the image's Rust; the kernel's
#    DEFAULT_ENV puts /usr/local first on PATH, so bare `cargo`/`rustc`
#    resolve here with no profile script.
# ---------------------------------------------------------------------------
if [ "${SIMPLE_KERNEL_NIGHTLY_RUST:-true}" = "true" ]; then
    hr "Installing nightly Rust (rustc cargo rust-std rust-src) -> /usr/local"
    docker run --rm --privileged \
        -v "$REPO_ROOT/$SIMPLE_KERNEL_DISK:/disk.img" \
        alpine:latest \
        sh -c '
            set -e
            mkdir -p /mnt/disk
            mount -o loop /disk.img /mnt/disk

            RUST_HOST=aarch64-unknown-linux-musl
            DIST=https://static.rust-lang.org/dist
            PREFIX=/mnt/disk/usr/local

            apk add --no-cache curl xz bash >/dev/null

            mkdir -p /tmp/rust
            for comp in \
                rustc-nightly-$RUST_HOST \
                cargo-nightly-$RUST_HOST \
                rust-std-nightly-$RUST_HOST \
                rust-std-nightly-aarch64-unknown-none \
                rust-src-nightly ; do
                echo "Downloading $comp ..."
                curl -fsSL "$DIST/$comp.tar.xz" -o /tmp/rust/$comp.tar.xz
                echo "Extracting $comp ..."
                xz -dc /tmp/rust/$comp.tar.xz | tar x -C /tmp/rust
                echo "Installing $comp -> $PREFIX ..."
                (cd /tmp/rust/$comp && ./install.sh --prefix="$PREFIX" --disable-ldconfig)
                rm -rf /tmp/rust/$comp /tmp/rust/$comp.tar.xz
            done

            echo "Nightly Rust installed:"
            ls /mnt/disk/usr/local/bin
            sync
            umount /mnt/disk
        '
else
    echo "Skipping nightly Rust (SIMPLE_KERNEL_NIGHTLY_RUST=false) — NO Rust on the image"
fi

# ---------------------------------------------------------------------------
# 8. Stage the SSH pubkey into /etc/sshd/authorized_keys. ed25519 ONLY (the
#    default sshd build rejects RSA and ignores disable_key_verification).
#    Without a staged key, sshd starts fine and every connection is refused
#    with "Permission denied (publickey)" — so a missing key is reported
#    loudly, not silently tolerated.
# ---------------------------------------------------------------------------
hr "Staging SSH pubkey"
if [ -f "$SIMPLE_KERNEL_SSH_PUBKEY" ]; then
    docker run --rm --privileged \
        -v "$REPO_ROOT/$SIMPLE_KERNEL_DISK:/disk.img" \
        -v "$SIMPLE_KERNEL_SSH_PUBKEY:/pubkey.pub:ro" \
        alpine:latest \
        sh -c '
            set -e
            mkdir -p /mnt/disk
            mount -o loop /disk.img /mnt/disk
            mkdir -p /mnt/disk/etc/sshd
            cp /pubkey.pub /mnt/disk/etc/sshd/authorized_keys
            chmod 644 /mnt/disk/etc/sshd/authorized_keys
            grep -q " ssh-ed25519 " /mnt/disk/etc/sshd/authorized_keys \
                || echo "WARNING: key does not look like ed25519 — a default sshd build will reject it" >&2
            sync
            umount /mnt/disk
        '
    echo "Staged $(basename "$SIMPLE_KERNEL_SSH_PUBKEY") -> /etc/sshd/authorized_keys"
else
    cat <<EOF
WARNING: no pubkey found at $SIMPLE_KERNEL_SSH_PUBKEY —
the image has NO /etc/sshd/authorized_keys, so sshd will refuse every
connection (publickey auth is mandatory in a default sshd build). Stage one
afterwards with:

docker run --rm --privileged \\
    -v "\$(pwd)/$SIMPLE_KERNEL_DISK:/disk.img" \\
    -v "\$HOME/.ssh/id_ed25519.pub:/pubkey.pub:ro" \\
    alpine:latest sh -c '
        mkdir -p /mnt/disk
        mount -o loop /disk.img /mnt/disk
        cp /pubkey.pub /mnt/disk/etc/sshd/authorized_keys
        chmod 644 /mnt/disk/etc/sshd/authorized_keys
        sync
        umount /mnt/disk'
EOF
fi

hr "Done: $SIMPLE_KERNEL_DISK ($SIMPLE_KERNEL_DISK_MB MB)"
ls -lh "$SIMPLE_KERNEL_DISK"
cat <<EOF

Next steps:
  1. Build the kernel:        scripts/build_devbox_smoltcp.sh   (or cargo build --release --features devbox-smoltcp,no-tests)
  2. Boot it with this disk:  overlays/simple-kernel-devbox/run.sh
  3. Connect:                 ssh -o StrictHostKeyChecking=no -p 2222 root@localhost
     (first boot generates the host key; after a REBUILD run: ssh-keygen -R "[localhost]:2222")

Guide for local AI agents: bootstrap/public/kernel-bootstrap-with-local-ai/index.md
EOF
