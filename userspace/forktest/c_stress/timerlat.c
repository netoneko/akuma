/* timerlat.c — how late do timed waits fire?
 *
 * Why: Chromium's frame pipeline is a chain of timers (BeginFrame at 60 Hz,
 * compositor and raster deadlines, the message pumps' epoll timeouts), and
 * `wakelat` showed a 100 us nanosleep landing at 1 ms median, 6 ms p90 and
 * 29 ms max on the ryzen metal. This measures the overshoot of every timed
 * wait Chromium uses, at the intervals it uses them:
 *   nanosleep         relative, 1 / 5 / 16 ms
 *   epoll_pwait       timeout 1 / 5 / 16 ms on an fd that never becomes ready
 *                     (base::MessagePumpEpoll's delayed tasks)
 *   futex-bitset-abs  FUTEX_WAIT_BITSET with an absolute CLOCK_MONOTONIC
 *                     deadline 1 / 5 / 16 ms out (pthread_cond_timedwait)
 *   ppoll             timeout 16 ms
 * Each row: N waits, requested interval, measured min/median/p90/max in us,
 * and the overshoot (median minus requested).
 *
 * Linux (any x86_64 box, default HZ): all land within ~50-100 us of the
 * request; with a 250 Hz HZ, nanosleep/epoll overshoot by up to 1 ms.
 *
 * Build: x86_64-linux-musl-gcc -O2 -static -pthread -o timerlat timerlat.c
 * Run:   ./timerlat [N=100]
 */
#define _GNU_SOURCE
#include <errno.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

#define FUTEX_WAIT_BITSET_PRIVATE (9 | 128)
#define FUTEX_BITSET_MATCH_ANY 0xffffffff

static int N = 100;

static long now_us(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec * 1000000L + ts.tv_nsec / 1000;
}
static int cmp(const void *a, const void *b) {
    long x = *(const long *)a, y = *(const long *)b;
    return x < y ? -1 : x > y;
}
static void report(const char *name, long req_us, long *v, int n) {
    qsort(v, n, sizeof *v, cmp);
    printf("timerlat: %-16s req %5ld us: min %6ld  med %6ld  p90 %6ld  max %6ld us  (median over by %ld us)\n",
           name, req_us, v[0], v[n / 2], v[n * 9 / 10], v[n - 1], v[n / 2] - req_us);
    fflush(stdout);
}

static void t_nanosleep(long us) {
    long *v = malloc(N * sizeof *v);
    for (int i = 0; i < N; i++) {
        struct timespec ts = { us / 1000000, (us % 1000000) * 1000 };
        long t0 = now_us();
        nanosleep(&ts, NULL);
        v[i] = now_us() - t0;
    }
    report("nanosleep", us, v, N);
    free(v);
}

static void t_epoll(long us) {
    long *v = malloc(N * sizeof *v);
    int ep = epoll_create1(0);
    int p[2];
    if (ep < 0 || pipe(p)) { perror("epoll/pipe"); return; }
    struct epoll_event ev = { .events = EPOLLIN, .data.fd = p[0] };
    epoll_ctl(ep, EPOLL_CTL_ADD, p[0], &ev);
    for (int i = 0; i < N; i++) {
        struct epoll_event out[4];
        long t0 = now_us();
        int r = epoll_pwait(ep, out, 4, (int)(us / 1000), NULL);
        v[i] = now_us() - t0;
        if (r != 0) { printf("timerlat: epoll_pwait returned %d (%s)\n", r, strerror(errno)); }
    }
    report("epoll_pwait", us, v, N);
    close(ep); close(p[0]); close(p[1]);
    free(v);
}

static void t_futex_abs(long us) {
    long *v = malloc(N * sizeof *v);
    int word = 0;
    for (int i = 0; i < N; i++) {
        struct timespec ts;
        clock_gettime(CLOCK_MONOTONIC, &ts);
        long t0 = ts.tv_sec * 1000000L + ts.tv_nsec / 1000;
        ts.tv_nsec += (us % 1000000) * 1000; ts.tv_sec += us / 1000000;
        if (ts.tv_nsec >= 1000000000L) { ts.tv_nsec -= 1000000000L; ts.tv_sec++; }
        long r = syscall(SYS_futex, &word, FUTEX_WAIT_BITSET_PRIVATE, 0, &ts, NULL, FUTEX_BITSET_MATCH_ANY);
        v[i] = now_us() - t0;
        if (r == 0 || errno != ETIMEDOUT) { printf("timerlat: futex returned %ld (%s)\n", r, strerror(errno)); }
    }
    report("futex-bitset-abs", us, v, N);
    free(v);
}

static void t_ppoll(long us) {
    long *v = malloc(N * sizeof *v);
    int p[2];
    if (pipe(p)) { perror("pipe"); return; }
    for (int i = 0; i < N; i++) {
        struct pollfd pf = { .fd = p[0], .events = POLLIN };
        struct timespec ts = { us / 1000000, (us % 1000000) * 1000 };
        long t0 = now_us();
        ppoll(&pf, 1, &ts, NULL);
        v[i] = now_us() - t0;
    }
    report("ppoll", us, v, N);
    close(p[0]); close(p[1]);
    free(v);
}

int main(int argc, char **argv) {
    if (argc > 1) N = atoi(argv[1]);
    printf("timerlat: N=%d\n", N);
    long ivs[] = { 1000, 5000, 16000 };
    for (int k = 0; k < 3; k++) t_nanosleep(ivs[k]);
    for (int k = 0; k < 3; k++) t_epoll(ivs[k]);
    for (int k = 0; k < 3; k++) t_futex_abs(ivs[k]);
    t_ppoll(16000);
    return 0;
}
