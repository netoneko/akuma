/*
 * sigspin: how long does a signal take to reach a thread that never makes a
 * syscall?
 *
 * A pure compute loop leaves user mode only when an interrupt takes it there,
 * so a kernel that delivers pending signals only on syscall return never
 * delivers to it at all: `kill -TERM` of a spinner does nothing, and a handler
 * never runs. Linux delivers on every return to user mode, interrupts included,
 * so every case here finishes in about one tick.
 *
 * Cases (each run ROUNDS times; the child is forked fresh every round):
 *   term          single-threaded spinner, SIGTERM, default action  -> WTERMSIG 15
 *   handler       spinner polling a flag its SIGUSR1 handler sets   -> exit 42
 *   kill9         single-threaded spinner, SIGKILL                  -> WTERMSIG 9
 *   thread-term   main thread and a pthread both spin, SIGTERM      -> WTERMSIG 15
 *   thread-kill9  the same, SIGKILL                                 -> WTERMSIG 9
 *
 * A round passes when the child is reaped with the expected status within
 * LIMIT_MS. If it is not, the probe sends SIGKILL so the next round starts clean
 * and reports the round as FAIL with what it saw. Output: one line per round,
 * then "PASS <case>" / "FAIL <case>" and a final "sigspin: N PASS, M FAIL".
 *
 * Usage: sigspin [rounds] [case]
 * Build: aarch64-linux-musl-gcc -O2 -static -pthread -o sigspin sigspin.c
 *        x86_64-linux-musl-gcc  -O2 -static -pthread -o sigspin sigspin.c
 */
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define LIMIT_MS 3000
#define SETTLE_MS 100

static volatile sig_atomic_t flag;
static volatile unsigned long sink;

static long long now_us(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (long long)ts.tv_sec * 1000000 + ts.tv_nsec / 1000;
}

static void spin_forever(void)
{
    for (;;)
        sink++;
}

static void *spin_thread(void *a)
{
    (void)a;
    spin_forever();
    return NULL;
}

static void on_usr1(int s)
{
    (void)s;
    flag = 1;
}

enum kind { TERM, HANDLER, KILL9, THREAD_TERM, THREAD_KILL9 };

static const struct {
    const char *name;
    enum kind kind;
    int sig;
} cases[] = {
    { "term", TERM, SIGTERM },
    { "handler", HANDLER, SIGUSR1 },
    { "kill9", KILL9, SIGKILL },
    { "thread-term", THREAD_TERM, SIGTERM },
    { "thread-kill9", THREAD_KILL9, SIGKILL },
};

static void child(enum kind k)
{
    if (k == HANDLER) {
        struct sigaction sa;
        memset(&sa, 0, sizeof sa);
        sa.sa_handler = on_usr1;
        sigaction(SIGUSR1, &sa, NULL);
        while (!flag)
            sink++;
        _exit(42);
    }
    if (k == THREAD_TERM || k == THREAD_KILL9) {
        pthread_t t;
        if (pthread_create(&t, NULL, spin_thread, NULL) != 0)
            _exit(3);
    }
    spin_forever();
}

/* One round. Returns 1 on pass. */
static int round_once(int ci, int r)
{
    pid_t pid = fork();
    if (pid < 0) {
        printf("  %s r%d: fork failed errno=%d\n", cases[ci].name, r, errno);
        return 0;
    }
    if (pid == 0)
        child(cases[ci].kind);

    usleep(SETTLE_MS * 1000);
    long long t0 = now_us();
    kill(pid, cases[ci].sig);
    int st = 0;
    pid_t w = 0;
    while (now_us() - t0 < (long long)LIMIT_MS * 1000) {
        w = waitpid(pid, &st, WNOHANG);
        if (w == pid)
            break;
        usleep(1000);
    }
    long long us = now_us() - t0;
    if (w != pid) {
        kill(pid, SIGKILL);
        waitpid(pid, &st, 0);
        printf("  %s r%d: NOT REAPED after %d ms (then SIGKILL: %s %d)\n", cases[ci].name, r,
               LIMIT_MS, WIFSIGNALED(st) ? "sig" : "exit",
               WIFSIGNALED(st) ? WTERMSIG(st) : WEXITSTATUS(st));
        return 0;
    }
    int ok;
    if (cases[ci].kind == HANDLER)
        ok = WIFEXITED(st) && WEXITSTATUS(st) == 42;
    else
        ok = WIFSIGNALED(st) && WTERMSIG(st) == cases[ci].sig;
    printf("  %s r%d: %s %d after %lld us %s\n", cases[ci].name, r,
           WIFSIGNALED(st) ? "sig" : "exit", WIFSIGNALED(st) ? WTERMSIG(st) : WEXITSTATUS(st), us,
           ok ? "ok" : "WRONG STATUS");
    return ok;
}

int main(int argc, char **argv)
{
    int rounds = argc > 1 ? atoi(argv[1]) : 5;
    const char *only = argc > 2 ? argv[2] : NULL;
    int pass = 0, fail = 0;
    setvbuf(stdout, NULL, _IOLBF, 0);
    for (unsigned ci = 0; ci < sizeof cases / sizeof cases[0]; ci++) {
        if (only && strcmp(only, cases[ci].name) != 0)
            continue;
        int good = 0;
        for (int r = 0; r < rounds; r++)
            good += round_once((int)ci, r);
        if (good == rounds) {
            printf("PASS %s\n", cases[ci].name);
            pass++;
        } else {
            printf("FAIL %s (%d/%d rounds)\n", cases[ci].name, good, rounds);
            fail++;
        }
    }
    printf("sigspin: %d PASS, %d FAIL\n", pass, fail);
    return fail != 0;
}
