/* smpstress — SMP=4 memory-corruption probe for the amd64 guest.
 *
 * Stresses the suspect surface from AKUMA_AMD64_SELFHOST_BUILD_SLOWNESS.md
 * "Open: SMP=4": fork + CoW write + mmap/munmap/madvise(MADV_DONTNEED) churn +
 * concurrent reads of a shared file mapping, with every thread re-verifying
 * its written patterns each iteration. A frame that goes to zeroes, a wrong
 * frame getting mapped, or a CoW break that loses a write shows up as a
 * pattern mismatch in seconds instead of ten minutes of cargo.
 *
 * Shape (mirrors a -j4 build):
 *   - one spawner; forks NPROC worker processes (default 4)
 *   - each worker has NT threads (default 2; rustc is multithreaded)
 *   - each thread owns a private anon region filled with a per-(pid,tid)
 *     pattern, and between churn steps verifies every byte
 *   - churn: madvise(MADV_DONTNEED) over half the region, mmap/munmap of
 *     throwaway regions, fork a grandchild that writes a copy and exits
 *   - every process also maps /tmp/smpstress.bin PROT_READ and verifies the
 *     per-offset pattern continuously (the "reads serving zeros" family)
 *
 * Build: x86_64-linux-musl-gcc -static -O2 -pthread -o smpstress smpstress.c
 * Run:   injected at /probes/smpstress, INIT=/probes/smpstress
 * Print: one CHECK-FAIL line per corruption, then exit_group(42); a clean run
 *        prints "smpstress: PASS <iters> iterations" and exits 0.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <unistd.h>
#include <fcntl.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <pthread.h>
#include <errno.h>
#include <sys/sysinfo.h>

#define NPROC 4
#define NT    2
#define REGION_BYTES  (512 * 1024)
#define FILE_BYTES    (256 * 1024)
#define FILE_PATH     "/tmp/smpstress.bin"
#define RUN_SECS      300
/* Per worker: ballast that forces real PMM pressure once all four run.
 * 4 x BALLAST + regions + kernel ~= 1.3 GiB of 2 GiB. Touched once, then
 * spot-verified on a rotating window, so a frame the kernel gave away or
 * zeroed while the mapping still exists shows up within a few iterations. */
#define BALLAST_BYTES (64u * 1024 * 1024)
#define BALLAST_SPOT  (4u * 1024 * 1024)

static void fill_file_pages(unsigned char *, size_t, uint64_t);

struct thread_arg;
static void diagnose(const unsigned char *, size_t, uint64_t, struct thread_arg *);

static volatile int g_fail;
static volatile int g_stop;

static uint64_t mix(uint64_t x) {
    x ^= x >> 33; x *= 0xff51afd7ed558ccdULL;
    x ^= x >> 33; x *= 0xc4ceb9fe1a85ec53ULL;
    x ^= x >> 33;
    return x;
}

static void fail(const char *what, long a, long expect, long got) {
    if (g_fail == 0) {
        printf("CHECK-FAIL %s a=%lx expect=%lx got=%lx pid=%d\n",
               what, a, expect, got, (int)getpid());
        fflush(stdout);
    }
    g_fail++;
}

static void fill_pattern(unsigned char *p, size_t n, uint64_t seed) {
    for (size_t i = 0; i < n; i += 8) {
        uint64_t v = mix(seed + i);
        memcpy(p + i, &v, (n - i < 8) ? n - i : 8);
    }
}

static void check_pattern(const unsigned char *p, size_t n, uint64_t seed,
                          const char *what, struct thread_arg *ta) {
    for (size_t i = 0; i < n; i += 8) {
        uint64_t want = mix(seed + i), got = 0;
        size_t k = (n - i < 8) ? n - i : 8;
        memcpy(&got, p + i, k);
        if (got != want) {
            fail(what, (long)(uintptr_t)p + i, (long)want, (long)got);
            if (ta) diagnose(p, i, seed, ta);
            return;
        }
    }
}


static void setup_file(void) {
    int fd = open(FILE_PATH, O_RDWR | O_CREAT | O_TRUNC, 0644);
    if (fd < 0) { fail("open-file", 0, 0, errno); exit(1); }
    static unsigned char buf[FILE_BYTES];
    fill_file_pages(buf, FILE_BYTES, 0xC0FFEE);
    if (write(fd, buf, FILE_BYTES) != FILE_BYTES) { fail("write-file", 0, 0, errno); exit(1); }
    close(fd);
}

/* Each thread maps the file read-only itself and re-checks it. */
/* Exec churn: the crashing build forks+execs constantly, and every exec maps
 * file-backed text through the shared file-page cache; a teardown/shootdown
 * race there serves wrong bytes as text — exactly the ring-3 #UD shape. */
static void *exec_churner(void *arg) {
    (void)arg;
    long n = 0;
    time_t t0 = time(NULL);
    /* Iteration cap, not just the clock: under full TCG saturation the guest
     * clock lags host time enough that a clock-only bound drags on. */
    int eagain_run = 0;
    while (n < 1500 && time(NULL) - t0 < RUN_SECS && !g_fail) {
        pid_t pid = fork();
        if (pid < 0) {
            /* Transient EAGAIN under churn is the aarch64 fork-exhaustion
             * family; only a sustained failure is a probe-worthy signal. */
            if (errno == EAGAIN && ++eagain_run < 50) { usleep(10000); continue; }
            fail("exec-fork", eagain_run, 0, errno);
            return NULL;
        }
        eagain_run = 0;
        if (pid == 0) {
            char *argv[] = {"hello", NULL};
            execv("/bin/hello", argv);
            _exit(127);
        }
        int st = 0;
        waitpid(pid, &st, 0);
        /* /bin/hello is the tree's self-check ELF: 0x7F means every probe
         * bit passed (its own argv/env/auxv/regpreservation checks), not a
         * plain exit-0 binary. */
        if (st != 0x7f << 8) { fail("exec-status", n, 0x7f << 8, st); return NULL; }
        n++;
    }
    printf("exec-churner pid=%d execs=%ld\n", (int)getpid(), n);
    fflush(stdout);
    return NULL;
}

/* File rewrite under mapping: a writer rewrites the shared file in place with
 * a new version tag; readers must always see *some* known version's pattern
 * per page — never zeros, never unrelated bytes. Pages may mix versions
 * within one pass (the write is not atomic against mappers); a page whose
 * bytes disagree with the version its own first qword names, or that matches
 * no version the reader has observed, is the bug. */
#define NVERSIONS 64

/* Page layout for the mapped file: the first qword of every 4 KiB page names
 * the version (the pattern seed); every other qword is mix(seed + offset). A
 * reader can therefore identify a page's version from the page itself and
 * verify the rest against it. */
static void fill_file_pages(unsigned char *buf, size_t n, uint64_t seed) {
    for (size_t off = 0; off < n; off += 4096) {
        memcpy(buf + off, &seed, 8);
        for (size_t i = off + 8; i < off + 4096; i += 8) {
            uint64_t v = mix(seed + i);
            memcpy(buf + i, &v, 8);
        }
    }
}

static void *file_writer(void *arg) {
    (void)arg;
    static unsigned char buf[FILE_BYTES];
    uint64_t version = 0;
    time_t t0 = time(NULL);
    while (time(NULL) - t0 < RUN_SECS && !g_fail) {
        version++;
        fill_file_pages(buf, FILE_BYTES, 0xC0FFEE + version);
        int fd = open(FILE_PATH, O_WRONLY);
        if (fd < 0) { fail("writer-open", 0, 0, errno); return NULL; }
        if (write(fd, buf, FILE_BYTES) != FILE_BYTES) { fail("writer-write", 0, 0, errno); close(fd); return NULL; }
        close(fd);
        usleep(20000);
    }
    printf("file-writer done versions=%llu\n", (unsigned long long)version);
    fflush(stdout);
    return NULL;
}

static void *file_checker(void *arg) {
    (void)arg;
    int fd = open(FILE_PATH, O_RDONLY);
    if (fd < 0) { fail("open-ro", 0, 0, errno); return NULL; }
    unsigned char *m = mmap(NULL, FILE_BYTES, PROT_READ, MAP_PRIVATE, fd, 0);
    if (m == MAP_FAILED) { fail("mmap-file", 0, 0, errno); return NULL; }
    uint64_t seen[NVERSIONS];
    int nseen = 0;
    seen[nseen++] = 0xC0FFEE;
    long iters = 0;
    while (!g_stop) {
        for (size_t off = 0; off < FILE_BYTES; off += 4096) {
            uint64_t v = 0;
            memcpy(&v, m + off, 8);
            int known = 0;
            for (int k = 0; k < nseen; k++)
                if (seen[k] == v) { known = 1; break; }
            if (!known) {
                /* First sighting: record it and verify next pass — the page
                 * may be mid-update, and half-old/half-new is the writer's
                 * right, not a corruption. */
                if (nseen < NVERSIONS) seen[nseen++] = v;
                else seen[iters % NVERSIONS] = v;
                continue;
            }
            /* verify this page against the version its tag qword names */
            for (size_t i = off + 8; i < off + 4096; i += 8) {
                uint64_t want = mix(v + i), got = 0;
                memcpy(&got, m + i, 8);
                if (got != want) {
                    /* The writer may have rewritten this page between the
                     * version read above and now. Only a mismatch that
                     * survives a re-read of the version tag is real. */
                    uint64_t v2 = 0;
                    memcpy(&v2, m + off, 8);
                    /* tag unchanged but body stale/wrong => corruption */
                    uint64_t got2 = 0;
                    memcpy(&got2, m + i, 8);
                    if (v2 == v && got2 != want) {
                        fail("file-map", (long)(uintptr_t)(m + i), (long)want, (long)got2);
                        return NULL;
                    }
                    break; /* page changed under us — next pass re-verifies */
                }
            }
        }
        iters++;
    }
    printf("file-checker pid=%d iters=%ld ok=%d\n", (int)getpid(), iters, g_fail == 0);
    fflush(stdout);
    return NULL;
}

struct thread_arg {
    int idx;
    unsigned char *region;
    uint64_t seed;
    unsigned char *ballast;
    struct thread_arg *peers[NT];
};

/* On a mismatch, identify the impostor: dump neighbour qwords and test the
 * got value against this thread's and its peer's patterns at nearby offsets. */
static void diagnose(const unsigned char *p, size_t i, uint64_t seed, /* seed already offset-adjusted */
                     struct thread_arg *ta) {
    printf("diagnose va=%p self-seed=%llx\n", (const void *)(p + i),
           (unsigned long long)seed);
    for (int d = -3; d <= 3; d++) {
        size_t off = (size_t)((long)i + d * 8);
        uint64_t v = 0;
        memcpy(&v, p + off, 8);
        printf("  self[%+d qwords] = %llx (want %llx)\n", d * 8,
               (unsigned long long)v,
               (unsigned long long)mix(seed + off));
    }
    for (int t = 0; t < NT; t++) {
        struct thread_arg *peer = ta ? ta->peers[t] : NULL;
        if (!peer) continue;
        printf("  peer%d region=%p ballast=%p seed=%llx\n", t,
               (void *)peer->region, (void *)peer->ballast,
               (unsigned long long)(peer->seed ^ 0xBA11A57ULL));
    }
    fflush(stdout);
}

static void fork_grandchild(unsigned char *region) {
    pid_t pid = fork();
    if (pid < 0) {
        if (errno == EAGAIN) return; /* transient; next iteration retries */
        fail("fork", 0, 0, errno);
        return;
    }
    if (pid == 0) {
        /* Child: writes its whole CoW copy, then exits. A lost CoW break or a
         * demote without a flush shows up as the parent's next check failing. */
        for (size_t i = 0; i < REGION_BYTES; i += 4096)
            region[i] ^= 0x5a;
        _exit(0);
    }
    int st = 0;
    waitpid(pid, &st, 0);
}

static void *worker(void *ap) {
    struct thread_arg *ta = ap;
    unsigned char *r = ta->region;
    uint64_t seed = ta->seed;
    printf("worker-va pid=%d idx=%d region=%p ballast=%p seed=%llx\n",
           (int)getpid(), ta->idx, (void *)r, (void *)ta->ballast,
           (unsigned long long)seed);
    fflush(stdout);
    fill_pattern(r, REGION_BYTES, seed);
    check_pattern(r, REGION_BYTES, seed, "initial", ta);

    /* Ballast: fill once, verify once, then re-verify a rotating window every
     * iteration. With four workers live this is ~1 GiB the kernel cannot
     * recycle without breaking the mapping — the pressure the cargo build
     * ran under at minute ten. */
    unsigned char *b = ta->ballast;
    fill_pattern(b, BALLAST_BYTES, seed ^ 0xBA11A57ULL);
    check_pattern(b, BALLAST_BYTES, seed ^ 0xBA11A57ULL, "ballast-init", ta);

    unsigned char *scratch = mmap(NULL, 128 * 1024, PROT_READ | PROT_WRITE,
                                  MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (scratch == MAP_FAILED) { fail("mmap-scratch", 0, 0, errno); g_stop = 1; return NULL; }

    time_t t0 = time(NULL);
    long it = 0;
    for (; !g_fail; it++) {
        if (time(NULL) - t0 >= RUN_SECS) break;

        /* spot-check a rotating 4 MiB window of the ballast */
        size_t off = (size_t)(it * BALLAST_SPOT) % (BALLAST_BYTES - BALLAST_SPOT);
        check_pattern(b + off, BALLAST_SPOT, (seed ^ 0xBA11A57ULL) + off, "ballast", ta);
        /* madvise DONTNEED over the second half, then refill it. First half
         * must survive untouched across every iteration. */
        if (madvise(r + REGION_BYTES / 2, REGION_BYTES / 2, MADV_DONTNEED) != 0)
            fail("madvise", 0, 0, errno);
        fill_pattern(r + REGION_BYTES / 2, REGION_BYTES / 2, seed + REGION_BYTES / 2);

        /* churn scratch: touch, dontneed, munmap, remap */
        fill_pattern(scratch, 128 * 1024, seed + it);
        madvise(scratch, 128 * 1024, MADV_DONTNEED);
        if ((it & 7) == 7) {
            munmap(scratch, 128 * 1024);
            scratch = mmap(NULL, 128 * 1024, PROT_READ | PROT_WRITE,
                           MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
            if (scratch == MAP_FAILED) { fail("mmap-scratch2", 0, 0, errno); break; }
        }

        /* fork churn every 16 iterations */
        if ((it % 64) == 32) {
            check_pattern(r, REGION_BYTES, seed, "pre-fork", ta);
            fork_grandchild(r);
        }

        check_pattern(r, REGION_BYTES, seed, "iter", ta);
        if ((it % 100) == 0 && ta->idx == 0) {
            struct sysinfo si;
            sysinfo(&si);
            printf("smpstress: pid=%d it=%ld freeram=%lu MB\n",
                   (int)getpid(), it, si.freeram >> 20);
            fflush(stdout);
        }
    }
    return NULL;
}

int main(void) {
    printf("smpstress: start pid=%d\n", (int)getpid());
    fflush(stdout);
    setup_file();

    pid_t kids[NPROC];
    for (int p = 0; p < NPROC; p++) {
        pid_t pid = fork();
        if (pid < 0) { fail("fork-worker", 0, 0, errno); return 1; }
        if (pid == 0) {
            struct thread_arg ta[NT];
            pthread_t th[NT];
            for (int t = 0; t < NT; t++) {
                ta[t].idx = t;
                ta[t].region = mmap(NULL, REGION_BYTES, PROT_READ | PROT_WRITE,
                                    MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
                if (ta[t].region == MAP_FAILED) { fail("mmap-region", 0, 0, errno); _exit(1); }
                ta[t].seed = mix(((uint64_t)getpid() << 20) ^ (uint64_t)t ^ 0xABCD);
                ta[t].ballast = mmap(NULL, BALLAST_BYTES, PROT_READ | PROT_WRITE,
                                     MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
                if (ta[t].ballast == MAP_FAILED) { fail("mmap-ballast", 0, 0, errno); _exit(1); }
            }
            /* one dedicated file checker + one exec churner per worker */
            pthread_t fc, ec;
            pthread_create(&fc, NULL, file_checker, NULL);
            pthread_create(&ec, NULL, exec_churner, NULL);
            for (int a0 = 0; a0 < NT; a0++)
                for (int a1 = 0; a1 < NT; a1++)
                    ta[a0].peers[a1] = &ta[a1];
            for (int t = 0; t < NT; t++)
                pthread_create(&th[t], NULL, worker, &ta[t]);
            void *ret;
            for (int t = 0; t < NT; t++)
                pthread_join(th[t], &ret);
            g_stop = 1;
            pthread_join(fc, &ret);
            pthread_join(ec, &ret);
            printf("worker pid=%d done fail=%d\n", (int)getpid(), g_fail);
            fflush(stdout);
            _exit(g_fail ? 42 : 0);
        }
        kids[p] = pid;
    }

    /* Parent checks the file mapping concurrently too (plus one writer). */
    pthread_t pfc, pfw;
    pthread_create(&pfc, NULL, file_checker, NULL);
    pthread_create(&pfw, NULL, file_writer, NULL);

    int nfail = 0;
    for (int p = 0; p < NPROC; p++) {
        int st = 0;
        waitpid(kids[p], &st, 0);
        if (st != 0) {
            printf("worker %d bad status=%d\n", p, st);
            nfail++;
        }
    }
    g_stop = 1;
    void *ret;
    pthread_join(pfc, &ret);
    pthread_join(pfw, &ret);
    if (g_fail) nfail++;
    printf("smpstress: %s bad_workers=%d\n", nfail ? "FAIL" : "PASS", nfail);
    fflush(stdout);
    _exit(nfail ? 42 : 0);
}
