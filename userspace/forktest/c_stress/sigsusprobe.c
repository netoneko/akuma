// sigsusprobe — rt_sigsuspend's mask: the handler runs under the temporary
// mask, and the mask from before the call is back when it returns.
//
// busybox ash's `wait` builtin parks in sigsuspend with SIGCHLD blocked around
// it. Calibrate on real Linux first:
//   docker run --rm -v "$PWD:/w:ro" alpine /w/sigsusprobe
//
//   1. SIGUSR1 blocked, sent by a child; sigsuspend(empty) returns -1 EINTR
//   2. the handler ran exactly once, with SIGUSR2 in its mask (sa_mask) and
//      SIGUSR1 too (no SA_NODEFER)
//   3. after the call SIGUSR1 is blocked again and SIGUSR2 is not
//   4. a signal the temporary mask blocks does not wake it: SIGUSR2 pended
//      under a mask that blocks it, then SIGUSR1 wakes the sleeper, and
//      SIGUSR2 is still pending afterwards

#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <sys/wait.h>
#include <unistd.h>

static int failures = 0;
static volatile sig_atomic_t usr1_runs = 0;
static volatile sig_atomic_t usr1_saw_usr1 = 0;
static volatile sig_atomic_t usr1_saw_usr2 = 0;

static void check(const char *what, int ok) {
    printf("%s %s\n", ok ? "PASS" : "FAIL", what);
    if (!ok) failures++;
}

static void on_usr1(int sig) {
    (void)sig;
    sigset_t now;
    sigprocmask(SIG_SETMASK, NULL, &now);
    usr1_runs++;
    usr1_saw_usr1 = sigismember(&now, SIGUSR1);
    usr1_saw_usr2 = sigismember(&now, SIGUSR2);
}

static void on_usr2(int sig) { (void)sig; }

static void send_later(int sig) {
    pid_t parent = getpid();
    if (fork() == 0) {
        usleep(200 * 1000);
        kill(parent, sig);
        _exit(0);
    }
}

int main(void) {
    struct sigaction sa = {0};
    sa.sa_handler = on_usr1;
    sigemptyset(&sa.sa_mask);
    sigaddset(&sa.sa_mask, SIGUSR2);
    sigaction(SIGUSR1, &sa, NULL);
    struct sigaction sb = {0};
    sb.sa_handler = on_usr2;
    sigaction(SIGUSR2, &sb, NULL);

    sigset_t block, empty, now;
    sigemptyset(&block);
    sigaddset(&block, SIGUSR1);
    sigprocmask(SIG_SETMASK, &block, NULL);
    sigemptyset(&empty);

    send_later(SIGUSR1);
    errno = 0;
    int r = sigsuspend(&empty);
    check("sigsuspend returns -1 EINTR", r == -1 && errno == EINTR);
    check("handler ran once", usr1_runs == 1);
    check("handler mask has SIGUSR1 (no SA_NODEFER)", usr1_saw_usr1 == 1);
    check("handler mask has SIGUSR2 (sa_mask)", usr1_saw_usr2 == 1);
    sigprocmask(SIG_SETMASK, NULL, &now);
    check("after: SIGUSR1 blocked again", sigismember(&now, SIGUSR1) == 1);
    check("after: SIGUSR2 not blocked", sigismember(&now, SIGUSR2) == 0);
    while (wait(NULL) > 0) {}

    // 4: SIGUSR2 pending and blocked by the temporary mask must not end the sleep.
    sigset_t b2;
    sigemptyset(&b2);
    sigaddset(&b2, SIGUSR1);
    sigaddset(&b2, SIGUSR2);
    sigprocmask(SIG_SETMASK, &b2, NULL);
    raise(SIGUSR2);
    sigset_t only2;
    sigemptyset(&only2);
    sigaddset(&only2, SIGUSR2);
    usr1_runs = 0;
    send_later(SIGUSR1);
    r = sigsuspend(&only2);
    check("blocked pending signal did not wake; SIGUSR1 did", r == -1 && errno == EINTR && usr1_runs == 1);
    sigset_t pend;
    sigpending(&pend);
    check("SIGUSR2 still pending", sigismember(&pend, SIGUSR2) == 1);
    while (wait(NULL) > 0) {}

    printf("%s sigsusprobe (%d failures)\n", failures ? "FAIL" : "PASS", failures);
    return failures != 0;
}
