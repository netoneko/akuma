// bssfork — after a fork, can the process's own threads still write its globals?
//
// The narrowest statement of the defect in
// proposals/COWSTALE_FORK_THREAD_SEGV.md. `cowstale` found that defect while
// looking for something else, so it carries an mmap region, two patterns and a
// verification pass that have nothing to do with it. This probe carries none of
// that: no mmap, no patterns, nothing but threads, `.bss`, and `fork`.
//
// The mechanism it targets:
//
//   `fork` demotes the whole address space to read-only so copy-on-write can
//   catch the next write. Every thread that then writes the same page faults at
//   once. The first one through breaks CoW — the page gets a private frame and a
//   writable PTE, and the CoW reference that made the break legal is consumed.
//   The threads behind it are serialised on the same page and arrive holding a
//   fault for a write that has since become legal. If the kernel judges that
//   fault against the *old* state instead of re-reading the page table, it finds
//   no CoW reference and no mapping record — an ELF `.data`/`.bss` page has no
//   `mmap` region — and kills the process for writing its own global variable.
//
// So: T threads incrementing adjacent counters (one `.bss` page, so they all
// contend on it), while the main thread forks R times. Nothing here is racy by
// design — every one of these writes is legal at every instant, on any OS. The
// counters are per thread, so no lock is needed and none is used: a lock would
// serialise the threads and close the very window this is aiming at.
//
// Detection:
//   - the process dying is the failure (a bad kernel SIGSEGVs it, EXIT=139)
//   - the parent's sentinel must never show the child's value (CoW must isolate)
//   - every thread must have run at all, checked once over the whole run rather
//     than per round: with more workers than cores a thread can sit off-CPU for a
//     long time, and a per-round deadline calls that a failure (it did — the
//     first version of this check failed on real Linux, which is the probe being
//     wrong, not the kernel)
//
// Build: aarch64-linux-musl-gcc -static -O2 -Wall -Wextra -o bssfork bssfork.c -pthread
// Calibrate: docker run --rm --platform linux/arm64 -v "$PWD/bssfork:/bssfork:ro"
//                alpine /bssfork 20 8   (expect PASS)
// Usage: bssfork [rounds] [threads] [spread]
//   spread=0 (default) — all counters share one page: threads contend, which is
//                        the condition the defect needs.
//   spread=1           — one page per thread: same thread count, same fork churn,
//                        no two threads faulting on the same page. The control.
//                        Use it to tell "this load is too much for the machine"
//                        from "this load hits the contended-fault path".
// Exit code 0 = every round clean.

#define _GNU_SOURCE
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/wait.h>
#include <unistd.h>

#define MAX_THREADS 32
#define PAGE_SIZE 4096

// All in `.bss` — the segment under test. Adjacent on purpose: the counters share
// one page, so every worker faults on the *same* page after each fork, which is
// what makes the losing-thread window reachable at all.
static volatile unsigned long g_ticks[MAX_THREADS];
static volatile unsigned long g_sentinel;
static volatile int g_stop;

// The control (`spread=1`): the same counters, one page apart, so the threads
// keep every other property of the load and lose only the shared page.
static volatile unsigned long g_spread[MAX_THREADS][PAGE_SIZE / sizeof(unsigned long)];
static int g_use_spread;

// Where thread `slot` counts. Both live in `.bss`; only the spacing differs.
static volatile unsigned long *tick_slot(size_t slot)
{
    return g_use_spread ? &g_spread[slot][0] : &g_ticks[slot];
}

#define PARENT_SENTINEL 0x5041524e544cUL
#define CHILD_SENTINEL  0x4348494c4432UL

static void *worker(void *arg)
{
    volatile unsigned long *ticks = tick_slot((size_t)(uintptr_t)arg);
    while (!g_stop) {
        (*ticks)++;
    }
    return NULL;
}

int main(int argc, char **argv)
{
    size_t rounds = (argc > 1) ? strtoul(argv[1], NULL, 0) : 20;
    int threads = (argc > 2) ? atoi(argv[2]) : 8;
    if (threads < 1) threads = 1;
    if (threads > MAX_THREADS) threads = MAX_THREADS;
    g_use_spread = (argc > 3) ? atoi(argv[3]) : 0;

    printf("bssfork: rounds=%zu threads=%d spread=%d ticks=%p sentinel=%p\n",
           rounds, threads, g_use_spread, (void *)tick_slot(0), (void *)&g_sentinel);
    fflush(stdout);

    g_sentinel = PARENT_SENTINEL;

    pthread_t th[MAX_THREADS];
    int started = 0;
    for (int i = 0; i < threads; i++) {
        if (pthread_create(&th[i], NULL, worker, (void *)(uintptr_t)i) != 0) {
            printf("bssfork: pthread_create failed at %d (continuing with fewer)\n", i);
            break;
        }
        started++;
    }
    if (started == 0) { printf("bssfork FAIL [no threads]\n"); return 2; }

    int failures = 0;
    unsigned long at_start[MAX_THREADS];
    for (int i = 0; i < started; i++) at_start[i] = *tick_slot(i);

    for (size_t round = 0; round < rounds; round++) {
        fflush(stdout);
        pid_t pid = fork();
        if (pid < 0) { perror("fork"); failures++; break; }
        if (pid == 0) {
            // Child: write the same globals. Only the forking thread survives into
            // the child, so this is the single-writer half of the same page.
            g_sentinel = CHILD_SENTINEL;
            (*tick_slot(0))++;
            _exit(g_sentinel == CHILD_SENTINEL ? 0 : 1);
        }

        int status = 0;
        if (waitpid(pid, &status, 0) < 0) { perror("waitpid"); failures++; break; }
        if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
            printf("bssfork FAIL [child] round=%zu status=0x%x\n", round, status);
            fflush(stdout);
            failures++;
        }

        // The child's write must not be visible here: that is CoW's whole job.
        if (g_sentinel != PARENT_SENTINEL) {
            printf("bssfork FAIL [sentinel] round=%zu want=%lx got=%lx\n",
                   round, PARENT_SENTINEL, g_sentinel);
            fflush(stdout);
            failures++;
        }

        if (failures > 8) {
            printf("bssfork: stopping early after %d failures\n", failures);
            break;
        }
    }

    g_stop = 1;
    for (int i = 0; i < started; i++) pthread_join(th[i], NULL);

    // Liveness, measured across the whole run rather than per round: a thread can
    // legitimately sit off-CPU for a long time when the workers outnumber the
    // cores, and a per-round deadline turns that into a false FAIL (it did — real
    // Linux failed the first version of this check).
    unsigned long total = 0;
    for (int i = 0; i < started; i++) {
        unsigned long now = *tick_slot(i);
        if (now == at_start[i]) {
            printf("bssfork FAIL [never ran] thread=%d ticks=%lu\n", i, now);
            failures++;
        }
        total += now;
    }
    printf("bssfork: threads=%d ticks=%lu failures=%d\n", started, total, failures);
    printf("bssfork %s\n", failures == 0 ? "PASS" : "FAIL");
    return failures == 0 ? 0 : 1;
}
