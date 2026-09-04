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
SIZE_MIB="${2:-8}"

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

# 1 KiB blocks, not the 4 KiB the aarch64 image uses. Deliberate: it makes the
# indirect-block paths reachable in a small image, and the ext2 driver's block
# size handling is a thing worth exercising rather than pinning to one value.
"$MKFS" -q -F -b 1024 -L AKUMA-AMD64 "$IMG" "${SIZE_MIB}m"

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

# tcc + its runtime archive. No musl static libc is staged on this image at
# all yet (there is no `apk` here the way `populate_disk.sh` has for the
# AArch64 image) — see `userspace/tcc/build.rs`'s comment on that gap — so
# `/probe_tcc.c` is deliberately libc-free (`-nostdlib`, its own `_start`,
# raw-syscall `_exit`) rather than the real `#include <stdio.h>` + `printf`
# hello.c the AArch64 acceptance tests use: that one needs a working link
# against `printf`/`__libc_start_main`, which fails *before* tcc ever reaches
# the point of writing an output file, so it would prove nothing about
# whether the write path (2026-09-04, `fd::sys_write_file`) actually works.
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
