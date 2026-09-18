/* mtstress — the LLD-shaped SMP probe: ONE process, MANY threads, ONE address
 * space.
 *
 * Why another probe. `smpstress` and `execleak2` are *process*-shaped — fork,
 * exec, CoW, grandchildren — and they found the two causes
 * AKUMA_AMD64_SELFHOST_BUILD_SLOWNESS.md §"Open: SMP=4" records as fixed. The
 * three failures still open there are not that shape:
 *
 *   | symptom                                        | where            |
 *   |------------------------------------------------|------------------|
 *   | processes created, never scheduled, build wedges| FC SMP=4 -j4     |
 *   | `rust-lld` SIGSEGV at the final link            | bare metal SMP=4 |
 *   | `rustc` derefs a pointer overwritten with ASCII | bare metal SMP=4 |
 *
 * All three involve a **multi-threaded user process on several cores**, and
 * `rust-lld` — one process, a default-on thread pool, deterministic — is the
 * cheapest of them. That is the shape this reproduces: sibling threads sharing
 * one CR3, so every `munmap`/`mprotect`/`madvise` one of them runs must reach
 * the cores the others are running on. Process churn does not exercise that at
 * all: separate processes have separate page tables, and a lost shootdown
 * between them is invisible.
 *
 * What each arm is for, and which symptom it answers:
 *
 *   p  pointer integrity.  Every block carries a pointer to itself. A block
 *      whose `self` no longer points at it has been overwritten by something,
 *      and the report prints the bad word as hex **and as ASCII** — because
 *      the bare-metal failure was `cr2=0x00004d5f4e4f4964`, which is not a
 *      pointer at all but the bytes `dION_M`. The payload is deliberately
 *      ASCII text for the same reason: when a stray copy lands in a pointer,
 *      this says *which* block's text it was, instead of "checksum mismatch".
 *
 *   s  shootdown churn.  mmap/mprotect/madvise(MADV_DONTNEED)/munmap in the
 *      shared address space while siblings fault and read. A TLB shootdown
 *      that fails to reach a peer core shows up as a read that still sees the
 *      old mapping (stale data after DONTNEED) or a write that should have
 *      faulted after mprotect(PROT_READ) and did not.
 *
 *   c  thread-pool churn.  Batches of short-lived threads created and joined,
 *      which is what a parallel linker does between passes and what recycles
 *      kernel thread slots — the `[TRAMP-MISMATCH]` storm's fuel.
 *
 *   h  heartbeat watchdog.  Every worker bumps a counter; the watchdog names
 *      any thread whose counter has not moved for WATCHDOG_S seconds. This is
 *      the "created and never scheduled" symptom, observed from *inside* the
 *      process — the wedge capture in §10 could only see it from the outside,
 *      as a `ps` line at 0:00 CPU.
 *
 *   f  fault-kill reaping.  Fork a multi-threaded child and let it die — half
 *      the time cleanly, half the time by dereferencing NULL, which is how the
 *      real victim died (`#PF ... cr2=0x34 ... killing the process`, rustc,
 *      2026-09-18). The parent then waits with a deadline. A wait that never
 *      returns for a child the kernel has already killed is the wedge itself:
 *      measured that day, `cargo` sat at 0:24 CPU while `ps` still listed the
 *      rustc the kernel had announced it was killing.
 *
 *      This arm reports only a **wait that does not return**. It does not
 *      report the status shape: a killed child exits `128+SIGSEGV` here rather
 *      than reporting a signalled status, which is a known, pinned divergence
 *      (it is why `eager_mprotect_probe` is on `amd64_mem_trials.py`'s
 *      EXPECTED_FAIL list) and not what this is hunting.
 *
 * Usage:  mtstress [seconds] [threads] [modes]
 *         mtstress 120 4 pschf    (the default: everything, 120 s, 4 threads)
 *         mtstress 60 4 p         (pointer integrity alone — is it corruption?)
 *         mtstress 60 4 s         (shootdowns alone — is it the TLB?)
 *         mtstress 60 4 f         (reaping alone — does a dead child wake wait?)
 *
 * The matrix in `scripts/benchmarks/amd64_fc_build_matrix.py` is why `f` is
 * here: `zerocopy` builds fine at vcpu=1/-j4 and at vcpu=4/-j1, and wedges only
 * at vcpu=4/-j4. Several processes AND several cores are both necessary, so a
 * single-process probe — which is all of p/s/c/h — cannot reach it.
 *
 * Build: x86_64-linux-musl-gcc -static -O2 -pthread -o mtstress mtstress.c
 * Run:   injected at /probes/mtstress, or copied into a guest and run directly.
 * Print: one MTSTRESS-FAIL / MTSTRESS-STUCK line per finding, then
 *        "mtstress: FAIL ..." and exit 42. A clean run prints
 *        "mtstress: PASS ..." and exits 0. A *silent* run is not a pass — the
 *        harness rule from mem_suite applies here too.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <unistd.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <pthread.h>
#include <errno.h>
#include <time.h>

#define MAX_THREADS   16
#define ARENA_BLOCKS  512
#define BLOCK_BYTES   1024
#define TEXT_BYTES    64
#define SCRATCH_BYTES (256 * 1024)
#define POOL_BATCH    4
#define WATCHDOG_S    20
#define PAGE          4096
/* How long a parent may wait for a child that has already died before the wait
 * itself is the finding. Generous: the guest is slow and heavily loaded, and a
 * false WAITSTUCK would be worse than a missed one. */
#define WAIT_BUDGET_S 30
#define CHILD_THREADS 2

/* A self-describing block. `self` is the instrument: it is the one field whose
 * correct value is known without reading anything else, so a mismatch needs no
 * second source to be believed. */
struct block {
    struct block *self;
    struct block *peer;            /* a block in another thread's arena */
    uint64_t seq;
    uint64_t owner;
    uint64_t csum;
    char text[TEXT_BYTES];         /* ASCII on purpose — see the header */
    uint64_t pad[(BLOCK_BYTES - 5 * 8 - TEXT_BYTES) / 8];
};

struct worker {
    int idx;
    struct block *arena;           /* ARENA_BLOCKS blocks */
    volatile uint64_t hb;          /* heartbeat, bumped every iteration */
    volatile uint64_t iters;
    uint64_t ever_ran;
};

static struct worker g_w[MAX_THREADS];
static int g_nthreads = 4;
static volatile int g_stop;
static volatile int g_fail;
static volatile int g_stuck;
static int g_mode_p = 1, g_mode_s = 1, g_mode_c = 1, g_mode_h = 1, g_mode_f = 1;

/* Start barrier. Without it the probe accuses the kernel of its own race:
 * `main` mmaps every arena before creating any thread, so `g_w[peer].arena` is
 * a valid pointer to *untouched* anonymous memory long before that peer has
 * filled it. Thread 2 then reads thread 3's arena while `main` is still inside
 * `pthread_create` for thread 3, sees zero pages, and reports a null `self`
 * pointer — which is exactly the shape of the kernel bug being hunted.
 *
 * Caught by running this binary on real Linux first (62 findings there), which
 * is the whole reason the calibration arm is not optional. */
static volatile int g_ready;
static void barrier_arrive_and_wait(void) {
    __sync_fetch_and_add(&g_ready, 1);
    while (g_ready < g_nthreads && !g_stop)
        usleep(1000);
}

static uint64_t mix(uint64_t x) {
    x ^= x >> 33; x *= 0xff51afd7ed558ccdULL;
    x ^= x >> 33; x *= 0xc4ceb9fe1a85ec53ULL;
    x ^= x >> 33;
    return x;
}

static uint64_t now_s(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec;
}

/* Render a word as the eight characters it would be if it were text, which is
 * the whole point: the bare-metal `cr2` was readable ASCII and nobody noticed
 * until it was printed this way. Non-printables become '.'. */
static void ascii8(uint64_t v, char out[9]) {
    for (int i = 0; i < 8; i++) {
        unsigned char c = (unsigned char)((v >> (8 * i)) & 0xff);
        out[i] = (c >= 0x20 && c < 0x7f) ? (char)c : '.';
    }
    out[8] = 0;
}

static void fail_word(const char *what, int idx, const void *at,
                      uint64_t want, uint64_t got) {
    char a[9];
    ascii8(got, a);
    if (g_fail < 16) {
        printf("MTSTRESS-FAIL %s thread=%d at=%p want=%#llx got=%#llx got_as_text=\"%s\"\n",
               what, idx, at, (unsigned long long)want, (unsigned long long)got, a);
        fflush(stdout);
    }
    g_fail++;
}

static uint64_t block_csum(const struct block *b) {
    uint64_t c = b->seq ^ (b->owner << 32);
    for (size_t i = 0; i < sizeof b->pad / sizeof b->pad[0]; i++)
        c = mix(c ^ b->pad[i]);
    for (int i = 0; i < TEXT_BYTES; i++)
        c = mix(c ^ (uint64_t)(unsigned char)b->text[i]);
    return c;
}

static void fill_block(struct block *b, int owner, uint64_t seq) {
    b->self = b;
    b->seq = seq;
    b->owner = (uint64_t)owner;
    /* Recognisable text, with the owner and sequence in it, so a fragment that
     * turns up inside a pointer names where it came from. */
    snprintf(b->text, TEXT_BYTES, "AKUMA_MTSTRESS_OWNER_%02d_SEQ_%012llu_TEXT",
             owner, (unsigned long long)seq);
    for (size_t i = 0; i < sizeof b->pad / sizeof b->pad[0]; i++)
        b->pad[i] = mix(seq + i + ((uint64_t)owner << 40));
    b->csum = block_csum(b);
}

static void check_block(struct block *b, int idx) {
    if (b->self != b) {
        fail_word("self-pointer", idx, (void *)&b->self,
                  (uint64_t)(uintptr_t)b, (uint64_t)(uintptr_t)b->self);
        return;
    }
    if (b->owner != (uint64_t)idx) {
        fail_word("owner", idx, (void *)&b->owner, (uint64_t)idx, b->owner);
        return;
    }
    uint64_t want = block_csum(b);
    if (want != b->csum)
        fail_word("checksum", idx, (void *)&b->csum, want, b->csum);
}

/* --- the shootdown arm ---------------------------------------------------
 *
 * Every step here is a page-table edit in an address space that sibling
 * threads are running in on other cores. The checks are the point: after
 * MADV_DONTNEED an anonymous page must read back as zero (a stale TLB entry
 * would still show the old bytes), and after mprotect(PROT_READ) a read must
 * still work (a shootdown that unmapped instead of demoting would fault).
 */
static void shootdown_step(int idx, uint64_t it) {
    unsigned char *s = mmap(NULL, SCRATCH_BYTES, PROT_READ | PROT_WRITE,
                            MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (s == MAP_FAILED) {
        if (errno != ENOMEM)
            fail_word("mmap-scratch", idx, NULL, 0, (uint64_t)errno);
        return;
    }
    uint64_t seed = mix(it ^ ((uint64_t)idx << 32));
    for (size_t off = 0; off < SCRATCH_BYTES; off += PAGE)
        *(uint64_t *)(s + off) = seed + off;

    for (size_t off = 0; off < SCRATCH_BYTES; off += PAGE) {
        uint64_t got = *(uint64_t *)(s + off);
        if (got != seed + off)
            fail_word("scratch-write", idx, s + off, seed + off, got);
    }

    /* Demote, read (must still work), restore. */
    if (mprotect(s, SCRATCH_BYTES / 2, PROT_READ) == 0) {
        for (size_t off = 0; off < SCRATCH_BYTES / 2; off += PAGE) {
            uint64_t got = *(volatile uint64_t *)(s + off);
            if (got != seed + off)
                fail_word("after-mprotect-read", idx, s + off, seed + off, got);
        }
        mprotect(s, SCRATCH_BYTES / 2, PROT_READ | PROT_WRITE);
    }

    /* Drop, then re-read: anonymous pages must come back zeroed. A stale
     * translation on a peer core is exactly what would not. */
    if (madvise(s, SCRATCH_BYTES, MADV_DONTNEED) == 0) {
        for (size_t off = 0; off < SCRATCH_BYTES; off += PAGE) {
            uint64_t got = *(uint64_t *)(s + off);
            if (got != 0)
                fail_word("stale-after-dontneed", idx, s + off, 0, got);
        }
    }
    munmap(s, SCRATCH_BYTES);
}

/* --- the thread-pool churn arm ------------------------------------------ */
static void *pool_member(void *arg) {
    struct worker *w = arg;
    unsigned char *p = mmap(NULL, 64 * 1024, PROT_READ | PROT_WRITE,
                            MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (p != MAP_FAILED) {
        for (size_t off = 0; off < 64 * 1024; off += PAGE)
            p[off] = (unsigned char)(w->idx + 1);
        for (size_t off = 0; off < 64 * 1024; off += PAGE)
            if (p[off] != (unsigned char)(w->idx + 1))
                fail_word("pool-page", w->idx, p + off,
                          (uint64_t)(w->idx + 1), (uint64_t)p[off]);
        munmap(p, 64 * 1024);
    }
    return NULL;
}

static void pool_step(struct worker *w) {
    pthread_t t[POOL_BATCH];
    int made = 0;
    for (int i = 0; i < POOL_BATCH; i++)
        if (pthread_create(&t[i], NULL, pool_member, w) == 0)
            made++;
        else
            break;
    for (int i = 0; i < made; i++)
        pthread_join(t[i], NULL);
}

/* --- the fault-kill reaping arm -----------------------------------------
 *
 * The shape of the observed wedge, made deterministic. A multi-threaded child
 * dies — cleanly, or by the same NULL dereference the real rustc died of — and
 * the parent waits with a deadline. `waitpid(WNOHANG)` in a poll loop rather
 * than a blocking wait plus `alarm`: signal delivery is itself partial on this
 * target, so a blocking wait that never returns would hang the probe instead of
 * reporting the hang, and the probe would go SILENT — which the harness scores
 * as "no verdict", not as the finding it is.
 */
static volatile uint64_t g_waitstuck;
static volatile uint64_t g_children;
/* The address the `f` arm faults on. A mutable global, so it is opaque to the
 * optimiser — see `fork_kill_cycle`. */
static volatile uintptr_t g_fault_addr = 0x34;

static void *child_thread(void *arg) {
    struct worker *w = arg;
    unsigned char *p = mmap(NULL, 128 * 1024, PROT_READ | PROT_WRITE,
                            MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (p == MAP_FAILED)
        return NULL;
    for (int round = 0; round < 8; round++) {
        for (size_t off = 0; off < 128 * 1024; off += PAGE)
            *(uint64_t *)(p + off) = mix(off + round + (uint64_t)w->idx);
        madvise(p, 128 * 1024, MADV_DONTNEED);
    }
    munmap(p, 128 * 1024);
    return NULL;
}

/* One fork/die/reap cycle. `die_by_fault` selects which way the child ends. */
static void fork_kill_cycle(struct worker *w, int die_by_fault) {
    pid_t pid = fork();
    if (pid < 0)
        return;
    if (pid == 0) {
        pthread_t t[CHILD_THREADS];
        int made = 0;
        for (int i = 0; i < CHILD_THREADS; i++)
            if (pthread_create(&t[i], NULL, child_thread, w) == 0)
                made++;
        /* Do not join: the real victim died with siblings still running, and
         * that is the case worth covering — a thread group torn down from a
         * fault raised on one of its members. */
        if (made == 0)
            _exit(3);
        if (die_by_fault) {
            /* Through a `volatile` global so the compiler cannot prove the
             * target is address zero and warn about it (-Warray-bounds): the
             * fault is the point, and a probe that builds with warnings is one
             * people stop rebuilding. 0x34 is the real victim's `cr2`. */
            volatile int *nowhere = (volatile int *)(uintptr_t)g_fault_addr;
            *nowhere = 1;              /* the rustc signature: a near-NULL write */
            _exit(4);                  /* not reached on a kernel that faults */
        }
        for (int i = 0; i < made; i++)
            pthread_join(t[i], NULL);
        _exit(0);
    }

    g_children++;
    uint64_t t0 = now_s();
    for (;;) {
        int st = 0;
        pid_t r = waitpid(pid, &st, WNOHANG);
        if (r == pid)
            return;                    /* reaped — the status shape is not ours to judge */
        if (r < 0 && errno == ECHILD)
            return;                    /* already reaped by something else */
        if (now_s() - t0 >= WAIT_BUDGET_S) {
            printf("MTSTRESS-WAITSTUCK child=%d by_fault=%d waited=%llus "
                   "waitpid=%d errno=%d\n",
                   (int)pid, die_by_fault,
                   (unsigned long long)(now_s() - t0), (int)r, errno);
            fflush(stdout);
            g_waitstuck++;
            return;
        }
        usleep(20000);
    }
}

/* --- the worker ---------------------------------------------------------- */
static void *worker(void *arg) {
    struct worker *w = arg;
    w->ever_ran = 1;
    uint64_t seq = 1;

    for (int i = 0; i < ARENA_BLOCKS; i++)
        fill_block(&w->arena[i], w->idx, seq++);

    /* Nobody reads a peer until every peer has filled its own arena. */
    barrier_arrive_and_wait();

    while (!g_stop) {
        w->hb++;
        w->iters++;

        if (g_mode_p) {
            /* Rewrite a window, then verify the whole arena. The window moves
             * with the iteration so a block is both freshly written and
             * long-settled across a run. */
            size_t base = (size_t)(w->iters * 37) % ARENA_BLOCKS;
            for (int k = 0; k < 32; k++)
                fill_block(&w->arena[(base + k) % ARENA_BLOCKS], w->idx, seq++);
            for (int i = 0; i < ARENA_BLOCKS; i++)
                check_block(&w->arena[i], w->idx);

            /* Read a peer's arena. Read-only, and only the self-pointer: a
             * peer rewrites its own blocks concurrently, so the checksum is
             * legitimately in flux, but `self` never changes once set. */
            int peer = (w->idx + 1) % g_nthreads;
            if (g_w[peer].arena) {
                for (int i = 0; i < ARENA_BLOCKS; i += 8) {
                    struct block *b = &g_w[peer].arena[i];
                    if (b->self != b)
                        fail_word("peer-self-pointer", w->idx, (void *)&b->self,
                                  (uint64_t)(uintptr_t)b,
                                  (uint64_t)(uintptr_t)b->self);
                }
            }
        }

        if (g_mode_s)
            shootdown_step(w->idx, w->iters);

        if (g_mode_c && (w->iters % 8) == 0)
            pool_step(w);

        /* Only thread 0 forks. A fork from several threads of one process at
         * once is `execleak2`'s `mtfork` shape and is already covered there;
         * duplicating it here would make a finding ambiguous between the two
         * probes, which is exactly what the mode split exists to avoid. */
        if (g_mode_f && w->idx == 0 && (w->iters % 4) == 0)
            fork_kill_cycle(w, (int)((w->iters / 4) % 2));
    }
    return NULL;
}

/* --- the watchdog -------------------------------------------------------- */
static void *watchdog(void *arg) {
    (void)arg;
    uint64_t last[MAX_THREADS];
    uint64_t stamp[MAX_THREADS];
    uint64_t t0 = now_s();
    for (int i = 0; i < g_nthreads; i++) { last[i] = 0; stamp[i] = t0; }

    while (!g_stop) {
        sleep(2);
        uint64_t now = now_s();
        for (int i = 0; i < g_nthreads; i++) {
            uint64_t hb = g_w[i].hb;
            if (hb != last[i]) { last[i] = hb; stamp[i] = now; continue; }
            if (now - stamp[i] >= WATCHDOG_S) {
                /* This is the "created and never scheduled" symptom, seen from
                 * inside. `ever_ran` separates the two cases that look alike
                 * from outside: a thread that never got a core at all, and one
                 * that ran and then stopped getting one. */
                printf("MTSTRESS-STUCK thread=%d hb=%llu stalled=%llus ever_ran=%llu\n",
                       i, (unsigned long long)hb,
                       (unsigned long long)(now - stamp[i]),
                       (unsigned long long)g_w[i].ever_ran);
                fflush(stdout);
                g_stuck++;
                stamp[i] = now;        /* report once per WATCHDOG_S, not per tick */
            }
        }
    }
    return NULL;
}

int main(int argc, char **argv) {
    int secs = (argc > 1) ? atoi(argv[1]) : 120;
    if (argc > 2) {
        g_nthreads = atoi(argv[2]);
        if (g_nthreads < 1) g_nthreads = 1;
        if (g_nthreads > MAX_THREADS) g_nthreads = MAX_THREADS;
    }
    if (argc > 3) {
        const char *m = argv[3];
        g_mode_p = strchr(m, 'p') != NULL;
        g_mode_s = strchr(m, 's') != NULL;
        g_mode_c = strchr(m, 'c') != NULL;
        g_mode_h = strchr(m, 'h') != NULL;
        g_mode_f = strchr(m, 'f') != NULL;
    }

    printf("mtstress: start pid=%d threads=%d secs=%d modes=%s%s%s%s%s\n",
           (int)getpid(), g_nthreads, secs,
           g_mode_p ? "p" : "", g_mode_s ? "s" : "",
           g_mode_c ? "c" : "", g_mode_h ? "h" : "", g_mode_f ? "f" : "");
    fflush(stdout);

    for (int i = 0; i < g_nthreads; i++) {
        g_w[i].idx = i;
        g_w[i].arena = mmap(NULL, ARENA_BLOCKS * sizeof(struct block),
                            PROT_READ | PROT_WRITE,
                            MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (g_w[i].arena == MAP_FAILED) {
            printf("mtstress: FAIL arena mmap thread=%d errno=%d\n", i, errno);
            fflush(stdout);
            return 1;
        }
    }

    pthread_t th[MAX_THREADS], wd;
    int made = 0;
    for (int i = 0; i < g_nthreads; i++) {
        if (pthread_create(&th[i], NULL, worker, &g_w[i]) != 0) {
            printf("MTSTRESS-FAIL pthread_create thread=%d errno=%d\n", i, errno);
            fflush(stdout);
            g_fail++;
            break;
        }
        made++;
    }
    /* A create that failed would otherwise leave every started thread spinning
     * at a barrier whose target can never be reached. */
    if (made < g_nthreads)
        g_nthreads = made;
    if (g_mode_h)
        pthread_create(&wd, NULL, watchdog, NULL);

    uint64_t t0 = now_s();
    while (now_s() - t0 < (uint64_t)secs && !g_stop)
        sleep(1);
    g_stop = 1;

    for (int i = 0; i < made; i++)
        pthread_join(th[i], NULL);
    if (g_mode_h)
        pthread_join(wd, NULL);

    uint64_t total = 0;
    int never = 0;
    for (int i = 0; i < g_nthreads; i++) {
        total += g_w[i].iters;
        if (!g_w[i].ever_ran) never++;
        printf("mtstress: thread=%d iters=%llu ever_ran=%llu\n",
               i, (unsigned long long)g_w[i].iters,
               (unsigned long long)g_w[i].ever_ran);
    }
    /* A thread that was created and never once reached its first instruction
     * is the strongest form of the wedge symptom, so it is a failure in its
     * own right rather than a footnote in the iteration count. */
    if (never)
        printf("MTSTRESS-FAIL threads-never-ran=%d\n", never);

    printf("mtstress: %s corruption=%d stuck=%d never_ran=%d waitstuck=%llu "
           "children=%llu iters=%llu\n",
           (g_fail || g_stuck || never || g_waitstuck) ? "FAIL" : "PASS",
           g_fail, g_stuck, never, (unsigned long long)g_waitstuck,
           (unsigned long long)g_children, (unsigned long long)total);
    fflush(stdout);
    return (g_fail || g_stuck || never || g_waitstuck) ? 42 : 0;
}
