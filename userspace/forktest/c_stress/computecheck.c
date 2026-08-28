// computecheck — is the corruption in the READ path or in COMPUTATION?
//
// A subagent reproduced busybox `md5sum` returning wrong, non-deterministic
// digests for an unmodified file >4096 bytes, while `cat` of the same file
// returned byte-perfect data and `dd` was always right. Those two facts do not
// fit a stale page cache: a stale page is a *consistent* wrong value and would
// corrupt `cat` too.
//
// This probe removes the filesystem from the question entirely. One buffer is
// filled in memory ONCE, then hashed N times. Nothing is read, written, mapped
// or reopened between iterations, so every digest MUST be identical. If they
// diverge, the defect is in computation across preemption (register/FP state),
// not in ext2 or the page cache.
//
// Build: aarch64-linux-musl-gcc -static -O2 -Wall -Wextra -o computecheck computecheck.c

#define _GNU_SOURCE
#include <stdio.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

// FNV-1a over the buffer: a long, purely-GPR dependent chain. No syscalls, no
// FP, no library calls inside the loop.
static uint64_t fnv1a(const unsigned char *p, size_t n)
{
    uint64_t h = 1469598103934665603ULL;
    for (size_t i = 0; i < n; i++) {
        h ^= p[i];
        h *= 1099511628211ULL;
    }
    return h;
}

// A second, structurally different chain, so a divergence can be attributed to
// one kind of arithmetic rather than to "hashing" in general. Uses doubles, so
// it exercises FP/NEON register save-restore across preemption.
static double fpsum(const unsigned char *p, size_t n)
{
    double a = 1.0, b = 0.5;
    for (size_t i = 0; i < n; i++) {
        a += (double)p[i] * b;
        b = b * 0.999999 + 1e-9;
    }
    return a;
}

int main(int argc, char **argv)
{
    size_t len   = (argc > 1) ? (size_t)strtoul(argv[1], NULL, 10) : (256u * 1024u);
    int    iters = (argc > 2) ? atoi(argv[2]) : 400;

    unsigned char *buf = malloc(len);
    if (!buf) { perror("malloc"); return 2; }

    // Fill once. Deterministic, no I/O.
    for (size_t i = 0; i < len; i++)
        buf[i] = (unsigned char)((i * 7u + (i >> 3) + 11u) & 0xFF);

    uint64_t want_i = fnv1a(buf, len);
    double   want_f = fpsum(buf, len);

    int bad_int = 0, bad_fp = 0;
    uint64_t first_bad_i = 0;

    for (int k = 0; k < iters; k++) {
        uint64_t got_i = fnv1a(buf, len);
        double   got_f = fpsum(buf, len);
        if (got_i != want_i) {
            if (!bad_int) first_bad_i = got_i;
            bad_int++;
        }
        // Exact equality is correct here: identical input, identical operation
        // order, so a conforming FPU must return bit-identical results.
        if (got_f != want_f) bad_fp++;
    }

    printf("computecheck: len=%zu iters=%d\n", len, iters);
    printf("  integer chain : %d / %d wrong\n", bad_int, iters);
    if (bad_int)
        printf("    want=%016llx first_wrong=%016llx\n",
               (unsigned long long)want_i, (unsigned long long)first_bad_i);
    printf("  fp chain      : %d / %d wrong\n", bad_fp, iters);
    printf("RESULT: %s\n", (bad_int == 0 && bad_fp == 0) ? "PASS (compute is stable)"
                                                         : "FAIL (compute corrupted, no I/O involved)");
    return (bad_int == 0 && bad_fp == 0) ? 0 : 1;
}
