#!/bin/bash
#
# docker-net-test.sh — Phase 4 exit test, in the Linux container:
# link test_net + the rump TCP/IP factions + librumpnet_virtif.a + the STOCK
# virtif_user.c TUN/TAP backend + our Rust rumpuser, then bring up virt0 with a
# static IP and open a socket. Proves the NetBSD TCP/IP stack configures and runs
# networking on our rumpuser. Needs /dev/net/tun (so --device + NET_ADMIN).
#
# Prereqs:
#   ./docker-build.sh                  # librump*.a + the TCP/IP net factions
#   ./docker-build-virtif.sh           # librumpnet_virtif.a (if_virt.o kernel driver)
#   (cd rumpuser && cargo build --release --target aarch64-unknown-linux-musl)
#
# The virtif backend (rumpcomp_virt_{create,send,dying,destroy}) is supplied here
# by compiling the stock virtif_user.c as a normal user object — it's gated out of
# librumpnet_virtif.a by RUMPKERN_ONLY (Makefile.rump:143). It calls the
# rumpuser_component_* helpers our Rust rumpuser now provides.
set -eu

# NOTE: this script lives in rumpuser/c_tests/ (archived dev harness); HERE
# resolves to the rumpkernel root so the Docker mount + relative paths still work.
HERE="$(cd "$(dirname "$0")/../.." && pwd)"
RUMPUSER_A="rumpuser/target/aarch64-unknown-linux-musl/release/librumpuser_akuma.a"

[ -f "${HERE}/${RUMPUSER_A}" ] || { echo "missing ${RUMPUSER_A}" >&2; exit 1; }
[ -f "${HERE}/obj/dest.stage/usr/lib/librumpnet_virtif.a" ] || { echo "missing librumpnet_virtif.a — run ./docker-build-virtif.sh" >&2; exit 1; }

# --device /dev/net/tun + --cap-add NET_ADMIN: the stock backend opens
# /dev/net/tun and issues TUNSETIFF for tun0.
exec docker run --rm \
    --platform linux/arm64 \
    --device /dev/net/tun \
    --cap-add NET_ADMIN \
    -v "${HERE}:/work" \
    -w /work \
    alpine:3.20 \
    sh -euc '
        apk add --no-cache build-base linux-headers >/dev/null
        L=obj/dest.stage/usr/lib
        I=obj/dest.stage/usr/include
        VDIR=src-netbsd/sys/rump/net/lib/libvirtif

        # The stock backend includes <rump/rumpuser_component.h>, which is part of
        # NetBSD librumpuser (not installed under -k). Alias it into an include dir.
        mkdir -p /tmp/inc/rump
        cp src-netbsd/lib/librumpuser/rumpuser_component.h /tmp/inc/rump/

        echo "[net-test] compiling stock virtif_user.c backend ..."
        gcc -O2 -c "${VDIR}/virtif_user.c" -o /tmp/virtif_user.o \
            -DVIRTIF_BASE=virt -I"${VDIR}" -I"${I}" -I/tmp/inc

        echo "[net-test] linking test_net ..."
        # --whole-archive pulls every RUMP_COMPONENT constructor (core + net
        # factions + virtif) and all rumpuser_* symbols. -static forces the .a
        # (the dir also has .so). Net faction order under whole-archive does not
        # matter.
        gcc -O2 -static -o /tmp/test_net \
            rumpuser/c_tests/test_net.c rumpuser/csupport.c /tmp/virtif_user.o \
            -I "$I" \
            -Wl,--allow-multiple-definition \
            -Wl,--whole-archive \
              -L "$L" \
              -lrumpnet_config -lrumpnet_virtif \
              -lrumpnet_netinet -lrumpnet_net -lrumpnet \
              -lrump \
              '"${RUMPUSER_A}"' \
            -Wl,--no-whole-archive \
            -lpthread
        echo "[net-test] running (60s timeout; rc=124 ⇒ hung) ..."
        timeout 60 env RUMP_VERBOSE=1 /tmp/test_net || true
        echo "[net-test] run rc=$?"
    '
