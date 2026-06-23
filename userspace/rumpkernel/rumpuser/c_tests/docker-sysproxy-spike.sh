#!/bin/bash
#
# docker-sysproxy-spike.sh — RUMP_SYSPROXY.md Step 1 (the spike).
# Prove the in-tree sysproxy SERVER source compiles + links against our header
# environment and our Rust rumpuser, so we can drop the 8 rumpuser_sp_* stubs.
#
#   src-netbsd/lib/librumpuser/rumpuser_sp.c   (#includes sp_common.c)
#
# Stage A: compile rumpuser_sp.c -> rumpuser_sp.o (catches the documented risks:
#          sys/atomic.h / machine/atomic.h on musl, the rumpuser_int.h /
#          rumpuser_port.h header environment).
# Stage B: link test_init + librump.a + rumpuser_akuma.a + rumpuser_sp.o, with
#          --allow-multiple-definition so the real sp_* override our Rust stubs.
set -eu
# NOTE: this script lives in rumpuser/c_tests/ (archived dev harness); HERE
# resolves to the rumpkernel root so the Docker mount + relative paths still work.
HERE="$(cd "$(dirname "$0")/../.." && pwd)"
RUMPUSER_A="rumpuser/target/aarch64-unknown-linux-musl/release/librumpuser_akuma.a"
[ -f "${HERE}/${RUMPUSER_A}" ] || { echo "missing ${RUMPUSER_A} — build it first" >&2; exit 1; }
[ -f "${HERE}/obj/dest.stage/usr/lib/librump.a" ] || { echo "missing librump.a — run ./docker-build.sh" >&2; exit 1; }

exec docker run --rm \
    --platform linux/arm64 \
    -v "${HERE}:/work" \
    -w /work \
    alpine:3.20 \
    sh -euc '
        set -o pipefail
        # bsd-compat-headers supplies sys/cdefs.h + sys/queue.h (musl lacks both).
        apk add --no-cache build-base bsd-compat-headers >/dev/null
        L=obj/dest.stage/usr/lib
        I=obj/dest.stage/usr/include
        SP=src-netbsd/lib/librumpuser
        # musl-tuned autoconf values (rumpuser_port.h hardcodes NetBSD/glibc-true
        # values unless RUMPUSER_CONFIG is set). Flip BSD-only HAVE_* off so the
        # header uses its musl fallbacks (e.g. SIN_SETLEN no-op: no sin_len on musl).
        mkdir -p /usr/local/include
        cat > /usr/local/include/rumpuser_config.h <<"CFG"
/*
 * rumpuser_config.h — musl/Alpine-tuned autoconf values for building the NetBSD
 * in-tree librumpuser sysproxy source. The HAVE_* names and intended semantics
 * are taken from NetBSD src/lib/librumpuser/rumpuser_port.h (NetBSD project,
 * BSD-licensed; copyright belongs to the NetBSD contributors). Values here are
 * adjusted for the musl C library (e.g. no sockaddr_in.sin_len, no getenv_r).
 */
#define HAVE_ALIGNED_ALLOC 1
#define HAVE_ARC4RANDOM_BUF 1
#define HAVE_CLOCKID_T 1
#define HAVE_CLOCK_GETTIME 1
#define HAVE_CLOCK_NANOSLEEP 1
#define HAVE_GETSUBOPT 1
#define HAVE_INTTYPES_H 1
#define HAVE_MEMORY_H 1
#define HAVE_PATHS_H 1
#define HAVE_POSIX_MEMALIGN 1
#define HAVE_STDINT_H 1
#define HAVE_STDLIB_H 1
#define HAVE_STRINGS_H 1
#define HAVE_STRING_H 1
#define HAVE_SYS_CDEFS_H 1
#define HAVE_SYS_PARAM_H 1
#define HAVE_SYS_STAT_H 1
#define HAVE_SYS_TYPES_H 1
#define HAVE_UNISTD_H 1
#define HAVE_UTIMENSAT 1
#define STDC_HEADERS 1
/* musl does NOT have: sin_len, getenv_r, set/getprogname, sys/atomic.h,
   chflags, kqueue, register_t, strsuftoll, sysctl, quotactl, dlinfo, etc.
   Leaving them undefined makes rumpuser_port.h use its portable fallbacks. */
CFG
        echo "=== Stage A: compile rumpuser_sp.c (+ sp_common.c) ==="
        # 2016 NetBSD source vs modern toolchain: -fcommon, tolerate warnings.
        # -DLIBRUMPUSER -D_KERNTYPES: from librumpuser/Makefile, opens the
        # rump/rumpuser.h kernel-consumer guard.
        gcc -O2 -fcommon -c -o /tmp/rumpuser_sp.o \
            "$SP/rumpuser_sp.c" \
            -I /usr/local/include -I "$I" -I "$SP" \
            -DLIBRUMPUSER -D_KERNTYPES -DRUMPUSER_CONFIG \
            -Wno-error 2>&1 | sed "s/^/[gcc] /" || { echo "STAGE_A_FAIL"; exit 1; }
        echo "STAGE_A_OK"
        ls -la /tmp/rumpuser_sp.o
        echo
        echo "=== Stage A2: compile rumpuser_errtrans.c (supplies rumpuser__errtrans) ==="
        gcc -O2 -fcommon -c -o /tmp/rumpuser_errtrans.o \
            "$SP/rumpuser_errtrans.c" \
            -I /usr/local/include -I "$I" -I "$SP" \
            -DLIBRUMPUSER -D_KERNTYPES -DRUMPUSER_CONFIG \
            -Wno-error 2>&1 | sed "s/^/[gcc] /" || { echo "STAGE_A2_FAIL"; exit 1; }
        echo "STAGE_A2_OK"
        echo
        echo "=== Stage B: link test_init + librump + rumpuser + sp + errtrans ==="
        gcc -O2 -static -o /tmp/test_init_sp \
            rumpuser/c_tests/test_init.c rumpuser/csupport.c \
            /tmp/rumpuser_sp.o /tmp/rumpuser_errtrans.o \
            -I "$I" \
            -Wl,--allow-multiple-definition \
            -Wl,--whole-archive \
              -L "$L" -lrump \
              '"${RUMPUSER_A}"' \
            -Wl,--no-whole-archive \
            -lpthread 2>&1 | sed "s/^/[ld] /" || { echo "STAGE_B_FAIL"; exit 1; }
        echo "STAGE_B_OK"
        ls -la /tmp/test_init_sp
        echo
        echo "=== Stage C: regression — rump_init() still boots with new rumpuser__hyp ==="
        RUMP_VERBOSE=1 /tmp/test_init_sp 2>&1 | sed "s/^/[run] /" || { echo "STAGE_C_FAIL rc=$?"; exit 1; }
        echo "STAGE_C_OK"
    '
