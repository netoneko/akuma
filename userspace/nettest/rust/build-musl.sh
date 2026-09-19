#!/bin/bash
# Build the two delayed-first-byte probes for the Akuma VM.
#
#   stdlib/   -> bootstrap/bin/nettest-std       (std::net + poll(2) + sync rustls)
#   reqwest/  -> bootstrap/bin/nettest-reqwest   (tokio + hyper + reqwest + rustls)
#   connect/  -> bootstrap/bin/nettest-connect   (raw connect(2) + poll/select/epoll)
#   unixsock/ -> bootstrap/bin/nettest-unix      (AF_UNIX: raw syscalls, no std::os wrappers)
#
# `connect/` belongs to a third investigation (cargo cannot reach crates.io,
# `docs/runbooks/cargo-cannot-reach-crates-io.md`) and `unixsock/` to a fourth
# (AF_UNIX, `docs/archive/UNIX_SOCKET_IMPROVEMENTS.md`), but both share this
# build path because they have the same requirement: no runtime, no TLS,
# nothing between the probe and the syscall.
#
# `unixsock/` needs the static-musl output for a second reason the others only
# benefit from: an AF_UNIX probe is entirely self-contained — no server, no
# network, no peer to blame — so running the identical binary under Docker Linux
# is the ONLY way to tell a kernel bug from a probe bug.
#
# This is NOT the sibling curl probe's build path. That one (./build.sh) runs
# cargo inside an Alpine arm64 container because it has to build libcurl +
# OpenSSL from source with autotools. These two are pure host cross-builds with
# the SAME toolchain `userspace/nca` uses for nca itself
# (`userspace/nca/build.rs`): aarch64-unknown-linux-musl + aarch64-linux-musl-gcc.
#
# Matching nca's toolchain is the point, not a convenience. The probes exist to
# answer "does nca's network stack hang, or does nca hang?" — an answer that is
# worthless if the probe and nca were built by different compilers against
# different libcs.
#
#   ./build-musl.sh            # all four probes, aarch64
#   ./build-musl.sh std        # just nettest-std
#   ./build-musl.sh reqwest    # just nettest-reqwest
#   ./build-musl.sh connect    # just nettest-connect
#   ./build-musl.sh unix       # just nettest-unix
#
#   ARCH=x86_64 ./build-musl.sh std        # -> bootstrap/bin/nettest-std.x86_64
#
# `ARCH` selects the machine the probe RUNS on. aarch64 output keeps its bare
# name (populate_disk.sh and every doc name it that way); anything else gets an
# arch suffix, so the two cannot overwrite each other in `bootstrap/bin/` and be
# mistaken for one another later.
#
# After building: scripts/populate_disk.sh copies bootstrap/bin/* into /bin.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
OUT_DIR="$REPO_ROOT/bootstrap/bin"

# Which machine the probe will run ON, not which one builds it. `ARCH=x86_64`
# (or a full triple in `NETTEST_TARGET`) is what makes these probes usable on
# the amd64 bare-metal box; the default stays aarch64 so every existing caller
# is unaffected.
#
# This was hardcoded to aarch64 until 2026-09-19, and the cost was concrete:
# `docs/archive/AMD64_TRASHCAN_ISSUES.md` §5 needs exactly the comparison
# `stdlib/Cargo.toml` describes — sync rustls against async rustls on ONE url —
# and could not run it, because the one x86-64 `nettest-reqwest` on that box was
# built by hand outside the repo and no `nettest-std` existed for it at all. A
# probe that cannot be built for the machine under test is not a probe.
case "${ARCH:-${NETTEST_ARCH:-aarch64}}" in
    aarch64|arm64)  ARCH_TRIPLE="aarch64-unknown-linux-musl" ;;
    x86_64|amd64)   ARCH_TRIPLE="x86_64-unknown-linux-musl" ;;
    *) echo "error: ARCH must be aarch64 or x86_64 (got '${ARCH:-$NETTEST_ARCH}')" >&2; exit 2 ;;
esac
TARGET="${NETTEST_TARGET:-$ARCH_TRIPLE}"

# The musl cross prefix and the cargo env var names all derive from the triple,
# which is what stops a future third target being added in one place and missed
# in another. Both spellings replace dashes with underscores, because a shell
# cannot `export` a name containing one — `CC_x86_64-unknown-linux-musl=...` is
# rejected as "not a valid identifier", and the cc crate reads the underscored
# form, which is the spelling this script used when it was aarch64-only.
CROSS="${TARGET%%-*}-linux-musl"
TARGET_ENV="$(echo "$TARGET" | tr 'a-z-' 'A-Z_')"
TARGET_VAR="$(echo "$TARGET" | tr '-' '_')"

# Where each probe's output lands. Named separately because the built path
# includes the triple, and a stale binary from the OTHER architecture sitting in
# `bootstrap/bin/` under the same name is exactly the confusion this script
# exists to prevent — so the arch is in the copied name too, with the aarch64
# spelling kept unsuffixed for compatibility with populate_disk.sh and every
# doc that names it.
case "$TARGET" in
    aarch64-*) SUFFIX="" ;;
    *)         SUFFIX=".${TARGET%%-*}" ;;
esac

want="${1:-all}"

command -v "${CROSS}-gcc" >/dev/null 2>&1 || {
    echo "error: ${CROSS}-gcc not found (brew install FiloSottile/musl-cross/musl-cross)" >&2
    exit 1
}
rustup target list --installed 2>/dev/null | grep -qx "$TARGET" || {
    echo "error: rust target $TARGET not installed (rustup target add $TARGET)" >&2
    exit 1
}

# Same cross-compilation environment userspace/nca/build.rs exports. aws-lc-rs
# (rustls' default crypto provider, and what nca's Cargo.lock resolves) shells
# out to cc/ar for its C core, so these are load-bearing for the reqwest probe.
export "CARGO_TARGET_${TARGET_ENV}_LINKER=${CROSS}-gcc"
export "CC_${TARGET_VAR}=${CROSS}-gcc"
export "CXX_${TARGET_VAR}=${CROSS}-g++"
export "AR_${TARGET_VAR}=${CROSS}-ar"

build_one() {
    local dir="$1" bin="$2"
    # Braces are load-bearing: bash treats the trailing multibyte character as
    # part of an unbraced variable name and dies with "TARGET…: unbound variable".
    echo "[nettest] building $bin ($dir) for ${TARGET}..."
    # `--target` explicitly, rather than relying on the crate's
    # `.cargo/config.toml` `[build] target`: that pins the DEFAULT machine
    # (aarch64) and this is how the other one gets selected. The linker and the
    # `-static` rustflags for both triples are declared in that same file.
    ( cd "$SCRIPT_DIR/$dir" && cargo build --release --target "$TARGET" )
    local built="$SCRIPT_DIR/$dir/target/$TARGET/release/$bin"
    [ -f "$built" ] || { echo "BUILD FAILED: $built missing" >&2; exit 1; }
    mkdir -p "$OUT_DIR"
    cp "$built" "$OUT_DIR/${bin}${SUFFIX}"
    chmod +x "$OUT_DIR/${bin}${SUFFIX}"
    echo "[nettest] -> $OUT_DIR/${bin}${SUFFIX} ($(wc -c < "$OUT_DIR/${bin}${SUFFIX}" | tr -d ' ') bytes)"
}

case "$want" in
    all)     build_one stdlib nettest-std; build_one reqwest nettest-reqwest; build_one connect nettest-connect; build_one unixsock nettest-unix ;;
    std)     build_one stdlib nettest-std ;;
    reqwest) build_one reqwest nettest-reqwest ;;
    connect) build_one connect nettest-connect ;;
    unix)    build_one unixsock nettest-unix ;;
    *)       echo "usage: $0 [all|std|reqwest|connect|unix]" >&2; exit 2 ;;
esac

echo "[nettest] done. Run scripts/populate_disk.sh to ship them to the disk image."
