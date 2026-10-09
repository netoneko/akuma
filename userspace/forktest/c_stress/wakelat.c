/* wakelat.c — how long does a cross-thread wake take to land?
 *
 * Why: kami's key->frame latency on the ryzen metal is ~120 ms for a page
 * Chromium rasters in a few ms. A screencast frame is a chain of cross-thread
 * handoffs (browser IO -> renderer IO -> main -> compositor -> raster ->
 * viz -> PNG encoder -> DevTools -> pipe -> daemon -> client), and
 * `amd64/src/smp.rs` says "No wake IPI. A core in hlt learns of new work at
 * its next timer tick" with a 10 ms tick. If each handoff waits for a tick,
 * a dozen of them is the whole 120 ms. This measures one handoff.
 *
 * Phases (every one a two-thread ping-pong, N round trips, per-trip
 * min/median/p90/max in microseconds, CLOCK_MONOTONIC):
 *   futex      wake(B); wait(A)   <->   wait(B); wake(A)
 *   pipe       1 byte each way over two pipes
 *   unix       1 byte each way over a socketpair(AF_UNIX)
 *   futex+busy as futex, but the pinger spins BUSY_US after waking the other
 *              side before it waits — the shape where the waker's core is
 *              not free to run the woken thread (reported with the spin
 *              subtracted). On a kernel with no wake IPI this is the case
 *              that costs a tick; on Linux the two futex rows are alike.
 *   ring4      a token around 4 threads (per-hop = trip / 4)
 *   nanosleep  requested 100 us, measured
 *
 * Linux (any x86_64 box): futex/pipe/unix round trips are 5-30 us, the busy
 * variant the same, nanosleep(100 us) lands within ~150 us.
 *
 * Build: x86_64-linux-musl-gcc -O2 -static -pthread -o wakelat wakelat.c
 *   (or on Akuma itself: gcc -O2 -static -pthread -o wakelat wakelat.c)
 * Run:   ./wakelat [N=200] [BUSY_US=3000]
 */
#define _GNU_SOURCE
#include <errno.h>
#include <linux/futex.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

static int N = 200;
static long BUSY_US = 3000;

static long now_us(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec * 1000000L + ts.tv_nsec / 1000;
}

static void spin_us(long us) {
    long end = now_us() + us;
    while (now_us() < end) { }
}

static int cmp(const void *a, const void *b) {
    long x = *(const long *)a, y = *(const long *)b;
    return x < y ? -1 : x > y;
}

static void report(const char *name, long *v, int n, long sub) {
    qsort(v, n, sizeof *v, cmp);
    printf("wakelat: %-11s n=%d min %6ld  med %6ld  p90 %6ld  max %6ld us%s\n",
           name, n, v[0] - sub, v[n / 2] - sub, v[n * 9 / 10] - sub, v[n - 1] - sub,
           sub ? "  (spin subtracted)" : "");
    fflush(stdout);
}

/* ---- futex ping-pong ------------------------------------------------- */
static volatile int fa, fb;
static long fut(volatile int *p, int op, int val) {
    return syscall(SYS_futex, p, op, val, NULL, NULL, 0);
}
static void fwait(volatile int *p, int *seen) {
    /* wait until *p != *seen, then advance seen */
    while (*p == *seen) fut(p, FUTEX_WAIT_PRIVATE, *seen);
    (*seen)++;
}
static void fwake(volatile int *p) {
    __sync_fetch_and_add(p, 1);
    fut(p, FUTEX_WAKE_PRIVATE, 1);
}
static long busy_after_wake;
static void *futex_pong(void *arg) {
    (void)arg;
    int seen = 0;
    for (int i = 0; i < N; i++) { fwait(&fb, &seen); fwake(&fa); }
    return NULL;
}
static void futex_test(const char *name, long busy) {
    pthread_t t;
    long *v = malloc(N * sizeof *v);
    fa = fb = 0; busy_after_wake = busy;
    int seen = 0;
    pthread_create(&t, NULL, futex_pong, NULL);
    for (int i = 0; i < N; i++) {
        long t0 = now_us();
        fwake(&fb);
        if (busy) spin_us(busy);
        fwait(&fa, &seen);
        v[i] = now_us() - t0;
    }
    pthread_join(t, NULL);
    report(name, v, N, busy);
    free(v);
}

/* ---- fd ping-pong (pipe or socketpair) -------------------------------- */
static int ab[2], ba[2]; /* ab: A writes, B reads; ba: the other way */
static void *fd_pong(void *arg) {
    (void)arg;
    char c;
    for (int i = 0; i < N; i++) {
        if (read(ab[0], &c, 1) != 1) { perror("pong read"); exit(2); }
        if (write(ba[1], &c, 1) != 1) { perror("pong write"); exit(2); }
    }
    return NULL;
}
static void fd_test(const char *name, int sock) {
    pthread_t t;
    long *v = malloc(N * sizeof *v);
    if (sock) {
        if (socketpair(AF_UNIX, SOCK_STREAM, 0, ab) || socketpair(AF_UNIX, SOCK_STREAM, 0, ba)) {
            printf("wakelat: %-11s socketpair: %s\n", name, strerror(errno)); return;
        }
    } else if (pipe(ab) || pipe(ba)) { perror("pipe"); exit(2); }
    pthread_create(&t, NULL, fd_pong, NULL);
    char c = 'x';
    for (int i = 0; i < N; i++) {
        long t0 = now_us();
        if (write(ab[1], &c, 1) != 1) { perror("ping write"); exit(2); }
        if (read(ba[0], &c, 1) != 1) { perror("ping read"); exit(2); }
        v[i] = now_us() - t0;
    }
    pthread_join(t, NULL);
    report(name, v, N, 0);
    close(ab[0]); close(ab[1]); close(ba[0]); close(ba[1]);
    free(v);
}

/* ---- ring of 4 -------------------------------------------------------- */
#define RING 4
static volatile int ring[RING];
struct ringarg { int idx; };
static void *ring_thread(void *arg) {
    int idx = ((struct ringarg *)arg)->idx, seen = 0;
    for (int i = 0; i < N; i++) { fwait(&ring[idx], &seen); fwake(&ring[(idx + 1) % RING]); }
    return NULL;
}
static void ring_test(void) {
    pthread_t t[RING - 1];
    struct ringarg a[RING - 1];
    long *v = malloc(N * sizeof *v);
    memset((void *)ring, 0, sizeof ring);
    for (int i = 1; i < RING; i++) { a[i - 1].idx = i; pthread_create(&t[i - 1], NULL, ring_thread, &a[i - 1]); }
    int seen = 0;
    for (int i = 0; i < N; i++) {
        long t0 = now_us();
        fwake(&ring[1]);
        fwait(&ring[0], &seen);
        v[i] = (now_us() - t0) / RING;
    }
    for (int i = 0; i < RING - 1; i++) pthread_join(t[i], NULL);
    report("ring4/hop", v, N, 0);
    free(v);
}

static void sleep_test(void) {
    long *v = malloc(N * sizeof *v);
    for (int i = 0; i < N; i++) {
        struct timespec ts = { 0, 100000 };
        long t0 = now_us();
        nanosleep(&ts, NULL);
        v[i] = now_us() - t0;
    }
    report("nanosleep100", v, N, 0);
    free(v);
}

int main(int argc, char **argv) {
    if (argc > 1) N = atoi(argv[1]);
    if (argc > 2) BUSY_US = atol(argv[2]);
    long t0 = now_us(); spin_us(1000); long c = now_us() - t0;
    printf("wakelat: N=%d busy=%ld us; clock: a 1000 us spin measured %ld us\n", N, BUSY_US, c);
    futex_test("futex", 0);
    fd_test("pipe", 0);
    fd_test("unix", 1);
    futex_test("futex+busy", BUSY_US);
    ring_test();
    sleep_test();
    return 0;
}
