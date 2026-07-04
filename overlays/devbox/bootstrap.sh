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
# 2. Create + base-populate the image with just bootstrap/bin + bootstrap/usr
#    (binaries, libs) — not bootstrap/etc (step 3 wipes /etc right after; no
#    point copying it) or the demo-content dirs (archives/models/music/public/
#    srv) the minimal SSH-over-rump image doesn't need.
#    A throwaway docker container does the mount+copy directly (not
#    scripts/populate_disk.sh) so this script owns its full behavior instead of
#    depending on that shared script's flag-mode side effects — see step 7's
#    history: reusing populate_disk.sh --overlay there once clobbered apk's
#    world file via a code path meant for something else entirely.
# ---------------------------------------------------------------------------
hr "Creating ${DEVBOX_DISK_MB}MB image $DEVBOX_DISK"
DISK="$DEVBOX_DISK" scripts/create_disk.sh "$DEVBOX_DISK_MB"

hr "Populating base binaries (bootstrap/bin + bootstrap/usr)"
docker run --rm --privileged \
    -v "$REPO_ROOT/$DEVBOX_DISK:/disk.img" \
    -v "$REPO_ROOT/bootstrap:/bootstrap:ro" \
    alpine:latest \
    sh -c '
        set -e
        mkdir -p /mnt/disk
        mount -o loop /disk.img /mnt/disk
        cp -rv /bootstrap/bin /mnt/disk/
        cp -rv /bootstrap/usr /mnt/disk/

        # git -> scratch for now; step 6 repoints /bin/git at the real
        # apk-installed binary. A handful of busybox symlinks so the image can
        # exec at all before step 4s full applet set is laid down.
        ln -sf scratch /mnt/disk/bin/git
        BB=busybox.static
        [ -e /mnt/disk/bin/$BB ] || BB=busybox
        if [ -e /mnt/disk/bin/$BB ]; then
            for cmd in sh chmod ls mkdir rm cat echo grep; do
                ln -sf $BB /mnt/disk/bin/$cmd 2>/dev/null || true
            done
        fi
        sync
        umount /mnt/disk
    '

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
docker run --rm --privileged \
    -v "$REPO_ROOT/$DEVBOX_DISK:/disk.img" \
    -v "$SCRIPT_DIR/rootfs:/overlay:ro" \
    alpine:latest \
    sh -c '
        set -e
        mkdir -p /mnt/disk
        mount -o loop /disk.img /mnt/disk
        cp -a /overlay/. /mnt/disk/

        BB=busybox.static
        [ -e /mnt/disk/bin/$BB ] || BB=busybox
        if [ -e /mnt/disk/bin/$BB ]; then
            APPLETS="$(/mnt/disk/bin/$BB --list 2>/dev/null || true)"
            if [ -z "$APPLETS" ]; then
                APPLETS="awk sed grep egrep fgrep find xargs tar gzip gunzip zcat bzip2 xz unxz cpio \
                    less more head tail sort uniq wc cut tr nl fold comm paste \
                    cat tac printf dir cp mv rm mkdir rmdir ln touch stat readlink realpath \
                    basename dirname pwd env printenv date sleep usleep sync truncate \
                    chmod chown chgrp du df mount umount losetup mknod mkfifo \
                    ps top kill killall pgrep pkill nice renice free uptime watch \
                    id whoami who groups hostname uname clear reset tty stty \
                    test true false yes seq expr dd shuf \
                    sha1sum sha256sum sha512sum md5sum cksum base64 \
                    tee od hexdump xxd cmp diff patch strings split \
                    which whereis mktemp getopt \
                    wget nc telnet ping traceroute nslookup ifconfig route netstat \
                    ash hush"
            fi
            for app in $APPLETS; do
                # Never clobber a real (non-symlink) binary we ship (git, vi->neatvi,
                # tcc, meow, curl, scratch, ...).
                if [ -e /mnt/disk/bin/$app ] && [ ! -L /mnt/disk/bin/$app ]; then
                    continue
                fi
                ln -sf $BB /mnt/disk/bin/$app 2>/dev/null || true
            done
        fi
        sync
        umount /mnt/disk
    '

# ---------------------------------------------------------------------------
# 5. Stage the TLS CA trust bundle so `curl https://host` (mbedTLS) can verify
#    peer certs. Install it via apk straight into the mounted image. Use the
#    `ca-certificates-bundle` subpackage on purpose: it is arch-independent and
#    has ZERO runtime dependencies, so it drops ONLY /etc/ssl/certs/
#    ca-certificates.crt (Mozilla's bundle, ~120 roots) — it does NOT drag in
#    musl/busybox/libcrypto3 and overwrite the image's own userspace the way the
#    `ca-certificates` meta-package would. `--initdb` bootstraps the minimal apk
#    database in the target (the image is not an Alpine system); `--allow-
#    untrusted` is needed because the fresh target has no alpine-keys yet.
#    Skip the whole step with DEVBOX_CA_CERTS=false (e.g. offline builds).
# ---------------------------------------------------------------------------
if [ "${DEVBOX_CA_CERTS:-true}" = "true" ]; then
    hr "Staging CA bundle (apk ca-certificates-bundle) for curl https"
    docker run --rm --privileged \
        -v "$REPO_ROOT/$DEVBOX_DISK:/disk.img" \
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
    echo "Skipping CA bundle (DEVBOX_CA_CERTS=false)"
fi

# ---------------------------------------------------------------------------
# 6. Install real `git` via apk (+ its `musl` dependency, which provides
#    /lib/ld-musl-aarch64.so.1 — the kernel's ELF loader already handles
#    PT_INTERP, see docs/APK_MISSING_SYSCALLS.md's "Dynamic Linking Support"
#    section). `--no-scripts` skips post-install triggers (there's no real
#    ldconfig to run against this image). git lands at /usr/bin/git; we then
#    repoint /bin/git at it, replacing the base populate's `git -> scratch`
#    symlink. scratch itself is untouched at /bin/scratch — still usable
#    directly, just no longer the default `git`.
#    Skip with DEVBOX_GIT=false to keep scratch as the default /bin/git.
# ---------------------------------------------------------------------------
if [ "${DEVBOX_GIT:-true}" = "true" ]; then
    hr "Installing git (apk) — scratch remains available at /bin/scratch"
    docker run --rm --privileged \
        -v "$REPO_ROOT/$DEVBOX_DISK:/disk.img" \
        alpine:latest \
        sh -c '
            set -e
            mkdir -p /mnt/disk
            mount -o loop /disk.img /mnt/disk
            apk add --root /mnt/disk --initdb --no-cache --no-scripts \
                --repository https://dl-cdn.alpinelinux.org/alpine/latest-stable/main \
                musl git
            ls -la /mnt/disk/usr/bin/git /mnt/disk/lib/ld-musl-aarch64.so.1
            ln -sf /usr/bin/git /mnt/disk/bin/git
            echo "/bin/git -> $(readlink /mnt/disk/bin/git)"
            sync
            umount /mnt/disk
        '
else
    echo "Skipping apk git install (DEVBOX_GIT=false); /bin/git stays symlinked to scratch"
fi

# ---------------------------------------------------------------------------
# 7. Rust toolchain (aarch64-unknown-linux-musl host, runs ON the devbox), installed
#    via apk — Alpine's own stable `rust`/`cargo` build — plus the C toolchain
#    (clang/lld/gcc/binutils/make/musl-dev) cargo's build scripts need. Previously this
#    step downloaded the nightly toolchain straight from static.rust-lang.org/dist; that
#    nightly `cargo` binary crashes the kernel's EL0 exception handler on every
#    invocation (see docs/RUST_TOOLCHAIN_ISSUES.md) — `rustc --version` works,
#    `cargo --version` faults with EC=0x0 at a fixed instruction, reproducing
#    identically across every kernel build tried (main, this branch, both profiles,
#    a kernel from 6 weeks earlier), which points at the nightly cargo binary itself
#    rather than the kernel. apk's stable `rust`/`cargo` is a completely different
#    build (Alpine's own, not upstream's static.rust-lang.org tarball) and installs
#    all its own shared-lib deps (LLVM, libcurl, libssl, libsqlite3, ...), so this
#    also exercises the dynamic linker much harder than the mostly-static nightly did.
#    All packages install in a single `apk add` transaction (matching step 6's fix):
#    doing it as separate `apk add` calls once reset apk's "wanted" set and purged
#    earlier steps' packages (see step 6's comment for the war story).
#    Skip with DEVBOX_RUST_TOOLCHAIN=false (large download; offline builds).
# ---------------------------------------------------------------------------
if [ "${DEVBOX_RUST_TOOLCHAIN:-true}" = "true" ]; then
    hr "Installing Rust toolchain (apk: rust + cargo, Alpine's stable aarch64-musl build)"
    docker run --rm --privileged \
        -v "$REPO_ROOT/$DEVBOX_DISK:/disk.img" \
        alpine:latest \
        sh -c '
            set -e
            mkdir -p /mnt/disk
            mount -o loop /disk.img /mnt/disk

            echo "Installing C toolchain + rust + cargo into disk..."
            apk --root /mnt/disk --no-scripts add clang lld gcc binutils make musl-dev rust cargo

            mkdir -p /mnt/disk/etc/profile.d
            printf "export PATH=/usr/bin:\$PATH\n" > /mnt/disk/etc/profile.d/rust.sh

            echo "Rust toolchain installed:"
            ls -la /mnt/disk/usr/bin/rustc /mnt/disk/usr/bin/cargo
            sync
            umount /mnt/disk
        '
else
    echo "Skipping Rust toolchain (DEVBOX_RUST_TOOLCHAIN=false)"
fi

# ---------------------------------------------------------------------------
# 8. Optional soundtrack (bootstrap/music) — pure bonus content, off by default.
#    Skip (default) leaves the image without it; set DEVBOX_SOUNDTRACK=true to
#    include it, e.g. DEVBOX_SOUNDTRACK=true overlays/devbox/bootstrap.sh
# ---------------------------------------------------------------------------
if [ "${DEVBOX_SOUNDTRACK:-false}" = "true" ]; then
    hr "Copying soundtrack (bootstrap/music)"
    docker run --rm --privileged \
        -v "$REPO_ROOT/$DEVBOX_DISK:/disk.img" \
        -v "$REPO_ROOT/bootstrap/music:/music:ro" \
        alpine:latest \
        sh -c '
            set -e
            mkdir -p /mnt/disk
            mount -o loop /disk.img /mnt/disk
            mkdir -p /mnt/disk/music
            cp -rv /music/. /mnt/disk/music/
            sync
            umount /mnt/disk
        '
else
    echo "Skipping soundtrack (DEVBOX_SOUNDTRACK=false; set true to include bootstrap/music)"
fi

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
