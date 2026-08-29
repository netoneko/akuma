/*
 * cow_fault_cost — what one copy-on-write write fault costs.
 *
 * The companion to `mem_op_cost.c`, and deliberately the thing that file
 * refuses to be: every arm here FAULTS. `mem_op_cost` keeps faulting arms out
 * because the PMM's variance swamps a decode change; this probe exists for the
 * opposite case, a change *on the fault path*, where the fault is the thing
 * being measured.
 *
 * Written for one specific question (2026-08-29). The EL0 write-fault handler's
 * CoW-break arm was gated on the region's recorded protection, to stop
 * `mprotect(PROT_READ)` being defeated by a fork
 * (docs/archive/AKUMA_EXTRACT_MMAP.md §10.4). That gate calls
 * `eager_region_flags_for_page_fault`, which takes `vm_lock` and walks the
 * region list — **per CoW fault**. Lock order forbids moving it inside the
 * `as_lock` hold where `cow_ref` is known, so it runs on every write-permission
 * fault that survives `stale_write_fault_absorbed`. Whether that is free or a
 * real cost on a fork-heavy workload is not a question to answer by reading.
 *
 * Method: a child faults N private pages that its parent has already made
 * resident, then exits. The parent times the whole fork/fault/exit cycle. Two
 * page counts bracket it, so fork and exit cancel:
 *
 *     per-fault ns = (cow_many - cow_one) / (MANY - ONE)
 *
 * `fork_exit` is the third arm and the control: if IT moves between two builds,
 * the builds are not comparable and the subtraction above is meaningless.
 *
 * The parent touches every page BEFORE forking on purpose. An untouched page
 * takes a translation fault (demand paging) instead of a permission fault, which
 * is a different path with a different cost, and mixing the two would measure
 * neither.
 *
 * Reads the cheapest pass, like its siblings — a fork-heavy loop is noisy and
 * the minimum is the closest thing to "what this costs when nothing else
 * intervenes". Report the RATIO to `fork_exit`, not the ns: boot-to-boot drift
 * here is multiplicative, the same warning `mem_op_cost.c` carries.
 *
 * Usage:  cow_fault_cost [passes]        (default 30)
 */
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define ONE   1
#define MANY  512

static long long now_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (long long)ts.tv_sec * 1000000000LL + ts.tv_nsec;
}

static char *region;
static long page;

/* Fork a child that writes the first `pages` pages, wait for it, return ns. */
static long long cycle(int pages) {
    long long t0 = now_ns();
    pid_t pid = fork();
    if (pid < 0) return -1;
    if (pid == 0) {
        for (int i = 0; i < pages; i++)
            region[(long)i * page] = (char)(i + 1);
        _exit(0);
    }
    int st = 0;
    waitpid(pid, &st, 0);
    long long d = now_ns() - t0;
    return (WIFEXITED(st) && WEXITSTATUS(st) == 0) ? d : -1;
}

static long long best_of(int passes, int pages, int *bad) {
    long long best = -1;
    for (int p = 0; p < passes; p++) {
        long long d = cycle(pages);
        if (d < 0) { (*bad)++; continue; }
        if (best < 0 || d < best) best = d;
    }
    return best;
}

int main(int argc, char **argv) {
    int passes = argc > 1 ? atoi(argv[1]) : 30;
    int bad = 0;
    if (passes < 1) { fprintf(stderr, "usage: %s [passes]\n", argv[0]); return 2; }
    page = sysconf(_SC_PAGESIZE);

    region = mmap(NULL, (size_t)MANY * page, PROT_READ | PROT_WRITE,
                  MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (region == MAP_FAILED) { perror("mmap"); return 2; }
    /* Resident BEFORE the fork: we want permission faults, not translation
     * faults. Mixing the two measures neither. */
    for (int i = 0; i < MANY; i++) region[(long)i * page] = 1;

    long long f = best_of(passes, 0, &bad);
    long long one = best_of(passes, ONE, &bad);
    long long many = best_of(passes, MANY, &bad);
    if (f < 0 || one < 0 || many < 0 || bad > passes) {
        printf("FAIL: %d bad cycle(s); the numbers below mean nothing\n", bad);
        return 1;
    }

    printf("cow_fault_cost: %d passes, cheapest wins, %ld-byte pages\n", passes, page);
    printf("%-14s %8lld ns   (control)\n", "fork_exit", f);
    printf("%-14s %8lld ns   (ratio %.2f)\n", "cow_1p", one, (double)one / (double)f);
    printf("%-14s %8lld ns   (ratio %.2f)\n", "cow_512p", many, (double)many / (double)f);
    printf("%-14s %8lld ns   per CoW write fault  [(cow_512p - cow_1p) / %d]\n",
           "per_fault", (many - one) / (MANY - ONE), MANY - ONE);
    if (bad) printf("note: %d bad cycle(s) skipped\n", bad);
    return 0;
}
