/*
 * epoll_wake_latency — how long a thread parked in epoll_wait takes to run again
 * after another thread makes its fd ready.
 *
 * The parking counterpart of `epoll_op_cost.c`, which never parks on purpose.
 * This one measures exactly what that one excludes: the wake path. It is the
 * userspace form of the kernel self-test `test_epoll_socket_waker`, built so the
 * same source runs on the AArch64 kernel, the amd64 kernel and Linux, before
 * and after a scheduler change:
 *
 *   aarch64-linux-musl-gcc -static -O2 -pthread -o epoll_wake_latency epoll_wake_latency.c
 *   x86_64-linux-musl-gcc  -static -O2 -pthread -o epoll_wake_latency epoll_wake_latency.c
 *
 * Results and the harness that ran them: docs/archive/AKUMA_MULTIARCH_ATTEMPT_0.md
 * (W3b follow-up).
 *
 * One iteration: the waiter arms, then parks in epoll_wait(-1) on a pipe. The
 * waker waits for the arm, lets the waiter get into the park (`delay_us`, by
 * nanosleep or by spinning — mode `sleep` or `spin`), takes a CLOCK_MONOTONIC
 * stamp and writes one byte. The waiter stamps again the moment epoll_wait
 * returns. The difference is the wake latency. Each side also records the CPU
 * it ran on (getcpu; -1 where a kernel does not answer it), so same-core and
 * cross-core wakes are reported apart, since they take different paths.
 *
 * `spin` keeps the waker's core busy across the wait, as the kernel test does;
 * `sleep` leaves it idle. If the waiter returns BEFORE the write (it should
 * not: the pipe is empty), the iteration is counted as `early` and left out.
 *
 * Usage:  epoll_wake_latency [iters] [delay_us] [sleep|spin]   (default 200 2000 spin)
 * Output: one summary line per bucket:
 *   <bucket> n=N min=A p50=B p90=C p99=D max=E (us)
 */
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

#define MAX_ITERS 5000

static int pipefd[2];
static int iters = 200;
static long delay_us = 2000;
static int spin_mode = 1;

static atomic_int armed;      /* iteration number the waiter has armed for */
static atomic_int done_iter;  /* iteration number the waiter has finished */

static uint64_t write_ns[MAX_ITERS];
static int waker_cpu[MAX_ITERS];

static uint64_t now_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec;
}

static int cpu(void) {
    unsigned c = 0;
    if (syscall(SYS_getcpu, &c, NULL, NULL) != 0) return -1;
    return (int)c;
}

static void *waker(void *arg) {
    (void)arg;
    for (int i = 1; i <= iters; i++) {
        while (atomic_load(&armed) < i) sched_yield();
        if (spin_mode) {
            uint64_t until = now_ns() + (uint64_t)delay_us * 1000ull;
            while (now_ns() < until) { }
        } else {
            struct timespec d = { delay_us / 1000000, (delay_us % 1000000) * 1000 };
            nanosleep(&d, NULL);
        }
        waker_cpu[i - 1] = cpu();
        write_ns[i - 1] = now_ns();
        char b = 1;
        if (write(pipefd[1], &b, 1) != 1) { perror("write"); exit(2); }
        while (atomic_load(&done_iter) < i) sched_yield();
    }
    return NULL;
}

static int cmp_u64(const void *a, const void *b) {
    uint64_t x = *(const uint64_t *)a, y = *(const uint64_t *)b;
    return x < y ? -1 : x > y;
}

static void report(const char *name, uint64_t *v, int n) {
    if (n == 0) { printf("%-10s n=0\n", name); return; }
    qsort(v, (size_t)n, sizeof v[0], cmp_u64);
    printf("%-10s n=%d min=%llu p50=%llu p90=%llu p99=%llu max=%llu (us)\n", name, n,
           (unsigned long long)(v[0] / 1000), (unsigned long long)(v[n / 2] / 1000),
           (unsigned long long)(v[(n * 9) / 10] / 1000), (unsigned long long)(v[(n * 99) / 100] / 1000),
           (unsigned long long)(v[n - 1] / 1000));
}

int main(int argc, char **argv) {
    if (argc > 1) iters = atoi(argv[1]);
    if (argc > 2) delay_us = atol(argv[2]);
    if (argc > 3) spin_mode = strcmp(argv[3], "sleep") != 0;
    if (iters < 1 || iters > MAX_ITERS) { fprintf(stderr, "iters 1..%d\n", MAX_ITERS); return 1; }

    if (pipe(pipefd) != 0) { perror("pipe"); return 1; }
    int ep = epoll_create1(0);
    if (ep < 0) { perror("epoll_create1"); return 1; }
    struct epoll_event ev = { .events = EPOLLIN, .data.u64 = 0xDEADBEEF };
    if (epoll_ctl(ep, EPOLL_CTL_ADD, pipefd[0], &ev) != 0) { perror("epoll_ctl"); return 1; }

    pthread_t t;
    if (pthread_create(&t, NULL, waker, NULL) != 0) { perror("pthread_create"); return 1; }

    static uint64_t same[MAX_ITERS], cross[MAX_ITERS], all[MAX_ITERS];
    int ns = 0, nc = 0, na = 0, early = 0, unknown_cpu = 0;
    for (int i = 1; i <= iters; i++) {
        atomic_store(&armed, i);
        struct epoll_event out;
        int r;
        do { r = epoll_wait(ep, &out, 1, -1); } while (r < 0 && errno == EINTR);
        uint64_t ret = now_ns();
        int my_cpu = cpu();
        if (r != 1 || out.data.u64 != 0xDEADBEEF) { fprintf(stderr, "epoll_wait=%d\n", r); return 1; }
        char b;
        if (read(pipefd[0], &b, 1) != 1) { perror("read"); return 1; }
        uint64_t w = write_ns[i - 1];
        if (w == 0 || ret < w) {
            early++;
        } else {
            uint64_t lat = ret - w;
            all[na++] = lat;
            if (my_cpu < 0 || waker_cpu[i - 1] < 0) unknown_cpu++;
            else if (my_cpu == waker_cpu[i - 1]) same[ns++] = lat;
            else cross[nc++] = lat;
        }
        atomic_store(&done_iter, i);
    }
    pthread_join(t, NULL);

    printf("epoll_wake_latency iters=%d delay_us=%ld mode=%s early=%d unknown_cpu=%d\n", iters,
           delay_us, spin_mode ? "spin" : "sleep", early, unknown_cpu);
    report("all", all, na);
    report("same-core", same, ns);
    report("cross-core", cross, nc);
    return 0;
}
