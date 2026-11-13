/* jvmprobe - the phase-1 probe suite (lesson 2, plan §Part I): every gate is a probe,
 * written before the kernel code that would close it. One static musl C binary, named
 * arms, jitprobe's shape (userspace/jitprobe/c/jit_probe.c): a crashy arm runs in a
 * forked child and reads as CRASH, never kills the suite. Each arm prints its own
 * NAME-prefixed verdict line so the phase-2 gates can grep one name each:
 *
 *   jvmprobe getcpu    -> GETCPU PASS cpu=N node=M | GETCPU RED ENOSYS | GETCPU RED errno=N (str)
 *   jvmprobe auxv      -> AUXV PASS/RED lines (AT_SYSINFO_EHDR = the vDSO gap)
 *   jvmprobe cycles    -> CYCLES PASS getpid=N cyc/call clock_gettime=N cyc/call
 *   jvmprobe rwx       -> RWX RED mmap-RWX-refused=EINVAL/EACCES (+ RWX PASS rw_to_rx=42)
 *                          | RWX RED rw_to_rx <why>   (the ability HotSpot minimally needs)
 *   jvmprobe java CMD  -> JVM PASS firstline | JVM RED <diagnosis> (+ evidence path)
 *
 * Exit code: 0 iff the arm PASSED. RED and CRASH exit 1: a clean, named failure is the
 * RED phase's expected outcome (lesson 2), not an error of the probe. The caller
 * (denver/probe-method.sh) judges the method, never the probe itself.
 *
 * Build: gcc -static -O2 -o jvmprobe jvmprobe.c   (host gcc is Alpine/musl; verified
 * static-pie output runs where tokioprobe runs).
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/auxv.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifndef SYS_getcpu
#define SYS_getcpu 168
#endif

static const char *estr(int e) { return strerror(e); }

/* ---------------- getcpu ---------------- */
/* Hypothesis H1 (taoxichuan attempt blocker 2, x86_64 transfer): getcpu(2) is served
 * nowhere in this tree (rg over amd64/, akuma-tokio-kernel*, akuma-syscalls*: no hits),
 * so the raw syscall answers ENOSYS and HotSpot's os::init_2 exits the VM. */
static int arm_getcpu(void) {
    unsigned int cpu = 0, node = 0;
    long r = syscall(SYS_getcpu, &cpu, &node, NULL);
    if (r == 0) {
        printf("GETCPU PASS cpu=%u node=%u\n", cpu, node);
        return 0;
    }
    if (errno == ENOSYS) {
        printf("GETCPU RED ENOSYS\n");
        return 1;
    }
    printf("GETCPU RED errno=%d (%s)\n", errno, estr(errno));
    return 1;
}

/* ---------------- auxv ---------------- */
/* Hypothesis H2 (this session's hs_err, gap #1): the kernel advertises no vDSO
 * (akuma-elf-exec/src/lib.rs fills AT_PHDR/AT_BASE/AT_ENTRY etc., never
 * AT_SYSINFO_EHDR), so HotSpot falls back to the legacy vsyscall page
 * (0xffffffffff600800) and dies on it (SEGV_MAPERR, RIP inside the page). */
static void auxv_line(const char *name, unsigned long key, int required_for_jvm) {
    unsigned long v = getauxval(key);
    if (v) {
        printf("AUXV PASS %s=%lu\n", name, v);
    } else {
        printf("AUXV %s %s missing\n", required_for_jvm ? "RED" : "INFO", name);
    }
}

static int arm_auxv(void) {
    auxv_line("AT_PAGESZ", 6, 0);
    auxv_line("AT_CLKTCK", 17, 0);
    auxv_line("AT_HWCAP", 16, 0);
    auxv_line("AT_SYSINFO_EHDR", 33, 1); /* the vDSO: RED = hypothesis H2 confirmed */
    /* diagnostics: can the kernel even serve /proc/self/auxv (taoxichuan precedent:
     * /proc/<pid>/maps was empty there; maps/auxv render is akuma-procfs's gated job) */
    FILE *f = fopen("/proc/self/auxv", "rb");
    if (!f) {
        printf("AUXV INFO /proc/self/auxv unreadable errno=%d (%s)\n", errno, estr(errno));
    } else {
        unsigned long buf[2];
        size_t n = 0, got;
        while ((got = fread(buf, 1, sizeof buf, f)) == sizeof buf) {
            if (buf[0] == 0) break;
            n += got;
        }
        fclose(f);
        printf("AUXV INFO /proc/self/auxv bytes=%zu\n", n * 2);
    }
    if (getauxval(33) == 0) return 1;
    return 0;
}

/* ---------------- cycles ---------------- */
/* Diagnostics arm (R8: diagnostics are first-class): what does a time read cost
 * without a vDSO? On Linux the vDSO answers in ~20 cycles; every call here is a
 * full syscall round trip. This arm cannot RED: its verdict is the number. */
static inline uint64_t rdtsc(void) {
    uint32_t lo, hi;
    __asm__ __volatile__("rdtsc" : "=a"(lo), "=d"(hi));
    return ((uint64_t)hi << 32) | lo;
}

static int arm_cycles(void) {
    volatile long sink = 0;
    uint64_t t0 = rdtsc();
    for (int i = 0; i < 1000; i++) sink += syscall(SYS_getpid);
    uint64_t t1 = rdtsc();
    uint64_t per_getpid = (t1 - t0) / 1000;

    struct timespec ts;
    t0 = rdtsc();
    for (int i = 0; i < 1000; i++) clock_gettime(CLOCK_MONOTONIC, &ts);
    t1 = rdtsc();
    uint64_t per_clock = (t1 - t0) / 1000;

    printf("CYCLES PASS getpid=%llu cyc/call clock_gettime=%llu cyc/call (no vDSO: syscall path)\n",
           (unsigned long long)per_getpid, (unsigned long long)per_clock);
    return 0;
}

/* ---------------- rwx ---------------- */
/* jitprobe's arms, kept (precedent, kernel 0.0.8 verdicts in its README): single-call
 * W|X is the policy baseline (refused), RW->mprotect(R|X)->call is the ability
 * HotSpot's code cache minimally needs. rw_to_rx failing = RED (the JVM-blocking
 * shape); the mmap refusal alone is the policy line, expected, not a verdict. */
static int child_rwx(void) {
    size_t sz = 4096;
    unsigned char *p = mmap(NULL, sz, PROT_READ | PROT_WRITE | PROT_EXEC,
                            MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (p == MAP_FAILED) {
        printf("RWX RED mmap-RWX-refused errno=%d (%s)\n", errno, estr(errno));
    } else {
        printf("RWX INFO mmap-RWX-allowed (policy baseline moved; attempt-doc blocker 1 may not apply)\n");
        munmap(p, sz);
    }
    /* RW -> RX -> call, the jitprobe rw_to_rx arm */
    p = mmap(NULL, sz, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (p == MAP_FAILED) {
        printf("RWX RED rw_to_rx mmap errno=%d (%s)\n", errno, estr(errno));
        return 1;
    }
    /* mov eax,42; ret */
    unsigned char code[] = {0xb8, 42, 0x00, 0x00, 0x00, 0xc3};
    memcpy(p, code, sizeof code);
    if (mprotect(p, sz, PROT_READ | PROT_EXEC) != 0) {
        printf("RWX RED rw_to_rx mprotect errno=%d (%s)\n", errno, estr(errno));
        return 1;
    }
    int (*fn)(void) = (int (*)(void))p;
    fflush(stdout); /* the verdict written before the jump, whatever the jump does */
    int got = fn();
    if (got == 42) {
        printf("RWX PASS rw_to_rx=42\n");
        return 0;
    }
    printf("RWX RED rw_to_rx got=%d\n", got);
    return 1;
}

static int arm_rwx(void) {
    pid_t pid = fork();
    if (pid == 0) _exit(child_rwx());
    int st = 0;
    waitpid(pid, &st, 0);
    if (WIFSIGNALED(st)) {
        printf("RWX RED rw_to_rx CRASH signal=%d\n", WTERMSIG(st));
        return 1;
    }
    return WEXITSTATUS(st);
}

/* ---------------- java ---------------- */
/* The workload (lesson 1): run it, capture stdout+stderr, name the failure. The
 * taoxichuan attempt's evidence lines came from exactly `java -version`. */
static int arm_java(char **argv) {
    char *cmd = argv[0];
    if (access(cmd, X_OK) != 0) {
        printf("JVM RED java-not-installed at=%s errno=%d (%s)\n", cmd, errno, estr(errno));
        return 1;
    }
    int out[2];
    if (pipe2(out, 0) != 0) {
        printf("JVM RED pipe2 errno=%d (%s)\n", errno, estr(errno));
        return 1;
    }
    /* musl loader pre-report (the image must carry it for an alpine JDK) */
    if (access("/lib/ld-musl-x86_64.so.1", X_OK) != 0)
        printf("JVM INFO loader /lib/ld-musl-x86_64.so.1 absent errno=%d\n", errno);
    else
        printf("JVM INFO loader present\n");

    pid_t pid = fork();
    if (pid == 0) {
        close(out[0]);
        dup2(out[1], 1);
        dup2(out[1], 2);
        close(out[1]);
        alarm(90); /* the child cannot hang the suite */
        execv(cmd, argv);
        _exit(127);
    }
    close(out[1]);
    char buf[16384];
    size_t n = 0;
    ssize_t got;
    alarm(120);
    while (n < sizeof buf - 1 && (got = read(out[0], buf + n, sizeof buf - 1 - n)) > 0) n += got;
    alarm(0);
    close(out[0]);
    buf[n] = 0;

    int st = 0;
    waitpid(pid, &st, 0);
    printf("JVM INFO exit=%d signal=%d bytes=%zu\n",
           WIFEXITED(st) ? WEXITSTATUS(st) : -1, WIFSIGNALED(st) ? WTERMSIG(st) : 0, n);

    /* first informative line = the diagnosis; the REASON is usually the next line
     * ("Error occurred during initialization of VM" / "Failed to mark memory page as
     * executable" - the attempt doc's blocker lines). hs_err headers are '#' walls. */
    char first[200] = {0};
    char reason[200] = {0};
    for (char *line = strtok(buf, "\n"); line; line = strtok(NULL, "\n")) {
        if (!*line || (line[0] == '#' && strlen(line) <= 30)) continue;
        if (!first[0]) {
            snprintf(first, sizeof first, "%.190s", line);
            continue;
        }
        if (!reason[0] && (strstr(line, "Failed") || strstr(line, "Error:") ||
                           strstr(line, "error") || strstr(line, "Exception")))
            snprintf(reason, sizeof reason, "%.190s", line);
        printf("JVM-INFO raw: %s\n", line);
    }
    if (WIFSIGNALED(st)) {
        if (reason[0])
            printf("JVM RED signal=%d first=\"%s\" reason=\"%s\"\n", WTERMSIG(st), first, reason);
        else
            printf("JVM RED signal=%d first=\"%s\"\n", WTERMSIG(st), first);
        return 1;
    }
    if (WIFEXITED(st) && WEXITSTATUS(st) == 0) {
        printf("JVM PASS %s\n", first);
        return 0;
    }
    if (reason[0])
        printf("JVM RED %s reason=\"%s\"\n", first, reason);
    else
        printf("JVM RED %s\n", first[0] ? first : "no output");
    return 1;
}

/* ---------------- main ---------------- */
static void usage(void) {
    fprintf(stderr,
            "usage: jvmprobe getcpu|auxv|cycles|rwx|java CMD [ARG]...\n"
            "  one arm per invocation; NAME VERDICT detail on stdout; exit 0 iff PASS\n");
}

int main(int argc, char **argv) {
    if (argc < 2) return usage(), 2;
    signal(SIGPIPE, SIG_IGN);
    /* verdict lines must survive fork + _exit and pipe-captured stdout: unbuffered */
    setvbuf(stdout, NULL, _IONBF, 0);
    const char *arm = argv[1];
    if (!strcmp(arm, "getcpu")) return arm_getcpu();
    if (!strcmp(arm, "auxv")) return arm_auxv();
    if (!strcmp(arm, "cycles")) return arm_cycles();
    if (!strcmp(arm, "rwx")) return arm_rwx();
    if (!strcmp(arm, "java")) return argc < 3 ? usage(), 2 : arm_java(argv + 2);
    return usage(), 2;
}
