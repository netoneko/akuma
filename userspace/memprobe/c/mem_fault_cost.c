/*
 * mem_fault_cost — what the memory paths that FAULT or ALLOCATE cost.
 *
 * The companion to `mem_op_cost.c`, and deliberately everything that file
 * refuses to be. `mem_op_cost` keeps faulting and allocating arms out because
 * the PMM's variance swamps a decode change; this probe exists for the opposite
 * case — a change *on the fault or allocation path*, where the fault is the
 * thing being measured.
 *
 * It began (2026-08-29) as `cow_fault_cost` with three arms, written for one
 * question: the EL0 write-fault handler's CoW-break arm was gated on the
 * region's recorded protection, to stop `mprotect(PROT_READ)` being defeated by
 * a fork (docs/archive/AKUMA_EXTRACT_MMAP.md §10.4). That gate calls
 * `eager_region_flags_for_page_fault`, which takes `vm_lock` and walks the
 * region list — **per CoW fault**. Lock order forbids moving it inside the
 * `as_lock` hold where `cow_ref` is known, so it runs on every write-permission
 * fault that survives `stale_write_fault_absorbed`.
 *
 * It was renamed and widened because that first version measured one path and
 * left four unmeasured, all of them in the extracted crates' blast radius:
 *
 *   - `plan()`'s central decision is lazy vs eager, and NOTHING priced the two
 *     outcomes against each other.
 *   - demand paging is a *translation* fault, a different path from the CoW
 *     *permission* fault, and only the latter was covered.
 *   - `set_brk` allocates and maps a page at a time on growth; only `brk(0)`
 *     was timed, which takes the no-growth exit.
 *   - `munmap` of a real mapping frees frames and flushes; only `munmap` of
 *     nothing was timed.
 *
 * METHOD. Every per-unit number is a BRACKET, never a single reading: two arms
 * that differ only in how many pages they touch, subtracted, so the mmap, the
 * munmap, the fork and the exit all cancel.
 *
 *     per-unit = (many - one) / (MANY - ONE)
 *
 * The control arms (`fork_exit`, `mmap_lazy`) are what make a comparison
 * legitimate: if THEY move between two builds, the builds are not comparable
 * and every subtraction below is meaningless. Report the RATIO to the control,
 * not the ns — boot-to-boot drift here is multiplicative, the same warning
 * `mem_op_cost.c` carries at length.
 *
 * The parent touches every page BEFORE forking in the CoW arms on purpose. An
 * untouched page takes a translation fault instead of a permission fault, which
 * is a different path with a different cost, and mixing the two measures
 * neither. That distinction is the whole reason `demand_*` and `cow_*` are
 * separate families here.
 *
 * Usage:  mem_fault_cost [passes]        (default 30)
 */
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define ONE   1
#define MANY  512

static long page;

static long long now_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (long long)ts.tv_sec * 1000000000LL + ts.tv_nsec;
}

/* Cheapest of `passes` runs of `f(arg)`; -1 if any run reported failure. */
static long long best_of(int passes, long long (*f)(int), int arg, int *bad) {
    long long best = -1;
    for (int p = 0; p < passes; p++) {
        long long d = f(arg);
        if (d < 0) { (*bad)++; continue; }
        if (best < 0 || d < best) best = d;
    }
    return best;
}

/* ---- allocation arms: what `plan()`'s two outcomes cost ------------------ */

/* mmap + munmap with no page ever touched. `extra` carries MAP_NORESERVE for
 * the lazy arm, which is what `plan()` keys on regardless of size.
 *
 * Looped MAP_REPS times per sample, and this is not optional: `clock_gettime`
 * truncates to MICROSECONDS here, so one cycle reads as 1000 or 2000 ns — one
 * or two ticks — and the "ratio 2.00" that falls out of that is quantisation,
 * not a measurement. The loop pushes a sample to ~1 ms so the tick is 0.1% of
 * it. Same reasoning as `mem_op_cost`'s `calls`; see method warning 3 there. */
#define MAP_REPS 1000
static long long arm_map(int extra) {
    long long t0 = now_ns();
    for (int i = 0; i < MAP_REPS; i++) {
        void *p = mmap(NULL, page, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS | extra, -1, 0);
        if (p == MAP_FAILED) return -1;
        munmap(p, page);
    }
    return (now_ns() - t0) / MAP_REPS;
}

/* ---- demand paging: translation faults ---------------------------------- */

/* A lazy mapping, `pages` of it touched, then unmapped. MAP_NORESERVE forces
 * the lazy path independently of MMAP_EAGER_MAX_PAGES, so this arm means the
 * same thing on every profile. */
static long long arm_demand(int pages) {
    long long t0 = now_ns();
    char *p = mmap(NULL, (size_t)MANY * page, PROT_READ | PROT_WRITE,
                   MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0);
    if (p == MAP_FAILED) return -1;
    for (int i = 0; i < pages; i++) p[(long)i * page] = 1;
    long long d = now_ns() - t0;
    munmap(p, (size_t)MANY * page);
    return d;
}

/* ---- brk growth: allocate + map, one page at a time --------------------- */

/* Grows MONOTONICALLY, and never gives the space back.
 *
 * The first version shrank with `brk(base)` after each pass to keep passes
 * comparable. It reported **0 ns** for growing 2 MB, because a brk shrink does
 * not unmap: pass 2 found every page already mapped, `set_brk`'s allocation
 * loop was skipped, and `best_of` — which takes the MINIMUM — reported the
 * warm pass. An arm that is not idempotent cannot be minimised over.
 *
 * So each pass grows from wherever the break now is, and every pass allocates
 * `pages` fresh pages. Cost: the 512-page arm grows the heap by
 * passes * 2 MB, which is why this probe should not be run with a large pass
 * count on a small-RAM profile. */
static long long arm_brk(int pages) {
    long base = syscall(SYS_brk, 0);
    if (base <= 0) return -1;
    long long t0 = now_ns();
    long got = syscall(SYS_brk, base + (long)pages * page);
    long long d = now_ns() - t0;
    return (got > 0) ? d : -1;
}

/* ---- CoW: write-permission faults on shared frames ---------------------- */

static char *cow_region;

static long long arm_cow(int pages) {
    long long t0 = now_ns();
    pid_t pid = fork();
    if (pid < 0) return -1;
    if (pid == 0) {
        for (int i = 0; i < pages; i++)
            cow_region[(long)i * page] = (char)(i + 1);
        _exit(0);
    }
    int st = 0;
    waitpid(pid, &st, 0);
    long long d = now_ns() - t0;
    return (WIFEXITED(st) && WEXITSTATUS(st) == 0) ? d : -1;
}

static void bracket(const char *unit, long long one, long long many, int n) {
    printf("  %-18s %8lld ns   [(many - one) / %d]\n", unit, (many - one) / n, n);
}

int main(int argc, char **argv) {
    int passes = argc > 1 ? atoi(argv[1]) : 30;
    int bad = 0;
    if (passes < 1) { fprintf(stderr, "usage: %s [passes]\n", argv[0]); return 2; }
    page = sysconf(_SC_PAGESIZE);

    cow_region = mmap(NULL, (size_t)MANY * page, PROT_READ | PROT_WRITE,
                      MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (cow_region == MAP_FAILED) { perror("mmap cow_region"); return 2; }
    /* Resident BEFORE any fork: permission faults, not translation faults. */
    for (int i = 0; i < MANY; i++) cow_region[(long)i * page] = 1;

    long long m_lazy  = best_of(passes, arm_map, MAP_NORESERVE, &bad);
    long long m_eager = best_of(passes, arm_map, 0, &bad);
    long long d_one   = best_of(passes, arm_demand, ONE, &bad);
    long long d_many  = best_of(passes, arm_demand, MANY, &bad);
    long long b_one   = best_of(passes, arm_brk, ONE, &bad);
    long long b_many  = best_of(passes, arm_brk, MANY, &bad);
    long long f_ctl   = best_of(passes, arm_cow, 0, &bad);
    long long c_one   = best_of(passes, arm_cow, ONE, &bad);
    long long c_many  = best_of(passes, arm_cow, MANY, &bad);

    if (m_lazy < 0 || m_eager < 0 || d_one < 0 || d_many < 0 ||
        b_one < 0 || b_many < 0 || f_ctl < 0 || c_one < 0 || c_many < 0) {
        printf("FAIL: an arm never completed; the numbers below mean nothing\n");
        return 1;
    }

    printf("mem_fault_cost: %d passes, cheapest wins, %ld-byte pages\n", passes, page);

    printf("\n[allocation — plan()'s two outcomes, %d reps per sample]\n", MAP_REPS);
    printf("%-18s %8lld ns   (control: region record only)\n", "mmap_lazy", m_lazy);
    printf("%-18s %8lld ns   (ratio %.2f)\n", "mmap_eager", m_eager,
           (double)m_eager / (double)m_lazy);
    printf("  %-18s %8lld ns   eager premium: one frame allocated + mapped\n",
           "eager_extra", m_eager - m_lazy);

    printf("\n[demand paging — translation faults]\n");
    printf("%-18s %8lld ns\n", "demand_1p", d_one);
    printf("%-18s %8lld ns\n", "demand_512p", d_many);
    bracket("per_demand_fault", d_one, d_many, MANY - ONE);

    printf("\n[brk growth — allocate + map per page]\n");
    printf("%-18s %8lld ns\n", "brk_grow_1p", b_one);
    printf("%-18s %8lld ns\n", "brk_grow_512p", b_many);
    bracket("per_brk_page", b_one, b_many, MANY - ONE);

    printf("\n[CoW — write-permission faults]\n");
    printf("%-18s %8lld ns   (control)\n", "fork_exit", f_ctl);
    printf("%-18s %8lld ns   (ratio %.2f)\n", "cow_1p", c_one,
           (double)c_one / (double)f_ctl);
    printf("%-18s %8lld ns   (ratio %.2f)\n", "cow_512p", c_many,
           (double)c_many / (double)f_ctl);
    bracket("per_cow_fault", c_one, c_many, MANY - ONE);

    if (bad) printf("\nnote: %d bad cycle(s) skipped\n", bad);
    return 0;
}
