/* jvmprobe - the phase-1 probe suite (lesson 2, plan §Part I): every gate is a probe,
 * written before the kernel code that would close it. One static musl C binary, named
 * arms, jitprobe's shape (userspace/jitprobe/c/jit_probe.c): a crashy arm runs in a
 * forked child and reads as CRASH, never kills the suite. Each arm prints its own
 * NAME-prefixed verdict line so the phase-2 gates can grep one name each:
 *
 *   jvmprobe getcpu    -> GETCPU PASS cpu=N node=M | GETCPU RED ENOSYS | GETCPU RED errno=N (str)
 *   jvmprobe auxv      -> AUXV PASS/RED lines (AT_SYSINFO_EHDR = the vDSO gap)
 *   jvmprobe cycles    -> CYCLES PASS getpid=N cyc/call clock_gettime=N cyc/call
 *   jvmprobe maps      -> MAPS PASS lines=N exec_lines=N | MAPS RED errno=N (the G4 flip)
 *   jvmprobe name RIP  -> NAME PASS rip in <range> | NAME SURPRISE | NAME MISSING (G9's owner)
 *   jvmprobe rwx       -> RWX RED mmap-RWX-refused=EINVAL/EACCES (+ RWX PASS rw_to_rx=42)
 *                          | RWX RED rw_to_rx <why>   (the ability HotSpot minimally needs)
 *   jvmprobe wxrange strict|relaxed -> WXRANGE PASS mode=... cases=3
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
#include <sys/sysinfo.h>
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
    /* the procfs arm (PHASE-2-PROCFS, G4): /proc/self/auxv must serve the raw
     * Elf64_auxv_t pairs - the vector getauxval reads, as bytes. A phase-1 INFO
     * became the verdict: errno=2 here was G4's whole evidence line. The
     * consistency check is the point: the kernel that builds the stack vector
     * and the kernel that renders /proc must answer identically (AT_PAGESZ and
     * AT_SYSINFO_EHDR must match getauxval), or one of the two renders lies. */
    FILE *f = fopen("/proc/self/auxv", "rb");
    if (!f) {
        printf("AUXV RED /proc/self/auxv unreadable errno=%d (%s)\n", errno, estr(errno));
        return 1;
    }
    static unsigned char aux[4096];
    size_t n = fread(aux, 1, sizeof aux, f);
    int ferr = ferror(f);
    fclose(f);
    if (ferr) {
        printf("AUXV RED /proc/self/auxv read errno=%d (%s)\n", errno, estr(errno));
        return 1;
    }
    if (n == 0 || (n & 15) != 0 || (n & 15) != 0) {
        printf("AUXV RED /proc/self/auxv bytes=%zu not whole pairs\n", n);
        return 1;
    }
    unsigned long pagesz = 0, ehdr_procfs = 0, ehdr_stack = getauxval(33);
    int terminated = 0;
    for (size_t i = 0; i + 16 <= n; i += 16) {
        unsigned long k, v;
        __builtin_memcpy(&k, aux + i, 8);
        __builtin_memcpy(&v, aux + i + 8, 8);
        if (k == 0) { terminated = 1; break; }
        if (k == 6) pagesz = v;         /* AT_PAGESZ */
        if (k == 33) ehdr_procfs = v;   /* AT_SYSINFO_EHDR */
    }
    int bad = 0;
    if (pagesz != 4096) { printf("AUXV RED procfs-auxv AT_PAGESZ=%lu want 4096\n", pagesz); bad = 1; }
    if (!terminated) { printf("AUXV RED procfs-auxv no AT_NULL terminator\n"); bad = 1; }
    if (ehdr_stack != 0 && ehdr_procfs != ehdr_stack) {
        printf("AUXV RED procfs-auxv AT_SYSINFO_EHDR stack=%lu procfs=%lu (one render lies)\n", ehdr_stack, ehdr_procfs);
        bad = 1;
    }
    if (bad) return 1;
    printf("AUXV PASS procfs-auxv bytes=%zu pairs=%zu pagesz=4096", n, n / 16 - 1);
    if (ehdr_stack != 0) printf(" AT_SYSINFO_EHDR matches stack=%lu", ehdr_stack);
    printf("\n");
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
/* amd64: mov eax, 42; ret */
static const unsigned char return_42[] = {0xb8, 42, 0x00, 0x00, 0x00, 0xc3};

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
    memcpy(p, return_42, sizeof return_42);
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

/* A three-page code cache can become executable while clean. Then dirty one
 * page, take it through read-only and back, and retry the whole-range change. */
static int check_wx_range(size_t dirty, int relaxed) {
    const size_t page = 4096, len = 3 * page;
    const int rw = PROT_READ | PROT_WRITE, rx = PROT_READ | PROT_EXEC;
    unsigned char *p = mmap(NULL, len, rw, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (p == MAP_FAILED) {
        printf("WXRANGE RED dirty=%zu mmap errno=%d\n", dirty, errno);
        return 1;
    }

    int failed = 1;
    if (mprotect(p, len, rx) != 0 || mprotect(p, len, rw) != 0) {
        printf("WXRANGE RED dirty=%zu clean RX/RW round trip errno=%d\n", dirty, errno);
        goto done;
    }
    unsigned char *code = p + dirty * page;
    memcpy(code, return_42, sizeof return_42);
    if (mprotect(code, page, PROT_READ) != 0 || mprotect(code, page, rw) != 0) {
        printf("WXRANGE RED dirty=%zu R/RW round trip errno=%d\n", dirty, errno);
        goto done;
    }
    int rc = mprotect(p, len, rx);
    int err = rc == 0 ? 0 : errno;
    if (relaxed ? rc != 0 : (rc != -1 || err != EACCES)) {
        printf("WXRANGE RED dirty=%zu RX range rc=%d errno=%d expected_errno=%d\n",
               dirty, rc, err, relaxed ? 0 : EACCES);
        goto done;
    }
    if (memcmp(code, return_42, sizeof return_42) != 0) {
        printf("WXRANGE RED dirty=%zu code bytes changed across mprotect\n", dirty);
        goto done;
    }

    if (relaxed) {
        int (*fn)(void) = (int (*)(void))code;
        if (fn() != 42) {
            printf("WXRANGE RED dirty=%zu RX code returned the wrong value\n", dirty);
            goto done;
        }
    } else {
        /* Every page must retain write access after a rejected range. A
         * partial application would make a prefix read-only. */
        volatile unsigned char *bytes = p;
        for (size_t i = 0; i < 3; i++) {
            size_t off = i * page + sizeof return_42;
            printf("WXRANGE CHECK dirty=%zu write-after-denial page=%zu\n", dirty, i);
            bytes[off] = 0x5a;
            if (bytes[off] != 0x5a) {
                printf("WXRANGE RED dirty=%zu write did not stick page=%zu\n", dirty, i);
                goto done;
            }
        }
    }
    failed = 0;
done:
    munmap(p, len);
    return failed;
}

static int arm_wxrange(int relaxed) {
    for (size_t dirty = 0; dirty < 3; dirty++) {
        if (check_wx_range(dirty, relaxed)) return 1;
    }
    printf("WXRANGE PASS mode=%s cases=3\n", relaxed ? "relaxed" : "strict");
    return 0;
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


/* ---------------- maps (PHASE-2-PROCFS) ---------------- */
/* /proc/self/maps must exist, parse, and name this process's own image and
 * stack: lines of `<start>-<end> <perms> <offset> <dev> <inode> [path]`, the
 * format sscanf("%lx-%lx") reads. Empty would be the confident lie the glue's
 * doc warns about ("this process has no mappings"); errno=2 was G4's other
 * evidence. The verdict also needs an executable line - the image's text - so
 * an arm attributing a fault rip has something executable to land in. */
static int arm_maps(void) {
    static char buf[262144];
    int fd = open("/proc/self/maps", O_RDONLY);
    if (fd < 0) {
        printf("MAPS RED /proc/self/maps errno=%d (%s)\n", errno, estr(errno));
        return 1;
    }
    size_t off = 0;
    ssize_t g;
    while (off < sizeof buf - 1 && (g = read(fd, buf + off, sizeof buf - 1 - off)) > 0) off += (size_t)g;
    int rerr = errno;
    close(fd);
    if (g < 0) {
        printf("MAPS RED /proc/self/maps read errno=%d (%s)\n", rerr, estr(rerr));
        return 1;
    }
    buf[off] = 0;
    size_t lines = 0, exec_lines = 0, bad = 0;
    for (char *p = buf; *p; ) {
        char *nl = strchr(p, '\n');
        if (!nl) nl = p + strlen(p);
        unsigned long s, e;
        char perms[8] = {0};
        if (sscanf(p, "%lx-%lx %7s", &s, &e, perms) == 3 && e > s
            && (perms[0] == 'r' || perms[0] == '-')
            && (perms[1] == 'w' || perms[1] == '-')
            && (perms[2] == 'x' || perms[2] == '-')) {
            lines++;
            if (perms[2] == 'x') exec_lines++;
            printf("MAPS-LINE: %.*s\n", (int)(nl - p), p);
        } else {
            bad++;
        }
        if (!*nl) break;
        p = nl + 1;
    }
    if (lines == 0 || bad) {
        printf("MAPS RED lines=%zu malformed=%zu bytes=%zu\n", lines, bad, off);
        return 1;
    }
    printf("MAPS PASS lines=%zu exec_lines=%zu bytes=%zu\n", lines, exec_lines, off);
    return 0;
}

/* ---------------- name (PHASE-2-PROCFS: the G9 rip gets an owner) ----------------
 * fork+exec the workload, poll /proc/<pid>/maps every 2 ms until it dies, keep
 * the LAST snapshot (the layout at death), print it whole, then attribute the
 * expected rip from the console's fault line. This is the gate's payoff arm:
 * a bare rip cannot be fixed (REQUIREMENTS G9); a rip inside a named range can.
 * usage: jvmprobe name CMD EXPECTED_RIP_HEX [ARG]...  (rip as written without 0x) */
static int arm_name(char **argv) {
    char *cmd = argv[0];
    unsigned long rip = strtoul(argv[1], NULL, 16);
    char **jargs = argv + 1; /* execv wants argv[0]=cmd then args; jargs[0] is rip now - fix below */
    char *child_argv[64];
    size_t na = 0;
    child_argv[na++] = cmd;
    for (char **a = argv + 2; *a && na < 63; a++) child_argv[na++] = *a;
    child_argv[na] = NULL;
    (void)jargs;
    if (access(cmd, X_OK) != 0) {
        printf("NAME RED not-executable at=%s errno=%d (%s)\n", cmd, errno, estr(errno));
        return 1;
    }
    static char snap[262144];
    size_t snap_len = 0;
    pid_t pid = fork();
    if (pid == 0) {
        execv(cmd, child_argv);
        _exit(127);
    }
    char path[64];
    int polls = 0;
    for (;;) { /* bounded: 30 s of 2 ms polls - java -version dies long before */
        snprintf(path, sizeof path, "/proc/%d/maps", pid);
        int fd = open(path, O_RDONLY);
        if (fd < 0) break; /* ENOENT: the pid dir is gone - dead (or reaped) */
        size_t off = 0;
        ssize_t g;
        while (off < sizeof snap - 1 && (g = read(fd, snap + off, sizeof snap - 1 - off)) > 0) off += (size_t)g;
        close(fd);
        if (off > 0) { snap_len = off; snap[snap_len] = 0; }
        if (++polls > 15000) break;
        usleep(2000);
    }
    int st = 0;
    waitpid(pid, &st, 0);
    printf("NAME INFO exit=%d signal=%d maps_bytes=%zu polls=%d\n",
           WIFEXITED(st) ? WEXITSTATUS(st) : -1, WIFSIGNALED(st) ? WTERMSIG(st) : 0,
           snap_len, polls);
    if (snap_len == 0) {
        printf("NAME MISSING no /proc/<pid>/maps snapshot before death - the diagnostic cannot do its job\n");
        return 1;
    }
    /* Best-effort file copy (PHASE-2-NULLSTORE D35): if sshd ever answers again,
     * the whole snapshot is recoverable; the console verdict never requires it. */
    { int df = open("/tmp/name-maps.log", O_WRONLY | O_CREAT | O_TRUNC, 0644);
      if (df >= 0) { size_t w = 0; while (w < snap_len) { ssize_t k = write(df, snap + w, snap_len - w); if (k <= 0) break; w += (size_t)k; } close(df); } }
    size_t lines = 0, exec_lines = 0;
    for (char *p = snap; *p; ) {
        char *nl = strchr(p, '\n');
        if (!nl) nl = p + strlen(p);
        unsigned long s, e;
        char perms[8] = {0};
        if (sscanf(p, "%lx-%lx %7s", &s, &e, perms) == 3) {
            lines++;
            if (perms[2] == 'x') exec_lines++;
            printf("NAME-MAPS: %.*s\n", (int)(nl - p), p);
        }
        if (!*nl) break;
        p = nl + 1;
    }
    printf("NAME INFO lines=%zu exec_lines=%zu\n", lines, exec_lines);
    for (char *p = snap; *p; ) {
        char *nl = strchr(p, '\n');
        if (!nl) nl = p + strlen(p);
        unsigned long s, e;
        char perms[8] = {0};
        if (sscanf(p, "%lx-%lx %7s", &s, &e, perms) == 3 && rip >= s && rip < e) {
            printf("NAME PASS rip=0x%lx in %08lx-%08lx perms=%s off=0x%lx\n", rip, s, e, perms, rip - s);
            return 0;
        }
        if (!*nl) break;
        p = nl + 1;
    }
    printf("NAME SURPRISE rip=0x%lx not in any captured line (layout moved; the maps above are the record)\n", rip);
    return 2;
}

/* ---------------- mmaparms (PHASE-2-NULLSTORE D35) ----------------
 * The taoxichuan blocker-3 suspects, as contract facts a healthy sshd can carry
 * (no java in the room): (1) mmap failures arrive as MAP_FAILED (-errno), never
 * 0 — suspect 1's "musl hands HotSpot a NULL cache base" shape; (2) the JIT
 * commit shape works: reserve anon PROT_NONE, mprotect a sub-range to RW and
 * write it, mprotect another to RX and call it — suspect 2's reserve/commit
 * contract; (3) under wx-relaxed, W+X maps and executes. Each sub-fact prints
 * its own MMARMS line; the arm's verdict is the AND. */
static int child_mmaparms(void) {
    int fails = 0;
    /* (1) failure conventions */
    errno = 0;
    void *z = mmap(NULL, 0, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (z != MAP_FAILED) { printf("MMARMS RED len0 mmap returned %p (must be MAP_FAILED)\n", z); fails++; }
    else if (errno != EINVAL) { printf("MMARMS RED len0 errno=%d (%s; Linux says EINVAL)\n", errno, estr(errno)); fails++; }
    else printf("MMARMS PASS len0 MAP_FAILED EINVAL\n");
    errno = 0;
    z = mmap(NULL, (size_t)1 << 62, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (z == NULL) { printf("MMARMS RED oversize mmap returned NULL (taoxichuan suspect 1)\n"); fails++; }
    else if (z != MAP_FAILED) { printf("MMARMS RED oversize mmap returned %p (must be MAP_FAILED)\n", z); fails++; }
    else printf("MMARMS PASS oversize MAP_FAILED errno=%d\n", errno);
    /* (2) the code-cache shape: 48 MiB PROT_NONE reserve, commit sub-ranges */
    size_t RES = 48u << 20;
    unsigned char *base = mmap(NULL, RES, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (base == MAP_FAILED || base == NULL) { printf("MMARMS RED reserve errno=%d (%s)\n", errno, estr(errno)); return 1; }
    printf("MMARMS PASS reserve base=%p size=%zu prot=NONE\n", (void *)base, RES);
    if (mprotect(base, 4096, PROT_READ | PROT_WRITE) != 0) {
        printf("MMARMS RED commit-RW errno=%d (%s)\n", errno, estr(errno)); fails++;
    } else {
        *(volatile unsigned int *)base = 0x42;
        if (*(volatile unsigned int *)base != 0x42) { printf("MMARMS RED commit-RW readback\n"); fails++; }
        else printf("MMARMS PASS commit-RW write-readback off=0\n");
    }
    unsigned char *rx = base + 2 * 4096;
    static const unsigned char code[] = { 0xb8, 0x2a, 0x00, 0x00, 0x00, /* mov eax,42 */
                                          0xc3 };                        /* ret */
    /* the JIT cycle: write while RW, flip to RX, call. Writing after the flip
     * faults on every W^X kernel (this arm did, on the metal host, before the
     * fix) — the bugs this arm exists to catch in the KERNEL are a flip that
     * does not take or a call that cannot fetch. */
    if (mprotect(rx, 4096, PROT_READ | PROT_WRITE) != 0) {
        printf("MMARMS RED commit-RW2 errno=%d (%s)\n", errno, estr(errno)); fails++;
    } else {
        memcpy(rx, code, sizeof code);
        if (mprotect(rx, 4096, PROT_READ | PROT_EXEC) != 0) {
            printf("MMARMS RED commit-RX errno=%d (%s)\n", errno, estr(errno)); fails++;
        } else {
            int (*f)(void) = (int (*)(void))rx;
            int v = f();
            printf("MMARMS %s commit-RX call=%d\n", v == 42 ? "PASS" : "RED", v);
            if (v != 42) fails++;
        }
    }
    /* (3) RWX under wx-relaxed: the request's W bit must be HONORED — mmap W+X,
     * write code straight into it (a fault here is this child's SIGSEGV: the
     * silent strip, PHASE-2-HEAPRO), then call it. The old shape read the
     * mapping's perms from /proc/self/maps, but maps shows the glue regions,
     * not the mmap slab's - saw_w stayed -1 and the arm REDd regardless of
     * what the kernel did (the recorded "mmap W+X dropped W" divergence was
     * this probe gap as much as a policy; D42 again: a probe that cannot see
     * the thing it judges reports its own blindness). The write is the test. */
    unsigned char *wx = mmap(NULL, 4096, PROT_READ | PROT_WRITE | PROT_EXEC,
                             MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (wx == MAP_FAILED) { printf("MMARMS INFO rwx mmap errno=%d (%s) (policy refused)\n", errno, estr(errno)); }
    else {
        memcpy(wx, code, sizeof code); /* the write bit, tested not read from maps */
        int (*f2)(void) = (int (*)(void))wx;
        int v = f2();
        printf("MMARMS %s rwx call=%d\n", v == 42 ? "PASS" : "RED", v);
        if (v != 42) fails++;
    }
    munmap(base, RES);
    return fails ? 1 : 0;
}

static int arm_mmaparms(void) {
    pid_t pid = fork();
    if (pid == 0) _exit(child_mmaparms());
    int st = 0;
    waitpid(pid, &st, 0);
    if (WIFSIGNALED(st)) {
        printf("MMARMS RED CRASH signal=%d\n", WTERMSIG(st));
        return 1;
    }
    return WEXITSTATUS(st);
}

/* ---------------- lazyrel (PHASE-2-HEAPRO D46; the release sweep) ----------------
 * The NULLFIX java death was a write to a PRESENT read-only page at a VA whose
 * only event was a MAP_FIXED release: the release sweep is present-only, so the
 * P=0 lazy records a lazy mapping leaves in the leaf slots survive it, and the
 * not-present fault arm (`lazy::fill`) decodes them with no region check — the
 * released range comes back. This arm reproduces that java-free, one fork per
 * sub-arm (the crashy path IS the pass, so the child dies by design):
 *
 *   anon: mmap 2 MiB RW ANON|PRIVATE|NORESERVE (NORESERVE => lazy whatever the
 *         size), touch two pages, munmap, write the released VA.
 *         fixed: SIGSEGV -> LAZYREL PASS anon-release-kills
 *         buggy: lazy::fill serves a fresh zero page, the write succeeds
 *                -> LAZYREL RED stale-anon-write
 *   file: mmap 2 MiB RO PRIVATE of /bin/jvmprobe (demand-paged file records,
 *         MMAP_FILE_BACKED_LAZY), read byte 0, munmap, read the released VA.
 *         fixed: SIGSEGV -> LAZYREL PASS file-release-kills
 *         buggy: the stale byte reads back -> LAZYREL RED stale-file-read
 *                (a write there would be the java death shape)
 * Every marker the child prints names its point, so a SIGSEGV is attributable:
 * the parent reports signal deaths relative to the last marker. */
#define LAZYREL_LEN (2ul * 1024 * 1024)

static int child_lazyrel_anon(void) {
    volatile char *p = mmap(NULL, LAZYREL_LEN, PROT_READ | PROT_WRITE,
                            MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0);
    if (p == MAP_FAILED) {
        printf("LAZYREL RED anon-mmap errno=%d (%s)\n", errno, estr(errno));
        return 1;
    }
    p[0] = 1;
    p[4096] = 1;
    if (p[0] != 1 || p[4096] != 1) {
        printf("LAZYREL RED anon-touch readback=%d/%d\n", p[0], p[4096]);
        return 1;
    }
    if (munmap((void *)p, LAZYREL_LEN) != 0) {
        printf("LAZYREL RED anon-munmap errno=%d (%s)\n", errno, estr(errno));
        return 1;
    }
    printf("LAZYREL INFO anon released; writing (fixed kernel: SIGSEGV expected)\n");
    p[0] = 2;
    printf("LAZYREL RED stale-anon-write survived readback=%d (lazy::fill served the released range)\n", p[0]);
    return 1;
}

static int child_lazyrel_file(void) {
    int fd = open("/bin/jvmprobe", O_RDONLY);
    if (fd < 0) {
        printf("LAZYREL RED file-open errno=%d (%s)\n", errno, estr(errno));
        return 1;
    }
    volatile char *q = mmap(NULL, LAZYREL_LEN, PROT_READ, MAP_PRIVATE, fd, 0);
    if (q == MAP_FAILED) {
        printf("LAZYREL RED file-mmap errno=%d (%s)\n", errno, estr(errno));
        close(fd);
        return 1;
    }
    close(fd);
    int b = q[0];
    if (b < 0) {
        printf("LAZYREL RED file-touch read=%d\n", b);
        return 1;
    }
    if (munmap((void *)q, LAZYREL_LEN) != 0) {
        printf("LAZYREL RED file-munmap errno=%d (%s)\n", errno, estr(errno));
        return 1;
    }
    printf("LAZYREL INFO file released; reading (fixed kernel: SIGSEGV expected)\n");
    b = q[0];
    printf("LAZYREL RED stale-file-read byte=0x%x (lazy::fill served the released range)\n", b);
    return 1;
}

/* One fork per sub-arm: the PASS path is a signal death, so the parent judges
 * the wait status against the child's last printed marker. */
static int run_lazyrel_child(int (*child)(void), const char *what) {
    pid_t pid = fork();
    if (pid == 0) _exit(child());
    int st = 0;
    waitpid(pid, &st, 0);
    if (WIFSIGNALED(st)) {
        if (WTERMSIG(st) == SIGSEGV) {
            printf("LAZYREL PASS %s-release-kills (SIGSEGV: the released range stayed dead)\n", what);
            return 0;
        }
        printf("LAZYREL RED %s CRASH signal=%d (not SIGSEGV - not the designed death)\n", what, WTERMSIG(st));
        return 1;
    }
    /* The child printed its own RED line naming the stale-fill survival. */
    return WEXITSTATUS(st);
}

static int arm_lazyrel(void) {
    int fails = run_lazyrel_child(child_lazyrel_anon, "anon");
    fails += run_lazyrel_child(child_lazyrel_file, "file");
    if (!fails)
        printf("LAZYREL PASS both released ranges stayed dead (lazy records swept)\n");
    return fails ? 1 : 0;
}

/* ---------------- sysinfo (PHASE-2-NULLSTORE D35; G7's feed) ----------------
 * `sysinfo(2)` was ENOSYS in phase 1 and the merged tree presumed it served over
 * by main's memory changes (REQUIREMENTS G9 row, "unverified: no sysinfo probe
 * ran"). This arm makes the presumption measured: served with sane totals, or
 * INFO errno=N for the record. G7 owns the verdict; this gate records. */
static int arm_sysinfo(void) {
    struct sysinfo si;
    memset(&si, 0, sizeof si);
    long r = sysinfo(&si);
    if (r != 0) {
        printf("SYSINFO INFO errno=%d (%s)\n", errno, estr(errno));
        return 1;
    }
    printf("SYSINFO PASS uptime=%lu totalram=%lu freeram=%lu mem_unit=%u\n",
           si.uptime, si.totalram, si.freeram, si.mem_unit);
    if (si.totalram == 0) {
        printf("SYSINFO RED totalram=0 (a sizing consumer divides by this)\n");
        return 1;
    }
    return 0;
}

/* ---------------- regpreserve (PHASE-2-NULLSTORE) ----------------
 * The kill-site evidence: JLI_Launch passed a real &sbuf (lea -0xc0(%rbp)) but
 * fstatat received buf=0; the fatal syscall's return also restored rsi/r9 to
 * their fstatat-ENTRY values instead of the syscall's. Class: user GPRs change
 * across a ring transition (fault or syscall). This arm sets canaries in the
 * caller-saved registers and reads them back after (a) a lazy fault (the first
 * store to a fresh anonymous page — the fill path) and (b) a plain syscall
 * (getppid; rcx/r11 are architecturally clobbered by the `syscall`
 * instruction itself, so they are excluded from (b) only). HOST = the ABI
 * truth; GUEST = the suspect. VERDICT: REGPASS kept | REGRED reg=..!=.. . */
static int child_regpreserve(void) {
    int fails = 0;
    void *p = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (p == MAP_FAILED) { printf("REGRED mmap errno=%d\n", errno); return 1; }
    unsigned long addr = (unsigned long)p;

    /* (a) lazy fault: canaries -> first store to the fresh page (faults, the
     * kernel fills, retries) -> read back. */
    unsigned long o8, o9, o10, o11, osi, odx, odi, ocx;
    asm volatile(
        "movabs $0x1111111111111001, %%r8\n\t"
        "movabs $0x2222222222222002, %%r9\n\t"
        "movabs $0x3333333333333003, %%r10\n\t"
        "movabs $0x4444444444444004, %%r11\n\t"
        "movabs $0x5555555555555005, %%rdi\n\t"
        "movabs $0x6666666666666006, %%rsi\n\t"
        "movabs $0x7777777777777007, %%rdx\n\t"
        "movabs $0x8888888888888008, %%rcx\n\t"
        "movb $0x42, (%[ad])\n\t"
        "movq %%r8, %[o8]\n\t"
        "movq %%r9, %[o9]\n\t"
        "movq %%r10, %[o10]\n\t"
        "movq %%r11, %[o11]\n\t"
        "movq %%rdi, %[odi]\n\t"
        "movq %%rsi, %[osi]\n\t"
        "movq %%rdx, %[odx]\n\t"
        "movq %%rcx, %[ocx]\n\t"
        : [o8]"=m"(o8), [o9]"=m"(o9), [o10]"=m"(o10), [o11]"=m"(o11),
          [odi]"=m"(odi), [osi]"=m"(osi), [odx]"=m"(odx), [ocx]"=m"(ocx)
        : [ad]"r"(addr)
        : "rax", "r8", "r9", "r10", "r11", "rdi", "rsi", "rdx", "rcx", "memory", "cc");
    struct { const char *n; unsigned long got, want; } fa[] = {
        { "r8", o8, 0x1111111111111001UL }, { "r9", o9, 0x2222222222222002UL },
        { "r10", o10, 0x3333333333333003UL }, { "r11", o11, 0x4444444444444004UL },
        { "rdi", odi, 0x5555555555555005UL }, { "rsi", osi, 0x6666666666666006UL },
        { "rdx", odx, 0x7777777777777007UL }, { "rcx", ocx, 0x8888888888888008UL },
    };
    for (int i = 0; i < 8; i++) {
        if (fa[i].got == fa[i].want) printf("REGPASS %s kept across lazy-fault store\n", fa[i].n);
        else { printf("REGRED %s=0x%lx != 0x%lx across lazy-fault store\n", fa[i].n, fa[i].got, fa[i].want); fails++; }
    }

    /* (b) syscall boundary: canaries -> getppid -> read back (rcx/r11 are the
     * `syscall` instruction's own clobbers, excluded). */
    unsigned long s8, s9, s10, sdi, ssi, sdx;
    asm volatile(
        "movabs $0x1111111111111001, %%r8\n\t"
        "movabs $0x2222222222222002, %%r9\n\t"
        "movabs $0x3333333333333003, %%r10\n\t"
        "movabs $0x5555555555555005, %%rdi\n\t"
        "movabs $0x6666666666666006, %%rsi\n\t"
        "movabs $0x7777777777777007, %%rdx\n\t"
        "mov $110, %%eax\n\t"
        "syscall\n\t"
        "movq %%r8, %[s8]\n\t"
        "movq %%r9, %[s9]\n\t"
        "movq %%r10, %[s10]\n\t"
        "movq %%rdi, %[sdi]\n\t"
        "movq %%rsi, %[ssi]\n\t"
        "movq %%rdx, %[sdx]\n\t"
        : [s8]"=m"(s8), [s9]"=m"(s9), [s10]"=m"(s10),
          [sdi]"=m"(sdi), [ssi]"=m"(ssi), [sdx]"=m"(sdx)
        :
        : "rax", "r8", "r9", "r10", "rdi", "rsi", "rdx", "rcx", "r11", "memory", "cc");
    struct { const char *n; unsigned long got, want; } sb[] = {
        { "r8", s8, 0x1111111111111001UL }, { "r9", s9, 0x2222222222222002UL },
        { "r10", s10, 0x3333333333333003UL },
        { "rdi", sdi, 0x5555555555555005UL }, { "rsi", ssi, 0x6666666666666006UL },
        { "rdx", sdx, 0x7777777777777007UL },
    };
    for (int i = 0; i < 6; i++) {
        if (sb[i].got == sb[i].want) printf("REGPASS %s kept across syscall\n", sb[i].n);
        else { printf("REGRED %s=0x%lx != 0x%lx across syscall\n", sb[i].n, sb[i].got, sb[i].want); fails++; }
    }
    /* (c) THE SHAPE FROM THE DEATH SITE: the kernel itself writes the result
     * into an UNFAULTED user page (stat's 144-byte copy out; the ring-0 fault
     * → fill → retry path). Canaries around it. */
    printf("REGINFO c-start\n");
    void *q = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (q == MAP_FAILED) { printf("REGRED mmap2 errno=%d\n", errno); return 1; }
    unsigned long qaddr = (unsigned long)q;
    printf("REGINFO c-mmap q=%p\n", q);
    unsigned long k8, k9, k10, kdi, ksi, kdx, kr;
    asm volatile(
        "movabs $0x1111111111111001, %%r8\n\t"
        "movabs $0x2222222222222002, %%r9\n\t"
        "movabs $0x3333333333333003, %%r10\n\t"
        "movabs $0x5555555555555005, %%rdi\n\t"
        "movabs $0x6666666666666006, %%rsi\n\t"
        "movabs $0x7777777777777007, %%rdx\n\t"
        "mov %[pa], %%rdi\n\t"        /* stat(path, q): q never touched */
        "mov %[qb], %%rsi\n\t"
        "mov $4, %%eax\n\t"
        "syscall\n\t"
        "movq %%rax, %[kr]\n\t"
        "movq %%r8, %[k8]\n\t"
        "movq %%r9, %[k9]\n\t"
        "movq %%r10, %[k10]\n\t"
        "movq %%rdi, %[kdi]\n\t"
        "movq %%rsi, %[ksi]\n\t"
        "movq %%rdx, %[kdx]\n\t"
        : [k8]"=m"(k8), [k9]"=m"(k9), [k10]"=m"(k10),
          [kdi]"=m"(kdi), [ksi]"=m"(ksi), [kdx]"=m"(kdx), [kr]"=m"(kr)
        : [pa]"r"(addr), [qb]"r"(qaddr)
        : "rax", "r8", "r9", "r10", "rdi", "rsi", "rdx", "rcx", "r11", "memory", "cc");
    printf("REGINFO c-syscall stat returned %ld (rdx canary %s)\n", (long)kr,
           kdx == 0x7777777777777007UL ? "kept" : "LOST");
    /* PHASE-2-NULLFIX: the truth for a preserved register is its value at the
     * syscall ENTRY, and this asm loads rdi/rsi with the arguments itself
     * (mov path/ buf) — so the entry truth for rdi/rsi is path/q, not the
     * canaries those two movabs put there a moment earlier (unsatisfiable on
     * any kernel, host included). The clobber this arm hunts shifts every
     * slot by one fold argument (rdi=AT_FDCWD, rsi=path, rdx=buf) and zeroes
     * r8/r9/r10 — a wrong-by-entry-value check still fails on all six. */
    struct { const char *n; unsigned long got, want; } kc[] = {
        { "r8", k8, 0x1111111111111001UL }, { "r9", k9, 0x2222222222222002UL },
        { "r10", k10, 0x3333333333333003UL },
        { "rdi", kdi, (unsigned long)addr },   /* entry value: the asm loaded the path into rdi */
        { "rsi", ksi, (unsigned long)qaddr },  /* entry value: the asm loaded the buf into rsi */
        { "rdx", kdx, 0x7777777777777007UL },
    };
    for (int i = 0; i < 6; i++) {
        if (kc[i].got == kc[i].want) printf("REGPASS %s kept across kernel-write-to-unfaulted-page\n", kc[i].n);
        else { printf("REGRED %s=0x%lx != 0x%lx across kernel-write-to-unfaulted-page\n", kc[i].n, kc[i].got, kc[i].want); fails++; }
    }
    munmap(q, 4096);
    munmap(p, 4096);
    return fails ? 1 : 0;
}

static int arm_regpreserve(void) {
    pid_t pid = fork();
    if (pid == 0) _exit(child_regpreserve());
    int st = 0;
    waitpid(pid, &st, 0);
    if (WIFSIGNALED(st)) {
        printf("REGRED CRASH signal=%d\n", WTERMSIG(st));
        return 1;
    }
    return WEXITSTATUS(st);
}

/* ---------------- main ---------------- */
static void usage(void) {
    fprintf(stderr,
            "usage: jvmprobe getcpu|auxv|cycles|rwx|maps|mmaparms|sysinfo|java CMD [ARG]...\n"
            "       jvmprobe name CMD EXPECTED_RIP_HEX [ARG]...\n"
            "       jvmprobe lazyrel\n"
            "       jvmprobe wxrange strict|relaxed\n"
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
    if (!strcmp(arm, "wxrange") && argc == 3) {
        if (!strcmp(argv[2], "strict")) return arm_wxrange(0);
        if (!strcmp(argv[2], "relaxed")) return arm_wxrange(1);
    }
    if (!strcmp(arm, "maps")) return arm_maps();
    if (!strcmp(arm, "mmaparms")) return arm_mmaparms();
    if (!strcmp(arm, "sysinfo")) return arm_sysinfo();
    if (!strcmp(arm, "regpreserve")) return arm_regpreserve();
    if (!strcmp(arm, "lazyrel")) return arm_lazyrel();
    if (!strcmp(arm, "name")) return argc < 4 ? usage(), 2 : arm_name(argv + 2);
    if (!strcmp(arm, "java")) return argc < 3 ? usage(), 2 : arm_java(argv + 2);
    return usage(), 2;
}
