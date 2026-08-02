/*
 * futextest.c — pure-C control for userspace/selfhost_repro/futextest.rs, to
 * disambiguate kernel-vs-Rust-runtime thread/futex hangs on Akuma
 * (docs/archive/AKUMA_SELF_HOSTING.md §7g/§7h, docs/archive/
 * SELFHOST_DEVBOX_SMOLTCP_2026-08-02.md "Open issue #2").
 *
 * Mirrors futextest.rs phase-for-phase using raw pthread + a raw FUTEX_WAIT/
 * FUTEX_WAKE park/unpark for phase 7 (std::thread::park has no direct POSIX
 * equivalent, so phase 7 here hand-rolls the same primitive Rust's std uses
 * on Linux: a futex word toggled 0/1 with direct syscall(SYS_futex, ...)).
 * musl's pthread_create/pthread_join/mutex/cond/barrier are themselves
 * clone()+futex under the hood, same as Rust's std on this target — the
 * point of running both is to see whether a hang/abort shows up here too
 * (kernel-level bug, language-independent) or only from the Rust binary
 * (something specific to how rustc/std sets up thread state).
 *
 * Static, musl, no Rust runtime. Each phase prints "[N] start" then "[N] ok"
 * — a missing "ok" is the culprit, same convention as futextest.rs. Set
 * FUTEXTEST_PHASE=N to run a single phase.
 *
 * Usage (Akuma, after staging the binary):
 *   /tmp/futextest_c
 *   FUTEXTEST_PHASE=5 /tmp/futextest_c
 */

#include <pthread.h>
#include <stdarg.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
#include <sys/syscall.h>

static void mark(const char *s) {
    fputs(s, stdout);
    fputc('\n', stdout);
    fflush(stdout);
}

static void markf(const char *fmt, ...) {
    char buf[256];
    va_list ap;
    va_start(ap, fmt);
    vsnprintf(buf, sizeof(buf), fmt, ap);
    va_end(ap);
    mark(buf);
}

/* (1) Spawn one thread and join it. The simplest pthread_create +
 * pthread_join, which on musl is clone + a futex wait on the child's
 * clear_child_tid. */
static void *ret42(void *arg) { (void)arg; return (void *)42; }

static void phase_spawn_join(void) {
    mark("[1] spawn+join single thread: start");
    pthread_t t;
    void *ret;
    if (pthread_create(&t, NULL, ret42, NULL) != 0) { perror("pthread_create"); exit(1); }
    if (pthread_join(t, &ret) != 0) { perror("pthread_join"); exit(1); }
    if ((intptr_t)ret != 42) { fprintf(stderr, "[1] bad return %ld\n", (long)(intptr_t)ret); exit(1); }
    mark("[1] ok");
}

/* (2) Tight spawn/join loop — stresses clone + exit + clear_child_tid futex
 * wake (the path that wakes a joiner). A lost wake here hangs join(). */
static void *ret_double(void *arg) {
    intptr_t i = (intptr_t)arg;
    return (void *)(i * 2);
}

static void phase_spawn_join_loop(void) {
    mark("[2] 200x spawn/join loop: start");
    for (intptr_t i = 0; i < 200; i++) {
        pthread_t t;
        void *ret;
        int rc = pthread_create(&t, NULL, ret_double, (void *)i);
        if (rc != 0) { markf("[2] pthread_create FAILED at iter %ld: rc=%d (%s)", (long)i, rc, strerror(rc)); exit(1); }
        rc = pthread_join(t, &ret);
        if (rc != 0) { markf("[2] pthread_join FAILED at iter %ld: rc=%d (%s)", (long)i, rc, strerror(rc)); exit(1); }
        if ((intptr_t)ret != i * 2) { fprintf(stderr, "[2] bad return at iter %ld\n", (long)i); exit(1); }
        if (i % 10 == 0) markf("[2]   iter %ld", (long)i);
    }
    mark("[2] ok");
}

/* (3) Fan-out: spawn N threads at once, join them all. Stresses N concurrent
 * clear_child_tid futex wakes landing on the main thread's joins. */
static atomic_ullong g_counter;

static void *bump1000(void *arg) {
    (void)arg;
    for (int i = 0; i < 1000; i++) atomic_fetch_add_explicit(&g_counter, 1, memory_order_relaxed);
    return NULL;
}

static void phase_fanout(int n) {
    markf("[3] fan-out %d threads + join all: start", n);
    atomic_store(&g_counter, 0);
    pthread_t *ts = malloc(sizeof(pthread_t) * n);
    for (int i = 0; i < n; i++) {
        if (pthread_create(&ts[i], NULL, bump1000, NULL) != 0) { perror("pthread_create"); exit(1); }
    }
    for (int i = 0; i < n; i++) {
        if (pthread_join(ts[i], NULL) != 0) { perror("pthread_join"); exit(1); }
    }
    unsigned long long got = atomic_load(&g_counter);
    if (got != (unsigned long long)n * 1000) {
        fprintf(stderr, "[3] bad counter: got=%llu want=%llu\n", got, (unsigned long long)n * 1000);
        exit(1);
    }
    free(ts);
    mark("[3] ok");
}

/* (4) Mutex + condvar producer/consumer — the core futex WAIT/WAKE path. The
 * consumer parks on the condvar (FUTEX_WAIT); the producer signals
 * (FUTEX_WAKE). A lost wake hangs the consumer. */
struct condvar_ctx {
    pthread_mutex_t m;
    pthread_cond_t cv;
    uint64_t v;
    uint64_t rounds;
};

static void *condvar_producer(void *arg) {
    struct condvar_ctx *c = arg;
    for (uint64_t i = 1; i <= c->rounds; i++) {
        pthread_mutex_lock(&c->m);
        c->v = i;
        pthread_cond_signal(&c->cv);
        pthread_mutex_unlock(&c->m);
    }
    return NULL;
}

static void phase_condvar(uint64_t rounds) {
    markf("[4] mutex+condvar %llu rounds: start", (unsigned long long)rounds);
    struct condvar_ctx c;
    pthread_mutex_init(&c.m, NULL);
    pthread_cond_init(&c.cv, NULL);
    c.v = 0;
    c.rounds = rounds;

    pthread_t prod;
    if (pthread_create(&prod, NULL, condvar_producer, &c) != 0) { perror("pthread_create"); exit(1); }

    pthread_mutex_lock(&c.m);
    while (c.v < rounds) {
        pthread_cond_wait(&c.cv, &c.m);
    }
    pthread_mutex_unlock(&c.m);

    pthread_join(prod, NULL);
    pthread_mutex_destroy(&c.m);
    pthread_cond_destroy(&c.cv);
    mark("[4] ok");
}

/* (5) Barrier across N threads, repeated — every thread FUTEX_WAITs until the
 * last arrives and FUTEX_WAKEs them all (a one-to-many wake). */
struct barrier_ctx {
    pthread_barrier_t b;
    int rounds;
};

static void *barrier_worker(void *arg) {
    struct barrier_ctx *c = arg;
    for (int i = 0; i < c->rounds; i++) pthread_barrier_wait(&c->b);
    return NULL;
}

static void phase_barrier(int n, int rounds) {
    markf("[5] barrier %d threads x %d rounds: start", n, rounds);
    struct barrier_ctx c;
    c.rounds = rounds;
    pthread_barrier_init(&c.b, NULL, n);
    pthread_t *ts = malloc(sizeof(pthread_t) * n);
    for (int i = 0; i < n; i++) {
        if (pthread_create(&ts[i], NULL, barrier_worker, &c) != 0) { perror("pthread_create"); exit(1); }
    }
    for (int i = 0; i < n; i++) pthread_join(ts[i], NULL);
    pthread_barrier_destroy(&c.b);
    free(ts);
    mark("[5] ok");
}

/* (6) Wake-before-wait race: the waker may fire before the waiter parks. The
 * kernel's sticky-wake flag must make schedule_blocking return immediately. */
struct wbw_ctx {
    pthread_mutex_t m;
    pthread_cond_t cv;
    int flag;
};

static void *wbw_waker(void *arg) {
    struct wbw_ctx *c = arg;
    pthread_mutex_lock(&c->m);
    c->flag = 1;
    pthread_cond_signal(&c->cv);
    pthread_mutex_unlock(&c->m);
    return NULL;
}

static void phase_wake_before_wait(int iters) {
    markf("[6] wake-before-wait race x %d: start", iters);
    for (int i = 0; i < iters; i++) {
        struct wbw_ctx c;
        pthread_mutex_init(&c.m, NULL);
        pthread_cond_init(&c.cv, NULL);
        c.flag = 0;

        pthread_t waker;
        if (pthread_create(&waker, NULL, wbw_waker, &c) != 0) { perror("pthread_create"); exit(1); }

        pthread_mutex_lock(&c.m);
        while (!c.flag) pthread_cond_wait(&c.cv, &c.m);
        pthread_mutex_unlock(&c.m);

        pthread_join(waker, NULL);
        pthread_mutex_destroy(&c.m);
        pthread_cond_destroy(&c.cv);
    }
    mark("[6] ok");
}

/* (7) park/unpark — hand-rolled with a raw futex word + direct
 * syscall(SYS_futex, ...), the same primitive std::thread::park uses on
 * Linux (not routed through pthread mutex/cond), including the
 * unpark-before-park sticky case (flag checked before FUTEX_WAIT). */
static long futex_wait(atomic_int *addr, int expected, const struct timespec *timeout) {
    return syscall(SYS_futex, addr, 0 /* FUTEX_WAIT */, expected, timeout, NULL, 0);
}
static long futex_wake(atomic_int *addr, int n) {
    return syscall(SYS_futex, addr, 1 /* FUTEX_WAKE */, n, NULL, NULL, 0);
}

struct park_ctx {
    atomic_int flag;   /* 0 = not set, 1 = set */
    pthread_t main_thread_marker; /* unused, kept for symmetry with .rs */
};

static void *park_signaler(void *arg) {
    struct park_ctx *c = arg;
    atomic_store_explicit(&c->flag, 1, memory_order_seq_cst);
    futex_wake(&c->flag, 1);
    return NULL;
}

static void phase_park_unpark(int iters) {
    markf("[7] park/unpark x %d: start", iters);
    for (int i = 0; i < iters; i++) {
        struct park_ctx c;
        atomic_store(&c.flag, 0);

        pthread_t h;
        if (pthread_create(&h, NULL, park_signaler, &c) != 0) { perror("pthread_create"); exit(1); }

        /* park_timeout(50ms) equivalent: bounded FUTEX_WAIT loop, re-checking
         * the flag each time (handles the wake-before-wait race too). */
        while (atomic_load_explicit(&c.flag, memory_order_seq_cst) == 0) {
            struct timespec ts = { .tv_sec = 0, .tv_nsec = 50 * 1000 * 1000 };
            futex_wait(&c.flag, 0, &ts);
        }

        pthread_join(h, NULL);
    }
    mark("[7] ok");
}

int main(void) {
    mark("=== FUTEXTEST_C start ===");
    const char *phase_env = getenv("FUTEXTEST_PHASE");
    int only = phase_env ? atoi(phase_env) : 0;
    int has_only = phase_env != NULL;
#define RUN(n) (!has_only || only == (n))

    if (RUN(1)) phase_spawn_join();
    if (RUN(2)) phase_spawn_join_loop();
    if (RUN(3)) phase_fanout(8);
    if (RUN(4)) phase_condvar(2000);
    if (RUN(5)) phase_barrier(6, 100);
    if (RUN(6)) phase_wake_before_wait(500);
    if (RUN(7)) phase_park_unpark(500);

    mark("=== FUTEXTEST_C DONE — all phases passed ===");
    return 0;
}
