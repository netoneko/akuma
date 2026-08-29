/* smapsdirty — /proc/self/smaps Shared_Dirty accounting + MADV_FREE semantics.
 *
 * WHY THIS EXISTS
 * ---------------
 * `redis-server` refuses to start on Akuma:
 *
 *   # Failed to test the kernel for a bug that could lead to data corruption
 *   #   during background save. Your system could be affected...
 *   # Redis will now exit to prevent data corruption. ... ignore-warnings ARM64-COW-BUG
 *
 * That is NOT a CoW bug report. Redis prints a *different* message when it
 * actually detects CoW corruption. This one means its probe could not run.
 * The probe is `checkLinuxMadvFreeForkBug()` in redis/src/syscheck.c, and it
 * reads `/proc/self/smaps` — which Akuma does not implement.
 *
 * Full investigation: docs/archive/LONG_ROAD_TO_REDIS.md
 *
 * CALIBRATE ON REAL LINUX FIRST. Every FAIL there means this probe is wrong,
 * not the kernel:
 *
 *   docker run --rm --platform linux/arm64 \
 *     -v "$PWD/smapsdirty:/smapsdirty:ro" alpine /smapsdirty
 *
 * Expected on Linux: 4 PASS. As of 2026-08-12 on Akuma: probes 1, 2 and 4 FAIL.
 */
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <stdarg.h>
#include <string.h>
#include <errno.h>
#include <unistd.h>
#include <sys/mman.h>
#include <sys/wait.h>

#ifndef MADV_FREE
#define MADV_FREE 8
#endif

static int failures;
static int diverges;

/* Three outcomes, not two.
 *
 * This probe is calibrated on Linux, where all four checks PASS. Three of them
 * do not pass on Akuma and are NOT expected to: `/proc/<pid>/` is not populated,
 * and MADV_FREE returns EINVAL on purpose (see probe 3). Reporting those as FAIL
 * made the probe permanently red, which is the state in which nobody reads it —
 * and it hid the one check that must stay green (probe 4).
 *
 * DIVERGE means "differs from Linux, documented, and the difference is the
 * intended behaviour". It is reported and does not fail the run. FAIL is
 * reserved for a difference nobody has signed off on — including a DOCUMENTED
 * divergence that has changed shape, which is why each site below tests for the
 * specific expected deviation rather than for "not PASS".
 *
 * Matches `scripts/mem_suite.py`, which inherits the distinction from
 * `epoll_suite.py`. */
enum outcome { OUT_PASS, OUT_FAIL, OUT_DIVERGE };

static void report_out(const char *name, enum outcome o, const char *fmt, ...)
{
    va_list ap;
    printf("%-28s %s  ", name,
           o == OUT_PASS ? "PASS" : (o == OUT_DIVERGE ? "DIVERGE" : "FAIL"));
    va_start(ap, fmt);
    vprintf(fmt, ap);
    va_end(ap);
    putchar('\n');
    if (o == OUT_FAIL)
        failures++;
    else if (o == OUT_DIVERGE)
        diverges++;
}

static void report(const char *name, int ok, const char *fmt, ...)
{
    va_list ap;
    printf("%-28s %s  ", name, ok ? "PASS" : "FAIL");
    va_start(ap, fmt);
    vprintf(fmt, ap);
    va_end(ap);
    putchar('\n');
    if (!ok)
        failures++;
}

/* Verbatim from redis/src/syscheck.c so the result is directly comparable. */
static int smaps_get_shared_dirty(unsigned long addr)
{
    int ret, in_mapping = 0, val = -1;
    unsigned long from, to;
    char buf[64];
    FILE *f;

    f = fopen("/proc/self/smaps", "r");
    if (!f)
        return -1;

    while (1) {
        if (!fgets(buf, sizeof(buf), f))
            break;
        ret = sscanf(buf, "%lx-%lx", &from, &to);
        if (ret == 2)
            in_mapping = from <= addr && addr < to;
        if (in_mapping && !memcmp(buf, "Shared_Dirty:", 13)) {
            sscanf(buf, "%*s %d", &val);
            break;
        }
    }
    fclose(f);
    return val;
}

/* ---- probe 1: does /proc/self/smaps exist at all? ---------------------- */
static void probe_smaps_present(void)
{
    FILE *f = fopen("/proc/self/smaps", "r");
    if (!f) {
        /* ENOENT is the documented Akuma state: /proc/<pid>/ is not populated
         * (docs/archive/LONG_ROAD_TO_REDIS.md). Any OTHER errno is a genuine
         * failure — a permissions or I/O error here would be new information. */
        report_out("smaps-present", errno == ENOENT ? OUT_DIVERGE : OUT_FAIL,
                   "fopen(/proc/self/smaps) errno=%d (%s)%s",
                   errno, strerror(errno),
                   errno == ENOENT ? " — /proc/<pid>/ not populated, known" : "");
        return;
    }
    fclose(f);
    report_out("smaps-present", OUT_PASS, "-");
}

/* ---- probe 2: the rest of /proc/<pid>/, which Redis is only the first to miss */
static void probe_proc_files(void)
{
    static const char *files[] = { "maps", "status", "stat", "statm", "cmdline", NULL };
    char path[64];
    int missing = 0;
    char miss[256] = "";

    for (int i = 0; files[i]; i++) {
        snprintf(path, sizeof(path), "/proc/self/%s", files[i]);
        if (access(path, R_OK) != 0) {
            missing++;
            strncat(miss, files[i], sizeof(miss) - strlen(miss) - 2);
            strncat(miss, " ", sizeof(miss) - strlen(miss) - 2);
        }
    }
    /* Same documented gap as probe 1: absent, not broken. */
    report_out("proc-self-files", missing == 0 ? OUT_PASS : OUT_DIVERGE,
               "%d missing: %s", missing, missing ? miss : "-");
}

/* ---- probe 3: MADV_FREE return value ----------------------------------
 * Linux returns 0 and really does defer the free.
 *
 * CORRECTED 2026-08-29. This comment used to say Akuma returns 0 and does
 * nothing, and scored 0 as PASS. That is no longer true and the expectation was
 * backwards: Akuma now returns EINVAL **on purpose**, because a no-op success
 * defeats capability probes — Redis reads EINVAL as "older kernel, presumably
 * not affected" and starts, where a fabricated 0 sent it into a THP self-check
 * it cannot pass without /proc/<pid>/smaps (LONG_ROAD_TO_REDIS.md §5, and
 * `akuma_syscalls_mem::madvise::action`'s divergence 3).
 *
 * So EINVAL is the DIVERGE outcome and 0 is PASS-on-Linux; anything else is a
 * real failure. Probe 4 is what checks the consequence. */
static void probe_madv_free(void)
{
    long ps = sysconf(_SC_PAGESIZE);
    char *p = mmap(NULL, ps, PROT_READ | PROT_WRITE,
                   MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (p == MAP_FAILED) {
        report("madv-free-accepted", 0, "mmap failed errno=%d", errno);
        return;
    }
    *(volatile char *)p = 1;
    errno = 0;
    int ret = madvise(p, ps, MADV_FREE);
    enum outcome o = (ret == 0) ? OUT_PASS
                   : (ret < 0 && errno == EINVAL) ? OUT_DIVERGE : OUT_FAIL;
    report_out("madv-free-accepted", o, "ret=%d errno=%d (%s)%s",
               ret, errno, ret < 0 ? strerror(errno) : "-",
               o == OUT_DIVERGE ? " — deliberate, see probe 4" : "");
    munmap(p, ps);
}

/* ---- probe 4: the exact Redis check ------------------------------------
 * Return convention (redis/src/server.c:7449) is inverted from intuition:
 *   >0 healthy   <0 CoW bug detected   0 could not test
 * Redis exits on anything <= 0. */
static void probe_redis_check(void)
{
    int ret, pipefd[2] = { -1, -1 };
    pid_t pid;
    char *p = NULL, *q;
    int res = 1;
    long ps = sysconf(_SC_PAGESIZE), ms = 3 * ps;
    const char *why = "-";

    p = mmap(NULL, ms, PROT_READ, MAP_ANONYMOUS | MAP_PRIVATE, -1, 0);
    if (p == MAP_FAILED) {
        report("redis-arm64-cow-check", 0, "mmap failed errno=%d", errno);
        return;
    }
    q = p + ps;

    if (mprotect(q, ps, PROT_READ | PROT_WRITE) < 0) {
        res = 0; why = "mprotect failed"; goto done;
    }
    *(volatile char *)q = 0;

    errno = 0;
    if (madvise(q, ps, MADV_FREE) < 0) {
        if (errno == EINVAL) {
            why = "MADV_FREE=EINVAL -> redis SKIPS the check and starts";
            goto done;   /* res stays 1 */
        }
        res = 0; why = "madvise failed (not EINVAL)"; goto done;
    }
    *(volatile char *)q = 0;

    if (pipe(pipefd) < 0)  { res = 0; why = "pipe failed";  goto done; }
    if ((pid = fork()) < 0) { res = 0; why = "fork failed"; goto done; }

    if (pid == 0) {
        int child = 1, sd = smaps_get_shared_dirty((unsigned long)q);
        if (!sd)            child = -1;   /* dirty bit lost -> the real CoW bug */
        else if (sd == -1)  child = 0;    /* could not read smaps */
        ssize_t w = write(pipefd[1], &child, sizeof(child));
        _exit(w == sizeof(child) ? 0 : 1);
    }

    ret = read(pipefd[0], &res, sizeof(res));
    if (ret != (int)sizeof(res)) { res = 0; why = "short read from child"; }
    else if (res == 0)  why = "child could not read /proc/self/smaps";
    else if (res < 0)   why = "Shared_Dirty==0 -> genuine ARM64 CoW dirty-bit bug";
    waitpid(pid, NULL, 0);

done:
    if (pipefd[0] != -1) close(pipefd[0]);
    if (pipefd[1] != -1) close(pipefd[1]);
    munmap(p, ms);

    report("redis-arm64-cow-check", res > 0, "res=%d (%s) -> redis %s",
           res, why, res > 0 ? "STARTS" : "EXITS");
}

int main(void)
{
    printf("smapsdirty: /proc/self/smaps + MADV_FREE probes "
           "(see docs/archive/LONG_ROAD_TO_REDIS.md)\n\n");
    probe_smaps_present();
    probe_proc_files();
    probe_madv_free();
    probe_redis_check();
    printf("\n%d failure(s), %d documented divergence(s)\n", failures, diverges);
    return failures ? 1 : 0;
}
