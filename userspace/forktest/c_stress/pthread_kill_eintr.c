// pthread_kill_eintr — does a `pthread_kill` signal interrupt a blocking read?
//
// Shaped after jobserver-rs's `Helper::join` (jobserver-0.1.35 src/unix.rs), the
// path every rustc that reaches codegen runs: a helper thread blocks in `read`
// on the jobserver pipe, and the joiner sends SIGUSR1 up to 100 times, 10ms
// apart, expecting the `read` to fail with EINTR so the thread can exit. If it
// never does, the thread leaks — one per rustc, quadrupled at -j4.
//
// Phase 1 (the fix): handler installed with SA_SIGINFO and *no* SA_RESTART,
//   exactly as jobserver does. Linux must return -1/EINTR.
// Phase 2 (the guard): same thing with SA_RESTART set. Linux must NOT report
//   EINTR — it restarts the read instead. Go installs its SIGURG preemption
//   handler this way, so a fix that interrupts unconditionally would break
//   every blocking syscall a Go program makes. Phase 2 catches that.
//
// Build: aarch64-linux-musl-gcc -static -O2 -o pthread_kill_eintr pthread_kill_eintr.c
// Exit code 0 = both phases pass.

#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#define NOT_RETURNED (-999)

// A probe that hangs teaches the gate nothing. Before this watchdog, every
// PHASE1 failure also wedged PHASE2 — main parked forever in `pthread_join`
// while the helper it was waiting for never exited — so `verify_trim.py`
// reported `TIMEOUT (still running after 420s)` and the summary could not
// distinguish "the kernel failed this probe" from "the kernel wedged". Both
// readings were available in the boot log and neither was in the gate.
//
// Two mechanisms, because they answer different questions:
//   * bounded joins, so the ORDINARY failure path still reaches the normal
//     RESULT: line and names which phase and why;
//   * this alarm, as the backstop for anything the bounded joins do not cover.
// 30 s is ~10x the probe's healthy runtime (~3 s) and far below the gate's
// 420 s per-exercise timeout, so the watchdog always wins that race.
#define WATCHDOG_SECS 30
#define JOIN_TIMEOUT_SECS 5

static volatile sig_atomic_t g_phase = 0;

// Async-signal-safe: write(2) only, no stdio, no formatting.
static void watchdog_fired(int sig)
{
    (void)sig;
    static const char msg[] =
        "WATCHDOG: probe did not terminate; phase = ";
    static const char tail[] = "\nRESULT: FAIL\n";
    char d = (char)('0' + (g_phase % 10));
    (void)!write(1, msg, sizeof msg - 1);
    (void)!write(1, &d, 1);
    (void)!write(1, tail, sizeof tail - 1);
    _exit(1);
}

// `pthread_join` with a deadline. Returns 0 on join, ETIMEDOUT on give-up.
static int join_bounded(pthread_t t)
{
    struct timespec ts;
    if (clock_gettime(CLOCK_REALTIME, &ts) != 0) {
        // No clock: fall back to the plain join and let the alarm cover us.
        return pthread_join(t, NULL);
    }
    ts.tv_sec += JOIN_TIMEOUT_SECS;
    return pthread_timedjoin_np(t, NULL, &ts);
}

struct helper {
    int         fd;             // read end the helper blocks on
    volatile int ret;           // read() return, or NOT_RETURNED while blocked
    volatile int err;           // errno at that point
};

static volatile sig_atomic_t sig_count = 0;

static void handler(int sig, siginfo_t *si, void *ctx)
{
    (void)sig; (void)si; (void)ctx;
    sig_count++;
}

static void *helper_thread(void *arg)
{
    struct helper *h = arg;
    char buf[1];
    errno = 0;
    ssize_t n = read(h->fd, buf, sizeof buf);   // nothing is ever written
    h->err = errno;
    h->ret = (int)n;                            // set last: publishes the result
    return NULL;
}

// Install `sig`'s handler with the given flags, spawn a thread blocked in
// read(), then signal it up to 100 times. Returns once the read returns or the
// attempts are exhausted.
static int run_phase(int sig, int extra_flags, pthread_t *t, struct helper *h,
                     int pipefd[2])
{
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_sigaction = handler;
    sa.sa_flags = SA_SIGINFO | extra_flags;
    if (sigaction(sig, &sa, NULL) != 0) { perror("sigaction"); return -1; }

    if (pipe(pipefd) != 0) { perror("pipe"); return -1; }

    h->fd = pipefd[0];
    h->ret = NOT_RETURNED;
    h->err = 0;
    sig_count = 0;

    if (pthread_create(t, NULL, helper_thread, h) != 0) {
        perror("pthread_create");
        return -1;
    }

    // Let the helper actually reach the blocking read before signalling.
    usleep(300 * 1000);

    for (int i = 0; i < 100; i++) {
        if (h->ret != NOT_RETURNED) break;
        pthread_kill(*t, sig);
        usleep(10 * 1000);
    }
    return 0;
}

int main(int argc, char **argv)
{
    pthread_t t;
    struct helper h;
    int pipefd[2];
    int failures = 0;

    // Arm the watchdog before anything can block. Every exit path below is
    // either bounded or covered by this.
    struct sigaction wd;
    memset(&wd, 0, sizeof wd);
    wd.sa_handler = watchdog_fired;
    if (sigaction(SIGALRM, &wd, NULL) != 0) { perror("sigaction SIGALRM"); return 2; }
    alarm(WATCHDOG_SECS);

    // `selftest-wedge` proves the watchdog itself works: block forever and
    // expect WATCHDOG + RESULT: FAIL rather than a hang. A guard mechanism that
    // has never been seen to fire is not a guard.
    if (argc > 1 && strcmp(argv[1], "selftest-wedge") == 0) {
        g_phase = 9;
        printf("SELFTEST: wedging on purpose; watchdog must fire in %d s\n",
               WATCHDOG_SECS);
        fflush(stdout);
        for (;;) pause();
    }

    // ---- Phase 1: no SA_RESTART -> must interrupt with EINTR ----------------
    g_phase = 1;
    if (run_phase(SIGUSR1, 0, &t, &h, pipefd) != 0) return 2;

    if (h.ret == NOT_RETURNED) {
        // Still blocked. Closing both ends below makes the blocked read return
        // EOF, so the helper CAN be reaped — but bounded, because the whole
        // point of this branch is that the thread is not behaving.
        printf("PHASE1 FAIL: read() never returned within %d attempts "
               "(handler ran %d times)\n", 100, (int)sig_count);
        failures++;
        close(pipefd[0]); close(pipefd[1]);
        if (join_bounded(t) != 0)
            printf("PHASE1 INFO: helper thread did not exit after both pipe "
                   "ends were closed; leaked\n");
        goto phase2;
    } else {
        join_bounded(t);
        if (h.ret == -1 && h.err == EINTR) {
            printf("PHASE1 PASS: read() = -1 EINTR after %d handler runs\n",
                   (int)sig_count);
        } else {
            printf("PHASE1 FAIL: read() = %d errno = %d (%s)\n",
                   h.ret, h.err, strerror(h.err));
            failures++;
        }
    }
    close(pipefd[0]); close(pipefd[1]);

phase2:
    // ---- Phase 2: SA_RESTART -> must NOT report EINTR -----------------------
    g_phase = 2;
    if (run_phase(SIGUSR2, SA_RESTART, &t, &h, pipefd) != 0) return 2;

    if (h.ret != NOT_RETURNED) {
        join_bounded(t);
        printf("PHASE2 FAIL: SA_RESTART read() returned %d errno = %d (%s) — "
               "an SA_RESTART handler must restart the syscall, not interrupt it\n",
               h.ret, h.err, strerror(h.err));
        failures++;
    } else {
        // Still blocked, as it should be. Prove the read is genuinely alive and
        // not wedged: feed it a byte and expect it back.
        char c = 'x';
        if (write(pipefd[1], &c, 1) != 1) { perror("write"); return 2; }
        if (join_bounded(t) != 0) {
            printf("PHASE2 FAIL: helper did not return from read() within %d s "
                   "of the write — the wake was lost, not merely late\n",
                   JOIN_TIMEOUT_SECS);
            failures++;
        }
        if (h.ret == 1) {
            printf("PHASE2 PASS: SA_RESTART read() never reported EINTR, "
                   "then returned 1 byte\n");
        } else {
            printf("PHASE2 FAIL: after write, read() = %d errno = %d (%s)\n",
                   h.ret, h.err, strerror(h.err));
            failures++;
        }
        // Informational, deliberately NOT a failure. Akuma delivers pending
        // signals at *syscall return*, so a handler for an SA_RESTART signal
        // does not run until the blocking syscall finishes — whereas Linux runs
        // it immediately and then restarts the syscall. The observable contract
        // this test guards (no spurious EINTR) holds either way, and nothing in
        // the tree depends on the stricter timing: jobserver uses no SA_RESTART,
        // and Go's SIGURG only preempts threads that are *running* Go code.
        // Implementing the strict form means re-entering blocking syscalls from
        // scratch, which silently extends nanosleep/ppoll deadlines (the reason
        // Linux carries a restart_block). Tracked as a known divergence.
        printf("PHASE2 INFO: handler ran %d times during the blocked read "
               "(Linux would run it immediately; Akuma defers to syscall "
               "return — known divergence, not a failure)\n", (int)sig_count);
    }
    close(pipefd[0]); close(pipefd[1]);

    alarm(0);   // past every blocking step; do not let the backstop fire late
    printf("RESULT: %s\n", failures == 0 ? "PASS" : "FAIL");
    return failures == 0 ? 0 : 1;
}
