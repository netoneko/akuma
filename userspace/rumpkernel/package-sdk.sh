#!/bin/bash
#
# package-sdk.sh — produce the rump-kernel SDK as a single installable tarball.
#
# This is the artifact you install into an Akuma environment to BUILD software
# against the NetBSD rump TCP/IP stack in-VM (e.g. sic over /dev/net/tap0 — see
# acceptance/11_netbsd_rumpkernel_irc.md). It bundles, under a /usr prefix so it
# untars straight onto a rootfs:
#
#   usr/lib/librump*.a            — rump kernel core + net factions
#   usr/lib/librumpnet_virtif.a   — the virtif NIC kernel driver
#   usr/lib/librumpuser_akuma.a   — OUR Rust rumpuser (the hypercall layer)
#   usr/include/rump/*.h          — rump + netconfig + rump_syscalls headers
#   usr/src/rumpuser/             — csupport.c + the virtif backend source, so the
#                                   final program can compile the C glue in-VM
#
# Output (git-ignored): bootstrap/archives/rump-sdk-aarch64-musl.tar.gz — staged
# like the rest of bootstrap/, so populate_disk.sh lands it at the VM's /archives
# (temporary install path until there's a real package step).
#
# Prereqs: ./docker-build.sh && ./docker-build-virtif.sh && the rumpuser .a built.
set -eu

HERE="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "${HERE}/../.." && pwd)"
STAGE="${HERE}/obj/dest.stage/usr"
RUMPUSER_A="${HERE}/rumpuser/target/aarch64-unknown-linux-musl/release/librumpuser_akuma.a"
VBACKEND="${HERE}/src-netbsd/sys/rump/net/lib/libvirtif/virtif_user.c"
COMPONENT_H="${HERE}/src-netbsd/lib/librumpuser/rumpuser_component.h"

OUT_DIR="${REPO_ROOT}/bootstrap/archives"
OUT="${OUT_DIR}/rump-sdk-aarch64-musl.tar.gz"

[ -d "${STAGE}/lib" ]    || { echo "missing ${STAGE}/lib — run ./docker-build.sh" >&2; exit 1; }
[ -f "${STAGE}/lib/librumpnet_virtif.a" ] || { echo "missing librumpnet_virtif.a — run ./docker-build-virtif.sh" >&2; exit 1; }
[ -f "${RUMPUSER_A}" ]   || { echo "missing librumpuser_akuma.a — build the rumpuser crate" >&2; exit 1; }

WORK="$(mktemp -d)"
trap 'rm -rf "${WORK}"' EXIT
mkdir -p "${WORK}/usr/lib" "${WORK}/usr/include" "${WORK}/usr/src/rumpuser"

echo "[sdk] collecting static archives + headers ..."
# Static archives only (.a); skip the .so set — Akuma is static-only.
cp "${STAGE}"/lib/librump*.a "${WORK}/usr/lib/"
cp "${RUMPUSER_A}" "${WORK}/usr/lib/"
cp -R "${STAGE}/include/." "${WORK}/usr/include/"
# the component header librumpuser would have installed (needed to compile the
# virtif backend in-VM); place it where the backend includes it from.
mkdir -p "${WORK}/usr/include/rump"
cp "${COMPONENT_H}" "${WORK}/usr/include/rump/"

echo "[sdk] collecting C glue sources (compiled in-VM by the final program) ..."
cp "${HERE}/rumpuser/csupport.c" "${WORK}/usr/src/rumpuser/"
cp "${VBACKEND}" "${WORK}/usr/src/rumpuser/virtif_user.c"
cp "${HERE}/src-netbsd/sys/rump/net/lib/libvirtif/if_virt.h" "${WORK}/usr/src/rumpuser/"
cp "${HERE}/src-netbsd/sys/rump/net/lib/libvirtif/virtif_user.h" "${WORK}/usr/src/rumpuser/"

cat > "${WORK}/usr/src/rumpuser/README" <<'EOF'
Akuma rump SDK — link a NetBSD-stack network program (static):

  tcc -static -I/usr/include -DVIRTIF_BASE=virt \
      yourprog.c /usr/src/rumpuser/csupport.c /usr/src/rumpuser/virtif_user.c \
      -Wl,--allow-multiple-definition -Wl,--whole-archive \
        -L/usr/lib -lrumpnet_config -lrumpnet_virtif -lrumpnet_netinet \
        -lrumpnet_net -lrumpnet -lrump -lrumpuser_akuma \
      -Wl,--no-whole-archive -lpthread -o yourprog

Then: rump_init(); rump_pub_netconfig_ifcreate("virt0");
      rump_pub_netconfig_ifsetlinkstr("virt0", "0"); ... ; rump_sys_socket(...).
The virtif backend opens /dev/net/tap0 (Akuma) — boot the kernel with RUMP_NIC=1.
EOF

mkdir -p "${OUT_DIR}"
echo "[sdk] writing ${OUT} ..."
tar -czf "${OUT}" -C "${WORK}" usr
echo "[sdk] done:"
ls -lh "${OUT}"
NA="$(tar -tzf "${OUT}" | grep -c 'usr/lib/.*\.a' || true)"
echo "[sdk] ${NA} static archives packaged; full listing via: tar -tzf ${OUT}"
