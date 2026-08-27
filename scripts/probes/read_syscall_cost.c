/*
 * read_syscall_cost — what one `read(2)` costs, split into fixed and per-byte.
 *
 * Built for BOTH kernels from this one source (musl static, aarch64), so an
 * Akuma number and a Linux number differ by the kernel and nothing else — the
 * same instruction stream issues the same `svc` on the same silicon. See
 * `docs/archive/EXT2_READ_PATH_STAGE_PROFILE.md`.
 *
 * Three arms, because "a read costs N ns" is not one number:
 *
 *   zero   read(2) of `len` bytes from /dev/zero  — syscall + a kernel memset,
 *          no filesystem at all.
 *   file   pread(2) of `len` bytes from a warm regular file — the real path.
 *   null   read(2) of ZERO bytes from the same file — syscall entry, fd lookup
 *          and return, with no bytes moved. This is the fixed cost, measured
 *          rather than fitted.
 *
 * `file - null` is what the bytes cost; `null` is what the syscall costs.
 * Fitting a line through two block sizes (what `read_path_ab.py --sweep` does
 * from outside) infers the same split, but infers it — this measures the
 * intercept directly, and the two disagreeing is itself a finding.
 *
 * Each arm reports three numbers, and only the first is comparable ACROSS
 * kernels:
 *
 *   batch  one clock_gettime pair around `iters` reads. Nothing but reads is in
 *          the loop, so this is the per-read cost on any kernel.
 *   timed  the same loop with a clock_gettime pair around EACH read. On Linux
 *          that is a vDSO call and costs tens of ns; on Akuma it is a real
 *          syscall, so `timed - batch` is roughly what two `clock_gettime`s
 *          cost there — worth knowing, but it is not part of `read`.
 *   min    the cheapest single read seen, from the `timed` loop. A preempted
 *          read is hundreds of microseconds and no averaging removes it
 *          (`src/read_profile.rs` § STAGE_MIN), so the minimum is the only
 *          interference-free per-read figure the harness can produce — but it
 *          is floored by the clock's resolution, which on Akuma is 1 us.
 *
 * Build (Linux):  gcc -O2 -static -o read_syscall_cost read_syscall_cost.c
 * Build (Akuma):  see scripts/probes/build_read_syscall_cost.sh
 */
#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

static long long now_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (long long)ts.tv_sec * 1000000000LL + ts.tv_nsec;
}

static int cmp_ll(const void *a, const void *b) {
    long long x = *(const long long *)a, y = *(const long long *)b;
    return (x > y) - (x < y);
}

/* One pass of `iters` reads; returns ns/read and updates *minp with the
 * cheapest single read seen. Timing every read individually is what makes the
 * minimum available; the per-call clock_gettime pair is charged to every arm
 * alike and cancels in the differences. */
/* Batched pass: ONE `clock_gettime` pair around the whole loop.
 *
 * The per-call variant below charges two `clock_gettime`s to every read. That is
 * a rounding error on Linux (vDSO, tens of ns) and it is the entire measurement
 * on Akuma, which has no vDSO — `clock_gettime` is a real `svc` there, and the
 * first run of this probe reported a **12 us** 0-byte read for that reason
 * alone, against 1.6 us of actual in-kernel excursion. Batching is the only arm
 * whose number means the same thing on both kernels. */
static double pass_batched(int fd, char *buf, size_t len, int iters,
                           int use_pread, off_t span) {
    off_t nslots = (len > 0 && span > (off_t)len) ? span / (off_t)len : 1;
    if (!use_pread && lseek(fd, 0, SEEK_SET) < 0) { perror("lseek"); exit(1); }
    long long t0 = now_ns();
    for (int i = 0; i < iters; i++) {
        off_t off = (off_t)len * (off_t)(i % nslots);
        /* The rewind is outside the timed region only for the first slot; a wrap
         * mid-loop costs one lseek per `nslots` reads, which at the sizes here
         * is at most one in two thousand. */
        if (!use_pread && i && i % nslots == 0 && lseek(fd, 0, SEEK_SET) < 0) {
            perror("lseek"); exit(1);
        }
        ssize_t n = use_pread ? pread(fd, buf, len, off) : read(fd, buf, len);
        if (n < 0 || (size_t)n != len) { perror("read"); exit(1); }
    }
    return (double)(now_ns() - t0) / iters;
}

static double pass(int fd, char *buf, size_t len, int iters, long long *minp,
                   int use_pread, off_t span) {
    long long total = 0;
    /* Sequential mode owns the fd offset, so it must rewind before the loop and
     * whenever it wraps — exactly like `pass_batched`. Without the rewind here
     * this arm inherited whatever offset the previous arm left and walked off
     * the end mid-loop, which surfaced as a "short read" that looked like a
     * kernel bug and was not one. */
    if (!use_pread && lseek(fd, 0, SEEK_SET) < 0) { perror("lseek"); exit(1); }
    /* Offsets wrap inside the file. Walking straight off the end instead makes
     * `pread` return 0 immediately, and a run of instant EOF returns reads as a
     * spectacularly fast `read(2)` — the first version of this probe reported
     * 131 ns for a 64 KB read that way, below its own measured minimum, which is
     * the tell. Every read here must move `len` bytes or the arm is void. */
    off_t nslots = (len > 0 && span > (off_t)len) ? span / (off_t)len : 1;
    for (int i = 0; i < iters; i++) {
        off_t off = (off_t)len * (off_t)(i % nslots);
        if (!use_pread && i && i % nslots == 0 && lseek(fd, 0, SEEK_SET) < 0) {
            perror("lseek"); exit(1);
        }
        long long t0 = now_ns();
        ssize_t n = use_pread ? pread(fd, buf, len, off) : read(fd, buf, len);
        long long d = now_ns() - t0;
        if (n < 0) { perror("read"); exit(1); }
        if ((size_t)n != len) {
            fprintf(stderr, "short read: wanted %zu got %zd at iter %d (%s off %lld)\n",
                    len, n, i, use_pread ? "pread" : "seq", (long long)off);
            exit(1);
        }
        total += d;
        if (d < *minp) *minp = d;
    }
    return (double)total / iters;
}

static void arm(const char *name, int fd, char *buf, size_t len, int iters, int reps,
                int use_pread, off_t span) {
    long long mn = (long long)1 << 62;
    long long *batch = malloc(sizeof(long long) * reps);
    long long *each = malloc(sizeof(long long) * reps);
    for (int r = 0; r < reps; r++) {
        batch[r] = (long long)(pass_batched(fd, buf, len, iters, use_pread, span) * 1000);
        each[r] = (long long)(pass(fd, buf, len, iters, &mn, use_pread, span) * 1000);
    }
    qsort(batch, reps, sizeof(long long), cmp_ll);
    qsort(each, reps, sizeof(long long), cmp_ll);
    /* `batch` is the number to compare across kernels; `timed` and `min` carry
     * the per-call harness and are only meaningful within one kernel. */
    printf("%-6s len=%-6zu batch=%8.0f ns/read   timed=%8.0f   min=%7lld   (%d x %d)\n",
           name, len, batch[reps / 2] / 1000.0, each[reps / 2] / 1000.0, mn, reps, iters);
    free(batch);
    free(each);
}

int main(int argc, char **argv) {
    const char *path = argc > 1 ? argv[1] : "/tmp/rsc.bin";
    int iters = argc > 2 ? atoi(argv[2]) : 2000;
    int reps = argc > 3 ? atoi(argv[3]) : 5;
    static const size_t sizes[] = {0, 4096, 8192, 65536};

    int fz = open("/dev/zero", O_RDONLY);
    int ff = open(path, O_RDONLY);
    if (ff < 0) { perror(path); return 1; }
    off_t span = lseek(ff, 0, SEEK_END);
    if (span <= 0) { fprintf(stderr, "%s is empty\n", path); return 1; }
    if (lseek(ff, 0, SEEK_SET) < 0) { perror("lseek"); return 1; }

    char *buf = malloc(65536);
    memset(buf, 0, 65536); /* fault the buffer in before timing anything */

    /* Warm the page/block cache for the file: an uncached first pass would be
     * measuring the disk, which is a different question. */
    for (int i = 0; i < 4; i++) {
        long long mn = (long long)1 << 62;
        pass(ff, buf, 65536, (int)(span / 65536), &mn, 1, span);
    }

    printf("== fixed cost (no bytes moved)\n");
    arm("null", ff, buf, 0, iters, reps, 1, span);
    if (fz >= 0) {
        printf("== /dev/zero (syscall + kernel fill, no filesystem)\n");
        for (unsigned i = 1; i < sizeof(sizes) / sizeof(sizes[0]); i++)
            arm("zero", fz, buf, sizes[i], iters, reps, 0, span);
    }
    printf("== warm file, pread (%s, %lld bytes)\n", path, (long long)span);
    for (unsigned i = 1; i < sizeof(sizes) / sizeof(sizes[0]); i++)
        arm("file", ff, buf, sizes[i], iters, reps, 1, span);

    /* Same file through `read(2)` rather than `pread(2)`. Akuma implements the
     * two in separate functions (`sys_read`'s File arm vs `sys_pread64`), and
     * only `sys_read` carries the `read-profile` instrumentation — so this arm
     * is the one whose calls the kernel's own `[READPROF]` windows describe.
     * On Linux the two share `vfs_read` and should agree; a gap here is
     * therefore itself informative. */
    printf("== warm file, read (sequential)\n");
    for (unsigned i = 1; i < sizeof(sizes) / sizeof(sizes[0]); i++)
        arm("fread", ff, buf, sizes[i], iters, reps, 0, span);
    return 0;
}
