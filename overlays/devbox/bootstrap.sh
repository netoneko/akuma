#!/bin/bash
# Build a MINIMAL Akuma devbox image: enough to SSH in over the rump network stack,
# nothing more (the toolchains / rump-routed meow / repo clone are layered
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
#   DEVBOX_DISK_MB         image size in MB (default: 6144 — the nightly toolchain +
#                          C toolchain no longer fit in the old 1024 MB default now
#                          that DEVBOX_NIGHTLY_RUST defaults on. Left at 6144 rather
#                          than shrunk when stable Rust went default-off, since
#                          DEVBOX_STABLE_RUST=true must still fit)
#   DEVBOX_BUILD_USERSPACE rebuild herd + sshd from source (default: true; false uses
#                          the existing bootstrap/bin binaries)
#   DEVBOX_RUST_TOOLCHAIN  step 7: C toolchain (+ optional stable Rust). Default true.
#                          false drops the C TOOLCHAIN TOO — cargo cannot link after.
#   DEVBOX_STABLE_RUST     step 7: also install apk's stable rust/cargo alongside
#                          nightly. Default FALSE — one toolchain (nightly) is the
#                          default image. Set true to regain the dynamic-linker
#                          coverage apk's build provides; /usr/local/bin still wins.
#   DEVBOX_NIGHTLY_RUST    step 7b: nightly toolchain -> /usr/local. Default true.
#                          This is the image's Rust; false leaves none by default.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$REPO_ROOT"

DEVBOX_DISK="${DEVBOX_DISK:-devbox.img}"
DEVBOX_DISK_MB="${DEVBOX_DISK_MB:-6144}"
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

        # Drop the neatvi /bin/vi that came in with bootstrap/bin — busybox vi
        # (laid down as an applet symlink in step 4) is the devbox editor.
        rm -f /mnt/disk/bin/vi

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
                    ash hush vi"
            fi
            for app in $APPLETS; do
                # Never clobber a real (non-symlink) binary we ship (git,
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
# 7. C toolchain (clang/lld/gcc/binutils/make/musl-dev) that cargo's build scripts and
#    linking need, plus — only when DEVBOX_STABLE_RUST=true — Alpine's own stable
#    `rust`/`cargo` from apk.
#
#    **Stable Rust defaults OFF since 2026-08-19; nightly (step 7b) is the toolchain.**
#    Two toolchains on one image is confusing: `cargo` resolving to apk-stable while
#    `/usr/local/bin/cargo` is nightly meant every in-guest build command had to say
#    which one it meant, and a command that forgot silently got the other one. The
#    historical reason for the split is gone — step 7b's note records that nightly
#    `cargo` no longer crashes the EL0 handler (re-verified 2026-08-11) — so the
#    default is now one toolchain, nightly, first on PATH.
#
#    Why the flag still exists rather than deleting the stable install outright:
#    apk's `rust`/`cargo` is a completely different build from upstream's
#    static.rust-lang.org tarball and pulls all its own shared libs (LLVM, libcurl,
#    libssl, libsqlite3, ...), so it exercises the **dynamic linker** far harder than
#    the mostly-static nightly does. That is real coverage, and it is the only thing
#    on this image that provides it — keep it reachable with DEVBOX_STABLE_RUST=true.
#
#    Historical (why stable was ever the default): the nightly `cargo` binary used to
#    crash the kernel's EL0 exception handler on every invocation — `rustc --version`
#    worked, `cargo --version` faulted with EC=0x0 at a fixed instruction, reproducing
#    across every kernel build tried (main, that branch, both profiles, a kernel from
#    6 weeks earlier), which pointed at the cargo binary rather than the kernel. See
#    docs/archive/RUST_TOOLCHAIN_ISSUES.md §1.
#
#    All packages install in a single `apk add` transaction (matching step 6's fix):
#    doing it as separate `apk add` calls once reset apk's "wanted" set and purged
#    earlier steps' packages (see step 6's comment for the war story). That is why the
#    package list is assembled into one variable instead of a second `apk add` call.
#
#    Skip the whole step with DEVBOX_RUST_TOOLCHAIN=false — but note that drops the
#    **C toolchain** too, so cargo cannot link afterwards. To get nightly-only (the
#    default) leave this true and leave DEVBOX_STABLE_RUST false.
# ---------------------------------------------------------------------------
if [ "${DEVBOX_RUST_TOOLCHAIN:-true}" = "true" ]; then
    DEVBOX_APK_PKGS="clang lld gcc binutils make musl-dev"
    if [ "${DEVBOX_STABLE_RUST:-false}" = "true" ]; then
        DEVBOX_APK_PKGS="$DEVBOX_APK_PKGS rust cargo"
        hr "Installing C toolchain + apk stable rust/cargo (DEVBOX_STABLE_RUST=true)"
    else
        hr "Installing C toolchain (nightly-only image; DEVBOX_STABLE_RUST=false)"
    fi
    # `-e VAR` (no value) forwards VAR from the CALLER environment, and a plain
    # `VAR=...` assignment in bash is a shell variable, not an exported one — so the
    # bare form silently passed nothing and `apk add` ran with ZERO packages,
    # reporting a cheerful "OK: 19.5 MiB in 17 packages" (the pre-existing set,
    # unchanged) while installing no C toolchain at all. Measured 2026-08-19 on the
    # first real bootstrap after this step was split. Pass the value explicitly.
    docker run --rm --privileged \
        -v "$REPO_ROOT/$DEVBOX_DISK:/disk.img" \
        -e DEVBOX_APK_PKGS="$DEVBOX_APK_PKGS" \
        alpine:latest \
        sh -c '
            set -e
            mkdir -p /mnt/disk
            mount -o loop /disk.img /mnt/disk

            # An empty list is always a bug here, and `apk add` with no arguments
            # exits 0 — so it has to be caught rather than trusted.
            if [ -z "$DEVBOX_APK_PKGS" ]; then
                echo "ERROR: DEVBOX_APK_PKGS is empty - package list did not reach the container" >&2
                umount /mnt/disk
                exit 1
            fi

            echo "Installing into disk: $DEVBOX_APK_PKGS"
            apk --root /mnt/disk --no-scripts add $DEVBOX_APK_PKGS

            # Prove the C toolchain actually landed; a silent no-op here is what the
            # guard above exists to prevent, and cc/ld missing only shows up much
            # later as a cargo link failure inside the guest.
            for tool in clang ld.lld gcc make; do
                [ -x "/mnt/disk/usr/bin/$tool" ] || { echo "ERROR: /usr/bin/$tool missing after apk add" >&2; umount /mnt/disk; exit 1; }
            done

            # No profile.d PATH script, deliberately. This step used to write
            # /etc/profile.d/rust.sh with `PATH=/usr/bin:$PATH` — which NEVER RAN.
            # busybox ash sources /etc/profile only for LOGIN shells, this image has
            # no /etc/profile at all (/etc comes solely from overlays/devbox/rootfs,
            # which has none), and every harness here drives the VM through
            # `ssh host cmd`, i.e. non-login. Verified on a live guest 2026-08-19:
            # rust.sh contained `PATH=/usr/bin:$PATH` while the actual PATH was
            # /usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin.
            #
            # PATH order comes from the KERNEL: akuma_exec::process::types::DEFAULT_ENV
            # (crates/akuma-exec/src/process/types.rs), which puts /usr/local ahead of
            # /usr by design so a locally-installed tool wins over the distro copy.
            # NOTE: no apostrophes in this comment block - it lives inside a
            # single-quoted sh -c string, so one would close it and break the script.
            # That is exactly the nightly-over-apk-stable ordering we want, it applies
            # to every process rather than to login shells only, and it needs nothing
            # here. Removing any stale copy so it cannot start working later and
            # silently invert the order.
            rm -f /mnt/disk/etc/profile.d/rust.sh

            echo "Installed:"
            ls -la /mnt/disk/usr/bin/rustc /mnt/disk/usr/bin/cargo 2>/dev/null \
                || echo "  (no apk rust/cargo — nightly-only image, see step 7b)"
            sync
            umount /mnt/disk
        '
else
    echo "Skipping toolchain step (DEVBOX_RUST_TOOLCHAIN=false) - no C toolchain either"
fi

# ---------------------------------------------------------------------------
# 7b. Nightly Rust toolchain (aarch64-unknown-linux-musl host), downloaded straight from
#    static.rust-lang.org/dist and installed under /usr/local. Previously pulled out of
#    step 7 because nightly `cargo` was found to crash the kernel's EL0 handler on every
#    invocation (EC=0x0; see docs/archive/RUST_TOOLCHAIN_ISSUES.md §1) — `rustc` alone was
#    fine. Re-added here, default ON, 2026-08-11: `cargo new`/`cargo build`/running the
#    resulting binary all worked cleanly under devbox-smoltcp (HVF on) — the crash no
#    longer reproduces (see docs/runbooks/build-devbox.md's toolchain note).
#
#    **This is now THE toolchain.** Since 2026-08-19 step 7 no longer installs apk
#    stable rust/cargo by default (DEVBOX_STABLE_RUST=false), and its profile.d puts
#    /usr/local/bin first — so plain `cargo`/`rustc` are nightly, and no in-guest
#    command needs to spell out which toolchain it means. The previous note here said
#    the opposite ("still resolve to apk-stable by default; invoke
#    /usr/local/bin/{rustc,cargo} explicitly for nightly"); that is no longer true.
#    With DEVBOX_STABLE_RUST=true both are present and /usr/local/bin still wins.
#    Skip with DEVBOX_NIGHTLY_RUST=false (large download; offline builds) — on a
#    default image that leaves NO Rust toolchain at all.
# ---------------------------------------------------------------------------
if [ "${DEVBOX_NIGHTLY_RUST:-true}" = "true" ]; then
    hr "Installing nightly Rust toolchain (static.rust-lang.org, aarch64-musl host) -> /usr/local"
    docker run --rm --privileged \
        -v "$REPO_ROOT/$DEVBOX_DISK:/disk.img" \
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

            echo "Nightly Rust toolchain installed:"
            ls /mnt/disk/usr/local/bin
            sync
            umount /mnt/disk
        '
else
    echo "Skipping nightly Rust toolchain (DEVBOX_NIGHTLY_RUST=false)"
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
