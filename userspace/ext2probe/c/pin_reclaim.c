/* pin_reclaim — does `unlink` of a MAPPED file return its blocks?
 *
 * The C restatement of `userspace/ext2probe/src/main.rs`'s `reclaim_pinned`
 * phase, and it exists because that probe is a `libakuma` binary and `libakuma`
 * does not build for x86_64 — so the one measurement that distinguishes the two
 * kernels could not be run on the one that was failing it.
 *
 * Static musl, two syscalls' worth of libc, so the SAME binary runs on Akuma
 * (either architecture) and on real Linux as the reference arm.
 *
 * # What it measures
 *
 * A file mapping takes an `InodePin` on the inode, and ext2 will not free a
 * pinned inode's blocks on `unlink` — it defers them, which is correct, because
 * the mapping is still reading them. The pin must therefore be *released* when
 * the mapping goes away. If `munmap` does not release it, the deferral never
 * drains and the blocks never come back.
 *
 * The sequence, and every step is load-bearing:
 *
 *   1. create N files, N deliberately **greater than the 1024-slot pin table**.
 *      Past saturation `is_pinned` answers `true` for every inode, which is what
 *      turns a leak of one file's blocks into a filesystem that stops freeing
 *      anything at all — the difference between a bug and an outage.
 *   2. `mmap` each one file-backed and `close` the fd, so the mapping is the
 *      only thing holding the inode.
 *   3. `unlink` all of them while mapped, so every free is deferred.
 *   4. `munmap` everything, which is what must drop the pins.
 *   5. touch and remove one more file, to give the deferral list a drain to run
 *      on, then read `statfs` and compare.
 *
 * A healthy filesystem returns ~all of it; `RECLAIM_OK_PCT` is 80 rather than
 * 100 because directory blocks and group metadata move a little in both
 * directions. Leaking returns ~0.
 *
 * # Reading the result
 *
 * Two phases print, and they fail for different reasons. `unpinned` is the
 * control: nothing maps those files, so `release_last_link` takes its immediate
 * path and this must reclaim regardless of anything to do with pins. If the
 * control leaks too, the defect is not the pin/deferral interaction and this
 * probe is pointing at the wrong thing.
 *
 *   PIN_RECLAIM: unpinned consumed=... returned=... 9x%  OK
 *   PIN_RECLAIM: pinned   consumed=... returned=... 0%   LEAK   <- the bug
 *
 * Background: `docs/archive/EXT2_UNLINK_INODE_BLOCK_LEAK.md` (the AArch64 original) and
 * `docs/archive/AKUMA_AMD64_SELFHOST_BUILD_SLOWNESS.md` §6.
 */
#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <unistd.h>

/* > the kernel's 1024 pin slots on purpose: the table must saturate. */
#define PINNED_FILES 1200
#define PINNED_SIZE (16 * 1024)

/* The control phase moves enough bytes for statfs to resolve the difference. */
#define PLAIN_FILES 64
#define PLAIN_SIZE (256 * 1024)

#define RECLAIM_OK_PCT 80

/* Sized by the LARGER of the two phases. It was sized by `PINNED_SIZE` alone,
 * and the control phase then wrote `PLAIN_SIZE` bytes out of a 16 KB buffer: a
 * 256 KB overread, whose `write` the kernel rejected. Every control-phase file
 * was empty, `consumed` came out 0, and the phase scored INCONCLUSIVE — a probe
 * bug that looked exactly like a filesystem that had stopped allocating. */
#define BUF_SIZE (PLAIN_SIZE > PINNED_SIZE ? PLAIN_SIZE : PINNED_SIZE)
static char buf[BUF_SIZE];

/* Free bytes on the filesystem holding `path`, or 0 if statfs cannot say. */
static unsigned long long free_bytes(const char *path) {
    struct statfs s;
    if (statfs(path, &s) != 0) {
        return 0;
    }
    return (unsigned long long)s.f_bfree * (unsigned long long)s.f_bsize;
}

static void make(const char *dir, int i, size_t size) {
    char p[320];
    snprintf(p, sizeof p, "%s/p%d", dir, i);
    int fd = open(p, O_CREAT | O_WRONLY | O_TRUNC, 0644);
    if (fd < 0) {
        return;
    }
    /* A short or failed write makes the phase measure nothing, which scores
     * INCONCLUSIVE rather than passing — but it is worth saying which file and
     * why, once, because the two look identical from the summary line. */
    ssize_t n = write(fd, buf, size);
    if (n != (ssize_t)size) {
        static int complained;
        if (!complained) {
            complained = 1;
            printf("PIN_RECLAIM: short write on %s: %zd of %zu\n", p, n, size);
        }
    }
    close(fd);
}

static void drop(const char *dir, int i) {
    char p[320];
    snprintf(p, sizeof p, "%s/p%d", dir, i);
    unlink(p);
}

/* One phase's verdict. `INCONCLUSIVE` is **not** a pass: a probe whose setup
 * silently did nothing consumes no space and would otherwise score 0-of-0 as a
 * clean reclaim — which is how a broken harness reports a kernel it never
 * tested. `scripts/mem_suite.py` refuses a silent probe for the same reason. */
enum verdict { LEAK = 0, OK = 1, INCONCLUSIVE = 2 };

static const char *verdict_name(enum verdict v) {
    return v == OK ? "OK" : v == LEAK ? "LEAK" : "INCONCLUSIVE";
}

/* Report one create-then-delete cycle. */
static enum verdict report(const char *label, unsigned long long before,
                           unsigned long long mid, unsigned long long after) {
    unsigned long long consumed = before > mid ? before - mid : 0;
    unsigned long long returned = after > mid ? after - mid : 0;
    printf("PIN_RECLAIM: %-8s consumed=%lluKB returned=%lluKB", label,
           consumed / 1024, returned / 1024);
    if (consumed == 0) {
        printf(" INCONCLUSIVE (create consumed no measurable space)\n");
        return INCONCLUSIVE;
    }
    long long pct = (long long)(returned * 100 / consumed);
    printf(" %lld%% %s\n", pct, pct >= RECLAIM_OK_PCT ? "OK" : "LEAK");
    return pct >= RECLAIM_OK_PCT ? OK : LEAK;
}

/* Control: nothing maps these, so the free is immediate. */
static enum verdict unpinned(const char *root) {
    char dir[128];
    snprintf(dir, sizeof dir, "%s/unpinned", root);
    mkdir(dir, 0755);
    unsigned long long before = free_bytes(root);
    for (int i = 0; i < PLAIN_FILES; i++) {
        make(dir, i, PLAIN_SIZE);
    }
    unsigned long long mid = free_bytes(root);
    for (int i = 0; i < PLAIN_FILES; i++) {
        drop(dir, i);
    }
    rmdir(dir);
    return report("unpinned", before, mid, free_bytes(root));
}

/* The subject: unlink while mapped, then drop the mappings. */
static enum verdict pinned(const char *root) {
    char dir[128];
    snprintf(dir, sizeof dir, "%s/pinned", root);
    mkdir(dir, 0755);
    unsigned long long before = free_bytes(root);

    for (int i = 0; i < PINNED_FILES; i++) {
        make(dir, i, PINNED_SIZE);
    }
    unsigned long long mid = free_bytes(root);

    /* Map each, then close the fd: the mapping alone holds the inode. */
    static void *maps[PINNED_FILES];
    int mapped = 0;
    for (int i = 0; i < PINNED_FILES; i++) {
        char p[320];
        snprintf(p, sizeof p, "%s/p%d", dir, i);
        int fd = open(p, O_RDONLY);
        if (fd < 0) {
            continue;
        }
        void *a = mmap(NULL, PINNED_SIZE, PROT_READ, MAP_PRIVATE, fd, 0);
        close(fd);
        if (a != MAP_FAILED) {
            maps[i] = a;
            mapped++;
        }
    }
    printf("PIN_RECLAIM: mapped %d of %d files (pin table has 1024 slots)\n",
           mapped, PINNED_FILES);

    /* Unlink while mapped -> every free defers. */
    for (int i = 0; i < PINNED_FILES; i++) {
        drop(dir, i);
    }
    /* Drop the pins. This is the step the kernel was not honouring. */
    for (int i = 0; i < PINNED_FILES; i++) {
        if (maps[i]) {
            munmap(maps[i], PINNED_SIZE);
        }
    }
    /* Give the deferral list a filesystem operation to drain on. */
    char drain[320];
    snprintf(drain, sizeof drain, "%s/drain", dir);
    int fd = open(drain, O_CREAT | O_WRONLY | O_TRUNC, 0644);
    if (fd >= 0) {
        ssize_t n = write(fd, "x", 1);
        (void)n;
        close(fd);
    }
    unlink(drain);
    rmdir(dir);

    return report("pinned", before, mid, free_bytes(root));
}

int main(int argc, char **argv) {
    const char *root = argc > 1 ? argv[1] : "/tmp";
    memset(buf, 'p', sizeof buf);
    printf("PIN_RECLAIM: start root=%s\n", root);
    enum verdict v_unpinned = unpinned(root);
    enum verdict v_pinned = pinned(root);
    /* Both verdicts, separately: see the header on why the control matters. */
    printf("PIN_RECLAIM: done unpinned=%s pinned=%s\n", verdict_name(v_unpinned),
           verdict_name(v_pinned));
    /* Only OK/OK is a pass. INCONCLUSIVE exits non-zero too: it means the probe
     * did not run, which a caller must not read as a clean kernel. */
    return (v_unpinned == OK && v_pinned == OK) ? 0 : 1;
}
