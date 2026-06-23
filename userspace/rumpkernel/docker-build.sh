#!/bin/bash
#
# docker-build.sh — build librump* for aarch64-linux-musl inside a Linux
# container, sidestepping the macOS-Apple-clang host-tool failures (see
# docs/PHASE01_BUILDRUMP.md).
#
# Why a container: the pinned NetBSD source is from 2016 and won't compile its
# *host* tools under modern Apple clang 17 (implicit-function-declaration /
# implicit-int are now hard errors). A Linux container gives us:
#   - gcc as the host compiler (lax: those stay warnings, not errors), and
#   - on an arm64 host, Alpine is musl-NATIVE on aarch64 — so building "natively"
#     in the container *is* building for aarch64-linux-musl (Akuma's target);
#     no cross toolchain needed, and the ABI matches userspace/build.sh's
#     aarch64-linux-musl-gcc output.
#   - `linux-headers` provides <linux/if_tun.h>, so RUMP_VIRTIF can be enabled
#     (the macOS musl sysroot lacked it — plan §7 #1).
#
# Usage:  ./docker-build.sh
# Outputs (git-ignored): obj/ + dest/ in this directory (written by the container).
set -eu

HERE="$(cd "$(dirname "$0")" && pwd)"
IMAGE="alpine:3.20"

# On an arm64 host, the default platform is linux/arm64 → musl-native aarch64.
# On x86_64 hosts, force arm64 emulation so the target triple still matches.
PLATFORM="linux/arm64"

echo "[docker-build] building librump* in ${IMAGE} (${PLATFORM}) — native aarch64-musl" >&2

exec docker run --rm \
    --platform "${PLATFORM}" \
    -v "${HERE}:/work" \
    -w /work \
    "${IMAGE}" \
    sh -euc '
        echo "[container] installing toolchain (gcc/g++/binutils/linux-headers/...)";
        apk add --no-cache build-base bash git linux-headers flex bison zlib-dev perl >/dev/null;
        echo "[container] gcc: $(gcc --version | head -1)";
        echo "[container] target triple: $(gcc -dumpmachine)";
        # gcc/g++/cc wrappers: force -fcommon (the 2016 NetBSD source relies on
        # pre-gcc10 common-symbol merging; gcc 10+ defaults to -fno-common →
        # "multiple definition" link errors), and -Wno-error so the old codebase
        # is not killed by new default-on -Werror warnings. Applied uniformly to
        # host tools, nbmake, and the target rump libs — robust vs. per-var flag
        # plumbing through NetBSD build.sh.
        # BSD cdefs shim: musls headers do not provide __BEGIN_DECLS/__END_DECLS
        # (glibc and BSD do), which NetBSDs own headers (e.g. include/regex.h)
        # assume. Force-include this so the host-tool build on musl matches what
        # the rump CIs glibc hosts got for free.
        mkdir -p /usr/local/include;
        cat > /usr/local/include/akuma_bsd_shim.h <<"SHIM"
#ifndef AKUMA_BSD_SHIM_H
#define AKUMA_BSD_SHIM_H
#ifndef __BEGIN_DECLS
# ifdef __cplusplus
#  define __BEGIN_DECLS extern "C" {
#  define __END_DECLS }
# else
#  define __BEGIN_DECLS
#  define __END_DECLS
# endif
#endif
#endif
SHIM
        # Flags go AFTER "$@" so they win over NetBSDs own CFLAGS: the 2016 source
        # is compiled with -Werror, and modern gcc flags many things as errors
        # (cast-function-type, uninitialized, macro redefined, ...). A trailing
        # -Wno-error downgrades them all back to warnings; -fcommon handles the
        # gcc10 -fno-common default. The shim (-include) supplies BSD cdefs for
        # the musl host tools (harmless, guarded, on the target).
        mkdir -p /usr/local/bin;
        for t in gcc g++ cc; do
            real="/usr/bin/${t}";
            [ "$t" = cc ] && real="/usr/bin/gcc";
            printf "#!/bin/sh\nexec %s -include /usr/local/include/akuma_bsd_shim.h \"\$@\" -fcommon -Wno-error\n" "$real" > "/usr/local/bin/${t}";
            chmod +x "/usr/local/bin/${t}";
        done;
        export PATH="/usr/local/bin:$PATH";
        # CROSS= → build.sh uses the native gcc/g++/as/ld/... (no prefix). On this
        # arm64 musl container that natively targets aarch64-linux-musl.
        export CROSS=;
        export JOBS="$(nproc)";
        # Clean any obj/ left from a prior attempt — its tooldir has stale paths.
        bash ./build.sh clean || true;
        bash ./build.sh build;
    '
