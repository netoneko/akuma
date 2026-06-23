#!/bin/bash
#
# docker-build-syslinux.sh — build librumpkern_sys_linux (the Linux syscall ABI
# translation layer for rump). buildrump skips it for evbarm64 (it's only added to
# DIRS_emul for i386/amd64/evbearm/evbppc), so we build the dir directly via the
# generated rumpmake — same trick as docker-build-virtif.sh.
#
# Goal: let the rump kernel accept the Linux syscall ABI natively (rump_linux_sys_*)
# so an unmodified Linux binary's args (sockaddr without sin_len, SOCK_NONBLOCK,
# Linux errnos/ioctls) are translated INSIDE the kernel instead of in our hijack
# shim. NetBSD 7.99.34 (2016) has no compat/linux/arch/aarch64, but the layer is
# mostly the arch-independent compat/linux/common/* sources — see if it builds.
#
# Output (git-ignored): obj/dest.stage/usr/lib/librumpkern_sys_linux{,_pic}.a
set -eu
HERE="$(cd "$(dirname "$0")" && pwd)"
[ -x "${HERE}/obj/tooldir/rumpmake" ] || { echo "run ./docker-build.sh first" >&2; exit 1; }

exec docker run --rm --platform linux/arm64 \
    -v "${HERE}:/work" -w /work alpine:3.20 \
    sh -euc '
        apk add --no-cache build-base bash linux-headers >/dev/null
        mkdir -p /usr/local/bin
        for t in gcc g++ cc; do real="/usr/bin/${t}"; [ "$t" = cc ] && real="/usr/bin/gcc";
            printf "#!/bin/sh\nexec %s \"\$@\" -fcommon -Wno-error\n" "$real" > "/usr/local/bin/${t}"; chmod +x "/usr/local/bin/${t}"; done
        export PATH="/usr/local/bin:$PATH"
        RM=/work/obj/tooldir/rumpmake
        cd /work/src-netbsd/sys/rump/kern/lib/libsys_linux
        for pass in obj includes dependall; do
            echo "[syslinux] === pass: ${pass} ==="
            "${RM}" "${pass}"
        done
        for a in librumpkern_sys_linux.a librumpkern_sys_linux_pic.a; do
            f=$(find /work/obj -name "$a" | head -1)
            [ -n "${f}" ] && cp "${f}" /work/obj/dest.stage/usr/lib/ && echo "[syslinux] STAGED ${a}" || echo "[syslinux] (no ${a})"
        done
    '
