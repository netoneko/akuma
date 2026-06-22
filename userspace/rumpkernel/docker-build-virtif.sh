#!/bin/bash
#
# docker-build-virtif.sh — build the ONE net faction the main build skips:
# librumpnet_virtif.a (the if_virt.c kernel NIC + a rumpcomp_user packet backend).
#
# Why it's not in docker-build.sh's output: buildrump runs with `-k` (kernel
# only), which skips `evalplatform` — the step that would set RUMP_VIRTIF=yes on
# Linux. The generated mk.conf therefore pins RUMP_VIRTIF=no, and
# Makefile.rumpnetcomp drops `virtif` from RUMPNETLIBS. Every OTHER net faction
# (incl. shmif, which also has a *_user.c backend) builds fine; virtif is the
# lone gated one. So we build just its source dir via the already-generated
# rumpmake wrapper, overriding RUMP_VIRTIF=yes on the command line (command-line
# vars beat mk.conf), then stage the resulting .a next to the others.
#
# Backend: this uses the STOCK virtif_user.c (Linux TUN/TAP) — the cheap,
# already-written packet path for proving the TCP/IP stack in-container. The
# Akuma backend (rumpcomp_user over /dev/net/tap0) is a later swap; the kernel
# driver (if_virt.c) is identical either way. Our Rust rumpuser now provides the
# rumpuser_component_* helpers this backend calls.
#
# Prereq: ./docker-build.sh has run (obj/ + the rumpmake wrapper + staged includes).
# Output (git-ignored): obj/dest.stage/usr/lib/librumpnet_virtif.a
set -eu

HERE="$(cd "$(dirname "$0")" && pwd)"
IMAGE="alpine:3.20"
PLATFORM="linux/arm64"

[ -x "${HERE}/obj/tooldir/rumpmake" ] || { echo "missing obj/tooldir/rumpmake — run ./docker-build.sh first" >&2; exit 1; }

echo "[virtif] building librumpnet_virtif.a in ${IMAGE} (${PLATFORM})" >&2

exec docker run --rm \
    --platform "${PLATFORM}" \
    -v "${HERE}:/work" \
    -w /work \
    "${IMAGE}" \
    sh -euc '
        apk add --no-cache build-base bash linux-headers >/dev/null
        # Same gcc/cc wrappers as docker-build.sh: -fcommon + -Wno-error so the
        # 2016 source compiles under modern gcc.
        mkdir -p /usr/local/bin
        for t in gcc g++ cc; do
            real="/usr/bin/${t}"; [ "$t" = cc ] && real="/usr/bin/gcc"
            printf "#!/bin/sh\nexec %s \"\$@\" -fcommon -Wno-error\n" "$real" > "/usr/local/bin/${t}"
            chmod +x "/usr/local/bin/${t}"
        done
        export PATH="/usr/local/bin:$PATH"

        RM=/work/obj/tooldir/rumpmake
        VDIR=/work/src-netbsd/sys/rump/net/lib/libvirtif
        cd "${VDIR}"
        echo "[virtif] rumpmake passes (RUMP_VIRTIF=yes) in ${VDIR}"
        for pass in obj includes dependall; do
            echo "[virtif] === pass: ${pass} ==="
            "${RM}" RUMP_VIRTIF=yes MKPIC=no "${pass}"
        done

        A=$(find /work/obj -name librumpnet_virtif.a | head -1)
        if [ -z "${A}" ]; then echo "[virtif] FAIL: librumpnet_virtif.a not produced" >&2; exit 1; fi
        echo "[virtif] built: ${A}"
        cp "${A}" /work/obj/dest.stage/usr/lib/librumpnet_virtif.a
        echo "[virtif] staged -> obj/dest.stage/usr/lib/librumpnet_virtif.a"
        ls -la /work/obj/dest.stage/usr/lib/librumpnet_virtif.a
    '
