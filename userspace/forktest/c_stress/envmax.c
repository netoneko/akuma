/*
 * envmax.c — how large an environment, and how long an argument, `execve(2)`
 * accepts.
 *
 * Written 2026-09-13, after an `execve` limit that nobody had measured turned
 * out to be the wall every proc-macro crate hit on amd64: the loader accepted
 * 64 envp entries, a `cargo` build script's environment is ~62, and the child
 * died before its first instruction with an errno the parent never saw — cargo
 * reported `exit status: 1` with no output at all
 * (`docs/archive/RUST_TOOLCHAIN_AMD64.md` § session 5, defect 9).
 *
 * The two Akuma kernels reach `execve` by different routes — AArch64 through
 * `akuma-syscalls-glue`, amd64 through its own arm and `loader::build_stack` —
 * so "what does execve accept" is a question with two answers, and neither was
 * written down. This probe answers it the only way that cannot drift: by
 * calling the syscall.
 *
 * FOUR SWEEPS, each a binary search on a child that does nothing but `execve`:
 *
 *   argstr   one argv string, growing        — Linux `MAX_ARG_STRLEN`, 128 KiB
 *   envstr   one envp string, growing        — same cap, same path
 *   envcount many tiny envp entries          — Linux has no count limit at all
 *   envbytes many 1 KiB entries, total bytes — Linux `ARG_MAX`-ish, ~2 MiB
 *
 * The child reports **which** errno stopped it (`E2BIG`, `ENOMEM`, …) rather
 * than just failing, because "refused correctly" and "refused for the wrong
 * reason" look identical in a pass/fail. A child that dies on a signal is
 * reported too: on a kernel whose stack builder asserts rather than returning
 * an error, the interesting answer is a crash, and a probe that scored that as
 * "limit reached" would hide it.
 *
 * Run the same static binary on real Linux for the calibration arm; a number
 * here means nothing except next to that one.
 *
 * Static, musl, pure C.
 * Build: <arch>-linux-musl-gcc -static -O2 -Wall -Wextra -o envmax envmax.c
 * Usage: envmax [helper]        (default /bin/busybox, run as `<helper> true`)
 */
#define _GNU_SOURCE
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

/* Ceilings for the search. Past Linux's own limits, so a kernel with none of
 * its own still terminates. */
#define ARGSTR_MAX (1u << 21)  /* 2 MiB */
#define COUNT_MAX  200000u
#define BYTES_MAX  (8u << 20)  /* 8 MiB */

static const char *helper = "/bin/busybox";

/* Exit codes the child uses to tell the parent what stopped it. */
#define KID_EXECVE_OK 0 /* unreachable: execve succeeded, the helper's status wins */
#define KID_E2BIG 90
#define KID_ENOMEM 91
#define KID_EFAULT 92
#define KID_EINVAL 93
#define KID_OTHER 94
#define KID_SETUP 95 /* the probe could not even build the arrays */

static int kid_code_for(int e) {
    switch (e) {
        case E2BIG: return KID_E2BIG;
        case ENOMEM: return KID_ENOMEM;
        case EFAULT: return KID_EFAULT;
        case EINVAL: return KID_EINVAL;
        default: return KID_OTHER;
    }
}

static const char *code_name(int code) {
    switch (code) {
        case KID_E2BIG: return "E2BIG";
        case KID_ENOMEM: return "ENOMEM";
        case KID_EFAULT: return "EFAULT";
        case KID_EINVAL: return "EINVAL";
        case KID_OTHER: return "other errno";
        case KID_SETUP: return "probe out of memory";
        default: return "?";
    }
}

/* Outcome of one attempt. */
struct outcome {
    int ok;      /* the helper ran and exited 0 */
    int code;    /* KID_* when execve failed */
    int signo;   /* non-zero when the child died on a signal */
};

/* Fork, build the requested argv/envp in the child, execve the helper. */
static struct outcome attempt(size_t argstr, size_t envstr, size_t envcount, size_t envbytes) {
    struct outcome o = {0, 0, 0};
    pid_t kid = fork();
    if (kid < 0) {
        o.code = KID_SETUP;
        return o;
    }
    if (kid == 0) {
        /* argv: helper, "true", and optionally one long string. */
        char *big = NULL;
        if (argstr) {
            big = malloc(argstr + 1);
            if (!big) _exit(KID_SETUP);
            memset(big, 'a', argstr);
            big[argstr] = 0;
        }
        char *argv[4];
        int ai = 0;
        argv[ai++] = (char *)helper;
        argv[ai++] = (char *)"true";
        if (big) argv[ai++] = big;
        argv[ai] = NULL;

        /* envp: `envcount` tiny entries, or entries of `envstr`/`envbytes`. */
        size_t n = envcount ? envcount : (envstr ? 1 : (envbytes ? envbytes / 1024 : 0));
        char **envp = calloc(n + 2, sizeof(char *));
        if (!envp) _exit(KID_SETUP);
        size_t each = envstr ? envstr : (envbytes ? 1024 : 8);
        for (size_t i = 0; i < n; i++) {
            char *e = malloc(each + 32);
            if (!e) _exit(KID_SETUP);
            int pre = snprintf(e, 32, "E%06zu=", i);
            memset(e + pre, 'v', each);
            e[pre + each] = 0;
            envp[i] = e;
        }
        envp[n] = NULL;

        execve(helper, argv, envp);
        _exit(kid_code_for(errno));
    }
    int st = 0;
    waitpid(kid, &st, 0);
    if (WIFSIGNALED(st)) {
        o.signo = WTERMSIG(st);
        return o;
    }
    int code = WEXITSTATUS(st);
    if (code == 0) {
        o.ok = 1;
    } else {
        o.code = code;
    }
    return o;
}

/* Largest accepted value of one parameter, by binary search over `attempt`. */
static void sweep(const char *name, int which, size_t lo, size_t hi) {
    /* `lo` must be known-good; if it is not, say so instead of searching. */
    struct outcome base = attempt(which == 0 ? lo : 0, which == 1 ? lo : 0,
                                  which == 2 ? lo : 0, which == 3 ? lo : 0);
    if (!base.ok) {
        printf("%-9s FAILS EVEN AT %zu (%s%s)\n", name, lo,
               base.signo ? "died on signal " : code_name(base.code),
               base.signo ? "" : "");
        if (base.signo) printf("%-9s   signal %d\n", name, base.signo);
        return;
    }
    size_t good = lo, bad = 0;
    int last_code = 0, last_signo = 0;
    /* Grow until something breaks, so an unbounded kernel stops at the ceiling. */
    for (size_t probe = lo * 2; probe <= hi; probe *= 2) {
        struct outcome o = attempt(which == 0 ? probe : 0, which == 1 ? probe : 0,
                                   which == 2 ? probe : 0, which == 3 ? probe : 0);
        if (o.ok) {
            good = probe;
        } else {
            bad = probe;
            last_code = o.code;
            last_signo = o.signo;
            break;
        }
    }
    if (!bad) {
        printf("%-9s >= %zu (no limit found below the ceiling)\n", name, good);
        return;
    }
    while (bad - good > (good / 64 + 1)) {
        size_t mid = good + (bad - good) / 2;
        struct outcome o = attempt(which == 0 ? mid : 0, which == 1 ? mid : 0,
                                   which == 2 ? mid : 0, which == 3 ? mid : 0);
        if (o.ok) {
            good = mid;
        } else {
            bad = mid;
            last_code = o.code;
            last_signo = o.signo;
        }
    }
    if (last_signo) {
        printf("%-9s %zu ok, %zu KILLED BY SIGNAL %d\n", name, good, bad, last_signo);
    } else {
        printf("%-9s %zu ok, %zu -> %s\n", name, good, bad, code_name(last_code));
    }
}

int main(int argc, char **argv) {
    if (argc > 1) helper = argv[1];
    /* One plain run first: if the helper cannot be exec'd at all, every number
     * below would be zero for a reason that has nothing to do with limits. */
    struct outcome base = attempt(0, 0, 0, 0);
    if (!base.ok) {
        printf("envmax: cannot exec %s at all (%s) — nothing measured\n", helper,
               base.signo ? "died on signal" : code_name(base.code));
        return 2;
    }
    printf("envmax: helper %s\n", helper);
    sweep("argstr", 0, 64, ARGSTR_MAX);
    sweep("envstr", 1, 64, ARGSTR_MAX);
    sweep("envcount", 2, 8, COUNT_MAX);
    sweep("envbytes", 3, 4096, BYTES_MAX);
    printf("envmax: END\n");
    return 0;
}
