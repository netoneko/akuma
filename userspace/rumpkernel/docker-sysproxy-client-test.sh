#!/bin/bash
#
# docker-sysproxy-client-test.sh — RUMP_SYSPROXY.md Step 3: prove stack sharing.
# Build NetBSD librumpclient (rumpclient.c + rump_syscalls.c, -DRUMP_CLIENT) + a
# tiny client, start the rump_server payload, and have the client run rump_sys_*
# against the SERVER stack over the sysproxy unix socket.
set -eu
HERE="$(cd "$(dirname "$0")" && pwd)"
cd "${HERE}"
[ -x out/rump_server_akuma ] || { echo "build out/rump_server_akuma first (./docker-build-rump-server.sh)" >&2; exit 1; }

exec docker run --rm \
    --platform linux/arm64 \
    -v "${HERE}:/work" -w /work \
    alpine:3.20 \
    sh -euc '
        set -o pipefail
        apk add --no-cache build-base bsd-compat-headers >/dev/null
        I=obj/dest.stage/usr/include
        LC=src-netbsd/lib/librumpclient
        SP=src-netbsd/lib/librumpuser
        KERN=src-netbsd/sys/rump/librump/rumpkern
        mkdir -p /usr/local/include/rump
        # rumpclient.h + rumpuser_port.h are not installed into dest; expose them
        # as <rump/...> (rumpclient.c includes <rump/rumpuser_port.h>).
        cp "$LC/rumpclient.h" /usr/local/include/rump/
        cp "$SP/rumpuser_port.h" /usr/local/include/rump/
        # rump_syscalls.c does #include <srcsys/...>; NetBSD build symlinks srcsys
        # -> src/sys/sys. Mirror that on the include path.
        ln -sf /work/src-netbsd/sys/sys /usr/local/include/srcsys
        # musl autoconf values (derived from NetBSD rumpuser_port.h; see spike).
        printf "%s\n" \
          "#define HAVE_ALIGNED_ALLOC 1" "#define HAVE_ARC4RANDOM_BUF 1" \
          "#define HAVE_CLOCKID_T 1" "#define HAVE_CLOCK_GETTIME 1" \
          "#define HAVE_CLOCK_NANOSLEEP 1" "#define HAVE_GETSUBOPT 1" \
          "#define HAVE_INTTYPES_H 1" "#define HAVE_POSIX_MEMALIGN 1" \
          "#define HAVE_SYS_CDEFS_H 1" "#define HAVE_SYS_PARAM_H 1" \
          "#define STDC_HEADERS 1" > /usr/local/include/rumpuser_config.h

        echo "=== compile librumpclient (rumpclient.c + rump_syscalls.c) ==="
        gcc -O2 -fcommon -c -o /tmp/rumpclient.o "$LC/rumpclient.c" \
            -I /usr/local/include -I "$I" -I "$LC" -I "$SP" \
            -DRUMP_CLIENT -D_KERNTYPES -DRUMPUSER_CONFIG -Wno-error 2>&1 | sed "s/^/[rc] /" \
            || { echo "RC_FAIL"; exit 1; }
        gcc -O2 -fcommon -fno-strict-aliasing -c -o /tmp/rump_syscalls.o "$KERN/rump_syscalls.c" \
            -I /usr/local/include -I "$I" -I "$LC" -I "$SP" \
            -DRUMP_CLIENT -D_KERNTYPES -DRUMPUSER_CONFIG -Wno-error 2>&1 | sed "s/^/[rs] /" \
            || { echo "RSYS_FAIL"; exit 1; }
        echo "=== link sp_client_test ==="
        gcc -O2 -static -o /tmp/sp_client_test \
            rumpuser/sp_client_test.c /tmp/rumpclient.o /tmp/rump_syscalls.o \
            -I /usr/local/include -I "$I" \
            -Wl,--allow-multiple-definition -lpthread 2>&1 | sed "s/^/[ld] /" \
            || { echo "LINK_FAIL"; exit 1; }
        echo "CLIENT_BUILT"

        echo "=== run: server (bg) + client ==="
        rm -f /tmp/rs.sock
        RUMP_VERBOSE=0 out/rump_server_akuma unix:///tmp/rs.sock virt0 > /tmp/rs.out 2>&1 &
        SPID=$!
        i=0; while [ $i -lt 30 ]; do [ -S /tmp/rs.sock ] && break; sleep 0.3; i=$((i+1)); done
        [ -S /tmp/rs.sock ] || { echo "SERVER_NO_SOCKET"; sed "s/^/[rs] /" /tmp/rs.out | tail; kill $SPID; exit 1; }
        echo "--- server up, socket present ---"
        RUMP_SERVER=unix:///tmp/rs.sock /tmp/sp_client_test 2>&1 | sed "s/^/[client] /"
        RC=${PIPESTATUS:-0}
        echo "--- server-side log (tail) ---"; sed "s/^/[rs] /" /tmp/rs.out | tail -6
        kill $SPID 2>/dev/null
        exit $RC
    '
