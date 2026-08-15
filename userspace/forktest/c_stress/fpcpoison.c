/*
 * fpcpoison.c — cross-process integrity probe for the shared file-page cache.
 *
 * WHY THIS EXISTS, and why mmapsum.c does not already cover it
 * -----------------------------------------------------------
 * `src/file_page_cache.rs` deduplicates read-only file-backed pages on
 * `(inode, file_offset)`, so the FIRST process to fault a page fills a frame and
 * publishes it, and every later mapper — in any other process — gets that frame
 * as a hit without touching the disk. The failure mode that follows is therefore
 * invisible to a single-process test: one bad fill becomes a permanent, shared,
 * cross-process wrong page. mmapsum.c hashes one file from one process (its "mt"
 * arm uses threads, which share an address space and one mapping), so it cannot
 * see it. This forks real processes that map the same file at the same instant.
 *
 * The signature to look for is a page of ZEROS. Fill frames come from
 * `alloc_pages_zeroed`, so a `read_at` that errors or comes up short leaves the
 * page zeroed, and publishing it makes those zeros authoritative. In the guest
 * this surfaced as rustc ICEing while reading crate metadata:
 *
 *     decode error: Expected header tag [79, 68, 72, 84] but found [0, 0, 0, 0]
 *
 * ([79,68,72,84] is "ODHT".) Pair a FAIL here with the kernel's `[FILL-SHORT]`
 * line — that print names the `(inode, file_off)` that failed to fill, and this
 * probe names the file offset that came back wrong. They should match.
 *
 * METHOD
 *   1. Parent computes a per-page FNV-1a digest of the file via read(), the
 *      known-good VFS path, into a MAP_SHARED array (8 bytes per 4 KB page).
 *   2. Each round forks N children. All spin on a gate so they fault the same
 *      pages concurrently rather than one warming the cache for the rest.
 *   3. Each child mmaps the file FRESH (PROT_READ, MAP_PRIVATE), digests every
 *      page, and compares against the reference.
 *   4. A mismatching page is reported with its offset and whether it is entirely
 *      zeros — that is what separates this bug from generic corruption.
 *
 * Once a page is poisoned it STAYS poisoned until the inode is invalidated, so
 * expect a clean run to go all-clean and a tripped run to fail from some round
 * onward. The round number of the first failure is the interesting number.
 *
 * Calibrate on real Linux arm64 first: it must print ALL PASS there. A FAIL in
 * the guest is the kernel.
 *
 * Static, musl, pure C — kernel-fault attribution, no runtime suspects.
 * Build: aarch64-linux-musl-gcc -static -O2 -Wall -Wextra -o fpcpoison fpcpoison.c
 * Usage: fpcpoison <path> [rounds] [nprocs]     (defaults: 20 rounds, 4 procs)
 *
 * Pick a big read-only file that is not otherwise busy — in the guest,
 * /usr/local/bin/rustc or a .rlib under ~/.cargo is the shape that actually broke.
 */

#include <fcntl.h>
#include <stdio.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

#define PAGE 4096u

/* Spin budget for the start gate before a child gives up and proceeds anyway.
 * Large enough that the gate does its job on a loaded guest, small enough that a
 * dead parent costs seconds rather than a wedged core. */
#define GATE_SPIN_LIMIT 200000000L

static uint64_t fnv1a(const unsigned char *p, size_t n) {
    uint64_t h = 0xcbf29ce484222325ULL;
    for (size_t i = 0; i < n; i++) {
        h ^= p[i];
        h *= 0x100000001b3ULL;
    }
    return h;
}

static int all_zero(const unsigned char *p, size_t n) {
    for (size_t i = 0; i < n; i++) {
        if (p[i]) return 0;
    }
    return 1;
}

/* Shared control block: the gate the children start on, plus the tally. */
struct ctl {
    volatile int gate;      /* parent flips to 1 to release the round      */
    volatile int ready;     /* children increment as they reach the gate   */
    volatile int bad_pages; /* total mismatching pages seen this round     */
    volatile int zero_pages;/* subset of the above that were entirely zero */
    volatile long first_bad;/* offset of the lowest bad page, -1 if none   */
};

/* Digest the file through read(), one page at a time. */
static int build_reference(const char *path, uint64_t *ref, size_t npages, size_t size) {
    int fd = open(path, O_RDONLY);
    if (fd < 0) { perror("open(reference)"); return -1; }
    unsigned char buf[PAGE];
    for (size_t i = 0; i < npages; i++) {
        size_t want = (i + 1) * PAGE <= size ? PAGE : size - i * PAGE;
        size_t done = 0;
        while (done < want) {
            ssize_t r = pread(fd, buf + done, want - done, (off_t)(i * PAGE + done));
            if (r < 0) { perror("pread(reference)"); close(fd); return -1; }
            if (r == 0) break;
            done += (size_t)r;
        }
        if (done != want) {
            fprintf(stderr, "reference read short at page %zu: %zu/%zu\n", i, done, want);
            close(fd);
            return -1;
        }
        ref[i] = fnv1a(buf, want);
    }
    close(fd);
    return 0;
}

/* One child: map the file fresh and verify every page against the reference. */
static void child_pass(const char *path, const uint64_t *ref, size_t npages,
                       size_t size, struct ctl *c) {
    int fd = open(path, O_RDONLY);
    if (fd < 0) { perror("open(child)"); _exit(2); }
    unsigned char *m = mmap(NULL, size, PROT_READ, MAP_PRIVATE, fd, 0);
    close(fd);
    if (m == MAP_FAILED) { perror("mmap(child)"); _exit(2); }

    /* Line every child up on the same instant so they race for the same pages.
     * BOUNDED: an unbounded spin here means that killing the parent leaves every
     * child burning a core forever, which wedged a guest during development. The
     * gate is a latency optimisation, not a correctness requirement — timing out
     * and proceeding just makes this child's pass less concurrent. */
    __sync_fetch_and_add(&c->ready, 1);
    for (long spins = 0; !c->gate; spins++) {
        if (spins > GATE_SPIN_LIMIT) {
            if (getppid() == 1) _exit(3);   /* parent died — do not spin on */
            break;                          /* proceed un-gated rather than hang */
        }
        __asm__ __volatile__("yield" ::: "memory");
    }

    int bad = 0;
    for (size_t i = 0; i < npages; i++) {
        size_t want = (i + 1) * PAGE <= size ? PAGE : size - i * PAGE;
        const unsigned char *p = m + i * PAGE;
        if (fnv1a(p, want) == ref[i]) continue;
        bad++;
        __sync_fetch_and_add(&c->bad_pages, 1);
        int z = all_zero(p, want);
        if (z) __sync_fetch_and_add(&c->zero_pages, 1);
        long off = (long)(i * PAGE);
        /* Keep the lowest offending offset; racy CAS loop is fine, it is a hint. */
        for (;;) {
            long cur = c->first_bad;
            if (cur >= 0 && cur <= off) break;
            if (__sync_bool_compare_and_swap(&c->first_bad, cur, off)) break;
        }
        if (bad <= 4) {
            fprintf(stderr,
                    "  pid=%d BAD page @ file_off=0x%lx (%zu bytes) %s\n",
                    (int)getpid(), off, want,
                    z ? "ENTIRELY ZERO  <-- file-page-cache poisoning signature"
                      : "wrong content");
        }
    }
    munmap(m, size);
    _exit(bad ? 1 : 0);
}

int main(int argc, char **argv) {
    if (argc < 2) {
        fprintf(stderr, "usage: %s <path> [rounds] [nprocs]\n", argv[0]);
        return 2;
    }
    const char *path = argv[1];
    int rounds = argc > 2 ? atoi(argv[2]) : 20;
    int nprocs = argc > 3 ? atoi(argv[3]) : 4;
    if (rounds < 1) rounds = 1;
    if (nprocs < 1) nprocs = 1;

    struct stat st;
    if (stat(path, &st) != 0) { perror("stat"); return 2; }
    if (!S_ISREG(st.st_mode) || st.st_size <= 0) {
        fprintf(stderr, "%s: not a regular non-empty file\n", path);
        return 2;
    }
    size_t size = (size_t)st.st_size;
    size_t npages = (size + PAGE - 1) / PAGE;

    printf("fpcpoison: %s  size=%zu  pages=%zu  rounds=%d  procs=%d\n",
           path, size, npages, rounds, nprocs);

    uint64_t *ref = mmap(NULL, npages * sizeof(uint64_t), PROT_READ | PROT_WRITE,
                         MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (ref == MAP_FAILED) { perror("mmap(ref)"); return 2; }
    if (build_reference(path, ref, npages, size) != 0) return 2;
    printf("reference built via read() over %zu pages\n", npages);

    struct ctl *c = mmap(NULL, sizeof(*c), PROT_READ | PROT_WRITE,
                         MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (c == MAP_FAILED) { perror("mmap(ctl)"); return 2; }

    int failed_rounds = 0, first_fail_round = -1;
    long total_bad = 0, total_zero = 0;

    for (int r = 1; r <= rounds; r++) {
        c->gate = 0; c->ready = 0; c->bad_pages = 0; c->zero_pages = 0; c->first_bad = -1;

        for (int i = 0; i < nprocs; i++) {
            pid_t p = fork();
            if (p < 0) { perror("fork"); return 2; }
            if (p == 0) child_pass(path, ref, npages, size, c);
        }
        /* Release only once everyone has its mapping and is at the gate — bounded,
         * because a child that dies before reaching the gate (the very corruption
         * this hunts can kill one) would otherwise hang the parent forever. */
        for (long spins = 0; c->ready < nprocs; spins++) {
            if (spins > GATE_SPIN_LIMIT) {
                fprintf(stderr, "  warning: only %d/%d children reached the gate; "
                                "releasing anyway\n", c->ready, nprocs);
                break;
            }
            __asm__ __volatile__("yield" ::: "memory");
        }
        c->gate = 1;

        int bad_children = 0;
        for (int i = 0; i < nprocs; i++) {
            int ws = 0;
            if (wait(&ws) < 0) { perror("wait"); return 2; }
            if (!WIFEXITED(ws) || WEXITSTATUS(ws) != 0) bad_children++;
        }
        total_bad += c->bad_pages;
        total_zero += c->zero_pages;
        if (bad_children) {
            failed_rounds++;
            if (first_fail_round < 0) first_fail_round = r;
            printf("round %d: FAIL — %d/%d children bad, %d pages (%d entirely zero), "
                   "lowest bad file_off=0x%lx\n",
                   r, bad_children, nprocs, c->bad_pages, c->zero_pages, c->first_bad);
        }
    }

    printf("\n");
    if (failed_rounds == 0) {
        printf("ALL PASS — %d rounds x %d procs, no page diverged from read()\n",
               rounds, nprocs);
        return 0;
    }
    printf("FAIL — %d/%d rounds bad, first at round %d; %ld bad pages total, "
           "%ld of them entirely zero\n",
           failed_rounds, rounds, first_fail_round, total_bad, total_zero);
    if (total_zero > 0) {
        printf("Zero pages are the file-page-cache poisoning signature: correlate the "
               "offsets above with the kernel's [FILL-SHORT] lines.\n");
    }
    return 1;
}
