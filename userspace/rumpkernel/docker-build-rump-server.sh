#!/bin/bash
#
# docker-build-rump-server.sh — RUMP_SYSPROXY.md Step 2: build the rump_server
# payload as a static aarch64-musl Akuma binary, in arm64 Alpine (the sysproxy C
# source needs bsd-compat-headers, so we build in-container like the spike).
#
# Links: rump_server.c + rumpcomp_tap.c (/dev/net/tap0 backend) + csupport.c
#        + the NetBSD sysproxy server objects (rumpuser_sp.o, rumpuser_errtrans.o)
#        + librump + the inet/virtif net stack + librumpkern_sysproxy (rump_init_server)
#        + our Rust rumpuser staticlib.
#
# Output (host): ./out/rump_server_akuma   (copy to disk.img:/bin/rump_server)
set -eu
HERE="$(cd "$(dirname "$0")" && pwd)"
cd "${HERE}"
RUMPUSER_A="rumpuser/target/aarch64-unknown-linux-musl/release/librumpuser_akuma.a"
[ -f "${RUMPUSER_A}" ] || { echo "missing ${RUMPUSER_A} — build it first" >&2; exit 1; }
[ -f "obj/dest.stage/usr/lib/librumpkern_sysproxy.a" ] || { echo "missing librumpkern_sysproxy.a" >&2; exit 1; }
mkdir -p out

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

        # musl-tuned autoconf values (see docker-sysproxy-spike.sh; derived from
        # NetBSD rumpuser_port.h, copyright the NetBSD contributors).
        mkdir -p /usr/local/include /tmp/inc/rump
        cp "$SP/rumpuser_component.h" /tmp/inc/rump/ 2>/dev/null || true
        printf "%s\n" \
          "#define HAVE_ALIGNED_ALLOC 1" "#define HAVE_ARC4RANDOM_BUF 1" \
          "#define HAVE_CLOCKID_T 1" "#define HAVE_CLOCK_GETTIME 1" \
          "#define HAVE_CLOCK_NANOSLEEP 1" "#define HAVE_GETSUBOPT 1" \
          "#define HAVE_INTTYPES_H 1" "#define HAVE_MEMORY_H 1" \
          "#define HAVE_PATHS_H 1" "#define HAVE_POSIX_MEMALIGN 1" \
          "#define HAVE_STDINT_H 1" "#define HAVE_STDLIB_H 1" \
          "#define HAVE_STRINGS_H 1" "#define HAVE_STRING_H 1" \
          "#define HAVE_SYS_CDEFS_H 1" "#define HAVE_SYS_PARAM_H 1" \
          "#define HAVE_SYS_STAT_H 1" "#define HAVE_SYS_TYPES_H 1" \
          "#define HAVE_UNISTD_H 1" "#define HAVE_UTIMENSAT 1" \
          "#define STDC_HEADERS 1" > /usr/local/include/rumpuser_config.h

        echo "=== compile sysproxy server objects ==="
        for f in rumpuser_sp rumpuser_errtrans; do
          gcc -O2 -fcommon -c -o "/tmp/$f.o" "$SP/$f.c" \
            -I /usr/local/include -I "$I" -I "$SP" \
            -DLIBRUMPUSER -D_KERNTYPES -DRUMPUSER_CONFIG -Wno-error \
            2>&1 | sed "s/^/[sp] /" || { echo "SP_COMPILE_FAIL $f"; exit 1; }
        done

        echo "=== link rump_server (static aarch64-musl) ==="
        gcc -O2 -static -o out/rump_server_akuma \
            rumpuser/rump_server.c rumpuser/rumpcomp_tap.c rumpuser/csupport.c \
            /tmp/rumpuser_sp.o /tmp/rumpuser_errtrans.o \
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
        file out/rump_server_akuma; ls -la out/rump_server_akuma
    '
