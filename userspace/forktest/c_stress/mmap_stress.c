/*
 * mmap_stress.c — pure-C control for forktest_parent to disambiguate
 * kernel-vs-Go-runtime crashes. Mirrors runMmapStress shape:
 *   for iter in [0, infty):
 *       for i in 0..NUM_ALLOCS:
 *           p = mmap(NULL, mb << 20, RW, PRIVATE|ANON, -1, 0)
 *           memset(p, 0, mb << 20)
 *           munmap(p, mb << 20)
 *       sleep ~100ms
 *
 * Static, musl, no Go runtime. If this still crashes, the kernel is at fault.
 *
 * Usage:
 *   mmap_stress -duration=10s -mmap_alloc_mb=70
 * Recognized flags (subset compatible with forktest_child for forktest_parent
 * arg-forwarding): -duration=DUR, -mmap_alloc_mb=N, -mmap_test=BOOL
 * (-mmap_test/-combined_stress accepted and ignored; this binary always runs
 * the stress loop).
 */

#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <signal.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>

static const int kNumAllocations = 5;

static void on_sigterm(int sig) {
    (void)sig;
    fprintf(stderr, "mmap_stress: Received terminated, exiting gracefully.\n");
    fflush(stderr);
    _exit(0);
}

static long parse_duration_secs(const char *s) {
    /* Very small parser: accepts "10s", "30s", "1m", "10000ms", or bare seconds. */
    char *end = NULL;
    long v = strtol(s, &end, 10);
    if (v <= 0) return 0;
    if (!end || *end == '\0') return v;
    if (end[0] == 's' && end[1] == '\0') return v;
    if (end[0] == 'm' && end[1] == 's' && end[2] == '\0') return v / 1000;
    if (end[0] == 'm' && end[1] == '\0') return v * 60;
    if (end[0] == 'h' && end[1] == '\0') return v * 3600;
    return v;
}

static int parse_kv(const char *arg, const char *key, const char **val) {
    size_t klen = strlen(key);
    if (strncmp(arg, key, klen) != 0) return 0;
    if (arg[klen] != '=') return 0;
    *val = arg + klen + 1;
    return 1;
}

int main(int argc, char **argv) {
    long duration_s = 10;
    int mb = 70;
    const char *child_id = getenv("FORKTEST_CHILD_ID");
    if (!child_id) child_id = "?";

    for (int i = 1; i < argc; i++) {
        const char *v = NULL;
        if (parse_kv(argv[i], "-duration", &v) || parse_kv(argv[i], "--duration", &v)) {
            long d = parse_duration_secs(v);
            if (d > 0) duration_s = d;
        } else if (parse_kv(argv[i], "-mmap_alloc_mb", &v) || parse_kv(argv[i], "--mmap_alloc_mb", &v)) {
            int m = atoi(v);
            if (m > 0) mb = m;
        }
    }

    struct sigaction sa;
    memset(&sa, 0, sizeof(sa));
    sa.sa_handler = on_sigterm;
    sigaction(SIGTERM, &sa, NULL);
    sigaction(SIGINT, &sa, NULL);

    fprintf(stdout, "mmap_stress %s: Hello (C control). duration=%lds mb=%d\n",
            child_id, duration_s, mb);
    fflush(stdout);

    size_t alloc_size = (size_t)mb * 1024UL * 1024UL;
    struct timespec t0, now;
    clock_gettime(CLOCK_MONOTONIC, &t0);

    int iter = 0;
    for (;;) {
        clock_gettime(CLOCK_MONOTONIC, &now);
        long elapsed = now.tv_sec - t0.tv_sec;
        if (elapsed >= duration_s) break;

        iter++;
        for (int i = 0; i < kNumAllocations; i++) {
            fprintf(stdout, "mmap_stress %s: [iter %d] Allocating %d MB (%d/%d)...\n",
                    child_id, iter, mb, i + 1, kNumAllocations);
            fflush(stdout);

            void *p = mmap(NULL, alloc_size, PROT_READ | PROT_WRITE,
                           MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
            if (p == MAP_FAILED) {
                fprintf(stderr, "mmap_stress %s: mmap(%zu) failed\n", child_id, alloc_size);
                fflush(stderr);
                return 2;
            }

            memset(p, 0, alloc_size);

            if (munmap(p, alloc_size) != 0) {
                fprintf(stderr, "mmap_stress %s: munmap failed\n", child_id);
                fflush(stderr);
                return 2;
            }

            struct timespec sleep_ts = { 0, 100 * 1000 * 1000 };
            nanosleep(&sleep_ts, NULL);

            clock_gettime(CLOCK_MONOTONIC, &now);
            if ((now.tv_sec - t0.tv_sec) >= duration_s) break;
        }
    }

    fprintf(stdout, "mmap_stress %s: stress finished (%d iterations).\n",
            child_id, iter);
    fflush(stdout);
    return 0;
}
