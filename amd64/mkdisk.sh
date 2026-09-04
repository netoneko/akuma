#!/bin/sh
# Build the amd64 target's ext2 disk image.
#
# Replaces the raw two-sector probe disk `mkdisk.py` made. That disk existed to
# prove the DMA path end to end before there was a filesystem; now the
# filesystem proves it better, and the low-level check reads the **ext2
# superblock** instead — a structure the next layer up is about to parse, rather
# than a pattern invented for the test.
#
# # No Docker, no mount, no root
#
# `mkfs.ext2` creates the image and `debugfs -w -R write` puts files in it. Both
# ship with e2fsprogs and neither needs to mount anything, which is what makes
# this work unprivileged on macOS. `scripts/populate_disk.sh` uses Docker for the
# aarch64 image because that one holds a whole distro; this holds a handful of
# files and does not need the machinery.
#
#   amd64/mkdisk.sh [image] [size-mib]
set -e

HERE=$(dirname "$0")
cd "$HERE/.."

IMG="${1:-target/x86_64-unknown-none/release/amd64-root.img}"
# 8 MiB was enough before a real static libc existed on this image; musl's
# `libc.a` alone (staged 2026-09-04 so tcc can link a real `printf` — see the
# musl staging block below) is ~9.4 MiB by itself, over the old default before
# adding a single byte of anything else. 32 MiB covered that, `/usr/include`'s
# ~200 files, and everything already here — but not a real `apk add`: a single
# `curl` install (14 packages: ca-certificates, musl, brotli-libs, c-ares,
# libcrypto3, libunistring, libidn2, nghttp2-libs, libpsl, libssl3, zlib,
# zstd-libs, libcurl, curl) ran the disk to `ENOSPC` partway through,
# discovered 2026-09-04 via `[close] persist failed for "...": no space` on
# the serial console — the real error, three layers back from what `apk`
# itself reported (`failed to commit …: No such file or directory`, its
# rename() finding no tmp file because the write that should have created it
# had already failed silently at `close(2)`; see that function's own comment
# in `amd64/src/fd.rs`). 128 MiB leaves real headroom for a handful of
# dependency chains like that one, not just the base image.
SIZE_MIB="${2:-128}"

# e2fsprogs is keg-only under Homebrew, so its tools are not on PATH by default.
find_tool() {
    for p in "$1" "/opt/homebrew/opt/e2fsprogs/sbin/$1" "/usr/local/sbin/$1" "/sbin/$1"; do
        command -v "$p" >/dev/null 2>&1 && { echo "$p"; return 0; }
    done
    return 1
}
MKFS=$(find_tool mkfs.ext2) || {
    echo "mkfs.ext2 not found. On macOS: brew install e2fsprogs" >&2
    exit 1
}
DEBUGFS=$(find_tool debugfs) || {
    echo "debugfs not found (it ships with e2fsprogs)" >&2
    exit 1
}

# The guest ELF, wherever cargo put it. Built by amd64/build.rs into OUT_DIR, so
# the path carries a hash and has to be searched for rather than spelled.
HELLO=$(find target/x86_64-unknown-none/release/build -name hello.elf 2>/dev/null | head -1)
FDPROBE=$(find target/x86_64-unknown-none/release/build -name fdprobe.elf 2>/dev/null | head -1)
[ -n "$HELLO" ] || {
    echo "hello.elf not found — run 'cargo build -p akuma-amd64 --target x86_64-unknown-none --release' first" >&2
    exit 1
}

# paws, the shell. Built here rather than by `userspace/build.sh`, which targets
# aarch64-musl: this is the same source compiled for `x86_64-unknown-none`
# against the ported `libakuma`. Best-effort — a tree where paws does not build
# still gets a bootable image with the probes on it.
PAWS=""
HTTPD=""
SSHD=""
for prog in paws httpd; do
    if (cd userspace && cargo build -q -p "$prog" --target x86_64-unknown-none --release 2>/dev/null); then
        found=$(find userspace/target/x86_64-unknown-none/release -maxdepth 1 -name "$prog" -type f | head -1)
        [ "$prog" = paws ] && PAWS="$found"
        [ "$prog" = httpd ] && HTTPD="$found"
    fi
done

# tcc, ported to this target 2026-09-04. `userspace/tcc` is not a workspace
# member (its own `Cargo.toml` declares `[workspace]` with no members, so it
# is its own root — see `userspace/Cargo.toml`'s comment on the
# submodule-backed crates), hence `--manifest-path` rather than `-p tcc`.
# `TCC_LIBTCC1` is the runtime archive + tcc's own internal headers
# (`libtcc1-x86_64.tar` — named apart from aarch64's `libtcc1.tar` so one
# arch's build can never clobber the other's, see `userspace/tcc/build.rs`).
TCC=""
TCC_LIBTCC1=""
if (cd userspace && cargo build -q --manifest-path tcc/Cargo.toml --target x86_64-unknown-none --release 2>/dev/null); then
    TCC=$(find userspace/tcc/target/x86_64-unknown-none/release -maxdepth 1 -name tcc -type f | head -1)
    [ -f userspace/tcc/dist/libtcc1-x86_64.tar ] && TCC_LIBTCC1="userspace/tcc/dist/libtcc1-x86_64.tar"
fi

# sshd, WITHOUT `fork-sessions` (a default feature): this target has no `fork`,
# so the cooperative single-process executor is the only one that runs. `akuma`
# is kept — it is what pulls in `libakuma` and its `net-async` — but the default
# set that also brings `fork-sessions` is dropped.
if (cd userspace && cargo build -q -p sshd --no-default-features --features akuma \
        --target x86_64-unknown-none --release 2>/dev/null); then
    SSHD=$(find userspace/target/x86_64-unknown-none/release -maxdepth 1 -name sshd -type f | head -1)
fi

# A test keypair for `sshd`'s pubkey auth. Generated once into `target/` (which
# is git-ignored) and reused, so `ssh -i` has a stable key. sshd generates its
# own *host* key on first run and tolerates not being able to persist it, so
# only the client key needs to survive across boots.
SSH_TEST_KEY="target/x86_64-unknown-none/release/amd64-ssh-test-key"
if [ -n "$SSHD" ] && [ ! -f "$SSH_TEST_KEY" ]; then
    ssh-keygen -q -t ed25519 -N '' -C 'akuma-amd64-test' -f "$SSH_TEST_KEY" </dev/null || true
fi

mkdir -p "$(dirname "$IMG")"
rm -f "$IMG"

# 4 KiB blocks — the same setup as the devbox image (`scripts/create_disk.sh`),
# not the 1 KiB this script used until 2026-09-04. The small-block choice was a
# deliberate indirect-block exercise, and `akuma-ext2`'s host tests cover those
# paths regardless; on real hardware/VMs the added divergence from the image
# the aarch64 kernel actually runs cost more than the coverage taught.
"$MKFS" -q -F -b 4096 -L AKUMA-AMD64 "$IMG" "${SIZE_MIB}m"

# A known text file, so a read can be checked against content rather than
# against "no error". The bytes are position-dependent for the same reason the
# old raw pattern was: a short read shows up as a mismatch at the first byte
# that was not transferred.
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
printf 'AKUMA/amd64 ext2 probe\n' > "$TMP/probe.txt"
i=0
while [ $i -lt 200 ]; do
    printf 'line %03d padding padding padding\n' "$i" >> "$TMP/probe.txt"
    i=$((i + 1))
done

"$DEBUGFS" -w -R "mkdir /bin" "$IMG" >/dev/null 2>&1
"$DEBUGFS" -w -R "write $HELLO bin/hello" "$IMG" >/dev/null 2>&1
[ -n "$FDPROBE" ] && "$DEBUGFS" -w -R "write $FDPROBE bin/fdprobe" "$IMG" >/dev/null 2>&1
[ -n "$PAWS" ] && "$DEBUGFS" -w -R "write $PAWS bin/paws" "$IMG" >/dev/null 2>&1
[ -n "$HTTPD" ] && "$DEBUGFS" -w -R "write $HTTPD bin/httpd" "$IMG" >/dev/null 2>&1

# tcc + its runtime archive.
if [ -n "$TCC" ]; then
    "$DEBUGFS" -w -R "write $TCC bin/tcc" "$IMG" >/dev/null 2>&1
    if [ -n "$TCC_LIBTCC1" ]; then
        LIBTCC1_STAGE=$(mktemp -d)
        tar xf "$TCC_LIBTCC1" -C "$LIBTCC1_STAGE"
        "$DEBUGFS" -w -R "mkdir /usr" "$IMG" >/dev/null 2>&1
        "$DEBUGFS" -w -R "mkdir /usr/lib" "$IMG" >/dev/null 2>&1
        "$DEBUGFS" -w -R "mkdir /usr/lib/tcc" "$IMG" >/dev/null 2>&1
        "$DEBUGFS" -w -R "mkdir /usr/lib/tcc/include" "$IMG" >/dev/null 2>&1
        "$DEBUGFS" -w -R "write $LIBTCC1_STAGE/usr/lib/tcc/libtcc1.a usr/lib/tcc/libtcc1.a" "$IMG" >/dev/null 2>&1
        for hdr in "$LIBTCC1_STAGE"/usr/lib/tcc/include/*; do
            [ -f "$hdr" ] && "$DEBUGFS" -w -R "write $hdr usr/lib/tcc/include/$(basename "$hdr")" "$IMG" >/dev/null 2>&1
        done
        rm -rf "$LIBTCC1_STAGE"
    fi
    "$DEBUGFS" -w -R "mkdir /tmp" "$IMG" >/dev/null 2>&1

    # A real musl static libc (2026-09-04), so tcc can link a real `printf`
    # instead of only the libc-free `-nostdlib` shape below. `userspace/tcc/
    # build.rs` already downloads+caches an Alpine musl-dev apk for x86_64 to
    # get *headers* to build tcc itself against; that same apk also carries
    # the static archive and crt objects (`usr/lib/libc.a`, `crt1.o`,
    # `crti.o`, `crtn.o`, …) and the full musl public header tree
    # (`usr/include/stdio.h` and friends) — apk-tools' `musl-dev` package is
    # the same package `populate_disk.sh --with-musl-dev` installs on the
    # AArch64 image via `apk add`, just reached here by unpacking the same
    # apk instead of running a package manager this target does not have.
    # `libc.a` alone is ~9.4 MiB, which is why `SIZE_MIB`'s default grew.
    MUSL_APK="userspace/tcc/vendor/musl-dev-x86_64.apk"
    if [ -f "$MUSL_APK" ]; then
        MUSL_STAGE=$(mktemp -d)
        tar xzf "$MUSL_APK" -C "$MUSL_STAGE" usr/lib usr/include
        "$DEBUGFS" -w -R "mkdir /usr/include" "$IMG" >/dev/null 2>&1
        # Static archives + crt objects only — `-static` never touches
        # `libc.so`, and nothing on this target links dynamically at all yet.
        for f in "$MUSL_STAGE"/usr/lib/*.a "$MUSL_STAGE"/usr/lib/*.o; do
            [ -f "$f" ] && "$DEBUGFS" -w -R "write $f usr/lib/$(basename "$f")" "$IMG" >/dev/null 2>&1
        done
        # `debugfs` has no recursive "write a directory tree" — one `mkdir`
        # per subdirectory (musl's public headers are one level deep: bits/,
        # sys/, netinet/, …), then every file, mirroring by hand what
        # `copy_dir_recursive` in `userspace/tcc/build.rs` does for tcc's own
        # (much smaller) internal header set.
        for d in $(find "$MUSL_STAGE/usr/include" -mindepth 1 -type d); do
            rel=${d#"$MUSL_STAGE/usr/include/"}
            "$DEBUGFS" -w -R "mkdir /usr/include/$rel" "$IMG" >/dev/null 2>&1
        done
        for f in $(find "$MUSL_STAGE/usr/include" -type f); do
            rel=${f#"$MUSL_STAGE/usr/include/"}
            "$DEBUGFS" -w -R "write $f usr/include/$rel" "$IMG" >/dev/null 2>&1
        done
        rm -rf "$MUSL_STAGE"
    fi

    # Two proof programs. `probe_tcc.c` is deliberately libc-free
    # (`-nostdlib`, its own `_start`, a raw-`syscall` `_exit`) — a fast,
    # independent check that tcc itself still compiles+links+writes even if
    # something above went wrong with the musl staging. `hello.c` is the
    # AArch64 acceptance tests' own file, byte-for-byte
    # (`bootstrap/tmp/hello.c`) — the real `#include <stdio.h>` + `printf`
    # test the musl staging above exists for.
    cat > "$TMP/probe_tcc.c" <<'EOF'
/* Deliberately libc-free — see mkdisk.sh's comment on why. */
static void _exit_now(int code) {
    __asm__ volatile (
        "syscall"
        :
        : "a"(231), "D"(code)
        : "rcx", "r11", "memory"
    );
}

void _start(void) {
    _exit_now(42);
}
EOF
    "$DEBUGFS" -w -R "write $TMP/probe_tcc.c tmp/probe_tcc.c" "$IMG" >/dev/null 2>&1
    cat > "$TMP/hello.c" <<'EOF'
#include <stdio.h>

int main() {
  printf("Hello, Akuma!\n");
  return 0;
}
EOF
    "$DEBUGFS" -w -R "write $TMP/hello.c tmp/hello.c" "$IMG" >/dev/null 2>&1
fi

# apk (2026-09-04): Alpine's real `apk-tools-static` binary, the same
# pre-built artifact `userspace/apk-tools` stages for the AArch64 image —
# fetched here directly rather than through that crate, because its own
# `build.rs` is pinned to the aarch64 download URLs and bootstrap paths.
# `apk.static` is `-static-pie` (`ET_DYN`, no `PT_INTERP`) — loadable only
# since `amd64/src/loader.rs` grew static-PIE support the same day; see that
# file's module header for why the kernel does *not* process its `DT_RELR`
# relocations itself (musl's own startup code does, once it can find its
# program headers via `AT_PHDR`).
APK_VENDOR="target/x86_64-unknown-none/release/apk-vendor"
mkdir -p "$APK_VENDOR"
APK_STATIC_APK="$APK_VENDOR/apk-tools-static.apk"
ALPINE_KEYS_APK="$APK_VENDOR/alpine-keys.apk"
CACERT="$APK_VENDOR/cacert.pem"
[ -f "$APK_STATIC_APK" ] || curl -sSLf -o "$APK_STATIC_APK" \
    "https://dl-cdn.alpinelinux.org/alpine/latest-stable/main/x86_64/apk-tools-static-3.0.8-r0.apk" \
    || rm -f "$APK_STATIC_APK"
[ -f "$ALPINE_KEYS_APK" ] || curl -sSLf -o "$ALPINE_KEYS_APK" \
    "https://dl-cdn.alpinelinux.org/alpine/latest-stable/main/x86_64/alpine-keys-2.6-r0.apk" \
    || rm -f "$ALPINE_KEYS_APK"
# Mozilla's CA bundle — Alpine's CDN has been HTTPS-only since early 2026
# (plain HTTP returns 403), so `apk update`/`apk add` cannot work without one.
[ -f "$CACERT" ] || curl -sSLf -o "$CACERT" "https://curl.se/ca/cacert.pem" || rm -f "$CACERT"

if [ -f "$APK_STATIC_APK" ]; then
    APK_STAGE=$(mktemp -d)
    tar xzf "$APK_STATIC_APK" -C "$APK_STAGE" sbin/apk.static
    "$DEBUGFS" -w -R "write $APK_STAGE/sbin/apk.static bin/apk" "$IMG" >/dev/null 2>&1

    # `/etc` itself isn't created until the SSHD/DNS blocks further down —
    # this block runs first, so it has to make its own parent directory
    # rather than assume one of theirs already ran. Idempotent like every
    # other `mkdir` in this script (`/etc` itself is mkdir'd twice more,
    # later, on purpose — see those blocks). `/etc/apk` is needed
    # unconditionally below (repositories/arch/world), not only when the
    # signing keys downloaded, so it is made here rather than nested inside
    # that `if`.
    "$DEBUGFS" -w -R "mkdir /etc" "$IMG" >/dev/null 2>&1
    "$DEBUGFS" -w -R "mkdir /etc/apk" "$IMG" >/dev/null 2>&1

    if [ -f "$ALPINE_KEYS_APK" ]; then
        tar xzf "$ALPINE_KEYS_APK" -C "$APK_STAGE" etc/apk/keys
        "$DEBUGFS" -w -R "mkdir /etc/apk/keys" "$IMG" >/dev/null 2>&1
        for key in "$APK_STAGE"/etc/apk/keys/*; do
            [ -f "$key" ] && "$DEBUGFS" -w -R "write $key etc/apk/keys/$(basename "$key")" "$IMG" >/dev/null 2>&1
        done
    fi

    # The two keys `overlays/devbox/rootfs/etc/apk/keys/` carries — same
    # files, byte for byte. `alpine-keys-2.6` (above) predates them, and
    # `616ae350` is the 4096-bit RSA key that signs *current* `latest-stable`
    # APKINDEX; without it apk 3.0.8 reports every fetched index as
    # `UNTRUSTED signature` even though the fetch itself was fine
    # (`docs/archive/APK_MISSING_SYSCALLS.md` "The key files", 2026-09-04).
    for key in overlays/devbox/rootfs/etc/apk/keys/*.pub; do
        [ -f "$key" ] && "$DEBUGFS" -w -R "write $key etc/apk/keys/$(basename "$key")" "$IMG" >/dev/null 2>&1
    done
    rm -rf "$APK_STAGE"

    printf 'https://dl-cdn.alpinelinux.org/alpine/latest-stable/main\nhttps://dl-cdn.alpinelinux.org/alpine/latest-stable/community\n' \
        > "$TMP/apk-repositories"
    "$DEBUGFS" -w -R "write $TMP/apk-repositories etc/apk/repositories" "$IMG" >/dev/null 2>&1
    printf 'x86_64\n' > "$TMP/apk-arch"
    "$DEBUGFS" -w -R "write $TMP/apk-arch etc/apk/arch" "$IMG" >/dev/null 2>&1
    printf '' > "$TMP/apk-world"
    "$DEBUGFS" -w -R "write $TMP/apk-world etc/apk/world" "$IMG" >/dev/null 2>&1

    if [ -f "$CACERT" ]; then
        "$DEBUGFS" -w -R "mkdir /etc/ssl" "$IMG" >/dev/null 2>&1
        "$DEBUGFS" -w -R "mkdir /etc/ssl/certs" "$IMG" >/dev/null 2>&1
        "$DEBUGFS" -w -R "write $CACERT etc/ssl/certs/ca-certificates.crt" "$IMG" >/dev/null 2>&1
        "$DEBUGFS" -w -R "write $CACERT etc/ssl/cert.pem" "$IMG" >/dev/null 2>&1
    fi

    # Seed the package database so `apk add` works without `--initdb`, and the
    # empty cache dir `apk` expects to find (both match
    # `userspace/apk-tools/build.rs`'s bootstrap layout for the AArch64 image).
    "$DEBUGFS" -w -R "mkdir /lib" "$IMG" >/dev/null 2>&1
    "$DEBUGFS" -w -R "mkdir /lib/apk" "$IMG" >/dev/null 2>&1
    "$DEBUGFS" -w -R "mkdir /lib/apk/db" "$IMG" >/dev/null 2>&1
    printf '' > "$TMP/apk-empty"
    "$DEBUGFS" -w -R "write $TMP/apk-empty lib/apk/db/installed" "$IMG" >/dev/null 2>&1
    "$DEBUGFS" -w -R "write $TMP/apk-empty lib/apk/db/triggers" "$IMG" >/dev/null 2>&1
    "$DEBUGFS" -w -R "mkdir /var" "$IMG" >/dev/null 2>&1
    "$DEBUGFS" -w -R "mkdir /var/cache" "$IMG" >/dev/null 2>&1
    "$DEBUGFS" -w -R "mkdir /var/cache/apk" "$IMG" >/dev/null 2>&1
fi

# busybox: a real static musl x86_64 binary (ET_EXEC, non-PIE), fetched once and
# cached. It is the test that the ELF loader and the Linux syscall surface hold
# up under a program the tree did not compile. `/bin/sh` and a handful of applet
# names are hard-linked to it so a multicall dispatch on `argv[0]` works.
BB_VER=1.35.0
BB="target/x86_64-unknown-none/release/busybox-x86_64"
if [ ! -f "$BB" ]; then
    mkdir -p "$(dirname "$BB")"
    curl -sSL -o "$BB" "https://www.busybox.net/downloads/binaries/${BB_VER}-x86_64-linux-musl/busybox" || rm -f "$BB"
fi
if [ -f "$BB" ]; then
    "$DEBUGFS" -w -R "write $BB bin/busybox" "$IMG" >/dev/null 2>&1
    for applet in sh uname ls cat echo pwd env cut head tail wc find wget; do
        "$DEBUGFS" -w -R "ln /bin/busybox /bin/$applet" "$IMG" >/dev/null 2>&1
    done
fi
if [ -n "$SSHD" ]; then
    "$DEBUGFS" -w -R "write $SSHD bin/sshd" "$IMG" >/dev/null 2>&1
    "$DEBUGFS" -w -R "mkdir /etc" "$IMG" >/dev/null 2>&1
    "$DEBUGFS" -w -R "mkdir /etc/sshd" "$IMG" >/dev/null 2>&1
    if [ -f "$SSH_TEST_KEY.pub" ]; then
        "$DEBUGFS" -w -R "write $SSH_TEST_KEY.pub etc/sshd/authorized_keys" "$IMG" >/dev/null 2>&1
    fi
    # The shell sshd starts in a session. `paws` is the shell that builds for
    # this target and is the default; `SSHD_SHELL=/bin/sh` points it at busybox
    # instead (exec-mode commands only — an interactive busybox needs `fork`).
    printf 'shell = %s\n' "${SSHD_SHELL:-/bin/paws}" > "$TMP/sshd.conf"
    "$DEBUGFS" -w -R "write $TMP/sshd.conf etc/sshd/sshd.conf" "$IMG" >/dev/null 2>&1
fi
# DNS. QEMU's usermode `-netdev user` and Firecracker's `net-setup.sh` tap both
# answer DNS themselves at `10.0.2.3` (usermode net's fixed proxy address; see
# `net-setup.sh`'s dnsmasq for the Firecracker side, same address by
# convention) — but nothing points a guest resolver at it without this file.
# musl's resolver reads `/etc/resolv.conf` and, finding none, falls back to
# `127.0.0.1`, so `wget http://a-hostname/` failed with "bad address" even
# though outbound TCP itself worked (`amd64/README.md`'s curl/wget check).
"$DEBUGFS" -w -R "mkdir /etc" "$IMG" >/dev/null 2>&1
printf 'nameserver 10.0.2.3\n' > "$TMP/resolv.conf"
"$DEBUGFS" -w -R "write $TMP/resolv.conf etc/resolv.conf" "$IMG" >/dev/null 2>&1

# Something for httpd to serve. httpd's document root is `/public`, and `GET /`
# maps to `/public/index.html` — put it where httpd looks, not at the root.
printf '<html><body><h1>Akuma/amd64</h1><p>httpd, over virtio-net.</p></body></html>\n' > "$TMP/index.html"
"$DEBUGFS" -w -R "mkdir /public" "$IMG" >/dev/null 2>&1
"$DEBUGFS" -w -R "write $TMP/index.html public/index.html" "$IMG" >/dev/null 2>&1
"$DEBUGFS" -w -R "write $TMP/probe.txt probe.txt" "$IMG" >/dev/null 2>&1

# `/tmp`: real writes landed 2026-09-04 (`fd::sys_write_file`/`fs::write_file`),
# and `akuma-ext2`'s `write_file` requires the parent directory to already
# exist — it does not create one, matching `open(2)`. tcc's own acceptance
# tests on the AArch64 side (`acceptance/05_meow_tcc_extreme_4mb.md`) write
# their output to `/tmp`, so this target's tcc port follows the same path
# rather than inventing a different convention.
"$DEBUGFS" -w -R "mkdir /tmp" "$IMG" >/dev/null 2>&1

echo "$IMG: ${SIZE_MIB} MiB ext2, /bin/hello, /probe.txt$([ -n "$PAWS" ] && echo ", /bin/paws ($(wc -c < "$PAWS" | tr -d ' ') bytes)")"
