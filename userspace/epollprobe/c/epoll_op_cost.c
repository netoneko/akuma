/*
 * epoll_op_cost — what one epoll/poll/select call costs, per op, with no parking.
 *
 * The sibling of `userspace/futexprobe/c/futex_op_cost.c`, deliberately down to
 * the reporting format: it prints the same `<arm> <ns> (floor+N) mean W worst X
 * ret=R` lines, so `scripts/benchmarks/futex_op_ab.py` drives it unchanged
 * (`--exe /tmp/epoll_op_cost`). That aggregator is arm-agnostic; a second copy
 * of its 146 lines under an epoll name would be a second thing to keep right.
 *
 * Built for BOTH kernels from this one source (musl static, aarch64), so an
 * Akuma number and a Linux number differ by the kernel and nothing else — the
 * same instruction stream issues the same `svc` on the same silicon.
 *
 * Why epoll specifically: `src/syscall/poll.rs`'s pure logic — the fd-state to
 * event-bits readiness map, the interest list and its `epoll_ctl` errno set,
 * the EPOLLET armed-state decision and the ppoll/pselect6 wire marshalling —
 * was extracted into `crates/akuma-syscalls-poll` so it could be host-tested
 * instead of boot-tested (`docs/archive/AKUMA_EXTRACT_SYSCALLS.md` §8.2). An
 * extraction is only allowed to be free; this is the instrument that says
 * whether it was.
 *
 * Every arm here **returns without parking**, on purpose. A parking arm
 * measures the scheduler's wake path — which this change does not touch, and
 * whose variance (hundreds of microseconds) would swamp the tens of nanoseconds
 * an extraction could plausibly cost. What is left is exactly the code that
 * moved: the ctl decode, the interest-list walk, the readiness map, the edge
 * decision and the fd-set marshalling.
 *
 *   getpid        control. Not an epoll call at all — the syscall floor. If
 *                 this moves between two arms, the arms are not comparable and
 *                 no number in the run means anything.
 *   epwait_empty  epoll_wait(timeout=0) on an instance with an EMPTY interest
 *                 list. Fd lookup + one interest-list snapshot that finds
 *                 nothing + the wait machine's first lap. The cheapest arm that
 *                 reaches the interest list, and the primary A/B number.
 *   epwait_1fd    the same with one registered, NOT-ready pipe fd. Adds one
 *                 readiness probe and one edge decision — the two functions the
 *                 extraction actually moved.
 *   epwait_ready  the same with one registered, ready pipe fd (level-triggered,
 *                 never drained, so it is idempotent across passes). Adds the
 *                 event copy_to_user.
 *   epctl_mod     EPOLL_CTL_MOD on a registered fd: the op decode, the 16-byte
 *                 `epoll_event` read from userspace, and one interest-list write.
 *   ppoll_1fd     poll(timeout=0) on the same not-ready fd. Same readiness map,
 *                 reached through the poll-bit marshalling instead.
 *   select_1fd    select(timeout=0) on the same fd. Same again, through the
 *                 fd_set bit marshalling — the third caller of one map.
 *
 * Reporting follows `docs/archive/AKUMA_SYSCALL_PERFORMANCE_AUDIT.md`
 * § "Method warnings" verbatim, and the three findings that method cost to get
 * right are worth repeating because each produced a wrong answer first:
 *
 *  1. A probe's own warm-up can invalidate it. The clock calibration below is
 *     bounded on PURPOSE — its ancestor spun 200,000 clock_gettime calls, which
 *     on Akuma is 200,000 real syscalls, and every arm after it read ~2x the
 *     floor the same boot's other probes reported. A process that has just
 *     issued a quarter-million syscalls is not a representative process.
 *  2. `floor+N` is NOT drift-invariant. Subtracting the control looks like it
 *     removes boot-to-boot drift; it does not, because the drift is
 *     multiplicative — a slower boot slows the whole syscall path, not just its
 *     fixed part. Read the RATIO (`arm / getpid`), which is what `--compare`
 *     tests, and prefer SMP=4, which is dramatically steadier here than SMP=1.
 *  3. The resolution floor is `clock_gettime`'s MICROSECOND truncation divided
 *     by `calls`, not the 41.7 ns counter tick: 1000/calls ns per call, i.e.
 *     2 ns at the default 500. Sweep `calls` to prove a delta is real work
 *     rather than quanta.
 *
 * Usage:  epoll_op_cost [passes] [calls]        (default 100 x 500)
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/select.h>
#include <sys/syscall.h>
#include <poll.h>
#include <time.h>
#include <unistd.h>

static long long now_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (long long)ts.tv_sec * 1000000000LL + ts.tv_nsec;
}

/* Plain globals, not stack: Akuma has no ASLR, so these addresses are stable
 * across runs too, which makes a debug line from one run comparable to the
 * next. */
static int ep_empty, ep_idle, ep_ready;
static int idle_pipe[2], ready_pipe[2];
static struct epoll_event evbuf[4];
static struct epoll_event modreg;

/* Rebuilt per call, because select(2) reports by OVERWRITING its fd sets — a
 * set reused across passes would be measuring a different call each time. */
static long select_once(void) {
    fd_set rd;
    FD_ZERO(&rd);
    FD_SET(idle_pipe[0], &rd);
    struct timeval tv = { 0, 0 };
    return select(idle_pipe[0] + 1, &rd, NULL, NULL, &tv);
}

static long poll_once(void) {
    struct pollfd pf = { .fd = idle_pipe[0], .events = POLLIN, .revents = 0 };
    return poll(&pf, 1, 0);
}

/* One arm. `expr` is expanded inline inside the timed loop — no function
 * pointer, so nothing but the syscall is between the two clock reads. */
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
        printf("%-12s %6lld ns   (floor%+5lld)   mean %6lld   worst %7lld   "  \
               "ret=%ld %s\n",                                                 \
               name, best, best - floor_ns, total / passes, worst, check,      \
               ok ? "" : "  <-- UNEXPECTED RETURN, arm is not measuring what it says"); \
        if (!ok) bad++;                                                        \
    } while (0)

int main(int argc, char **argv) {
    int passes = argc > 1 ? atoi(argv[1]) : 100;
    int calls = argc > 2 ? atoi(argv[2]) : 500;
    int bad = 0;
    /* Set by the first arm (`getpid`), which is why that arm must stay first:
     * every later arm prints its distance from it. */
    long long floor_ns = -1;
    if (passes < 1 || calls < 1) {
        fprintf(stderr, "usage: %s [passes] [calls]\n", argv[0]);
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
    printf("epoll_op_cost: %d passes x %d calls, cheapest pass wins; "
           "clock tick %lld ns\n", passes, calls, cres);

    if (pipe(idle_pipe) < 0 || pipe(ready_pipe) < 0) {
        fprintf(stderr, "pipe() failed\n");
        return 2;
    }
    /* The ready arm's fd stays readable forever: level-triggered and never
     * drained, so every pass measures the same call. */
    if (write(ready_pipe[1], "r", 1) != 1) {
        fprintf(stderr, "priming write failed\n");
        return 2;
    }

    ep_empty = epoll_create1(0);
    ep_idle = epoll_create1(0);
    ep_ready = epoll_create1(0);
    if (ep_empty < 0 || ep_idle < 0 || ep_ready < 0) {
        fprintf(stderr, "epoll_create1 failed: %s\n", strerror(errno));
        return 2;
    }
    struct epoll_event reg = { .events = EPOLLIN, .data = { .u64 = 1 } };
    if (epoll_ctl(ep_idle, EPOLL_CTL_ADD, idle_pipe[0], &reg) < 0 ||
        epoll_ctl(ep_ready, EPOLL_CTL_ADD, ready_pipe[0], &reg) < 0) {
        fprintf(stderr, "epoll_ctl ADD failed: %s\n", strerror(errno));
        return 2;
    }
    modreg = reg;

    /* Control: the syscall floor. Not cached by the probe — if this drifts
     * between two builds, so did everything else. */
    ARM("getpid", LONG_WANT_ANY, syscall(SYS_getpid));

    ARM("epwait_empty", 0, epoll_wait(ep_empty, evbuf, 4, 0));
    ARM("epwait_1fd", 0, epoll_wait(ep_idle, evbuf, 4, 0));
    ARM("epwait_ready", 1, epoll_wait(ep_ready, evbuf, 4, 0));
    ARM("epctl_mod", 0, epoll_ctl(ep_idle, EPOLL_CTL_MOD, idle_pipe[0], &modreg));
    ARM("ppoll_1fd", 0, poll_once());
    ARM("select_1fd", 0, select_once());

    if (bad) {
        printf("FAIL: %d arm(s) returned something other than the documented "
               "result — the numbers above are not measuring what they name\n", bad);
        return 1;
    }
    return 0;
}
