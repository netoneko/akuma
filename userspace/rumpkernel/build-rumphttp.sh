#!/bin/bash
#
# build-rumphttp.sh — link the M1 in-box payload `rumphttp` as a static Akuma
# binary (aarch64-linux-musl), with the rump TCP/IP stack + our rumpuser + the
# /dev/net/tap0 backend. Host link (same toolchain as userspace/build.sh).
#
# Output: /tmp/rumphttp_akuma  (copy to disk.img:/bin/rumphttp; run in a RUMP_NIC=1 box)
set -eu
HERE="$(cd "$(dirname "$0")" && pwd)"
cd "${HERE}"
I=obj/dest.stage/usr/include
L=obj/dest.stage/usr/lib
VDIR=src-netbsd/sys/rump/net/lib/libvirtif
RUMPUSER_A=rumpuser/target/aarch64-unknown-linux-musl/release/librumpuser_akuma.a

mkdir -p /tmp/inc/rump
cp src-netbsd/lib/librumpuser/rumpuser_component.h /tmp/inc/rump/

echo "[rumphttp] linking static Akuma binary ..."
aarch64-linux-musl-gcc -O2 -static -o /tmp/rumphttp_akuma \
    rumpuser/rumphttp.c rumpuser/rumpcomp_tap.c rumpuser/csupport.c \
    -I "$I" -I "$VDIR" -I /tmp/inc -DVIRTIF_BASE=virt \
    -Wl,--allow-multiple-definition \
    -Wl,--whole-archive \
      -L "$L" \
      -lrumpnet_config -lrumpnet_virtif -lrumpnet_netinet -lrumpnet_net -lrumpnet \
      -lrumpdev_bpf -lrumpdev -lrumpvfs \
      -lrump \
      "${RUMPUSER_A}" \
    -Wl,--no-whole-archive \
    -lpthread
echo "[rumphttp] built:"; file /tmp/rumphttp_akuma; ls -la /tmp/rumphttp_akuma
