#!/bin/bash
#
# docker-sp-fd-test.sh — RUMP_SYSPROXY.md Step 4 feasibility test: prove the
# sysproxy server can serve on a PRE-CONNECTED fd (kernel-pipe transport), by
# compiling sp_serve_fd.c (adds rumpuser_sp_init_fd) and running sp_fd_test.c.
set -eu
HERE="$(cd "$(dirname "$0")" && pwd)"
cd "${HERE}"
RUMPUSER_A="rumpuser/target/aarch64-unknown-linux-musl/release/librumpuser_akuma.a"
[ -f "${RUMPUSER_A}" ] || { echo "missing ${RUMPUSER_A} — build it first" >&2; exit 1; }

exec docker run --rm \
    --platform linux/arm64 \
    -v "${HERE}:/work" -w /work \
    alpine:3.20 \
    sh -euc '
        set -o pipefail
        apk add --no-cache build-base bsd-compat-headers >/dev/null
        I=obj/dest.stage/usr/include
        L=obj/dest.stage/usr/lib
        SP=src-netbsd/lib/librumpuser
        VDIR=src-netbsd/sys/rump/net/lib/libvirtif
        RUMPUSER_A='"${RUMPUSER_A}"'
        mkdir -p /usr/local/include /tmp/inc/rump
        cp "$SP/rumpuser_component.h" /tmp/inc/rump/ 2>/dev/null || true
        printf "%s\n" \
          "#define HAVE_ALIGNED_ALLOC 1" "#define HAVE_ARC4RANDOM_BUF 1" \
          "#define HAVE_CLOCKID_T 1" "#define HAVE_CLOCK_GETTIME 1" \
          "#define HAVE_CLOCK_NANOSLEEP 1" "#define HAVE_GETSUBOPT 1" \
          "#define HAVE_INTTYPES_H 1" "#define HAVE_POSIX_MEMALIGN 1" \
          "#define HAVE_SYS_CDEFS_H 1" "#define HAVE_SYS_PARAM_H 1" \
          "#define STDC_HEADERS 1" > /usr/local/include/rumpuser_config.h

        echo "=== compile sp_serve_fd.c (includes NetBSD rumpuser_sp.c) ==="
        gcc -O2 -fcommon -c -o /tmp/sp_serve_fd.o rumpuser/sp_serve_fd.c \
            -I /usr/local/include -I "$I" -I "$SP" \
            -DLIBRUMPUSER -D_KERNTYPES -DRUMPUSER_CONFIG -Wno-error \
            2>&1 | sed "s/^/[sp] /" || { echo "SP_FAIL"; exit 1; }
        gcc -O2 -fcommon -c -o /tmp/rumpuser_errtrans.o "$SP/rumpuser_errtrans.c" \
            -I /usr/local/include -I "$I" -I "$SP" \
            -DLIBRUMPUSER -D_KERNTYPES -DRUMPUSER_CONFIG -Wno-error \
            2>&1 | sed "s/^/[et] /" || { echo "ET_FAIL"; exit 1; }

        echo "=== link sp_fd_test ==="
        gcc -O2 -static -o /tmp/sp_fd_test \
            rumpuser/sp_fd_test.c rumpuser/rumpcomp_tap.c rumpuser/csupport.c \
            /tmp/sp_serve_fd.o /tmp/rumpuser_errtrans.o \
            -I "$I" -I "$VDIR" -I /tmp/inc -DVIRTIF_BASE=virt \
            -Wl,--allow-multiple-definition \
            -Wl,--whole-archive \
              -L "$L" \
              -lrumpnet_config -lrumpnet_virtif -lrumpnet_netinet -lrumpnet_net -lrumpnet \
              -lrumpkern_sysproxy \
              -lrumpdev_bpf -lrumpdev -lrumpvfs \
              -lrump \
              "$RUMPUSER_A" \
            -Wl,--no-whole-archive \
            -lpthread 2>&1 | sed "s/^/[ld] /" || { echo "LINK_FAIL"; exit 1; }
        echo "BUILD_OK"

        echo "=== run ==="
        RUMP_VERBOSE=0 timeout 30 /tmp/sp_fd_test 2>&1 | sed "s/^/[run] /"
    '
