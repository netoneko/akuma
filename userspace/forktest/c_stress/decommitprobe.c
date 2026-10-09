/*
 * decommitprobe: an allocator's decommit/recommit cycle, as Chromium's
 * PartitionAlloc does it inside the PROT_NONE pool it reserves up front:
 *
 *   reserve   mmap(PROT_NONE, big)
 *   commit    mprotect(sub-range, PROT_READ|PROT_WRITE)         -> use the pages
 *   decommit  madvise(MADV_DONTNEED) and mprotect(PROT_NONE)   (either order)
 *   recommit  mprotect(sub-range, PROT_READ|PROT_WRITE)         -> use again
 *
 * After a recommit the pages must be writable, and after DONTNEED they must read
 * zero; an mprotect(NONE) -> mprotect(RW) with no DONTNEED keeps the contents.
 *
 * Akuma amd64 before 2026-10-09: `mprotect(NONE)` rewrites a present page to
 * `user = false`, and the leaf rewrite skipped every `user = false` page, so the
 * recommit changed the region but not the page: the next write was `#PF`
 * protection (SEGV_ACCERR) on memory the region said was RW. Every Chromium
 * renderer died of it at its third document load (kami's `nav_try.py`). The
 * same `user` test in MADV_DONTNEED left a hidden page's old bytes behind a
 * recommit. Run once in the parent and once in a forked child (CoW-shared pages
 * take the same path).
 *
 * Verified 2026-10-09 on the ryzen metal: fixed kernel, 36 checks ok and PASS;
 * the kernel before the fix (negative control), `A first page writable FAIL`,
 * `A last page writable FAIL`, then the next plain store dies (the handler exits
 * 99). **The Linux control was not run** (no Docker on the laptop that day, and
 * the ryzen was in Akuma): the expectations are the documented ones — DONTNEED
 * zero-fills private anonymous memory, mprotect alone keeps contents — and
 * macOS, which does not zero on DONTNEED, is not a control. Run it on Linux
 * before trusting a surprising failure.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <setjmp.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

#define PG 4096UL
#define PAGES 64UL
#define POOL (16UL << 20)

static int fails;
static sigjmp_buf jb;
static volatile sig_atomic_t in_try;

static void on_segv(int sig) { (void)sig; if (in_try) siglongjmp(jb, 1); _exit(99); }

static void check(const char *tag, const char *what, int ok) {
    printf("decommitprobe: %s %s %s\n", tag, what, ok ? "ok" : "FAIL");
    if (!ok) fails++;
}

/* A store that reports a SIGSEGV as 0 instead of dying. */
static int try_write(volatile char *p, char v) {
    in_try = 1;
    if (sigsetjmp(jb, 1)) { in_try = 0; return 0; }
    *p = v;
    in_try = 0;
    return 1;
}

static int all_zero(const char *p, size_t n) {
    for (size_t i = 0; i < n; i++) if (p[i]) return 0;
    return 1;
}

static void cycle(const char *tag) {
    char *pool = mmap(NULL, POOL, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    check(tag, "reserve PROT_NONE", pool != MAP_FAILED);
    if (pool == MAP_FAILED) return;
    char *r = pool + (1UL << 20);            /* a sub-range inside the pool */
    size_t len = PAGES * PG;

    check(tag, "commit mprotect RW", mprotect(r, len, PROT_READ | PROT_WRITE) == 0);
    memset(r, 0xA5, len);

    /* A: decommit = mprotect(NONE) then DONTNEED; recommit = mprotect(RW). */
    check(tag, "A mprotect NONE", mprotect(r, len, PROT_NONE) == 0);
    check(tag, "A madvise DONTNEED", madvise(r, len, MADV_DONTNEED) == 0);
    check(tag, "A recommit RW", mprotect(r, len, PROT_READ | PROT_WRITE) == 0);
    check(tag, "A first page writable", try_write(r, 1));
    check(tag, "A last page writable", try_write(r + len - 1, 1));
    r[0] = 0; r[len - 1] = 0;
    check(tag, "A reads zero after DONTNEED", all_zero(r, len));
    memset(r, 0xA5, len);

    /* B: decommit = DONTNEED then mprotect(NONE). */
    check(tag, "B madvise DONTNEED", madvise(r, len, MADV_DONTNEED) == 0);
    check(tag, "B mprotect NONE", mprotect(r, len, PROT_NONE) == 0);
    check(tag, "B recommit RW", mprotect(r, len, PROT_READ | PROT_WRITE) == 0);
    check(tag, "B writable", try_write(r + 5 * PG, 1));
    r[5 * PG] = 0;
    check(tag, "B reads zero", all_zero(r, len));
    memset(r, 0x5A, len);

    /* C: NONE then RW with no DONTNEED keeps the contents (Linux does). */
    check(tag, "C mprotect NONE", mprotect(r, len, PROT_NONE) == 0);
    check(tag, "C recommit RW", mprotect(r, len, PROT_READ | PROT_WRITE) == 0);
    check(tag, "C writable", try_write(r + 7 * PG, 0x5A));
    int kept = 1;
    for (size_t i = 0; i < len; i++) if (r[i] != 0x5A) { kept = 0; break; }
    check(tag, "C contents kept", kept);

    munmap(pool, POOL);
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_segv;
    sigaction(SIGSEGV, &sa, NULL);

    cycle("parent");

    /* The same, in a forked child over CoW-shared pages. */
    char *pool = mmap(NULL, POOL, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    char *r = pool + (2UL << 20);
    mprotect(r, PAGES * PG, PROT_READ | PROT_WRITE);
    memset(r, 0xC3, PAGES * PG);
    pid_t pid = fork();
    if (pid == 0) {
        int bad = 0;
        if (mprotect(r, PAGES * PG, PROT_NONE)) bad++;
        if (madvise(r, PAGES * PG, MADV_DONTNEED)) bad++;
        if (mprotect(r, PAGES * PG, PROT_READ | PROT_WRITE)) bad++;
        if (!try_write(r + 3 * PG, 7)) bad++;
        r[3 * PG] = 0;
        if (!all_zero(r, PAGES * PG)) bad++;
        cycle("child");
        _exit(bad || fails ? 1 : 0);
    }
    int st = 0;
    waitpid(pid, &st, 0);
    check("fork", "child cycle clean", WIFEXITED(st) && WEXITSTATUS(st) == 0);
    int intact = 1;
    for (size_t i = 0; i < PAGES * PG; i++) if (r[i] != (char)0xC3) { intact = 0; break; }
    check("fork", "parent's pages untouched by the child's DONTNEED", intact);
    printf("decommitprobe: %s\n", fails ? "FAIL" : "PASS");
    return fails != 0;
}
