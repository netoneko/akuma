/* abortsig.c — does abort() actually get its SIGABRT delivered?
 *
 * Disassembling the `-j4` self-host crash showed the faulting PC is musl's
 * `abort()` tail (docs/archive/SELFHOST_DEVBOX_SMOLTCP.md):
 *
 *     bl   raise                    ; raise(SIGABRT)
 *     rt_sigaction(SIGABRT, SIG_DFL)
 *     tkill(self->tid, SIGABRT)
 *     rt_sigprocmask(SIG_UNBLOCK, {SIGABRT})
 *     strb wzr, [x0]                ; a_crash() -- x0 = 0
 *
 * i.e. the observed "SIGSEGV at FAR=0" is not a crash at all: it is musl giving
 * up after FOUR attempts to die by SIGABRT did nothing. That reframes the whole
 * failure — the SIGABRT and SIGSEGV reports cargo alternates between are the
 * SAME event, and the kernel bug is a signal that is raised but never delivered.
 *
 * Every victim in the build was a thread younger than 0.05s, so the phases below
 * separate "abort works at all" from "abort works on a freshly spawned thread".
 * The parent reports the delivered signal, so the two outcomes are distinguishable:
 *
 *     WTERMSIG == SIGABRT (6)  -> delivery works
 *     WTERMSIG == SIGSEGV (11) -> musl reached a_crash(); SIGABRT was NOT delivered
 *
 * Build on the host:
 *   aarch64-linux-musl-gcc -O2 -static -o abortsig abortsig.c
 */
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <signal.h>
#include <pthread.h>
#include <sys/wait.h>
#include <errno.h>

static void *abort_from_new_thread(void *arg)
{
    (void)arg;
    abort();
    return NULL;
}

static void *spin_then_abort(void *arg)
{
    (void)arg;
    /* Do a little work first so the abort does not land in the very first
     * instructions of the thread — distinguishes "brand new" from "running". */
    for (volatile int i = 0; i < 100000; i++) { }
    abort();
    return NULL;
}

static const char *signame(int s)
{
    if (s == SIGABRT) return "SIGABRT (delivery OK)";
    if (s == SIGSEGV) return "SIGSEGV (musl a_crash -- SIGABRT NOT DELIVERED)";
    return "other";
}

static int run_case(const char *name, int mode)
{
    fflush(stdout);
    pid_t pid = fork();
    if (pid < 0) { printf("[%s] fork failed\n", name); return 1; }
    if (pid == 0) {
        if (mode == 0) {
            abort();                       /* main thread */
        } else if (mode == 1) {
            pthread_t t;
            pthread_create(&t, NULL, abort_from_new_thread, NULL);
            pthread_join(t, NULL);
        } else {
            pthread_t t;
            pthread_create(&t, NULL, spin_then_abort, NULL);
            pthread_join(t, NULL);
        }
        _exit(55);                         /* abort() must never return */
    }
    int st = 0;
    if (waitpid(pid, &st, 0) != pid) { printf("[%s] waitpid failed\n", name); return 1; }
    if (WIFSIGNALED(st)) {
        int s = WTERMSIG(st);
        printf("[%s] killed by signal %d -- %s\n", name, s, signame(s));
        return s == SIGABRT ? 0 : 1;
    }
    printf("[%s] exited %d WITHOUT dying from a signal (abort returned!)\n",
           name, WEXITSTATUS(st));
    return 1;
}

int main(void)
{
    setvbuf(stdout, NULL, _IOLBF, 0);
    printf("=== ABORTSIG start ===\n");
    int bad = 0;
    bad |= run_case("A main-thread abort   ", 0);
    bad |= run_case("B new-thread abort    ", 1);
    bad |= run_case("C thread-then-abort   ", 2);
    /* Repeat the new-thread case: the build's failure was intermittent. */
    int fails = 0;
    for (int i = 0; i < 20; i++) {
        pid_t pid = fork();
        if (pid == 0) {
            pthread_t t;
            pthread_create(&t, NULL, abort_from_new_thread, NULL);
            pthread_join(t, NULL);
            _exit(55);
        }
        int st = 0;
        waitpid(pid, &st, 0);
        if (!WIFSIGNALED(st) || WTERMSIG(st) != SIGABRT) fails++;
    }
    printf("[D] 20x new-thread abort: %d did NOT die by SIGABRT\n", fails);
    bad |= (fails != 0);
    printf("=== ABORTSIG DONE — %s ===\n", bad ? "SIGABRT DELIVERY BROKEN" : "all aborts delivered");
    return bad;
}
