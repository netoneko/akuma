#!/bin/bash
#
# build.sh — drive buildrump.sh to cross-build the NetBSD rump kernel libraries
# (librump*) for Akuma's target: aarch64-linux-musl, static.
#
# This is the userspace half of the rump port (IMPLEMENTATION_PLAN.md Phases
# 0/1/4). The KERNEL half (the raw L2 /dev/net/tap0 packet path) is already done
# — see docs/PHASE3_KERNEL_TAP.md. What this script produces is the set of
# librump*.a static archives an Akuma program links against to host a NetBSD
# TCP/IP stack; our Rust `rumpuser` (Phase 2) is substituted for NetBSD's C
# librumpuser at the final link, which is why we build with `-k` (kernel only,
# no POSIX hypercalls).
#
# Usage:
#   ./build.sh              # checkout (if needed) + cross-build librump* for aarch64
#   ./build.sh checkout     # fetch the pinned NetBSD source subset only
#   ./build.sh host         # Phase 0: native host sanity build (de-risk the loop)
#   ./build.sh clean        # remove obj/ and dest/
#
# Env overrides:
#   JOBS=N      parallelism (default: CPU count)
#   CROSS=...   cross-tool prefix (default: aarch64-linux-musl-)
#
# Outputs (git-ignored): obj/ (build) and dest/ (installed librump*.a + headers).
#
# NOTE: This is the plan's #1 risk (§7) — feeding NetBSD's build system an
# aarch64-linux-musl-static target. Expect to iterate on toolchain probing and
# musl sysroot gaps (e.g. a missing <linux/if_tun.h>, TLS model, -static + -ldl).
# Failures here are localized to this subtree and do not affect the kernel build.
set -eu

HERE="$(cd "$(dirname "$0")" && pwd)"
BR="${HERE}/buildrump.sh"
OBJ="${HERE}/obj"
DEST="${HERE}/dest"
SRC="${HERE}/src-netbsd"   # checkout.sh target
JOBS="${JOBS:-$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)}"
# `-` not `:-`: an explicitly-empty CROSS="" means "native toolchain" (gcc/g++/…
# with no prefix), used by the Linux-container build where aarch64-musl is native.
# Only an UNSET CROSS falls back to the macOS cross prefix.
CROSS="${CROSS-aarch64-linux-musl-}"

cmd="${1:-build}"

die() { echo "[rump-build] ERROR: $*" >&2; exit 1; }
info() { echo "[rump-build] $*" >&2; }

do_checkout() {
    if [ -d "${SRC}/sys" ]; then
        info "source already present at ${SRC} (skip checkout)"
        return 0
    fi
    info "fetching pinned NetBSD source subset (rev $(cat "${BR}/.srcgitrev"))..."
    # checkout.sh fetches the rump-kernel src-netbsd subset into the given dir.
    ( cd "${BR}" && ./checkout.sh git "${SRC}" )
}

# Cross-build for aarch64-linux-musl. CC drives buildrump's MACHINE detection
# (aarch64 → evbarm64). Static everywhere (Akuma has no dynamic loader — cf.
# akuma_own_tcc_build). `-k` = kernel only, so NetBSD's C librumpuser is skipped
# (our Rust rumpuser replaces it). RUMP_VIRTIF=yes pulls the virtif NIC faction.
do_cross() {
    command -v "${CROSS}gcc" >/dev/null 2>&1 || die "${CROSS}gcc not on PATH"
    do_checkout

    export CC="${CROSS}gcc"
    export CXX="${CROSS}g++"
    export AS="${CROSS}as"
    export LD="${CROSS}ld"
    export AR="${CROSS}ar"
    export NM="${CROSS}nm"
    export OBJCOPY="${CROSS}objcopy"
    export RANLIB="${CROSS}ranlib"
    export RUMP_VIRTIF=yes

    # NOTE on flags:
    #  - No `-static` here. buildrump emits librump*.a static ARCHIVES regardless;
    #    static linking is decided when WE link the Akuma program against them
    #    (Phase 4/5). Forcing -static into EXTRA_CFLAGS also leaks onto the HOST
    #    tools (nbmake/compat), which macOS cannot fully static-link.
    #  - `-Wno-error=implicit-function-declaration`: the pinned NetBSD source is
    #    from 2016 (NetBSD 7.99.34). Modern clang (≥16, current Apple clang)
    #    promotes implicit function declarations to hard errors, which breaks the
    #    host `tools/compat` build (e.g. `mi_vector_hash`). Downgrade it back to a
    #    warning so the 2016 source compiles. (Plan §7 risk #2: source pin age.)
    info "cross-building librump* (CC=${CC}, jobs=${JOBS}) → ${DEST}"
    ( cd "${BR}" && ./buildrump.sh \
        -k \
        -j "${JOBS}" \
        -s "${SRC}" \
        -o "${OBJ}" \
        -d "${DEST}" \
        -F CFLAGS=-fno-stack-protector \
        -F CFLAGS=-fcommon \
        -F CFLAGS=-Wno-error=implicit-function-declaration \
        -F CWARNFLAGS=-Wno-error=implicit-function-declaration \
        -V HOST_CFLAGS=-Wno-error=implicit-function-declaration \
        -V HOST_CPPFLAGS=-Wno-error=implicit-function-declaration \
        fullbuild )
    # buildrump stages the installed libs under ${OBJ}/dest.stage/usr/lib.
    local libdir="${OBJ}/dest.stage/usr/lib"
    local n
    n=$(ls -1 "${libdir}"/librump*.a 2>/dev/null | wc -l | tr -d ' ')
    info "done. ${n} librump*.a archives in ${libdir}"
    info "  TCP/IP stack: librumpnet_netinet.a | core: librump.a | netconfig/DHCP: librumpnet_config.a"
    [ "${n}" = "0" ] && info "no librump*.a — see build log above"
}

# Phase 0: native host sanity build to learn the build/test loop on a platform
# rump already supports, independent of the cross target.
do_host() {
    info "host sanity build (native ${CC:-cc}) → ${DEST}-host"
    ( cd "${BR}" && ./buildrump.sh -o "${OBJ}-host" -d "${DEST}-host" )
}

case "${cmd}" in
    checkout) do_checkout ;;
    build|cross) do_cross ;;
    host) do_host ;;
    clean) rm -rf "${OBJ}" "${DEST}" "${OBJ}-host" "${DEST}-host"; info "cleaned" ;;
    *) die "unknown command '${cmd}' (use: build | checkout | host | clean)" ;;
esac
