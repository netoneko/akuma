/*
 * futex_op_cost — what one `futex(2)` op costs, per op, with no parking.
 *
 * Built for BOTH kernels from this one source (musl static, aarch64), so an
 * Akuma number and a Linux number differ by the kernel and nothing else — the
 * same instruction stream issues the same `svc` on the same silicon. Same
 * construction as its sibling `userspace/ext2probe/c/read_syscall_cost.c`, and
 * for the same reason: building it separately in each guest would put a
 * different libc's wrapper in front of each `svc`.
 *
 * Why futex specifically: `src/syscall/sync.rs` is the futex *family* — op
 * decode, the `(tgid, uaddr)` waiter table, the deadline algebra and the wait
 * loop's outcome decision. That logic is being extracted into
 * `crates/akuma-syscalls-sync` so it can be host-tested instead of boot-tested
 * (`docs/archive/AKUMA_EXTRACT_SYSCALLS.md` §8). An extraction is only allowed
 * to be free; this is the instrument that says whether it was. It is a
 * before/after A/B tool first and a cross-kernel tool second.
 *
 * Every arm here is a futex call that **returns without parking**, on purpose.
 * A parking arm measures the scheduler's wake path, which no table refactor
 * touches and whose variance (hundreds of microseconds) would swamp the tens
 * of nanoseconds an extraction could plausibly cost. What is left is exactly
 * the code that moved: decode, validate, key, table.
 *
 *   getpid        control. Not a futex at all — the syscall floor. If this
 *                 moves between two arms, the arms are not comparable and no
 *                 futex number in the run means anything (the audit's method
 *                 warning 6: instrument the floor, not just the feature).
 *   einval        FUTEX_WAIT_BITSET with val3 == 0. Rejected by op decode
 *                 before the waiter table is touched: decode cost alone.
 *   wake_empty    FUTEX_WAKE on an address with no waiters. Decode + key
 *                 resolution + one table lookup that misses. The cheapest arm
 *                 that reaches the table, and the primary A/B number.
 *   wait_eagain   FUTEX_WAIT whose expected value never matches. Reaches the
 *                 enqueue path, reads the futex word under the table hold,
 *                 and backs out with EAGAIN. The waiter never sleeps.
 *   requeue       FUTEX_CMP_REQUEUE with a matching val3 on an empty queue —
 *                 the requeue path's decode + two-key table work.
 *   wake_op       FUTEX_WAKE_OP. The val3 opcode decode, a read-modify-write
 *                 of the second word, and up to two table lookups.
 *
 * Reporting follows the audit's method warnings verbatim
 * (`docs/archive/AKUMA_SYSCALL_PERFORMANCE_AUDIT.md` § "Method warnings"):
 * interference here is a few multi-hundred-microsecond stalls per thousand
 * syscalls, so a single long loop always contains one and even its minimum
 * measures interference. Every arm is `passes` × `calls`, timed once per pass,
 * and the reported number is the **cheapest pass**. Mean and worst are printed
 * next to it so a run that was drowning in interference is visible rather than
 * silently averaged in.
 *
 * The resolution floor is NOT the 41.7 ns counter tick, and assuming it was
 * would misread this probe. Each pass is timed once around `calls` syscalls, so
 * the counter's granularity is divided by `calls` (0.4 ns at 100) and the
 * binding limit is instead `clock_gettime`'s **microsecond** truncation on
 * Akuma: 1000 ns / `calls`, i.e. 10 ns per call at the default. That is why
 * every number at `calls=100` is a multiple of 10.
 *
 * Which raises the obvious suspicion about a `floor+30` reading — that it is
 * three quanta of nothing rather than 30 ns of work. Measured 2026-08-29 on one
 * boot, same kernel, sweeping only the pass length:
 *
 *   calls/pass     100      500     2000     (resolution: 10, 2, 0.5 ns)
 *   wake_empty     +20      +23      +22
 *   wait_eagain    +30      +26      +28
 *   requeue        +25      +24      +25
 *   wake_op        +70      +65      +65
 *
 * The costs survive a 20x finer quantum and land on values that are multiples
 * of neither 10 nor 41.7, so they are real work. `calls=500` is the default
 * because it buys 5x the resolution while a pass is still only ~70 us — short
 * enough that the multi-hundred-microsecond stalls this method exists to dodge
 * still miss most passes.
 *
 * Two columns, and the second is the one an A/B reads. The absolute cost drifts
 * with whatever the host and the guest's own tick governor are doing — measured
 * 130 -> 230 ns on the `getpid` control across three runs inside ONE boot, which
 * is larger than any effect a table refactor could have. `floor+N` is that arm
 * minus the `getpid` control from the same process a few microseconds earlier,
 * so the drift divides out. Compare `floor+N` across builds; use the absolute
 * column only to check that the two boots were comparable at all.
 *
 * Usage:  futex_op_cost [passes] [calls]        (default 100 x 500)
 */
#define _GNU_SOURCE
#include <errno.h>
#include <linux/futex.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

#ifndef FUTEX_PRIVATE_FLAG
#define FUTEX_PRIVATE_FLAG 128
#endif

/* Raw syscall, not a libc wrapper: musl has no futex() wrapper anyway, and the
 * point is to time the `svc`, not glibc's or musl's bookkeeping around it. */
static inline long fx(volatile uint32_t *uaddr, int op, uint32_t val,
                      unsigned long to, volatile uint32_t *uaddr2, uint32_t val3) {
    return syscall(SYS_futex, (void *)uaddr, op, val, to, (void *)uaddr2, val3);
}

static long long now_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (long long)ts.tv_sec * 1000000000LL + ts.tv_nsec;
}

/* The two futex words. Deliberately plain globals in .bss: Akuma has no ASLR,
 * so a stack address would still be stable, but a global is stable ACROSS runs
 * too, which makes a `[futex-dbg]` log line from one run comparable to the
 * next. `w2` is the WAKE_OP / requeue target and is never waited on. */
static volatile uint32_t w1;
static volatile uint32_t w2;

/* One arm. `expr` is expanded inline inside the timed loop — no function
 * pointer, so nothing but the syscall is between the two clock reads. */
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
        printf("%-12s %6lld ns   (floor%+5lld)   mean %6lld   worst %7lld   "  \
               "ret=%ld %s\n",                                                 \
               name, best, best - floor_ns, total / passes, worst, check,      \
               ok ? "" : "  <-- UNEXPECTED RETURN, arm is not measuring what it says"); \
        if (!ok) bad++;                                                        \
    } while (0)

#define LONG_WANT_ANY 0x7fffffffL

int main(int argc, char **argv) {
    int passes = argc > 1 ? atoi(argv[1]) : 100;
    int calls = argc > 2 ? atoi(argv[2]) : 500;
    int bad = 0;
    /* Set by the first arm (`getpid`), which is why that arm must stay first:
     * every later arm prints its distance from it. The absolute column drifts
     * — this host's floor moved 130 -> 230 ns WITHIN one boot as the tick
     * governor settled — but the distance does not, because both halves of it
     * are measured microseconds apart in the same process. An A/B across two
     * kernel builds should read the `floor+N` column and use the absolute one
     * only to confirm the two boots were comparable at all. */
    long long floor_ns = -1;
    if (passes < 1 || calls < 1) {
        fprintf(stderr, "usage: %s [passes] [calls]\n", argv[0]);
        return 2;
    }

    /* Calibrate the clock before trusting any minimum: on Akuma
     * clock_gettime's own resolution is the floor under every number below,
     * and a 1 us floor would make every arm here read as 0. Print it rather
     * than assume it (method warning 2). */
    long long c0 = now_ns(), cres = -1;
    for (int i = 0, seen = 0; i < 20000 && seen < 50; i++) {
        long long c1 = now_ns();
        if (c1 != c0) {
            if (cres < 0 || c1 - c0 < cres) cres = c1 - c0;
            c0 = c1;
            seen++;
        }
    }
    /* Bounded on PURPOSE, and the bound is not cosmetic. The first draft spun
     * 200,000 clock_gettime calls here, which on Akuma is 200,000 real
     * syscalls — and every arm after it then measured ~2x the floor this
     * kernel's own probe (`read_syscall_cost`) reports on the same boot, with
     * a per-pass mean 8x its own minimum. A process that has just issued a
     * quarter-million syscalls is not a representative process: it is one the
     * scheduler has every reason to treat as a CPU hog. Warm-up is not free
     * here, so there is as little of it as the measurement can survive on. */
    printf("futex_op_cost: %d passes x %d calls, cheapest pass wins; "
           "clock tick %lld ns\n", passes, calls, cres);

    w1 = 1;
    w2 = 0;

    /* Control: the syscall floor, so the futex arms below have something to be
     * read against. Not cached by the probe — if this drifts between two
     * builds, so did everything else. */
    ARM("getpid", LONG_WANT_ANY, syscall(SYS_getpid));

    /* Decode-only reject: FUTEX_WAIT_BITSET requires a non-zero bitset. Never
     * reaches the waiter table, so `wake_empty - einval` is what the table
     * lookup itself costs. */
    ARM("einval", EINVAL,
        fx(&w1, FUTEX_WAIT_BITSET | FUTEX_PRIVATE_FLAG, 1, 0, NULL, 0));

    /* Primary arm: one table lookup that misses. */
    ARM("wake_empty", 0, fx(&w1, FUTEX_WAKE | FUTEX_PRIVATE_FLAG, 1, 0, NULL, 0));

    /* Enqueue path without the sleep: w1 is 1, we claim it is 0, so the
     * in-hold re-read of the futex word disagrees and the call backs out with
     * EAGAIN before parking. */
    ARM("wait_eagain", EAGAIN,
        fx(&w1, FUTEX_WAIT | FUTEX_PRIVATE_FLAG, 0, 0, NULL, 0));

    /* CMP_REQUEUE: val3 must equal *uaddr (1) or it is EAGAIN. Empty queue, so
     * it wakes 0 and requeues 0 — the two-key table path with no waiters. The
     * requeue-count rides in the timeout argument slot. */
    ARM("requeue", 0,
        fx(&w1, FUTEX_CMP_REQUEUE | FUTEX_PRIVATE_FLAG, 1, 1, &w2, 1));

    /* WAKE_OP: FUTEX_OP_ADD 0 with cmp EQ 0 — encoded as
     * (op=1)<<28 | (cmp=0)<<24 | (oparg=0)<<12 | cmparg=0. The RMW leaves w2
     * unchanged, so the arm is idempotent across passes, and oldval==0 makes
     * the comparison true, which is what buys the SECOND table lookup. */
    ARM("wake_op", 0,
        fx(&w1, FUTEX_WAKE_OP | FUTEX_PRIVATE_FLAG, 1, 1, &w2, (1u << 28)));

    if (w2 != 0) {
        printf("NOTE: w2 = %u after wake_op — the RMW is not idempotent, "
               "so the wake_op arm drifted across passes\n", w2);
    }
    if (bad) {
        printf("FAIL: %d arm(s) returned something other than the documented "
               "result — the numbers above are not measuring what they name\n", bad);
        return 1;
    }
    return 0;
}
