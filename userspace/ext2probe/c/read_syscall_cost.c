/*
 * read_syscall_cost — what one `read(2)` costs, split into fixed and per-byte.
 *
 * Built for BOTH kernels from this one source (musl static, aarch64), so an
 * Akuma number and a Linux number differ by the kernel and nothing else — the
 * same instruction stream issues the same `svc` on the same silicon. See
 * `docs/archive/EXT2_READ_PATH_STAGE_PROFILE.md`.
 *
 * Lives next to `ext2probe` because it answers the question `ext2probe` raises
 * and cannot settle: `ext2probe` times a whole workload from userspace, so its
 * `seq_read` number is syscall + userspace + scheduling together. This splits
 * the syscall out, and the kernel-side companion (`--features read-profile`)
 * splits that syscall into stages. Three tools, three altitudes, one question.
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
 *          (`src/syscall/utils/read_profile.rs` § STAGE_MIN), so the minimum is the only
 *          interference-free per-read figure the harness can produce — but it
 *          is floored by the clock's resolution, which on Akuma is 1 us.
 *
 * Built by `userspace/build.sh` into `bootstrap/bin/`, so a populated disk has
 * it at /bin/read_syscall_cost. `userspace/ext2probe/c/build.sh` builds it
 * standalone and can push it into an already-running Akuma or Lima guest.
 */
#define _GNU_SOURCE
#include <fcntl.h>
#include <sys/syscall.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
#include <stdint.h>

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
    /* Many short passes for the batched arm — see the `getpid` block in main().
     * A long pass reliably catches one of the multi-hundred-microsecond stalls
     * that land a few times per thousand syscalls, so with long passes even the
     * minimum measures interference. Same total work, hundredfold better floor. */
    int npass = reps * 20, per = iters / 20 > 0 ? iters / 20 : 1;
    long long *batch = malloc(sizeof(long long) * npass);
    long long *each = malloc(sizeof(long long) * reps);
    for (int r = 0; r < npass; r++)
        batch[r] = (long long)(pass_batched(fd, buf, len, per, use_pread, span) * 1000);
    for (int r = 0; r < reps; r++)
        each[r] = (long long)(pass(fd, buf, len, iters, &mn, use_pread, span) * 1000);
    qsort(batch, npass, sizeof(long long), cmp_ll);
    qsort(each, reps, sizeof(long long), cmp_ll);
    /* `batch` is the number to compare across kernels; `timed` and `min` carry
     * the per-call harness and are only meaningful within one kernel.
     *
     * `best` — the cheapest whole pass — is what a kernel A/B should read. This
     * host's wall clock moves several-fold between boots, and a kernel A/B
     * cannot interleave its arms (each needs a reboot), so a median across
     * passes inside one boot still carries whatever the host was doing during
     * that boot. The minimum pass is the least-disturbed one, and interference
     * can only ever add. A 17% difference measured on medians here reversed
     * sign on the arms either side of it. */
    printf("%-6s len=%-6zu median=%8.0f ns/read  best=%8.0f  timed=%8.0f  (%d x %d)\n",
           name, len, batch[npass / 2] / 1000.0, batch[0] / 1000.0,
           each[reps / 2] / 1000.0, npass, per);
    (void)mn;
    free(batch);
    free(each);
}

/* The floor: a syscall that does no work at all.
 *
 * `getpid` reaches the dispatch table and returns a field. If this costs the
 * same as a 0-byte `read`, then nothing in the read path is worth looking at
 * and the cost is the EL0 round trip — the `svc`, the vector asm's register
 * save/restore, and the `eret`. Raw `syscall()` rather than the libc wrapper so
 * no libc can cache it. */
static double pass_syscall(long nr, int iters) {
    long long t0 = now_ns();
    for (int i = 0; i < iters; i++) syscall(nr);
    return (double)(now_ns() - t0) / iters;
}

/* Same, for a syscall that needs one argument. `uname` is the reason: it is the
 * only other call that reports the build identity, so it is the natural
 * comparison for `akuma_version` — but it cannot be timed by `floor_arm`,
 * because it writes a 390-byte `struct utsname` and needs somewhere to put it. */
static double pass_syscall1(long nr, long a0, int iters) {
    long long t0 = now_ns();
    for (int i = 0; i < iters; i++) syscall(nr, a0);
    return (double)(now_ns() - t0) / iters;
}

static void floor_arm1(const char *name, long nr, long a0, int iters, int reps) {
    int npass = reps * 20, per = iters / 20 > 0 ? iters / 20 : 1;
    long long *g = malloc(sizeof(long long) * npass);
    for (int r = 0; r < npass; r++) g[r] = (long long)(pass_syscall1(nr, a0, per) * 1000);
    qsort(g, npass, sizeof(long long), cmp_ll);
    printf("%-8s nr=%-4ld median=%8.0f ns/call  best=%8.0f  (%d x %d)\n",
           name, nr, g[npass / 2] / 1000.0, g[0] / 1000.0, npass, per);
    free(g);
}

/* Many short passes, take the minimum — see the comment in main(). */
static void floor_arm(const char *name, long nr, int iters, int reps) {
    int npass = reps * 20, per = iters / 20 > 0 ? iters / 20 : 1;
    long long *g = malloc(sizeof(long long) * npass);
    for (int r = 0; r < npass; r++) g[r] = (long long)(pass_syscall(nr, per) * 1000);
    qsort(g, npass, sizeof(long long), cmp_ll);
    printf("%-8s nr=%-4ld median=%8.0f ns/call  best=%8.0f  (%d x %d)\n",
           name, nr, g[npass / 2] / 1000.0, g[0] / 1000.0, npass, per);
    free(g);
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

    printf("== floor: syscalls that do (almost) nothing\n");
    /* MANY SHORT passes, not few long ones, and report the minimum.
     *
     * Interference here arrives as a handful of multi-hundred-microsecond
     * stalls per thousand calls. A 2000-call pass almost always catches one, so
     * every pass is contaminated and even the minimum is an interference
     * measurement — which is how this arm first reported a 3.0 us `getpid`.
     * Short passes make a clean pass likely, and the cheapest of a hundred of
     * them is the syscall itself. Same total work either way.
     *
     * Three numbers, not one: if they disagree, the cheap ones are taking a
     * path the dispatcher's common prologue does not. `getppid` is the control
     * for `getpid`; 4095 is deliberately not a syscall, so it must traverse the
     * whole dispatch and return ENOSYS. */
    floor_arm("getpid", SYS_getpid, iters, reps);
    floor_arm("getppid", SYS_getppid, iters, reps);
    /* An unimplemented syscall, which is what a libc probing for a feature and
     * falling back actually does. 107 is `timer_create`: allocated in the
     * aarch64 ABI, not implemented by this kernel, so it reaches the
     * dispatcher's ENOSYS arm.
     *
     * The number matters. An earlier version used 4095 on the reasoning that
     * "not a syscall" was the cleanest possible case, and measured 2 ms — which
     * was then attributed to the ENOSYS console print. Wrong path entirely:
     * `src/exceptions.rs` treats **any number above 500** as a stale-I-cache
     * JIT artifact and answers with `ic iallu` + an instruction replay, not with
     * ENOSYS. The 4095 arm below still measures that, deliberately, because it
     * is worth knowing what the JIT band costs — but it is not the ENOSYS
     * number and must never be read as one. */
    floor_arm("ENOSYS", 107, iters, reps);

    /* THE floor, and the reason the three arms above are not it.
     *
     * `getpid` has been standing in for "a syscall that does nothing", but its
     * arm still resolves a process, so what it measures is the boundary PLUS an
     * arm. Akuma-private 328 (`akuma_get_version`) reads no arguments, touches
     * no user memory and resolves nothing: it returns a compile-time constant
     * packed into x0. Its cost is the EL0 round trip, the wrapper layer and
     * `handle_syscall`'s prologue/epilogue, and nothing else — which is exactly
     * the quantity docs/archive/AKUMA_SYSCALL_PERFORMANCE_AUDIT.md is pricing.
     *
     * `getpid - version` is therefore what `getpid`'s own arm costs, measured
     * rather than assumed. On a Linux guest 328 is not a syscall, so this arm
     * reports that kernel's ENOSYS path and must not be compared across kernels.
     *
     * `uname` is the control that makes the point: it reports the SAME build
     * identity from the same compile-time constants and computes nothing, but
     * it has to deliver it through a 390-byte `struct utsname`. The gap between
     * the two arms is what the ABI's shape costs, with the work held at zero. */
    floor_arm("version", 328, iters, reps);
    static char uts[390];
    floor_arm1("uname", SYS_uname, (long)(uintptr_t)uts, iters, reps);

    /* LAST, and that placement is the whole point. This arm is not a syscall:
     * `src/exceptions.rs` treats any number above 500 as a stale-I-cache JIT
     * artifact and answers with `ic iallu` + an instruction replay. On QEMU
     * `ic iallu` calls tb_flush(), so 10,000 calls here throw away the entire
     * instruction cache and translation buffer 10,000 times.
     *
     * It used to run fourth, and every arm after it measured a cold machine:
     * `version` — an arm that returns a constant — read 290-410 ns against
     * `getpid`'s 160-200, which looks exactly like a dispatch finding and is
     * not one. Anything that flushes global state belongs at the end of a
     * measurement, not in the middle of one. */
    floor_arm("JITband", 4095, iters, reps);

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
