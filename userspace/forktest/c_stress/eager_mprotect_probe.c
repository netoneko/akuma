// eager_mprotect_probe — does mprotect still hold on an EAGER mmap after the
// Failure-A recovery path (`MmapRegion::flags` + the `[EAGER-UPGRADE]` fault
// handler repair) landed?
//
// Background: an eager anonymous mmap (small, RW, no MAP_NORESERVE — see
// `MMAP_EAGER_MAX_PAGES` in src/config.rs) installs its pages up front and
// registers no lazy region. Before the fix, a page left mapped read-only with
// `cow_ref=0` inside such a region had no recovery path at all and SIGSEGV'd
// by construction (Failure A). The fix adds `MmapRegion::flags`, threads real
// protection through mmap/mprotect/mremap/munmap-split/fork, and teaches the
// EL0 data-abort handler to upgrade a stale-read-only PTE back to the
// region's *recorded* protection — gated on that protection actually
// including PROT_WRITE. See
// docs/archive/J4_WRITE_PERM_FAULT_AND_HALF_WRITTEN_LINKER_OUTPUT.md §3, §6a.
//
// The failure mode this probe is built to catch: the upgrade gate is wrong
// and fires even when `mprotect` downgraded the region to read-only (or
// PROT_NONE), silently defeating `mprotect` — a write that must fault
// instead succeeds.
//
// Each phase forks a child that performs the write; the parent checks how the
// child died. Forking isolates the (expected) fatal SIGSEGV to a throwaway
// process instead of tearing down the probe itself.
//
// Build: aarch64-linux-musl-gcc -static -O2 -o eager_mprotect_probe eager_mprotect_probe.c
// Exit code 0 = both phases correctly SIGSEGV'd (mprotect semantics intact).

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

// Fork off `body`, wait for it, and report PASS iff it died from SIGSEGV.
static int expect_segv(const char *phase, void (*body)(void))
{
    fflush(stdout);
    pid_t pid = fork();
    if (pid < 0) { perror("fork"); return 1; }
    if (pid == 0) {
        body();
        // If we get here, the write did NOT fault — that is the failure case.
        printf("%s FAIL: write succeeded, no SIGSEGV — mprotect was defeated\n", phase);
        _exit(0);
    }
    int status = 0;
    if (waitpid(pid, &status, 0) != pid) { perror("waitpid"); return 1; }
    if (WIFSIGNALED(status) && WTERMSIG(status) == 11 /* SIGSEGV */) {
        printf("%s PASS: write correctly SIGSEGV'd\n", phase);
        return 0;
    }
    if (WIFEXITED(status)) {
        // Child printed its own FAIL line before a clean exit.
        return 1;
    }
    printf("%s FAIL: child died unexpectedly, status=0x%x\n", phase, status);
    return 1;
}

static void *g_eager_rw;      // phase 1: eager RW region, downgraded to PROT_READ
static void *g_eager_guard;   // phase 2: eager RW region's second page, PROT_NONE

static void phase1_body(void) { *(volatile char *)g_eager_rw = 'x'; }
static void phase2_body(void) { *(volatile char *)g_eager_guard = 'x'; }

int main(void)
{
    int failures = 0;
    long page = sysconf(_SC_PAGESIZE);

    // ---- Phase 1: eager RW mmap, mprotect(PROT_READ), write must SIGSEGV ----
    void *p1 = mmap(NULL, page, PROT_READ | PROT_WRITE,
                     MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (p1 == MAP_FAILED) { perror("mmap p1"); return 2; }
    *(volatile char *)p1 = 'a'; // touch it while writable, prove it's really mapped
    if (mprotect(p1, page, PROT_READ) != 0) { perror("mprotect p1"); return 2; }
    g_eager_rw = p1;
    failures += expect_segv("PHASE1(mprotect PROT_READ)", phase1_body);

    // ---- Phase 2: eager RW mmap, second page mprotect(PROT_NONE) guard ----
    void *p2 = mmap(NULL, page * 2, PROT_READ | PROT_WRITE,
                     MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (p2 == MAP_FAILED) { perror("mmap p2"); return 2; }
    char *guard = (char *)p2 + page;
    *(volatile char *)p2 = 'a';       // first page stays RW, touched to confirm it's live
    *(volatile char *)guard = 'a';    // touch the second page too, before downgrading it
    if (mprotect(guard, page, PROT_NONE) != 0) { perror("mprotect p2"); return 2; }
    g_eager_guard = guard;
    failures += expect_segv("PHASE2(mprotect PROT_NONE guard)", phase2_body);

    printf("RESULT: %s\n", failures == 0 ? "PASS" : "FAIL");
    return failures == 0 ? 0 : 1;
}
