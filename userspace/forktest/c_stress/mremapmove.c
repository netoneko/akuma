// mremapmove — does `mremap`'s payload move copy the WHOLE region?
//
// The regression probe docs/archive/USER_COPY_FOLD.md §5 asked for and item 2 of
// its §11 left open.
//
// Akuma's `sys_mremap` moves a mapping by allocating a new one and copying the
// bytes across. It validated the SOURCE pointer and never the destination, so the
// copy-out went through the raw helper with no check and no prefault: the first
// still-lazy page in the fresh destination faulted, the byte loop `break`ed, and
// the moved region was **silently truncated**. A `break` is indistinguishable from
// completion at the call site, so nothing failed loudly — hence this probe.
//
// The destination of a `MREMAP_MAYMOVE` grow is a brand-new anonymous mapping and
// is therefore entirely lazy, which is exactly the case no existing test covered.
// The regions here are megabytes so the truncation point is far past the first
// page: a fix that only prefaults the head would still fail phase 1.
//
// Calibrate it on real Linux before believing a FAIL here — every FAIL on Linux
// means the probe is wrong, not the kernel (same rule as `madvshared`):
//   docker run --rm --platform linux/arm64 -v "$PWD:/w:ro" alpine /w/mremapmove
//
// Phases:
//   1. grow+move a fully-resident 4 MB region -> every old byte must survive
//   2. the grown tail must read back as zero (no stale bytes exposed)
//   3. grow+move a SPARSELY touched region -> written pages must survive and
//      never-touched pages must still read zero (the source side of lazy)

#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/mman.h>

#define PAGE 4096
#define OLD_LEN (4u * 1024 * 1024)
#define NEW_LEN (8u * 1024 * 1024)

static int failures = 0;

// Varies with both the page index and the offset within the page, so a page left
// zero, a page copied from the wrong source page, and a short copy are all
// distinguishable — and never zero, so "untouched" is never mistaken for "copied".
static unsigned char pat(size_t i)
{
    return (unsigned char)(((i >> 12) * 31u + (i & 0xffu)) | 1u);
}

static void *fresh(size_t len)
{
    void *p = mmap(NULL, len, PROT_READ | PROT_WRITE,
                   MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (p == MAP_FAILED) {
        perror("mmap");
        exit(2);
    }
    return p;
}

// Report the FIRST divergence rather than a count: for a truncation the first
// offset IS the answer — it names the page the copy stopped at.
static void check_range(const char *phase, const unsigned char *p,
                        size_t from, size_t to, int expect_pattern)
{
    for (size_t i = from; i < to; i++) {
        unsigned char want = expect_pattern ? pat(i) : 0;
        if (p[i] != want) {
            printf("mremapmove: FAIL %s at offset %zu (page %zu): got 0x%02x want 0x%02x\n",
                   phase, i, i / PAGE, p[i], want);
            failures++;
            return;
        }
    }
    printf("mremapmove: PASS %s (%zu bytes)\n", phase, to - from);
}

int main(void)
{
    // ── Phase 1+2: fully-resident source, grow with MAYMOVE ──────────────────
    unsigned char *p = fresh(OLD_LEN);
    for (size_t i = 0; i < OLD_LEN; i++)
        p[i] = pat(i);

    unsigned char *q = mremap(p, OLD_LEN, NEW_LEN, MREMAP_MAYMOVE);
    if (q == MAP_FAILED) {
        perror("mremap(grow, MAYMOVE)");
        return 2;
    }
    check_range("resident-grow", q, 0, OLD_LEN, 1);
    check_range("grown-tail-zero", q, OLD_LEN, NEW_LEN, 0);
    munmap(q, NEW_LEN);

    // ── Phase 3: sparsely-touched source ─────────────────────────────────────
    // Only every 4th page is written, so three quarters of the SOURCE are still
    // lazy when the move runs. Both halves have to come out right: a written page
    // must arrive intact, an untouched one must still read zero.
    unsigned char *s = fresh(OLD_LEN);
    for (size_t off = 0; off < OLD_LEN; off += 4u * PAGE)
        for (size_t i = off; i < off + PAGE; i++)
            s[i] = pat(i);

    unsigned char *t = mremap(s, OLD_LEN, NEW_LEN, MREMAP_MAYMOVE);
    if (t == MAP_FAILED) {
        perror("mremap(sparse grow, MAYMOVE)");
        return 2;
    }
    for (size_t off = 0; off < OLD_LEN; off += 4u * PAGE) {
        for (size_t i = off; i < off + PAGE; i++) {
            if (t[i] != pat(i)) {
                printf("mremapmove: FAIL sparse-written at offset %zu (page %zu): got 0x%02x want 0x%02x\n",
                       i, i / PAGE, t[i], pat(i));
                failures++;
                goto sparse_done;
            }
        }
        for (size_t i = off + PAGE; i < off + 4u * PAGE && i < OLD_LEN; i++) {
            if (t[i] != 0) {
                printf("mremapmove: FAIL sparse-untouched at offset %zu (page %zu): got 0x%02x want 0x00\n",
                       i, i / PAGE, t[i]);
                failures++;
                goto sparse_done;
            }
        }
    }
    printf("mremapmove: PASS sparse-grow (%u bytes, 1 page in 4 written)\n", OLD_LEN);
sparse_done:
    munmap(t, NEW_LEN);

    if (failures == 0) {
        printf("mremapmove: ALL PASS\n");
        return 0;
    }
    printf("mremapmove: %d FAILURES\n", failures);
    return 1;
}
