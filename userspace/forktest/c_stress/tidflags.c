/*
 * tidflags — deterministic probe of clone(2)'s three tid flags.
 *
 * Linux keeps three flags strictly separate, and a kernel that conflates them
 * corrupts musl silently:
 *
 *   CLONE_PARENT_SETTID   write the child tid to `ptid` at clone time
 *   CLONE_CHILD_SETTID    write the child tid to `ctid` at clone time
 *   CLONE_CHILD_CLEARTID  write *zero* to `ctid` at child EXIT, + futex wake
 *
 * CLEARTID says nothing about clone time. It must leave `ctid` untouched until
 * the child dies. That matters because musl's `pthread_create` passes CLEARTID
 * *without* CHILD_SETTID, and the pointer it hands over is `&__thread_list_lock`
 * — a global mutex word, not a tid slot. A kernel that writes the child's tid
 * there stamps a live tid into a lock, and musl's `__tl_lock` fast path
 *
 *     int val = __thread_list_lock;
 *     if (val == tid) { tl_lock_count++; return; }   // "already mine"
 *
 * then hands the lock to the one thread whose tid was written — the new child —
 * so its `__pthread_exit` unlinks itself from the thread list with no lock held,
 * racing its own parent's link. Observed as a SIGSEGV writing to address 0x8
 * (`str x0, [x1, #8]` with a NULL `self->prev`), plus a permanently leaked
 * `tl_lock_count` that wedges every later pthread call in the process.
 *
 * Every check here is one clone and one load, with the child parked on a gate,
 * so there is no race and no stress loop: a FAIL is a kernel divergence and
 * nothing else. That is the point — the stress repro (spawnalias) hits this
 * maybe one run in three, which is not enough to A/B a fix.
 *
 * Calibrate by running the same binary on real Linux — every FAIL there means
 * the probe is wrong, not the kernel:
 *   docker run --rm --platform linux/arm64 -v "$PWD/tidflags:/tidflags:ro" alpine /tidflags
 *
 * Diagnosis: docs/runbooks/debug-thread-spawn-segv.md
 */

#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <time.h>

#define CLONE_VM 0x00000100
#define CLONE_FS 0x00000200
#define CLONE_FILES 0x00000400
#define CLONE_SIGHAND 0x00000800
#define CLONE_THREAD 0x00010000
#define CLONE_PARENT_SETTID 0x00100000
#define CLONE_CHILD_CLEARTID 0x00200000
#define CLONE_CHILD_SETTID 0x01000000

#define BASE_FLAGS (CLONE_VM | CLONE_FS | CLONE_FILES | CLONE_SIGHAND | CLONE_THREAD)

#define SENTINEL 0x5A5A5A5A

/*
 * Raw clone, register-for-register the same shape as musl's `__clone`, so the
 * child starts on exactly the instructions a real pthread starts on.
 *
 *   x0=fn  x1=stack  x2=flags  x3=arg  x4=ptid  x5=tls  x6=ctid
 */
__asm__(".text\n"
        ".globl clone_probe\n"
        ".type clone_probe,%function\n"
        "clone_probe:\n"
        "	and	x1, x1, #-16\n"
        "	stp	x0, x3, [x1, #-16]!\n"
        "	mov	x0, x2\n"
        "	mov	x2, x4\n"
        "	mov	x3, x5\n"
        "	mov	x4, x6\n"
        "	mov	x8, #220\n"
        "	svc	#0\n"
        "	cbz	x0, 1f\n"
        "	ret\n"
        "1:	ldp	x9, x0, [sp], #16\n"
        "	blr	x9\n"
        "	mov	x8, #93\n"
        "	svc	#0\n"
        ".size clone_probe,.-clone_probe\n");

extern long clone_probe(void (*fn)(void *), void *stack, unsigned long flags,
                        void *arg, int *ptid, void *tls, int *ctid);

/*
 * The child must not touch TLS (we deliberately do not pass CLONE_SETTLS) and
 * must not touch libc. Spin on a plain volatile gate, then return — clone_probe
 * turns that into SYS_exit.
 */
static volatile int gate;

static void child_fn(void *arg)
{
    (void)arg;
    /* Bounded so a broken kernel cannot hang the probe. ~seconds at 4 cores. */
    for (unsigned long i = 0; i < 3000000000UL; i++) {
        if (gate)
            return;
        __asm__ __volatile__("yield" ::: "memory");
    }
}

static void *new_stack(void)
{
    void *p = mmap(NULL, 128 * 1024, PROT_READ | PROT_WRITE,
                   MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (p == MAP_FAILED) {
        perror("mmap");
        exit(2);
    }
    return (char *)p + 128 * 1024;
}

static int failures;

static void report(const char *name, int ok, const char *detail)
{
    printf("  %-46s %s%s%s\n", name, ok ? "PASS" : "FAIL",
           detail && *detail ? " — " : "", detail ? detail : "");
    if (!ok)
        failures++;
}

/* Let the child reach its exit and the kernel run the CLEARTID write. */
static void release_and_reap(volatile int *word, int expect_zero)
{
    gate = 1;
    for (int i = 0; i < 2000; i++) {
        if (expect_zero && *word == 0)
            break;
        struct timespec ts = {0, 1000000};
        nanosleep(&ts, NULL);
    }
    if (!expect_zero) {
        struct timespec ts = {0, 50000000};
        nanosleep(&ts, NULL);
    }
    gate = 0;
}

/*
 * Probe 1 — the one that matters. CLEARTID *without* CHILD_SETTID: the ctid
 * word must be untouched while the child lives, then zeroed when it exits.
 * This is musl's exact pthread_create shape.
 */
static void probe_cleartid_only(void)
{
    static volatile int ctid;
    char buf[128];

    ctid = SENTINEL;
    gate = 0;
    long tid = clone_probe(child_fn, new_stack(),
                           BASE_FLAGS | CLONE_CHILD_CLEARTID,
                           NULL, NULL, NULL, (int *)&ctid);
    if (tid < 0) {
        report("CLEARTID: clone succeeds", 0, "clone failed");
        return;
    }

    /* The kernel's clone-time write, if any, already happened: it is inside the
     * syscall we just returned from. No race with the child. */
    int seen = ctid;
    snprintf(buf, sizeof buf, "ctid=0x%x (want sentinel 0x%x, child tid=%ld)",
             seen, SENTINEL, tid);
    report("CLEARTID alone leaves ctid untouched at clone", seen == SENTINEL, buf);
    if (seen == (int)tid)
        printf("      ^ kernel wrote the CHILD TID into a CLEARTID-only word.\n"
               "        For musl this word is &__thread_list_lock — see file header.\n");

    release_and_reap(&ctid, 1);
    seen = ctid;
    snprintf(buf, sizeof buf, "ctid=0x%x (want 0)", seen);
    report("CLEARTID zeroes ctid at child exit", seen == 0, buf);
}

/* Probe 2 — CHILD_SETTID does write at clone time, and does NOT clear at exit. */
static void probe_child_settid(void)
{
    static volatile int ctid;
    char buf[128];

    ctid = SENTINEL;
    gate = 0;
    long tid = clone_probe(child_fn, new_stack(),
                           BASE_FLAGS | CLONE_CHILD_SETTID,
                           NULL, NULL, NULL, (int *)&ctid);
    if (tid < 0) {
        report("CHILD_SETTID: clone succeeds", 0, "clone failed");
        return;
    }

    /* Linux performs the CHILD_SETTID write in the *child's* context (see
     * schedule_tail), not in the parent inside the clone syscall, so it is not
     * observable the instant clone returns. Poll for it. */
    int seen = ctid;
    for (int i = 0; i < 2000 && seen != (int)tid; i++) {
        struct timespec ts = {0, 1000000};
        nanosleep(&ts, NULL);
        seen = ctid;
    }
    snprintf(buf, sizeof buf, "ctid=0x%x (want tid=%ld)", seen, tid);
    report("CHILD_SETTID writes the child tid once child runs", seen == (int)tid, buf);

    release_and_reap(&ctid, 0);
    seen = ctid;
    snprintf(buf, sizeof buf, "ctid=0x%x (want tid=%ld, no clear)", seen, tid);
    report("CHILD_SETTID alone does NOT clear at exit", seen == (int)tid, buf);
}

/* Probe 3 — PARENT_SETTID writes ptid at clone time. musl relies on this. */
static void probe_parent_settid(void)
{
    static volatile int ptid;
    char buf[128];

    ptid = SENTINEL;
    gate = 0;
    long tid = clone_probe(child_fn, new_stack(),
                           BASE_FLAGS | CLONE_PARENT_SETTID,
                           NULL, (int *)&ptid, NULL, NULL);
    if (tid < 0) {
        report("PARENT_SETTID: clone succeeds", 0, "clone failed");
        return;
    }

    int seen = ptid;
    snprintf(buf, sizeof buf, "ptid=0x%x (want tid=%ld)", seen, tid);
    report("PARENT_SETTID writes the child tid at clone", seen == (int)tid, buf);
    release_and_reap(&ptid, 0);
}

/* Probe 4 — a non-NULL ctid with neither flag must never be written at all. */
static void probe_no_flags(void)
{
    static volatile int ctid;
    char buf[128];

    ctid = SENTINEL;
    gate = 0;
    long tid = clone_probe(child_fn, new_stack(), BASE_FLAGS,
                           NULL, NULL, NULL, (int *)&ctid);
    if (tid < 0) {
        report("no tid flags: clone succeeds", 0, "clone failed");
        return;
    }

    int seen = ctid;
    snprintf(buf, sizeof buf, "ctid=0x%x (want sentinel 0x%x)", seen, SENTINEL);
    report("no tid flags: ctid untouched at clone", seen == SENTINEL, buf);

    release_and_reap(&ctid, 0);
    seen = ctid;
    snprintf(buf, sizeof buf, "ctid=0x%x (want sentinel 0x%x)", seen, SENTINEL);
    report("no tid flags: ctid untouched at exit", seen == SENTINEL, buf);
}

/*
 * Probe 5 — the end-to-end consequence, in real pthreads. If the kernel stamps
 * a tid into &__thread_list_lock, short-lived threads unlink themselves without
 * the thread-list lock and the process eventually dies on 0x8 or wedges. This
 * is the flaky one; it is here as corroboration, not as the verdict.
 */
#include <pthread.h>
static void *nop_thread(void *a) { return a; }

static void probe_pthread_churn(int rounds)
{
    char buf[128];
    for (int r = 0; r < rounds; r++) {
        pthread_t t[8];
        int n = 0;
        for (int i = 0; i < 8; i++)
            if (pthread_create(&t[i], NULL, nop_thread, NULL) == 0)
                n++;
            else
                break;
        for (int i = 0; i < n; i++)
            pthread_join(t[i], NULL);
        if (n != 8) {
            snprintf(buf, sizeof buf, "round %d: only %d/8 threads created", r, n);
            report("pthread churn survives", 0, buf);
            return;
        }
    }
    snprintf(buf, sizeof buf, "%d rounds x 8 threads", rounds);
    report("pthread churn survives", 1, buf);
}

int main(int argc, char **argv)
{
    int rounds = argc > 1 ? atoi(argv[1]) : 400;

    printf("[tidflags] clone(2) tid-flag semantics, deterministic\n");
    probe_cleartid_only();
    probe_child_settid();
    probe_parent_settid();
    probe_no_flags();
    probe_pthread_churn(rounds);

    printf("[tidflags] %s (%d failure%s)\n", failures ? "FAIL" : "PASS",
           failures, failures == 1 ? "" : "s");
    return failures ? 1 : 0;
}
