/*
 * jit_probe — can a userspace JIT run on this kernel under its W^X policy?
 *
 * Question (from netoneko/akuma-cli-wgpu, docs/fbdev-wgpu-plan.md §5b): the
 * kernel refuses PROT_WRITE|PROT_EXEC in one call (amd64/src/mm.rs sys_mmap and
 * sys_mprotect), so the plan assumed a JIT needs memfd_create dual-mapping.
 * But cranelift-jit and most JITs never hold a W+X page: they mmap RW, emit
 * code, mprotect to R+X, run, and (to patch) mprotect back to RW. If that
 * sequence works, a shader JIT needs no kernel change at all.
 *
 * Arms, each run in a forked child so a SIGSEGV is reported, not fatal:
 *   1 wx_mmap      mmap(RWX) — expected EINVAL (policy, confirms the baseline)
 *   2 rw_to_rx     mmap RW, write `mov eax,42; ret`, mprotect RX, call -> 42
 *   3 wx_mprotect  mprotect(RWX) on a live page — expected EINVAL
 *   4 rewrite      RX -> RW -> rewrite code -> RX -> call, twice (re-JIT cycle)
 *   5 fork_exec    RX page survives fork; child calls it
 *   6 speed        1e8-iteration native loop in JITed code, ns/iter
 *   7 mmap_rx_fd   mmap(PROT_EXEC) of an anonymous page directly (RX, no RW
 *                  phase) — what a loader does; informational
 *
 * Exit 0 iff arms 2, 4, 5, 6 pass (the ones a JIT needs). Arms 1 and 3 are
 * reported but do not gate the exit code: they document policy.
 *
 * Build: userspace/jitprobe/c/build.sh   (x86_64 musl static)
 */
#define _GNU_SOURCE
#include <errno.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define PG 4096

static const uint8_t RET_N[] = { 0xB8, 0, 0, 0, 0, 0xC3 };          /* mov eax,imm32; ret */
/* xor rax,rax; L: add rax,rdi; dec rdi; jnz L; ret   -> sum 1..rdi */
static const uint8_t LOOP[] = { 0x48,0x31,0xC0, 0x48,0x01,0xF8, 0x48,0xFF,0xCF, 0x75,0xF8, 0xC3 };

static void emit_ret(uint8_t *p, uint32_t n) { memcpy(p, RET_N, sizeof RET_N); memcpy(p + 1, &n, 4); }
static uint64_t now_ns(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return t.tv_sec * 1000000000ull + t.tv_nsec; }
static void *rw_page(void) { return mmap(0, PG, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0); }

/* each arm returns 0 = pass; runs in the child */
static int a_wx_mmap(void) {
    void *p = mmap(0, PG, PROT_READ | PROT_WRITE | PROT_EXEC, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (p == MAP_FAILED) { printf("  mmap(RWX) refused errno=%d (%s)\n", errno, strerror(errno)); return 0; }
    printf("  mmap(RWX) ALLOWED\n"); return 0;
}
static int a_rw_to_rx(void) {
    uint8_t *p = rw_page(); if (p == MAP_FAILED) { printf("  mmap RW failed errno=%d\n", errno); return 1; }
    emit_ret(p, 42);
    if (mprotect(p, PG, PROT_READ | PROT_EXEC)) { printf("  mprotect RX failed errno=%d (%s)\n", errno, strerror(errno)); return 1; }
    int r = ((int (*)(void))p)();
    printf("  called JITed fn, got %d\n", r); return r != 42;
}
static int a_wx_mprotect(void) {
    uint8_t *p = rw_page(); if (p == MAP_FAILED) return 1;
    int rc = mprotect(p, PG, PROT_READ | PROT_WRITE | PROT_EXEC);
    printf("  mprotect(RWX) %s errno=%d\n", rc ? "refused" : "ALLOWED", rc ? errno : 0); return 0;
}
static int a_rewrite(void) {
    uint8_t *p = rw_page(); if (p == MAP_FAILED) return 1;
    for (uint32_t i = 1; i <= 2; i++) {
        emit_ret(p, 100 + i);
        if (mprotect(p, PG, PROT_READ | PROT_EXEC)) { printf("  ->RX #%u failed errno=%d\n", i, errno); return 1; }
        int r = ((int (*)(void))p)();
        if (r != (int)(100 + i)) { printf("  cycle %u: got %d want %u (stale code?)\n", i, r, 100 + i); return 1; }
        if (mprotect(p, PG, PROT_READ | PROT_WRITE)) { printf("  ->RW #%u failed errno=%d\n", i, errno); return 1; }
    }
    printf("  2 RW->RX->RW cycles, fresh code each time\n"); return 0;
}
static int a_fork_exec(void) {
    uint8_t *p = rw_page(); if (p == MAP_FAILED) return 1;
    emit_ret(p, 7); if (mprotect(p, PG, PROT_READ | PROT_EXEC)) return 1;
    pid_t k = fork(); if (k < 0) { printf("  fork failed errno=%d\n", errno); return 1; }
    if (k == 0) _exit(((int (*)(void))p)() == 7 ? 0 : 3);
    int st; waitpid(k, &st, 0);
    printf("  child status=0x%x\n", st); return !(WIFEXITED(st) && WEXITSTATUS(st) == 0);
}
static int a_speed(void) {
    uint8_t *p = rw_page(); if (p == MAP_FAILED) return 1;
    memcpy(p, LOOP, sizeof LOOP);
    if (mprotect(p, PG, PROT_READ | PROT_EXEC)) { printf("  mprotect RX failed errno=%d\n", errno); return 1; }
    uint64_t n = 100000000ull, t0 = now_ns();
    uint64_t r = ((uint64_t (*)(uint64_t))p)(n);
    uint64_t dt = now_ns() - t0;
    printf("  sum(1..%llu)=%llu (want %llu)  %.3f ns/iter  %.2f ms\n", (unsigned long long)n,
           (unsigned long long)r, (unsigned long long)(n * (n + 1) / 2), (double)dt / n, dt / 1e6);
    return r != n * (n + 1) / 2;
}
static int a_mmap_rx(void) {
    void *p = mmap(0, PG, PROT_READ | PROT_EXEC, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    printf("  mmap(R+X anon) %s errno=%d\n", p == MAP_FAILED ? "refused" : "ok", p == MAP_FAILED ? errno : 0); return 0;
}

struct arm { const char *name; int (*fn)(void); int gates; } arms[] = {
    {"wx_mmap", a_wx_mmap, 0}, {"rw_to_rx", a_rw_to_rx, 1}, {"wx_mprotect", a_wx_mprotect, 0},
    {"rewrite", a_rewrite, 1}, {"fork_exec", a_fork_exec, 1}, {"speed", a_speed, 1}, {"mmap_rx_anon", a_mmap_rx, 0},
};

int main(void) {
    int fail = 0;
    for (unsigned i = 0; i < sizeof arms / sizeof *arms; i++) {
        printf("[%s]\n", arms[i].name); fflush(stdout);
        pid_t k = fork();
        if (k < 0) { printf("  fork failed errno=%d\n", errno); fail |= arms[i].gates; continue; }
        if (k == 0) _exit(arms[i].fn());
        int st; waitpid(k, &st, 0);
        const char *v = WIFSIGNALED(st) ? "CRASH" : WEXITSTATUS(st) ? "FAIL" : "PASS";
        if (WIFSIGNALED(st)) printf("  killed by signal %d\n", WTERMSIG(st));
        printf("  => %s%s\n", v, arms[i].gates ? "" : " (informational)");
        if (arms[i].gates && strcmp(v, "PASS")) fail = 1;
    }
    printf(fail ? "JITPROBE FAIL\n" : "JITPROBE OK\n");
    return fail;
}
