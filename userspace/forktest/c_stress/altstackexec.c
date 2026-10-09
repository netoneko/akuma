/* altstackexec.c — is a thread's sigaltstack what Linux says it is after
 * fork, execve and thread churn?
 *
 * Why: on the ryzen metal (2026-10-09) a Chromium child died of its own CHECK
 * `int3` with `[signal] sig 5 declined: frame write to 0x100002e38 failed`:
 * the kernel placed the SIGTRAP frame on an alternate stack that lay in a
 * read-only file mapping of that process. Chromium installs an altstack on
 * every thread and its helpers are fork+exec'd, and the kernel cleared the
 * altstack on exec only in the `Process`, not in the per-thread-slot copy that
 * delivery reads.
 *
 * Linux semantics checked (sigaltstack(2), execve(2), clone(2)):
 *   fork         the child inherits the caller's altstack and a SA_ONSTACK
 *                handler runs on it
 *   execve       the altstack is disabled (SS_DISABLE) in the new image
 *   pthread      a new thread starts with SS_DISABLE, however many threads
 *                with altstacks came and went before it
 * Each case raises SIGTRAP with `int3` and checks the SA_ONSTACK handler ran
 * (on the altstack when there is one, on the thread stack when not).
 *
 * The parent's altstack is at a fixed high address (ALT) that the exec'd image
 * never maps, so a stale altstack after exec is a frame write to unmapped
 * memory: on a kernel with the bug the child dies of SIGTRAP instead of
 * printing its PASS line.
 *
 * Build: x86_64-linux-musl-gcc -O2 -static -pthread -o altstackexec altstackexec.c
 * Run:   ./altstackexec           (prints PASS/FAIL per case, exit 0 iff all pass)
 */
#define _GNU_SOURCE
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

#define ALT ((void *)0x5a5a00000000ULL)
#define ALT_SIZE (64 * 1024)

static volatile sig_atomic_t hits;
static volatile uintptr_t handler_sp;

static void on_trap(int sig, siginfo_t *si, void *uc) {
    (void)sig; (void)si; (void)uc;
    int here;
    handler_sp = (uintptr_t)&here;
    hits++;
}

static void install(void) {
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_sigaction = on_trap;
    sa.sa_flags = SA_SIGINFO | SA_ONSTACK;
    sigemptyset(&sa.sa_mask);
    sigaction(SIGTRAP, &sa, NULL);
}

static int on_alt(uintptr_t sp, stack_t *ss) {
    return sp >= (uintptr_t)ss->ss_sp && sp < (uintptr_t)ss->ss_sp + ss->ss_size;
}

/* Raise SIGTRAP; return 0 iff the handler ran, on the altstack iff `want_alt`. */
static int trap_check(const char *who, int want_alt) {
    stack_t cur;
    sigaltstack(NULL, &cur);
    int before = hits;
#if defined(__x86_64__)
    __asm__ volatile("int3"); /* what a Chromium CHECK is */
#else
    raise(SIGTRAP);
#endif
    int ran = hits == before + 1;
    int alt = !(cur.ss_flags & SS_DISABLE) && on_alt(handler_sp, &cur);
    int ok = ran && alt == want_alt;
    printf("altstackexec: %-28s %s (flags=%d sp=%p handler=%s, %s)\n", who, ok ? "PASS" : "FAIL",
           cur.ss_flags, cur.ss_sp, ran ? "ran" : "did not run", alt ? "on altstack" : "on thread stack");
    fflush(stdout);
    return ok ? 0 : 1;
}

static void *thread_with_alt(void *arg) {
    (void)arg;
    void *mem = malloc(ALT_SIZE);
    stack_t ss = { .ss_sp = mem, .ss_size = ALT_SIZE, .ss_flags = 0 };
    sigaltstack(&ss, NULL);
    int here;
    (void)here;
    /* Exit with it still set, as Chromium's threads do. The memory leaks on
     * purpose: freeing it would let a later thread's stale altstack land in
     * live heap and pass by accident. */
    return NULL;
}

static void *thread_fresh(void *arg) {
    int *fail = arg;
    stack_t cur;
    sigaltstack(NULL, &cur);
    if (!(cur.ss_flags & SS_DISABLE)) {
        printf("altstackexec: new thread inherited altstack sp=%p flags=%d FAIL\n", cur.ss_sp, cur.ss_flags);
        *fail = 1;
    }
    *fail |= trap_check("pthread after churn", 0);
    return NULL;
}

int main(int argc, char **argv) {
    install();
    if (argc > 1 && strcmp(argv[1], "exec-child") == 0) {
        stack_t cur;
        sigaltstack(NULL, &cur);
        int ok = (cur.ss_flags & SS_DISABLE) != 0;
        printf("altstackexec: %-28s %s (flags=%d sp=%p)\n", "exec'd image altstack", ok ? "PASS" : "FAIL",
               cur.ss_flags, cur.ss_sp);
        fflush(stdout);
        return trap_check("exec'd image trap", 0) | !ok;
    }

    int fails = 0;
    void *mem = mmap(ALT, ALT_SIZE, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE, -1, 0);
    if (mem != ALT) { perror("mmap ALT"); return 2; }
    stack_t ss = { .ss_sp = mem, .ss_size = ALT_SIZE, .ss_flags = 0 };
    if (sigaltstack(&ss, NULL)) { perror("sigaltstack"); return 2; }
    fails |= trap_check("parent", 1);

    /* fork: inherited, usable. */
    pid_t p = fork();
    if (p == 0) _exit(trap_check("fork child", 1));
    int st;
    waitpid(p, &st, 0);
    if (!WIFEXITED(st) || WEXITSTATUS(st)) {
        printf("altstackexec: fork child ended %s %d FAIL\n", WIFSIGNALED(st) ? "by signal" : "with status",
               WIFSIGNALED(st) ? WTERMSIG(st) : WEXITSTATUS(st));
        fails = 1;
    }

    /* fork + exec: disabled in the new image. */
    p = fork();
    if (p == 0) {
        execl("/proc/self/exe", argv[0], "exec-child", (char *)NULL);
        execl(argv[0], argv[0], "exec-child", (char *)NULL);
        _exit(3);
    }
    waitpid(p, &st, 0);
    if (!WIFEXITED(st) || WEXITSTATUS(st)) {
        printf("altstackexec: exec child ended %s %d FAIL\n", WIFSIGNALED(st) ? "by signal" : "with status",
               WIFSIGNALED(st) ? WTERMSIG(st) : WEXITSTATUS(st));
        fails = 1;
    }

    /* thread churn: 64 threads that set an altstack and exit, then a fresh one. */
    for (int i = 0; i < 64; i++) {
        pthread_t t;
        pthread_create(&t, NULL, thread_with_alt, NULL);
        pthread_join(t, NULL);
    }
    int tfail = 0;
    pthread_t t;
    pthread_create(&t, NULL, thread_fresh, &tfail);
    pthread_join(t, NULL);
    fails |= tfail;

    /* The parent's own altstack survived all of it. */
    fails |= trap_check("parent after churn", 1);
    printf("altstackexec: %s\n", fails ? "FAIL" : "all PASS");
    return fails;
}
