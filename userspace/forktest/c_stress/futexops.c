/*
 * futexops.c — probe Akuma's `sys_futex` (src/syscall/sync.rs) against Linux
 * semantics, op by op, to find lost-wakeup generators.
 *
 * Written for the "Open issue #2" lost-wakeup stall in
 * docs/archive/SELFHOST_DEVBOX_SMOLTCP_2026-08-02.md: two rustc worker threads
 * parked in an unreturned futex syscall forever, low CPU. futextest.c already
 * showed the *common* paths (WAIT/WAKE, condvar, barrier, park/unpark) are fine,
 * so this probes the less-travelled ops that a lost wakeup could hide in.
 *
 * Each probe prints PASS (matches Linux) / FAIL (diverges) / SKIP, and says what
 * the divergence would cost. Run the same binary on Linux to confirm the probes
 * themselves are right — every FAIL here should be a PASS there.
 *
 * Static, musl, no Rust runtime. Build:
 *   aarch64-linux-musl-gcc -O2 -static -o futexops futexops.c
 */

#include <errno.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
#include <sys/syscall.h>

#define FUTEX_WAIT            0
#define FUTEX_WAKE            1
#define FUTEX_REQUEUE         3
#define FUTEX_WAKE_OP         5
#define FUTEX_WAIT_BITSET     9
#define FUTEX_WAKE_BITSET    10
#define FUTEX_PRIVATE_FLAG  128

#define FUTEX_OP_SET 0
#define FUTEX_OP_CMP_NE 1

/* Encode a FUTEX_WAKE_OP `val3`: (op << 28) | (cmp << 24) | (oparg << 12) | cmparg */
#define FUTEX_OP(op, oparg, cmp, cmparg) \
    (((op) << 28) | ((cmp) << 24) | (((oparg) & 0xfff) << 12) | ((cmparg) & 0xfff))

static int fails = 0;

static long futex(volatile uint32_t *u, int op, uint32_t val,
                  const struct timespec *ts, volatile uint32_t *u2, uint32_t val3) {
    return syscall(SYS_futex, u, op, val, ts, u2, val3);
}

static void ok(const char *name, const char *detail) {
    printf("PASS %s — %s\n", name, detail);
    fflush(stdout);
}

static void bad(const char *name, const char *detail) {
    printf("FAIL %s — %s\n", name, detail);
    fflush(stdout);
    fails++;
}

/* ---------------------------------------------------------------------------
 * Probe 1: FUTEX_WAKE_OP must perform the atomic op on uaddr2.
 *
 * Linux: *uaddr2 = (oldval OP oparg) happens unconditionally, before any wake.
 * A kernel that skips it leaves userspace memory stale — whoever is polling
 * *uaddr2 for the new value waits forever, with no waiter queue involved at all.
 * No second thread needed: the memory write alone is observable.
 * ------------------------------------------------------------------------ */
static void probe_wake_op_writes_uaddr2(void) {
    static volatile uint32_t w1 = 0;
    static volatile uint32_t w2 = 0;

    w2 = 0;
    /* SET *w2 = 5; wake 0 waiters on w1; wake 0 on w2 if (oldval != 999) */
    long r = futex(&w1, FUTEX_WAKE_OP | FUTEX_PRIVATE_FLAG, 0,
                   (struct timespec *)(uintptr_t)0, &w2,
                   FUTEX_OP(FUTEX_OP_SET, 5, FUTEX_OP_CMP_NE, 999));

    if (r < 0 && errno == ENOSYS) {
        printf("SKIP wake_op_writes_uaddr2 — ENOSYS (op not implemented at all; "
               "honest refusal, userspace can detect it)\n");
        fflush(stdout);
        return;
    }
    if (w2 == 5) {
        ok("wake_op_writes_uaddr2", "*uaddr2 updated as Linux specifies");
    } else {
        char buf[192];
        snprintf(buf, sizeof(buf),
                 "*uaddr2 still %u, expected 5 (rc=%ld). The kernel returns SUCCESS "
                 "without performing the atomic op: userspace sees a value that can "
                 "never change -> permanent stall, no queue involved",
                 w2, r);
        bad("wake_op_writes_uaddr2", buf);
    }
}

/* ---------------------------------------------------------------------------
 * Probe 2: FUTEX_WAKE_OP's conditional second wake (waiters on uaddr2).
 *
 * Linux: if (oldval CMP cmparg) also wake up to val2 waiters on uaddr2.
 * A kernel that only ever wakes uaddr never wakes them -> lost wakeup.
 * ------------------------------------------------------------------------ */
static volatile uint32_t op_w1 = 0;
static volatile uint32_t op_w2 = 0;
static atomic_int op_waiter_woke = 0;

static void *op_waiter(void *arg) {
    (void)arg;
    /* Park on op_w2 (value 0). Only WAKE_OP's second wake should release us. */
    futex(&op_w2, FUTEX_WAIT | FUTEX_PRIVATE_FLAG, 0, NULL, NULL, 0);
    atomic_store(&op_waiter_woke, 1);
    return NULL;
}

static void probe_wake_op_second_wake(void) {
    pthread_t t;
    op_w2 = 0;
    atomic_store(&op_waiter_woke, 0);
    if (pthread_create(&t, NULL, op_waiter, NULL) != 0) {
        printf("SKIP wake_op_second_wake — pthread_create failed\n");
        return;
    }
    /* Give the waiter time to actually park. */
    struct timespec nap = { 0, 200 * 1000 * 1000 };
    nanosleep(&nap, NULL);

    /* val2 (the uaddr2 wake count) rides in the `timeout` argument slot. */
    long r = futex(&op_w1, FUTEX_WAKE_OP | FUTEX_PRIVATE_FLAG, 0,
                   (struct timespec *)(uintptr_t)1 /* val2 = 1 */, &op_w2,
                   FUTEX_OP(FUTEX_OP_SET, 0, FUTEX_OP_CMP_NE, 999));

    nanosleep(&nap, NULL);
    int woke = atomic_load(&op_waiter_woke);

    if (woke) {
        ok("wake_op_second_wake", "waiter on uaddr2 was woken");
        pthread_join(t, NULL);
    } else {
        char buf[192];
        snprintf(buf, sizeof(buf),
                 "waiter on uaddr2 NOT woken (rc=%ld). Any userspace relying on "
                 "WAKE_OP's conditional wake parks forever -> exactly the observed "
                 "signature. (Leaking the stuck thread; it is unkillable.)", r);
        bad("wake_op_second_wake", buf);
        /* Release it so the process can exit. */
        futex(&op_w2, FUTEX_WAKE | FUTEX_PRIVATE_FLAG, 1, NULL, NULL, 0);
        nanosleep(&nap, NULL);
        pthread_join(t, NULL);
    }
}

/* ---------------------------------------------------------------------------
 * Probe 3: FUTEX_WAKE_BITSET must only wake waiters whose bitset intersects.
 *
 * Linux: a WAIT_BITSET waiter with bitset A is NOT woken by WAKE_BITSET with a
 * disjoint bitset B. A kernel that ignores the bitset over-wakes. Over-waking
 * looks harmless (spurious wakeups are legal) but is not: a WAKE_BITSET with
 * val=1 that lands on a non-matching waiter CONSUMES the single wake the
 * matching waiter was owed -> the intended waiter is never woken.
 * ------------------------------------------------------------------------ */
static volatile uint32_t bs_word = 0;
static atomic_int bs_woke = 0;

static void *bitset_waiter(void *arg) {
    (void)arg;
    /* Wait with bitset 0x1 only. */
    futex(&bs_word, FUTEX_WAIT_BITSET | FUTEX_PRIVATE_FLAG, 0, NULL, NULL, 0x1);
    atomic_store(&bs_woke, 1);
    return NULL;
}

static void probe_wake_bitset_selectivity(void) {
    pthread_t t;
    bs_word = 0;
    atomic_store(&bs_woke, 0);
    if (pthread_create(&t, NULL, bitset_waiter, NULL) != 0) {
        printf("SKIP wake_bitset_selectivity — pthread_create failed\n");
        return;
    }
    struct timespec nap = { 0, 200 * 1000 * 1000 };
    nanosleep(&nap, NULL);

    /* Wake with a DISJOINT bitset (0x2). Linux wakes nobody. */
    long r = futex(&bs_word, FUTEX_WAKE_BITSET | FUTEX_PRIVATE_FLAG, 1, NULL, NULL, 0x2);
    nanosleep(&nap, NULL);
    int woke = atomic_load(&bs_woke);

    if (!woke && r == 0) {
        ok("wake_bitset_selectivity", "disjoint bitset woke nobody, rc=0");
    } else {
        char buf[192];
        snprintf(buf, sizeof(buf),
                 "disjoint-bitset wake returned %ld and woke=%d (Linux: 0 and 0). "
                 "The bitset is ignored, so a val=1 wake can be consumed by a "
                 "non-matching waiter, stranding the intended one", r, woke);
        bad("wake_bitset_selectivity", buf);
    }
    /* Release the waiter either way. */
    futex(&bs_word, FUTEX_WAKE_BITSET | FUTEX_PRIVATE_FLAG, 1, NULL, NULL, 0xFFFFFFFF);
    nanosleep(&nap, NULL);
    pthread_join(t, NULL);
}

/* ---------------------------------------------------------------------------
 * Probe 4: a WAIT with an INVALID timeout pointer must fail, not hang.
 *
 * Linux: EFAULT. A kernel that silently treats "can't read the timespec" as
 * "no timeout" converts a transient fault into an infinite park. Probed with a
 * 5s alarm-free guard: we run it on a thread and see if it ever returns.
 * ------------------------------------------------------------------------ */
static volatile uint32_t to_word = 0;
static atomic_int to_returned = 0;
static atomic_long to_rc = 0;
static atomic_int to_errno = 0;

static void *bad_timeout_waiter(void *arg) {
    (void)arg;
    /* 0x10 is below any mapping but non-NULL: a bad timespec pointer. */
    errno = 0;
    long r = futex(&to_word, FUTEX_WAIT_BITSET | FUTEX_PRIVATE_FLAG, 0,
                   (const struct timespec *)(uintptr_t)0x10, NULL, 0xFFFFFFFF);
    atomic_store(&to_rc, r);
    atomic_store(&to_errno, errno);
    atomic_store(&to_returned, 1);
    return NULL;
}

static void probe_bad_timeout_ptr(void) {
    pthread_t t;
    to_word = 0;
    atomic_store(&to_returned, 0);
    if (pthread_create(&t, NULL, bad_timeout_waiter, NULL) != 0) {
        printf("SKIP bad_timeout_ptr — pthread_create failed\n");
        return;
    }
    struct timespec nap = { 1, 0 };
    for (int i = 0; i < 3 && !atomic_load(&to_returned); i++) nanosleep(&nap, NULL);

    if (atomic_load(&to_returned)) {
        long r = atomic_load(&to_rc);
        int e = atomic_load(&to_errno);
        if (r < 0 && e == EFAULT) {
            ok("bad_timeout_ptr", "returned EFAULT as Linux does");
        } else {
            char buf[160];
            snprintf(buf, sizeof(buf), "returned rc=%ld errno=%d (Linux: -1/EFAULT)", r, e);
            bad("bad_timeout_ptr", buf);
        }
        pthread_join(t, NULL);
    } else {
        bad("bad_timeout_ptr",
            "still parked after 3s — an unreadable timespec silently became an "
            "INFINITE wait instead of EFAULT: a transient fault turns into a "
            "permanent hang. (Leaking the stuck thread.)");
        futex(&to_word, FUTEX_WAKE | FUTEX_PRIVATE_FLAG, 1, NULL, NULL, 0);
        nanosleep(&nap, NULL);
    }
}

/* ---------------------------------------------------------------------------
 * Probe 5: a REQUEUEd waiter that then TIMES OUT must leave no trace.
 *
 * This is the one that runs on a path musl actually uses: pthread_cond_broadcast
 * is FUTEX_REQUEUE, and pthread_cond_timedwait times out.
 *
 * Akuma's requeue MOVES the waiter's tid from the condvar's queue to the mutex's
 * queue (src/syscall/sync.rs:134-172), but the waiting thread's own loop only
 * ever checks/removes itself from the key it ORIGINALLY waited on
 * (sync.rs:337-384). So a requeued waiter that leaves by timeout (or EINTR)
 * never removes itself from the mutex queue -> a permanent stale tid.
 *
 * Each stale entry silently absorbs one future FUTEX_WAKE on that address: the
 * kernel counts it as "woken" and the thread that was actually owed the wake is
 * never woken. That is a lost wakeup that accumulates over a process's life.
 *
 * Observable directly: FUTEX_WAKE returns how many it woke. After the waiter has
 * timed out and been joined, waking the mutex address must report 0.
 * ------------------------------------------------------------------------ */
static volatile uint32_t rq_cond = 0;
static volatile uint32_t rq_mutex = 0;
static atomic_long rq_rc = 0;
static atomic_int rq_errno = 0;
static atomic_int rq_done = 0;

static void *requeue_waiter(void *arg) {
    (void)arg;
    struct timespec deadline;
    clock_gettime(CLOCK_MONOTONIC, &deadline);
    deadline.tv_nsec += 400 * 1000 * 1000;
    if (deadline.tv_nsec >= 1000000000) { deadline.tv_nsec -= 1000000000; deadline.tv_sec++; }

    errno = 0;
    long r = futex(&rq_cond, FUTEX_WAIT_BITSET | FUTEX_PRIVATE_FLAG, 0,
                   &deadline, NULL, 0xFFFFFFFF);
    atomic_store(&rq_rc, r);
    atomic_store(&rq_errno, errno);
    atomic_store(&rq_done, 1);
    return NULL;
}

static void probe_requeue_timeout_leaves_stale_waiter(void) {
    pthread_t t;
    rq_cond = 0;
    rq_mutex = 0;
    atomic_store(&rq_done, 0);
    if (pthread_create(&t, NULL, requeue_waiter, NULL) != 0) {
        printf("SKIP requeue_timeout_leaves_stale_waiter — pthread_create failed\n");
        return;
    }

    /* Let it park on rq_cond. */
    struct timespec nap = { 0, 150 * 1000 * 1000 };
    nanosleep(&nap, NULL);

    /* Broadcast-style: wake 0, requeue up to 1 from rq_cond onto rq_mutex.
     * val2 (max to requeue) rides in the timeout argument slot. */
    long rq = futex(&rq_cond, FUTEX_REQUEUE | FUTEX_PRIVATE_FLAG, 0,
                    (struct timespec *)(uintptr_t)1, &rq_mutex, 0);

    /* Wait for the waiter to hit its 400ms timeout and exit. */
    struct timespec longnap = { 1, 0 };
    for (int i = 0; i < 3 && !atomic_load(&rq_done); i++) nanosleep(&longnap, NULL);

    if (!atomic_load(&rq_done)) {
        bad("requeue_timeout_leaves_stale_waiter",
            "requeued waiter never returned at all — its timeout was lost by the "
            "requeue (even worse than the stale-entry bug this probes for)");
        futex(&rq_mutex, FUTEX_WAKE | FUTEX_PRIVATE_FLAG, 1, NULL, NULL, 0);
        nanosleep(&longnap, NULL);
        return;
    }
    pthread_join(t, NULL);

    /* The waiter is gone. Nothing may remain queued on rq_mutex. */
    long woke = futex(&rq_mutex, FUTEX_WAKE | FUTEX_PRIVATE_FLAG, 1, NULL, NULL, 0);

    if (woke == 0) {
        ok("requeue_timeout_leaves_stale_waiter",
           "no stale waiter left on the requeue target");
    } else {
        char buf[256];
        snprintf(buf, sizeof(buf),
                 "FUTEX_WAKE on the requeue target reported %ld woken after the "
                 "waiter already timed out and was joined (requeue rc=%ld, waiter "
                 "rc=%ld/errno=%d). A dead tid is still queued: every such entry "
                 "eats one future wake, so a live thread owed that wake parks "
                 "forever. musl's pthread_cond_broadcast + timedwait hits this",
                 woke, rq, atomic_load(&rq_rc), atomic_load(&rq_errno));
        bad("requeue_timeout_leaves_stale_waiter", buf);
    }
}

int main(void) {
    printf("=== FUTEXOPS start ===\n");
    fflush(stdout);
    probe_wake_op_writes_uaddr2();
    probe_wake_op_second_wake();
    probe_wake_bitset_selectivity();
    probe_bad_timeout_ptr();
    probe_requeue_timeout_leaves_stale_waiter();
    printf("=== FUTEXOPS DONE — %d divergence(s) from Linux ===\n", fails);
    fflush(stdout);
    return fails ? 1 : 0;
}
