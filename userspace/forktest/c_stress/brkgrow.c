/* brkgrow.c — does brk(2) move the break the way Linux does?
 *
 * Why: until 2026-11-12 the two kernels had two answers. AArch64 grew the
 * break with no check for what it grew over and never unmapped on a shrink;
 * amd64 never moved it at all. Both run `Process::set_brk` now.
 *
 * Linux semantics checked (brk(2), raw syscall, not sbrk):
 *   grow          the new pages are there and zero
 *   shrink+regrow the pages past a lowered break come back zero
 *   over a map    growth onto a MAP_FIXED page answers the old break,
 *                 and the page is untouched
 *   huge          a break 1 TiB up answers the old break
 *
 * Build: aarch64-linux-musl-gcc -O2 -static -o brkgrow brkgrow.c
 * Run:   ./brkgrow       (PASS/FAIL per case, exit 0 iff all pass)
 */
#define _GNU_SOURCE
#include <stdint.h>
#include <stdio.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <unistd.h>

static int fails;

static void check(const char *what, int ok) {
    printf("brkgrow: %-22s %s\n", what, ok ? "PASS" : "FAIL");
    fails |= !ok;
}

static uintptr_t brk_(uintptr_t to) { return (uintptr_t)syscall(SYS_brk, to); }

static int zero(const unsigned char *p, size_t n) {
    for (size_t i = 0; i < n; i++) if (p[i]) return 0;
    return 1;
}

int main(void) {
    const size_t pg = 4096, n = 16 * pg;
    uintptr_t base = (brk_(0) + pg - 1) & ~(uintptr_t)(pg - 1);
    brk_(base);

    unsigned char *p = (unsigned char *)base;
    int grew = brk_(base + n) == base + n;
    check("grow", grew && zero(p, n));
    if (!grew) return 1;
    for (size_t i = 0; i < n; i++) p[i] = 0xa5;

    check("shrink", brk_(base + pg) == base + pg);
    check("regrow is zero", brk_(base + n) == base + n && p[0] == 0xa5 && zero(p + pg, n - pg));

    /* A page of someone else's, right above the break. */
    unsigned char *m = mmap((void *)(base + n + 4 * pg), pg, PROT_READ | PROT_WRITE,
                            MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0);
    if (m != MAP_FAILED) {
        m[0] = 0x5a;
        check("refused over a map", brk_(base + n + 8 * pg) == base + n && m[0] == 0x5a);
        munmap(m, pg);
    }
    check("huge refused", brk_(base + n + ((uintptr_t)1 << 40)) == base + n);
    check("still usable", brk_(base + 2 * n) == base + 2 * n && zero(p + n, n));

    puts(fails ? "brkgrow: FAIL" : "brkgrow: all PASS");
    return fails;
}
