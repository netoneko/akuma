#!/bin/bash
#
# docker-rumpuser-test.sh — Phase 2 exit test, in the Linux container:
# link librump.a + our Rust rumpuser staticlib + the dprintf C shim, and prove
# rump_init() returns success (a NetBSD rump kernel booting on our hypercalls).
#
# Prereqs (run first):
#   ./build.sh checkout && ./docker-build.sh                 # → obj/dest.stage/usr/{lib,include}
#   (cd rumpuser && cargo build --release --target aarch64-unknown-linux-musl)  # → the .a
#
# The rumpuser staticlib is built on the host (no_std, no link step needed) and
# linked here; same aarch64-musl ABI as the container.
set -eu

HERE="$(cd "$(dirname "$0")" && pwd)"
RUMPUSER_A="rumpuser/target/aarch64-unknown-linux-musl/release/librumpuser_akuma.a"

[ -f "${HERE}/${RUMPUSER_A}" ] || { echo "missing ${RUMPUSER_A} — build it first" >&2; exit 1; }
[ -f "${HERE}/obj/dest.stage/usr/lib/librump.a" ] || { echo "missing librump.a — run ./docker-build.sh" >&2; exit 1; }

exec docker run --rm \
    --platform linux/arm64 \
    -v "${HERE}:/work" \
    -w /work \
    alpine:3.20 \
    sh -euc '
        apk add --no-cache build-base >/dev/null
        L=obj/dest.stage/usr/lib
        I=obj/dest.stage/usr/include
        echo "[test] linking test_init + librump.a + rumpuser_akuma.a ..."
        # --whole-archive pulls the RUMP_COMPONENT constructors out of librump.a
        # (they self-register via linker sets), and all our rumpuser_* symbols.
        # -static: force the .a (the dir also has librump.so, which -lrump would
        # otherwise prefer → runtime "librump.so.0 not found"). Also matches
        # Akumas static-only ELF model.
        gcc -O2 -static -o /tmp/test_init \
            rumpuser/test_init.c rumpuser/csupport.c \
            -I "$I" \
            -Wl,--whole-archive \
              -L "$L" -lrump \
              '"${RUMPUSER_A}"' \
            -Wl,--no-whole-archive \
            -lpthread
        echo "[test] running rump_init() ..."
        RUMP_VERBOSE=1 /tmp/test_init
        echo "[test] exit=$?"
    '
