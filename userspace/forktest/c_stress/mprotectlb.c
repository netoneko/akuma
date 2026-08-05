/*
 * mprotectlb — does `mprotect` actually take effect on a page that is already
 * in the TLB?
 *
 * Deterministic probe (one mmap, one touch, one mprotect, one access — no
 * stress loop) for the 2026-08-05 defect in `flush_tlb_range`: it invalidated
 * with `tlbi vale1is, va>>12`, whose ASID field (operand bits [63:48]) is zero
 * for every user VA, while every user process runs under a *non-zero* ASID. The
 * invalidation matched nothing, so `sys_mprotect` — which publishes its PTE
 * edits through that function — could not downgrade a translation the CPU had
 * already cached.
 *
 * Both phases below are permission *downgrades* on a page that has been touched
 * first, which is the only shape that can catch it: an upgrade re-faults and
 * self-heals, and an untouched page has no cached entry to go stale.
 *
 * musl reaches this shape on every `pthread_create` (`mmap` the stack, then
 * `mprotect(guard_page, PROT_NONE)`) and every dynamic loader reaches it via
 * RELRO (`mprotect(GOT, PROT_READ)` after relocating).
 *
 * A correct kernel prints "0 divergence(s)". Calibrate on real Linux, where it
 * must also print 0:
 *   docker run --rm --platform linux/arm64 -v "$PWD/mprotectlb:/mprotectlb:ro" alpine /mprotectlb
 */
#define _GNU_SOURCE
#include <setjmp.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

#define MAGIC 0x5150524fu

static sigjmp_buf jb;
static volatile int faulted;

static void segv(int sig)
{
    (void)sig;
    faulted = 1;
    siglongjmp(jb, 1);
}

/* Returns 1 if the access faulted, 0 if it went through. */
static int access_faults(volatile unsigned *p, int write)
{
    faulted = 0;
    if (sigsetjmp(jb, 1) == 0) {
        if (write)
            *p = 0xdeadbeef;
        else
            (void)*p;
        return 0;
    }
    return 1;
}

int main(void)
{
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = segv;
    sa.sa_flags = SA_NODEFER; /* deliver again after siglongjmp out of the handler */
    sigaction(SIGSEGV, &sa, NULL);
    sigaction(SIGBUS, &sa, NULL);

    int div = 0;

    /* Phase 1 — RW -> PROT_NONE, the musl guard-page shape.
     * The write below faults the page in and leaves a writable translation in
     * this core's TLB; mprotect must then invalidate it. */
    {
        volatile unsigned *p = mmap(NULL, 4096, PROT_READ | PROT_WRITE,
                                    MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (p == MAP_FAILED) {
            printf("FAIL phase1 — mmap failed\n");
            return 2;
        }
        *p = MAGIC;
        if (mprotect((void *)p, 4096, PROT_NONE) != 0) {
            printf("FAIL phase1 — mprotect(PROT_NONE) failed\n");
            return 2;
        }
        if (!access_faults(p, 0)) {
            printf("FAIL rw_to_none_read — read of a PROT_NONE page succeeded "
                   "(stale TLB entry: mprotect did not invalidate)\n");
            div++;
        } else {
            printf("PASS rw_to_none_read — PROT_NONE page faults on read\n");
        }
        munmap((void *)p, 4096);
    }

    /* Phase 2 — RW -> read-only, the RELRO shape. */
    {
        volatile unsigned *p = mmap(NULL, 4096, PROT_READ | PROT_WRITE,
                                    MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (p == MAP_FAILED) {
            printf("FAIL phase2 — mmap failed\n");
            return 2;
        }
        *p = MAGIC;
        if (mprotect((void *)p, 4096, PROT_READ) != 0) {
            printf("FAIL phase2 — mprotect(PROT_READ) failed\n");
            return 2;
        }
        if (access_faults(p, 0)) {
            printf("FAIL ro_read — read of a PROT_READ page faulted\n");
            div++;
        } else if (*p != MAGIC) {
            printf("FAIL ro_read — PROT_READ page lost its contents (got 0x%x)\n", *p);
            div++;
        } else if (!access_faults(p, 1)) {
            printf("FAIL rw_to_ro_write — write to a PROT_READ page succeeded "
                   "(stale TLB entry: mprotect did not invalidate)\n");
            div++;
        } else {
            printf("PASS rw_to_ro_write — PROT_READ page faults on write, reads fine\n");
        }
        munmap((void *)p, 4096);
    }

    /* Phase 3 — guard page inside a larger mapping, byte-for-byte the musl
     * pthread_create sequence: the whole region is touched first, then only the
     * lowest page is dropped to PROT_NONE. Catches a flush that invalidates the
     * wrong page of the range as well as one that invalidates nothing. */
    {
        const size_t n = 8;
        volatile unsigned char *base = mmap(NULL, n * 4096, PROT_READ | PROT_WRITE,
                                            MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (base == MAP_FAILED) {
            printf("FAIL phase3 — mmap failed\n");
            return 2;
        }
        for (size_t i = 0; i < n; i++)
            base[i * 4096] = (unsigned char)i;
        if (mprotect((void *)base, 4096, PROT_NONE) != 0) {
            printf("FAIL phase3 — mprotect(guard, PROT_NONE) failed\n");
            return 2;
        }
        int bad = 0;
        if (!access_faults((volatile unsigned *)base, 1)) {
            printf("FAIL guard_write — write to the PROT_NONE guard page succeeded\n");
            bad = 1;
        }
        for (size_t i = 1; i < n && !bad; i++) {
            if (access_faults((volatile unsigned *)(base + i * 4096), 1)) {
                printf("FAIL guard_neighbour — page %zu above the guard faulted "
                       "(flush hit the wrong page)\n", i);
                bad = 1;
            }
        }
        if (bad)
            div++;
        else
            printf("PASS guard_page — guard faults, the 7 pages above it stay writable\n");
        munmap((void *)base, n * 4096);
    }

    printf("=== MPROTECTLB DONE — %d divergence(s) from Linux ===\n", div);
    return div ? 1 : 0;
}
