/* fbstress — concurrent demand faults on FILE-BACKED pages, shared between
 * processes, verified byte for byte.
 *
 * Why this exists, and why `mtstress` does not cover it
 * ----------------------------------------------------
 * `mtstress` stresses ANONYMOUS memory (its arena is malloc'd; `shootdown_step`
 * churns mmap/munmap) and passes at SMP=4 on Akuma/amd64. The failure this
 * probe hunts kills `cargo`/`rustc` on the bare-metal box at SMP>1 — `rc=139`,
 * at a random crate, ~3 builds in 4 — and the thing `rustc` does that
 * `mtstress` does not is map **files**: hundreds of megabytes of
 * `librustc_driver.so` and rlibs, in many processes at once, through a kernel
 * page cache that SHARES the physical frames between them
 * (`akuma-fpcache`, keyed `(inode, mount id, file offset)`).
 *
 * `AKUMA_AMD64_SELFHOST_BUILD_SLOWNESS.md` §14 found and fixed exactly this
 * shape for anonymous pages — "a second demand fault on the same anonymous page
 * replaced it with zeros". The file-backed analogue, across processes, on real
 * cores, is untested. That is the hypothesis here; a clean run refutes it and is
 * worth as much as a failure.
 *
 * What it does
 * ------------
 * One data file whose every 8-byte word is a pure function of its own offset,
 * so ANY word can be checked in isolation with no bookkeeping:
 *
 *     word(off) = (off * 0x9E3779B97F4A7C15) ^ FBMAGIC
 *
 * Then `-p` processes x `-t` threads each, all mapping that one file
 * `MAP_PRIVATE, PROT_READ`, doing:
 *
 *   - **verify**: read words at pseudo-random page offsets and check them.
 *   - **remap** (`-m`): `munmap` a slice and map it again, so the page must be
 *     demand-faulted afresh — the operation that re-enters the cache.
 *   - **cow** (`-c`): a `PROT_READ|PROT_WRITE, MAP_PRIVATE` window, written and
 *     read back. A private write to a file page must break copy-on-write; if the
 *     break is wrong the write lands in the SHARED frame and every other process
 *     sees it, which the verify arm in the peers detects as corruption.
 *
 * What a failure tells you, which is the point
 * --------------------------------------------
 * A mismatch is classified rather than merely reported, because the three cases
 * have different causes and the classification is free:
 *
 *   ZEROS      — the page was mapped but never filled. The §14 class: a racing
 *                second fault replaced a filled page with a fresh empty one.
 *   WRONG-PAGE — the word belongs to a DIFFERENT file offset, and the report
 *                names which one. That is a frame identity bug: an fpcache key
 *                collision, a refcount error handing out a live frame, or a
 *                stale TLB entry pointing at a neighbour.
 *   GARBAGE    — neither. Torn or partial fill, or memory that was never a page
 *                of this file at all.
 *
 * WRONG-PAGE naming its actual offset is the finding that would point straight
 * at a cache or shootdown bug, so the delta is printed in pages.
 *
 * Correct kernels print, on Linux and on Akuma alike:
 *
 *     fbstress: OK  procs=P threads=T secs=S checks=N ... 0 mismatches
 *
 * and exit 0. ANY mismatch exits 1 with the classification. A child that dies
 * on a signal is reported too — on Akuma a corrupted `musl` heap shows up as
 * `SIGSEGV`/`SIGILL` inside the allocator rather than as a bad word here.
 *
 * Architecture-neutral: no asm, no syscall numbers, no page-size constant
 * (`sysconf(_SC_PAGESIZE)`). Build and run the SAME binary on Linux as a
 * control — that is the c_stress convention and it is what makes a verdict
 * here mean something:
 *
 *     cc -O2 -static -pthread -o fbstress fbstress.c
 *     ./fbstress -f /tmp/fbdata -s 8 -p 4 -t 4
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define FBMAGIC 0xA5A5C3C300FF00FFULL

static size_t PAGE;

static inline uint64_t word_for(uint64_t off) {
    return (off * 0x9E3779B97F4A7C15ULL) ^ FBMAGIC;
}

/* Given a value that should have been word_for(off) but was not, find which
 * offset WOULD produce it. The multiplier is odd, so it is invertible mod 2^64
 * and this is exact rather than a search. */
static uint64_t offset_of_word(uint64_t v) {
    /* Newton iteration for the modular inverse of the odd multiplier. */
    uint64_t k = 0x9E3779B97F4A7C15ULL, inv = k;
    for (int i = 0; i < 6; i++) inv *= 2 - k * inv;
    return (v ^ FBMAGIC) * inv;
}

static long g_secs = 8, g_procs = 4, g_threads = 4;
static int g_do_remap = 1, g_do_cow = 1;
static const char *g_path = "/tmp/fbdata";
static size_t g_size = 192u * 1024 * 1024;

static volatile int g_stop;
static uint64_t g_file_len;

struct shared {
    volatile long checks;
    volatile long mismatches;
};
static struct shared *g_sh;

static double now_s(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (double)ts.tv_sec + (double)ts.tv_nsec / 1e9;
}

static void report(const char *arm, uint64_t off, uint64_t want, uint64_t got) {
    const char *kind;
    char extra[128];
    extra[0] = 0;
    if (got == 0) {
        kind = "ZEROS";
    } else {
        uint64_t src = offset_of_word(got);
        if (src < g_file_len && (src % 8) == 0) {
            kind = "WRONG-PAGE";
            long dpages = ((long long)src - (long long)off) / (long long)PAGE;
            snprintf(extra, sizeof extra,
                     " actually_offset=%llu delta=%+ld pages",
                     (unsigned long long)src, dpages);
        } else {
            kind = "GARBAGE";
        }
    }
    fprintf(stderr,
            "fbstress: MISMATCH %s arm=%s pid=%d offset=%llu page=%llu "
            "want=0x%016llx got=0x%016llx%s\n",
            kind, arm, (int)getpid(), (unsigned long long)off,
            (unsigned long long)(off / PAGE), (unsigned long long)want,
            (unsigned long long)got, extra);
    __sync_fetch_and_add(&g_sh->mismatches, 1);
}

struct targ {
    int idx;
    unsigned long seed;
};

static void *worker(void *a) {
    struct targ *t = a;
    unsigned long rnd = t->seed;
    long local = 0;
    size_t span = (size_t)g_file_len;

    int fd = open(g_path, O_RDONLY);
    if (fd < 0) {
        fprintf(stderr, "fbstress: open: %s\n", strerror(errno));
        return NULL;
    }
    unsigned char *base = mmap(NULL, span, PROT_READ, MAP_PRIVATE, fd, 0);
    if (base == MAP_FAILED) {
        fprintf(stderr, "fbstress: mmap: %s\n", strerror(errno));
        close(fd);
        return NULL;
    }

    while (!g_stop) {
        /* --- verify: scattered words, each checked against its own offset --- */
        for (int k = 0; k < 64 && !g_stop; k++) {
            rnd = rnd * 6364136223846793005ULL + 1442695040888963407ULL;
            uint64_t off = ((rnd >> 16) % (span / 8)) * 8;
            uint64_t got = *(volatile uint64_t *)(base + off);
            uint64_t want = word_for(off);
            if (got != want) report("verify", off, want, got);
            local++;
        }

        /* --- remap: force the page back through the fault path --- */
        if (g_do_remap && !g_stop) {
            rnd = rnd * 6364136223846793005ULL + 1442695040888963407ULL;
            size_t npages = 16 + (rnd >> 8) % 48;
            size_t pgoff = ((rnd >> 24) % (span / PAGE - npages - 1));
            size_t len = npages * PAGE;
            off_t fo = (off_t)(pgoff * PAGE);
            if (munmap(base + fo, len) == 0) {
                void *r = mmap(base + fo, len, PROT_READ,
                               MAP_PRIVATE | MAP_FIXED, fd, fo);
                if (r == MAP_FAILED) {
                    fprintf(stderr, "fbstress: remap: %s\n", strerror(errno));
                    break;
                }
                /* Every page of the slice must read correctly immediately. */
                for (size_t p = 0; p < npages; p++) {
                    uint64_t off = (uint64_t)fo + p * PAGE;
                    uint64_t got = *(volatile uint64_t *)(base + off);
                    uint64_t want = word_for(off);
                    if (got != want) report("remap", off, want, got);
                    local++;
                }
            }
        }

        /* --- cow: a private write must not reach the shared frame --- */
        if (g_do_cow && !g_stop) {
            rnd = rnd * 6364136223846793005ULL + 1442695040888963407ULL;
            size_t npages = 8;
            size_t pgoff = ((rnd >> 20) % (span / PAGE - npages - 1));
            size_t len = npages * PAGE;
            off_t fo = (off_t)(pgoff * PAGE);
            unsigned char *w = mmap(NULL, len, PROT_READ | PROT_WRITE,
                                    MAP_PRIVATE, fd, fo);
            if (w != MAP_FAILED) {
                /* Read first: the private mapping must start as the file. */
                for (size_t p = 0; p < npages; p++) {
                    uint64_t off = (uint64_t)fo + p * PAGE;
                    uint64_t got = *(volatile uint64_t *)(w + p * PAGE);
                    uint64_t want = word_for(off);
                    if (got != want) report("cow-pre", off, want, got);
                    local++;
                }
                /* Now scribble, and read our own scribble back. */
                uint64_t stamp = 0xDEADBEEF00000000ULL | (uint64_t)getpid();
                for (size_t p = 0; p < npages; p++)
                    *(volatile uint64_t *)(w + p * PAGE) = stamp + p;
                for (size_t p = 0; p < npages; p++) {
                    uint64_t got = *(volatile uint64_t *)(w + p * PAGE);
                    if (got != stamp + p)
                        report("cow-own", (uint64_t)fo + p * PAGE, stamp + p, got);
                    local++;
                }
                munmap(w, len);
            }
        }
    }

    __sync_fetch_and_add(&g_sh->checks, local);
    munmap(base, span);
    close(fd);
    return NULL;
}

static void run_child(int pidx) {
    pthread_t th[64];
    struct targ ta[64];
    long n = g_threads;
    if (n > 64) n = 64;
    for (long i = 0; i < n; i++) {
        ta[i].idx = (int)i;
        ta[i].seed = 0x1234567u + (unsigned long)pidx * 7919u + (unsigned long)i * 104729u;
        if (pthread_create(&th[i], NULL, worker, &ta[i]) != 0) {
            fprintf(stderr, "fbstress: pthread_create: %s\n", strerror(errno));
            n = i;
            break;
        }
    }
    double end = now_s() + (double)g_secs;
    while (now_s() < end) usleep(50 * 1000);
    g_stop = 1;
    for (long i = 0; i < n; i++) pthread_join(th[i], NULL);
    _exit(0);
}

static int make_file(void) {
    struct stat st;
    if (stat(g_path, &st) == 0 && (size_t)st.st_size == g_size) {
        g_file_len = (uint64_t)st.st_size;
        return 0; /* reuse — creating it is the slow part */
    }
    fprintf(stderr, "fbstress: creating %s (%llu MiB)\n", g_path,
            (unsigned long long)(g_size / 1024 / 1024));
    int fd = open(g_path, O_RDWR | O_CREAT | O_TRUNC, 0644);
    if (fd < 0) { perror("fbstress: create"); return -1; }
    size_t chunk = 1u << 20;
    uint64_t *buf = malloc(chunk);
    if (!buf) { close(fd); return -1; }
    for (uint64_t off = 0; off < g_size; off += chunk) {
        for (size_t i = 0; i < chunk / 8; i++) buf[i] = word_for(off + i * 8);
        if (write(fd, buf, chunk) != (ssize_t)chunk) {
            perror("fbstress: write");
            free(buf); close(fd); return -1;
        }
    }
    free(buf);
    fsync(fd);
    close(fd);
    g_file_len = g_size;
    return 0;
}

int main(int argc, char **argv) {
    PAGE = (size_t)sysconf(_SC_PAGESIZE);
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "-s") && i + 1 < argc) g_secs = atol(argv[++i]);
        else if (!strcmp(argv[i], "-p") && i + 1 < argc) g_procs = atol(argv[++i]);
        else if (!strcmp(argv[i], "-t") && i + 1 < argc) g_threads = atol(argv[++i]);
        else if (!strcmp(argv[i], "-f") && i + 1 < argc) g_path = argv[++i];
        else if (!strcmp(argv[i], "-z") && i + 1 < argc)
            g_size = (size_t)atol(argv[++i]) * 1024 * 1024;
        else if (!strcmp(argv[i], "-M")) g_do_remap = 0;
        else if (!strcmp(argv[i], "-C")) g_do_cow = 0;
        else {
            fprintf(stderr,
                    "usage: fbstress [-f file] [-z MiB] [-s secs] [-p procs] "
                    "[-t threads] [-M no-remap] [-C no-cow]\n");
            return 2;
        }
    }
    if (make_file() != 0) return 2;

    g_sh = mmap(NULL, sizeof *g_sh, PROT_READ | PROT_WRITE,
                MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (g_sh == MAP_FAILED) { perror("fbstress: shared"); return 2; }
    g_sh->checks = 0;
    g_sh->mismatches = 0;

    printf("fbstress: start procs=%ld threads=%ld secs=%ld file=%s %lluMiB page=%zu\n",
           g_procs, g_threads, g_secs, g_path,
           (unsigned long long)(g_file_len / 1024 / 1024), PAGE);
    fflush(stdout);

    pid_t kids[64];
    long np = g_procs > 64 ? 64 : g_procs;
    for (long i = 0; i < np; i++) {
        pid_t p = fork();
        if (p == 0) run_child((int)i);
        if (p < 0) { perror("fbstress: fork"); np = i; break; }
        kids[i] = p;
    }

    int signalled = 0, nonzero = 0;
    for (long i = 0; i < np; i++) {
        int st = 0;
        waitpid(kids[i], &st, 0);
        if (WIFSIGNALED(st)) {
            fprintf(stderr, "fbstress: child %d died on signal %d\n",
                    (int)kids[i], WTERMSIG(st));
            signalled++;
        } else if (WIFEXITED(st) && WEXITSTATUS(st) != 0) {
            fprintf(stderr, "fbstress: child %d exited %d\n",
                    (int)kids[i], WEXITSTATUS(st));
            nonzero++;
        }
    }

    long mm = g_sh->mismatches;
    printf("fbstress: %s procs=%ld threads=%ld secs=%ld checks=%ld "
           "mismatches=%ld signalled=%d nonzero=%d\n",
           (mm == 0 && signalled == 0 && nonzero == 0) ? "OK" : "FAIL",
           g_procs, g_threads, g_secs, g_sh->checks, mm, signalled, nonzero);
    return (mm == 0 && signalled == 0 && nonzero == 0) ? 0 : 1;
}
