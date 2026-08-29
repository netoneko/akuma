/*
 * mem_op_cost — what one memory syscall costs, per op, without faulting.
 *
 * The sibling of `userspace/epollprobe/c/epoll_op_cost.c` and
 * `userspace/futexprobe/c/futex_op_cost.c`, deliberately down to the reporting
 * format: it prints the same `<arm> <ns> (floor+N) mean W worst X ret=R` lines,
 * so `scripts/benchmarks/futex_op_ab.py` drives it unchanged
 * (`--exe /tmp/mem_op_cost`). That aggregator is arm-agnostic; a third copy of
 * its 146 lines under a mem name would be a third place to get the
 * ratio-not-ns rule wrong.
 *
 * Built for BOTH kernels from this one source (musl static, aarch64), so an
 * Akuma number and a Linux number differ by the kernel and nothing else.
 *
 * Why the memory family: `src/syscall/mem.rs`'s pure decisions — mmap's
 * mapping-kind plan and MAP_FIXED validation, mremap's move-vs-expand, madvise's
 * advice decode, munmap's sizing and membarrier's command decode — were
 * extracted into `crates/akuma-syscalls-mem` so they could be host-tested
 * instead of boot-tested (`docs/archive/AKUMA_EXTRACT_MMAP.md` §10). An
 * extraction is only allowed to be free; this is the instrument that says
 * whether it was. Unlike epoll, mmap/munmap sit on the fault path, so this gate
 * has a real chance of saying something.
 *
 * Every arm here **returns without faulting**, on purpose. An arm that
 * demand-pages measures the fault path and the PMM, whose variance swamps
 * anything a decode change could do. What is left is the argument decode, the
 * validation, and the bookkeeping the extraction actually moved.
 *
 *   getpid        control. Not a memory call at all — the syscall floor. If this
 *                 moves between two arms, the arms are not comparable and no
 *                 number in the run means anything. MUST stay first.
 *   mmap_einval   mmap(MAP_FIXED, unaligned addr) -> EINVAL. Pure decode: the
 *                 alignment guard rejects it BEFORE any process lookup, so this
 *                 is the cheapest path that reaches the extracted code and is
 *                 the primary A/B number.
 *   mmap_enomem   raw mmap(len = SIZE_MAX) -> ENOMEM. The length guard added
 *                 2026-08-29. See the note below: this arm CANNOT be run
 *                 against a pre-fix baseline, because pre-fix it does not
 *                 return.
 *   munmap_noent  munmap() of a page-aligned VA with nothing mapped there. The
 *                 sizing decision plus an empty region detach; no frame is
 *                 freed, but the span TLB flush still runs.
 *   mprotect_noop mprotect() to the protection a mapped page already has. The
 *                 prot -> PTE flag decode plus the lazy/eager region flag
 *                 update. Touches no page contents.
 *   madv_unmapped MADV_DONTNEED over a reserved-but-never-touched range. Every
 *                 page takes the `Nothing` arm of the per-page rule, so this
 *                 measures the range rule and the walk with no frame work.
 *   madv_einval   madvise(len = SIZE_MAX) -> EINVAL. The range guard added
 *                 2026-08-29. Same caveat as mmap_enomem.
 *   membarrier    MEMBARRIER_CMD_QUERY -> supported bitmask. Command decode
 *                 only; the QUERY arm issues no barrier.
 *   brk_query     brk(0) -> current break. The cheapest real memory syscall,
 *                 and a second floor-ish reading next to getpid.
 *   brk_noop      brk(current) -> the same break. Reaches `set_brk` and takes
 *                 its no-growth exit, so it prices the call without the page
 *                 allocation a real grow does (that is `mem_fault_cost`).
 *   mremap_inplace  mremap() shrinking within the pages already held. The
 *                 shrink short-circuit: pure decision, returns the old address
 *                 before any process is resolved. Idempotent across passes
 *                 precisely BECAUSE of divergence 5 — the tail stays mapped.
 *   mremap_efault mremap() with an out-of-range old address. Decode only in
 *                 principle — but the kernel writes an `[EFAULT]` diagnostic
 *                 line to the serial console on EVERY EFAULT-returning syscall
 *                 (`config::SYSCALL_ERRNO_DIAG_ENABLED`, gated `&& is_efault`,
 *                 on by default outside `extreme` for the GO_FORKTEST_DEBUG §E
 *                 investigation). So this arm reads ~250 us, ~1600x the floor,
 *                 and what it measures is a UART write. Kept, because that IS
 *                 what an EFAULT costs on this kernel today and userspace can
 *                 trigger it at will; the `>50x floor` marker says so out loud.
 *                 Turn the flag off and it should collapse to about
 *                 `mmap_einval` — the EINVAL path, which is NOT traced.
 *   madv_willneed MADV_WILLNEED over an already-resident page: the advice
 *                 decode plus a walk that finds nothing to prefault, so it
 *                 allocates nothing. The prefaulting case is a PMM measurement
 *                 and lives in `mem_fault_cost`.
 *
 * A/B NOTE. `mmap_enomem` and `madv_einval` exercise guards that did not exist
 * before 2026-08-29. Against a pre-fix baseline they do not return a number —
 * they hang the guest, because the unvalidated length became a ~4.5e15-iteration
 * loop inside an `MmBklGuard` window (AKUMA_EXTRACT_MMAP.md §10.1). That is not
 * a measurement failure; it is the finding. Compare the other seven arms.
 *
 * Reporting follows `docs/archive/AKUMA_SYSCALL_PERFORMANCE_AUDIT.md`
 * § "Method warnings", and the three findings that method cost to get right:
 *
 *  1. A probe's own warm-up can invalidate it. The clock calibration below is
 *     bounded on PURPOSE — its ancestor spun 200,000 clock_gettime calls, which
 *     on Akuma is 200,000 real syscalls, and every arm after it read ~2x the
 *     floor the same boot's other probes reported.
 *  2. `floor+N` is NOT drift-invariant. The drift is multiplicative: a slower
 *     boot slows the whole syscall path, not just its fixed part. Read the RATIO
 *     (`arm / getpid`), and prefer SMP=4, which is far steadier than SMP=1.
 *  3. The resolution floor is `clock_gettime`'s MICROSECOND truncation divided
 *     by `calls`, not the 41.7 ns counter tick: 1000/calls ns per call, i.e.
 *     2 ns at the default 500. Sweep `calls` to prove a delta is real work.
 *
 * Usage:  mem_op_cost [passes] [calls] [hostile]   (default 100 x 500 x 1)
 *         hostile=0 skips the two arms a pre-fix kernel cannot survive.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

#ifndef __NR_membarrier
#define __NR_membarrier 283
#endif

static long long now_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (long long)ts.tv_sec * 1000000000LL + ts.tv_nsec;
}

/* Plain globals, not stack: Akuma has no ASLR, so these addresses are stable
 * across runs, which makes a line from one run comparable to the next. */
static char *rw_page;      /* one resident, writable page for mprotect_noop */
static char *reserved;     /* PROT_NONE reservation, never touched */
static char *hole;         /* page-aligned VA with nothing mapped at it */
static char *remap2;       /* two resident pages, for the mremap shrink arm */
static long  cur_brk;      /* the break as it stands, for the no-op brk arm */

#define RESERVE_BYTES (64 * 4096)

#define LONG_WANT_ANY 0x7fffffffL
#define ARM(name, want, expr)                                                  \
    do {                                                                       \
        long long best = -1, worst = 0, total = 0;                             \
        long r = 0;                                                            \
        (void)r;                                                               \
        for (int p = 0; p < passes; p++) {                                     \
            long long t0 = now_ns();                                           \
            for (int i = 0; i < calls; i++) {                                  \
                r = (expr);                                                    \
            }                                                                  \
            long long d = (now_ns() - t0) / calls;                             \
            if (best < 0 || d < best) best = d;                                \
            if (d > worst) worst = d;                                          \
            total += d;                                                        \
        }                                                                      \
        /* Verify the arm did what it claims before believing its cost: an arm \
         * silently returning EINVAL from a typo would be the cheapest and     \
         * most convincing number in the table. */                             \
        errno = 0;                                                             \
        long check = (expr);                                                   \
        int ok = ((want) == LONG_WANT_ANY) ? (check >= 0)                      \
                 : (check == (want) || (check == -1 && errno == (want)));      \
        if (floor_ns < 0) floor_ns = best;                                     \
        /* An arm dozens of times the syscall floor is not measuring a decode.\
         * `mremap_efault` read 1600x on 2026-08-29: the kernel writes a ~192-byte\
         * `[EFAULT]` line to the SERIAL CONSOLE on every EFAULT-returning syscall\
         * (`config::SYSCALL_ERRNO_DIAG_ENABLED`, gated `&& is_efault`), so the arm\
         * was timing a UART write. Silence would have put a console write in a\
         * decode-cost table — the same class of error as an arm returning the\
         * wrong value, so it gets the same treatment. It FLAGS, it does not fail:\
         * some arms legitimately do real work (`madv_unmapped` walks 64 pages). */\
        int loud = floor_ns > 0 && best > floor_ns * 50;              \
        printf("%-14s %6lld ns   (floor%+5lld)   mean %6lld   worst %7lld   "\
               "ret=%ld %s%s\n",                                      \
               name, best, best - floor_ns, total / passes, worst, check,      \
               ok ? "" : "  <-- UNEXPECTED RETURN, arm is not measuring what it says",\
               loud ? "  <-- >50x floor: something other than the named op dominates" : "");\
        if (!ok) bad++;                                                        \
    } while (0)

int main(int argc, char **argv) {
    int passes = argc > 1 ? atoi(argv[1]) : 100;
    int calls = argc > 2 ? atoi(argv[2]) : 500;
    /* 0 skips the two arms that a pre-fix kernel cannot survive. */
    int hostile = argc > 3 ? atoi(argv[3]) : 1;
    int bad = 0;
    long long floor_ns = -1;
    if (passes < 1 || calls < 1) {
        fprintf(stderr, "usage: %s [passes] [calls] [hostile]\n", argv[0]);
        return 2;
    }

    /* Bounded warm-up — see method warning 1 in the header. */
    long long c0 = now_ns(), cres = -1;
    for (int i = 0, seen = 0; i < 20000 && seen < 50; i++) {
        long long c1 = now_ns();
        if (c1 != c0) {
            if (cres < 0 || c1 - c0 < cres) cres = c1 - c0;
            c0 = c1;
            seen++;
        }
    }
    printf("mem_op_cost: %d passes x %d calls, cheapest pass wins; "
           "clock tick %lld ns\n", passes, calls, cres);

    rw_page = mmap(NULL, 4096, PROT_READ | PROT_WRITE,
                   MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (rw_page == MAP_FAILED) { fprintf(stderr, "mmap rw_page failed\n"); return 2; }
    rw_page[0] = 1;   /* make it resident so mprotect_noop touches no fault path */

    reserved = mmap(NULL, RESERVE_BYTES, PROT_NONE,
                    MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0);
    if (reserved == MAP_FAILED) { fprintf(stderr, "mmap reserved failed\n"); return 2; }

    /* A page-aligned VA with nothing mapped at it: take a mapping and give it
     * back, so the address is known-valid-shaped and known-empty. */
    hole = mmap(NULL, 4096, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (hole == MAP_FAILED) { fprintf(stderr, "mmap hole failed\n"); return 2; }
    munmap(hole, 4096);

    /* Two pages for the mremap shrink arm. It must stay repeatable, and it does:
     * a shrink here returns the old address WITHOUT unmapping the tail
     * (divergence 5), so pass 2 sees exactly what pass 1 did. */
    remap2 = mmap(NULL, 2 * 4096, PROT_READ | PROT_WRITE,
                  MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (remap2 == MAP_FAILED) { fprintf(stderr, "mmap remap2 failed\n"); return 2; }
    remap2[0] = 1;
    remap2[4096] = 1;

    cur_brk = syscall(SYS_brk, 0);
    if (cur_brk <= 0) { fprintf(stderr, "brk(0) failed\n"); return 2; }

    /* Control: the syscall floor. MUST stay first — every later arm prints its
     * distance from it. */
    ARM("getpid", LONG_WANT_ANY, syscall(SYS_getpid));

    /* `getuid` is a `FastPath::Leaf` (2026-08-29): its arm is literally
     * `nr::GETUID => 0`, so the prologue skips the identity read and the two
     * `Process` syscall stamps, and the epilogue skips its re-resolve, its
     * stats row and its `/proc/<pid>/syscalls` entry. `getpid` is `Full` and
     * otherwise identical in shape — it also takes no arguments and returns one
     * integer — so **the gap between these two arms IS the prologue+epilogue
     * cost**, measured live on every run rather than inferred from an ablation
     * build. Keep them adjacent. */
    ARM("getuid_leaf", LONG_WANT_ANY, syscall(SYS_getuid));

    /* Decode-only rejection: MAP_FIXED with an unaligned address. Rejected
     * before any process lookup, which is the ordering `sys_mmap` guarantees. */
    ARM("mmap_einval", EINVAL,
        (long)(intptr_t)mmap((void *)0x1001, 4096, PROT_READ,
                             MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0));

    ARM("munmap_noent", 0, munmap(hole, 4096));
    ARM("mprotect_noop", 0, mprotect(rw_page, 4096, PROT_READ | PROT_WRITE));
    ARM("madv_unmapped", 0, madvise(reserved, RESERVE_BYTES, MADV_DONTNEED));

    /* The shrink short-circuit: decided from the arguments, before any process
     * lookup. `old_size` stays 2 pages every pass because the tail is never
     * unmapped — see divergence 5, which is what makes this arm repeatable. */
    ARM("mremap_inplace", (long)(intptr_t)remap2,
        (long)(intptr_t)mremap(remap2, 2 * 4096, 4096, MREMAP_MAYMOVE));

    /* Out-of-range old address: EFAULT from the decode, nothing resolved. */
    ARM("mremap_efault", EFAULT,
        (long)(intptr_t)mremap((void *)(1UL << 48), 4096, 2 * 4096, MREMAP_MAYMOVE));

    /* Advice decode + a walk that finds every page already resident, so the
     * prefault list comes back empty and no frame is allocated. */
    ARM("madv_willneed", 0, madvise(rw_page, 4096, MADV_WILLNEED));

    ARM("membarrier", LONG_WANT_ANY, syscall(__NR_membarrier, 0, 0));
    ARM("brk_query", LONG_WANT_ANY, syscall(SYS_brk, 0));
    ARM("brk_noop", LONG_WANT_ANY, syscall(SYS_brk, cur_brk));

    /* The two guards added 2026-08-29, LAST and behind a flag on purpose.
     *
     * Against a pre-fix baseline these do not return — the unvalidated length
     * became a ~4.5e15-iteration loop inside an `MmBklGuard` window. Running
     * them last means a baseline arm still emits every comparable number before
     * it wedges; `hostile=0` means it never wedges at all, which is what an A/B
     * run should pass. See AKUMA_EXTRACT_MMAP.md §10.1. */
    if (hostile) {
        /* Raw `syscall`, NOT musl's `mmap` wrapper. musl rejects
         * `len >= PTRDIFF_MAX` in userspace and never issues the syscall — the
         * first version of this arm measured 2 ns, i.e. a userspace compare,
         * and would have reported the kernel guard as free without ever
         * reaching it. Anything measuring a kernel path must reach the kernel. */
        ARM("mmap_enomem", ENOMEM,
            syscall(SYS_mmap, (void *)0, SIZE_MAX, PROT_READ,
                    MAP_PRIVATE | MAP_ANONYMOUS, -1, (off_t)0));
        ARM("madv_einval", EINVAL, madvise((void *)0x1000, SIZE_MAX, MADV_DONTNEED));
    } else {
        printf("mmap_enomem / madv_einval  SKIPPED (hostile=0): "
               "these hang a pre-fix kernel by design\n");
    }

    if (bad) {
        printf("FAIL: %d arm(s) returned something other than the documented "
               "result — the numbers above are not measuring what they name\n", bad);
        return 1;
    }
    return 0;
}
