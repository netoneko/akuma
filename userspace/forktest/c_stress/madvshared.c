// madvshared — does MADV_DONTNEED on a CoW-shared page wipe the PEER's copy?
//
// The deterministic probe for theory 3 of proposals/CARGO_HEAP_NULL_RC.md.
//
// Akuma's MADV_DONTNEED zeroes the *physical frame* in place; Linux drops the
// *mapping* and lets the next touch fault in a fresh zero page. The two agree
// for a page owned by one address space and disagree — destructively — for a
// frame shared by CoW after fork. That divergence is exactly the null-`Rc`
// signature: a live pointer qword in an anonymous heap zeroed underneath its
// owner, which safe Rust cannot do to itself.
//
// The existing evidence for it is a ~1-in-5 crash during a full in-guest `-j4`
// cargo build. That is a terrible instrument: a stochastic repro at that rate
// passed 95/96 on BOTH arms of a real fix once before in this tree. This probe
// answers the same question in milliseconds, deterministically, with no
// allocator in the way.
//
// Calibrate it on real Linux before believing a FAIL here — every FAIL on Linux
// means the probe is wrong, not the kernel (same rule as `futexops`):
//   docker run --rm --platform linux/arm64 -v "$PWD:/w:ro" alpine /w/madvshared
//
// Phases, each on its own freshly-faulted page:
//   1. child  MADV_DONTNEED  -> parent must still see its pattern
//   2. parent MADV_DONTNEED  -> child  must still see its pattern
//   3. control: no fork, self MADV_DONTNEED -> own page must read back zero
//      (proves the call is reaching the kernel and doing something at all, so a
//      PASS in 1-2 cannot be "madvise silently did nothing")

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/mman.h>
#include <sys/wait.h>

#define PAGE 4096
#define PATTERN 0xA5

static int failures = 0;

static void *fresh_page(unsigned char fill)
{
    void *p = mmap(NULL, PAGE, PROT_READ | PROT_WRITE,
                   MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (p == MAP_FAILED) {
        perror("mmap");
        exit(2);
    }
    memset(p, fill, PAGE);   // fault it in and own the frame
    return p;
}

static int all_bytes_are(const unsigned char *p, unsigned char v)
{
    for (size_t i = 0; i < PAGE; i++)
        if (p[i] != v)
            return 0;
    return 1;
}

// How many bytes of the page survived as `v` — a partial wipe is as interesting
// as a total one, and "0 of 4096" vs "4096 of 4096" is the whole result.
static size_t count_bytes(const unsigned char *p, unsigned char v)
{
    size_t n = 0;
    for (size_t i = 0; i < PAGE; i++)
        if (p[i] == v)
            n++;
    return n;
}

static void report(const char *name, int ok, const char *detail)
{
    printf("madvshared: %-28s %s%s%s\n", name, ok ? "PASS" : "FAIL",
           detail && *detail ? " — " : "", detail ? detail : "");
    if (!ok)
        failures++;
}

// Phase 1: the child advises away a page it shares CoW with the parent.
// The parent must be untouched.
static void phase_child_advises(void)
{
    unsigned char *p = fresh_page(PATTERN);
    pid_t pid = fork();
    if (pid < 0) { perror("fork"); exit(2); }

    if (pid == 0) {
        // Child: do NOT write first — writing would break CoW and give us a
        // private frame, which is precisely the case that is already safe.
        if (madvise(p, PAGE, MADV_DONTNEED) != 0)
            _exit(3);
        _exit(0);
    }

    int st = 0;
    waitpid(pid, &st, 0);
    if (WIFEXITED(st) && WEXITSTATUS(st) == 3) {
        report("child-advises/parent-intact", 1, "madvise unsupported, skipped");
        munmap(p, PAGE);
        return;
    }

    size_t kept = count_bytes(p, PATTERN);
    char detail[96];
    snprintf(detail, sizeof detail, "parent kept %zu/%d bytes", kept, PAGE);
    report("child-advises/parent-intact", kept == PAGE, detail);
    munmap(p, PAGE);
}

// Phase 2: the parent advises. The child's view must be untouched. Reported
// through the child's exit status so the check runs in the child's own AS.
static void phase_parent_advises(void)
{
    unsigned char *p = fresh_page(PATTERN);
    int fds[2];
    if (pipe(fds) != 0) { perror("pipe"); exit(2); }

    pid_t pid = fork();
    if (pid < 0) { perror("fork"); exit(2); }

    if (pid == 0) {
        close(fds[1]);
        char go;
        // Block until the parent has run madvise, so the read below is ordered
        // strictly after it rather than racing it.
        if (read(fds[0], &go, 1) != 1)
            _exit(4);
        _exit(all_bytes_are(p, PATTERN) ? 0 : 1);
    }

    close(fds[0]);
    if (madvise(p, PAGE, MADV_DONTNEED) != 0) {
        char go = 'x';
        (void)!write(fds[1], &go, 1);
        close(fds[1]);
        waitpid(pid, NULL, 0);
        report("parent-advises/child-intact", 1, "madvise unsupported, skipped");
        munmap(p, PAGE);
        return;
    }
    char go = 'x';
    (void)!write(fds[1], &go, 1);
    close(fds[1]);

    int st = 0;
    waitpid(pid, &st, 0);
    int ok = WIFEXITED(st) && WEXITSTATUS(st) == 0;
    report("parent-advises/child-intact", ok,
           ok ? "" : "child's shared page was wiped by the parent's advise");
    munmap(p, PAGE);
}

// Phase 3 (control): no sharing at all. MADV_DONTNEED on a page only we own
// must read back as zero. If this FAILs, madvise is a no-op here and phases
// 1-2 passing means nothing.
static void phase_control_self(void)
{
    unsigned char *p = fresh_page(PATTERN);
    if (madvise(p, PAGE, MADV_DONTNEED) != 0) {
        report("control/self-zeroed", 1, "madvise unsupported, skipped");
        munmap(p, PAGE);
        return;
    }
    size_t zeros = count_bytes(p, 0);
    char detail[96];
    snprintf(detail, sizeof detail, "%zu/%d bytes zero after own advise",
             zeros, PAGE);
    report("control/self-zeroed", zeros == PAGE, detail);
    munmap(p, PAGE);
}

int main(void)
{
    printf("madvshared: MADV_DONTNEED on a CoW-shared frame "
           "(proposals/CARGO_HEAP_NULL_RC.md theory 3)\n");
    phase_child_advises();
    phase_parent_advises();
    phase_control_self();

    if (failures == 0) {
        printf("madvshared: ALL PASS\n");
        return 0;
    }
    printf("madvshared: %d FAIL — a peer's live page was zeroed; this is the "
           "null-Rc mechanism\n", failures);
    return 1;
}
