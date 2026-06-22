#!/bin/bash
#
# docker-hijack-demo.sh — THE proof: an UNMODIFIED `busybox wget` does HTTP over
# the NetBSD rump TCP/IP stack (not the host kernel), with instrumentation at the
# rump↔wire seam proving every byte rode the rump stack.
#
# Pipeline in the container:
#   1. host side: persistent TAP `tun0` = 10.0.0.1/24, + busybox httpd serving a page
#   2. build hijack.so = our LD_PRELOAD shim + the instrumented virtif backend +
#      the whole rump stack (PIC archives) + our rumpuser, as one shared object
#   3. run: LD_PRELOAD=hijack.so busybox wget http://10.0.0.1/   (no DNS — IP URL)
#      → the .so's constructor brings up rump virt0 (10.0.0.2) on tun0; busybox's
#        socket/connect/read/write are interposed onto rump_sys_*.
#   4. proof: wget prints the page, AND [VIRTIF TX/RX]/[VIRTIF STATS] show the
#      frames that the NetBSD stack put on / took off the wire.
#
# Needs /dev/net/tun + NET_ADMIN. Prereqs: docker-build.sh, the PIC virtif
# (libvirtif_pic), and rumpuser built PIC (relocation-model=pic).
set -eu
HERE="$(cd "$(dirname "$0")" && pwd)"
RUMPUSER_A="rumpuser/target/aarch64-unknown-linux-musl/release/librumpuser_akuma.a"
[ -f "${HERE}/${RUMPUSER_A}" ] || { echo "missing ${RUMPUSER_A}" >&2; exit 1; }
[ -f "${HERE}/obj/dest.stage/usr/lib/librumpnet_virtif_pic.a" ] || { echo "missing librumpnet_virtif_pic.a" >&2; exit 1; }

exec docker run --rm --platform linux/arm64 \
    --device /dev/net/tun --cap-add NET_ADMIN \
    -v "${HERE}:/work" -w /work \
    alpine:3.20 sh -euc '
        apk add --no-cache build-base linux-headers iproute2 python3 curl >/dev/null
        L=obj/dest.stage/usr/lib
        I=obj/dest.stage/usr/include
        VDIR=src-netbsd/sys/rump/net/lib/libvirtif
        mkdir -p /tmp/inc/rump && cp src-netbsd/lib/librumpuser/rumpuser_component.h /tmp/inc/rump/

        echo "[demo] === host side: persistent TAP tun0 = 10.0.0.1/24 + httpd ==="
        ip tuntap add dev tun0 mode tap
        ip addr add 10.0.0.1/24 dev tun0
        ip link set tun0 up
        mkdir -p /www
        printf "<html><body>HELLO-FROM-NETBSD-RUMP-STACK</body></html>\n" > /www/index.html
        ( cd /www && python3 -m http.server 80 --bind 10.0.0.1 >/tmp/httpd.log 2>&1 & )
        sleep 1
        echo "[demo] httpd up on 10.0.0.1:80; tun0:"; ip -br addr show tun0

        echo "[demo] === build hijack.so (shim + instrumented virtif + rump PIC) ==="
        gcc -shared -fPIC -O2 -o /tmp/hijack.so \
            rumpuser/hijack.c rumpuser/virtif_user_instr.c rumpuser/csupport.c \
            -I "$I" -I "$VDIR" -I /tmp/inc -DVIRTIF_BASE=virt \
            -Wl,--allow-multiple-definition \
            -Wl,--whole-archive \
              -L "$L" \
              -lrumpnet_config_pic -lrumpnet_virtif_pic \
              -lrumpnet_netinet_pic -lrumpnet_net_pic -lrumpnet_pic \
              -lrump_pic \
              '"${RUMPUSER_A}"' \
            -Wl,--no-whole-archive \
            -ldl -lpthread
        echo "[demo] hijack.so built: $(ls -la /tmp/hijack.so | awk "{print \$5}") bytes"

        echo "[demo] sanity: host can see httpd (NOT via rump — control):"
        wget -q -O - http://10.0.0.1/ || echo "[demo] (host wget failed)"

        echo "[demo] === run UNMODIFIED curl over the rump stack ==="
        echo "[demo] curl: $(curl --version | head -1)"
        set +e
        RUMP_VIRTIF_TRACE=1 LD_PRELOAD=/tmp/hijack.so \
            curl -s -S -o /tmp/body http://10.0.0.1/ 2>/tmp/curl.err
        CRC=$?
        set -e
        echo "[demo] curl rc=${CRC}"
        echo "[demo] === hijack stderr (init + per-frame trace + stats) ==="
        cat /tmp/curl.err
        echo "[demo] === body fetched by curl THROUGH THE RUMP STACK ==="
        cat /tmp/body 2>/dev/null || echo "(no body)"
        echo "[demo] === VERDICT ==="
        if grep -q "HELLO-FROM-NETBSD-RUMP-STACK" /tmp/body 2>/dev/null \
           && grep -q "VIRTIF STATS] tx=[1-9]" /tmp/curl.err; then
            echo "[demo] PASS: unmodified curl fetched the page over the NetBSD rump stack."
        else
            echo "[demo] FAIL: see trace above."
        fi
    '
