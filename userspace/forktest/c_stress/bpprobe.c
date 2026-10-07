/*
 * bpprobe: what a ring-3 `int3` becomes. Chromium's CHECK failures are
 * `int3`, so this decides whether a crash reads as a CHECK or a segfault.
 * Linux: the handler runs with SIGTRAP / si_code SI_KERNEL (0x80), and a child
 * with no handler dies of signal 5. (The other ring-3 exceptions are
 * trapprobe.c's.)
 */
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

static volatile int got_sig, got_code;
static void on_trap(int sig, siginfo_t *si, void *uc) {
    (void)uc;
    got_sig = sig;
    got_code = si->si_code;
}

int main(void) {
    int fails = 0;
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_sigaction = on_trap;
    sa.sa_flags = SA_SIGINFO;
    sigaction(SIGTRAP, &sa, NULL);
    __asm__ volatile("int3");
    printf("bpprobe: handled int3 -> sig=%d si_code=0x%x (want 5, 0x80)\n", got_sig, got_code);
    if (got_sig != SIGTRAP || got_code != 0x80) fails++;

    pid_t p = fork();
    if (p == 0) {
        signal(SIGTRAP, SIG_DFL);
        __asm__ volatile("int3");
        _exit(0);
    }
    int st = 0;
    waitpid(p, &st, 0);
    printf("bpprobe: unhandled int3 -> signaled=%d sig=%d (want 1, 5)\n",
           WIFSIGNALED(st), WIFSIGNALED(st) ? WTERMSIG(st) : -1);
    if (!WIFSIGNALED(st) || WTERMSIG(st) != SIGTRAP) fails++;
    printf("bpprobe: %s\n", fails ? "FAIL" : "PASS");
    return fails != 0;
}
