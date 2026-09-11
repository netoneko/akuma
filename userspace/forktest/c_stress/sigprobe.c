/* sigprobe — does this kernel deliver a signal to a userspace handler, and
 * return from one?
 *
 * Background: amd64 had every *half* of signals except the one that looks. The
 * pending set, the blocked mask and the sigaltstack were in `akuma-threading`;
 * `rt_sigaction`, `kill`, `tkill` and `tgkill` were in `akuma-syscalls-glue`;
 * `deliver_signal` pended on a whole thread group. Nothing on that target ever
 * examined the pending set at a syscall return, so `kill(2)` returned 0 and did
 * nothing at all, and `rt_sigaction` was a literal `=> 0`
 * (`docs/archive/AKUMA_SELF_HOSTING_AMD64.md`, the YOU-ARE-HERE item 1).
 *
 * **Every step is announced BEFORE it runs, through write(2) rather than
 * stdio.** Two of the eight rungs fail by hanging rather than by answering —
 * a blocking `read` that is never interrupted, and a child that is never
 * killed — so the last line printed has to name the operation that did not
 * return. The others fail by answering wrongly, which the rung number reports.
 *
 * The rungs, each isolating one thing the one before it does not need:
 *
 *   1 raise        a handler runs at all, and execution resumes after it
 *   2 resume       the interrupted code's locals survived the excursion
 *   3 siginfo      SA_SIGINFO's second and third arguments are real
 *   4 mask         a blocked signal PENDS and fires on unblock, in that order
 *   5 nested       a second signal during a handler reaches a second handler
 *   6 eintr        a signal breaks a blocking read (no SA_RESTART)
 *   7 fatal        SIG_DFL SIGTERM to a child is WIFSIGNALED, not ignored
 *   8 abort        abort() reaches SIGABRT through musl's block/tkill/unblock
 *
 * Statically linked musl, so the same binary runs on real Linux for an A/B —
 * every rung must pass there, and if one does not, the probe is wrong rather
 * than the kernel (`docs/archive/LINUX_AB_PROBE_TECHNIQUE.md`).
 *
 * Exits 0 on success, or the rung number that failed.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <signal.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

static void say(const char *s)
{
    (void)write(1, s, strlen(s));
}

/* Wait for the child's "I am set up" byte.
 *
 * **Not an optimisation — the signal must not arrive first.** Rung 6's child
 * installs its handler *after* it is forked, and a `SIGUSR1` that lands before
 * that takes the signal's default disposition; more sharply on Akuma, `kill(2)`
 * also raises the Ctrl-C `interrupted` flag, which makes the target's very next
 * syscall return `EINTR` from the dispatch prologue — so the racing `sigaction`
 * itself failed, and the rung reported `60 + EINTR` rather than anything about
 * signals. A `sleep` is not a handshake: it loses at `SMP=4` under emulation,
 * which is exactly where it was observed (22 of 25 runs).
 */
static void await_ready(int fd)
{
    char c;
    while (read(fd, &c, 1) < 0 && errno == EINTR)
        ;
}

/* Kill `child` until it dies, then reap it.
 *
 * **The retry is not belt and braces either.** [`await_ready`] closes the gap
 * up to the child's *setup*, but not the last one — between its ready byte and
 * its entry into the blocking `read`. A signal landing in there runs (or is
 * dropped) at once and leaves the read to block, so a single `kill` would hang
 * the probe, and a hang has no rung number. Re-sending every 50 ms removes the
 * ordering question entirely.
 *
 * `release` is the write end of the pipe the child is blocked on: if the signal
 * never does anything at all, writing a byte lets the child out, and the rung
 * fails with an exit status instead of never returning. That is the whole point
 * of the argument — on a kernel with no delivery, this is what turns "the gate
 * hung" into "rung 6 returned 62".
 */
static int reap_with_signal(pid_t child, int sig, int release)
{
    int st = 0;
    int i;
    for (i = 0; i < 40; i++) {
        (void)kill(child, sig);
        usleep(50000);
        if (waitpid(child, &st, WNOHANG) == child)
            return st;
    }
    (void)write(release, "x", 1);
    if (waitpid(child, &st, 0) != child)
        return -1;
    return st;
}

static void sayn(const char *s, long v)
{
    char buf[32];
    int i = 31;
    int neg = v < 0;
    unsigned long u = neg ? (unsigned long)(-v) : (unsigned long)v;
    buf[i--] = '\n';
    if (u == 0)
        buf[i--] = '0';
    while (u) {
        buf[i--] = (char)('0' + (u % 10));
        u /= 10;
    }
    if (neg)
        buf[i--] = '-';
    say(s);
    (void)write(1, buf + i + 1, (size_t)(31 - i));
}

static volatile sig_atomic_t hits[65];
static volatile sig_atomic_t order;
static volatile sig_atomic_t seen_signo;
static volatile int seen_info_ok;
static volatile int seen_uc_ok;
static volatile sig_atomic_t nested_inner;

static void plain(int sig)
{
    if (sig >= 0 && sig <= 64)
        hits[sig]++;
    order++;
}

static void with_info(int sig, siginfo_t *info, void *uc)
{
    if (sig >= 0 && sig <= 64)
        hits[sig]++;
    seen_signo = info ? info->si_signo : -1;
    seen_info_ok = info != NULL;
    seen_uc_ok = uc != NULL;
}

/* Rung 5: raising SIGUSR2 from inside SIGUSR1's handler must reach SIGUSR2's,
 * which needs the frame to nest on the stack the first handler is running on. */
static void outer(int sig)
{
    (void)sig;
    hits[SIGUSR1]++;
    raise(SIGUSR2);
    nested_inner = hits[SIGUSR2];
}

/* `sigaction` with explicit flags: musl's `signal()` sets SA_RESTART, which is
 * exactly what rung 6 must not have. */
static int install(int sig, void (*fn)(int), int flags)
{
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = fn;
    sa.sa_flags = flags;
    return sigaction(sig, &sa, NULL);
}

static int install_info(int sig, void (*fn)(int, siginfo_t *, void *))
{
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_sigaction = fn;
    sa.sa_flags = SA_SIGINFO;
    return sigaction(sig, &sa, NULL);
}

int main(void)
{
    say("=== sigprobe\n");

    /* ---- 1: a handler runs at all --------------------------------------- */
    say("1 raise\n");
    if (install(SIGUSR1, plain, 0) != 0) {
        say("   sigaction failed\n");
        return 1;
    }
    if (raise(SIGUSR1) != 0 || hits[SIGUSR1] != 1) {
        sayn("   hits=", hits[SIGUSR1]);
        return 1;
    }

    /* ---- 2: the interrupted code resumes intact -------------------------- */
    /* A `volatile` sum across the excursion: the compiler must recompute it
     * from memory either side, so a register file the kernel restored wrongly
     * shows up as a wrong total rather than as a crash. */
    say("2 resume\n");
    {
        volatile long acc = 0;
        long i;
        for (i = 0; i < 1000; i++) {
            acc += i;
            if (i == 500)
                raise(SIGUSR1);
        }
        if (acc != 499500 || hits[SIGUSR1] != 2) {
            sayn("   acc=", (long)acc);
            return 2;
        }
    }

    /* ---- 3: SA_SIGINFO's arguments -------------------------------------- */
    say("3 siginfo\n");
    if (install_info(SIGUSR2, with_info) != 0)
        return 3;
    if (raise(SIGUSR2) != 0)
        return 3;
    if (seen_signo != SIGUSR2 || !seen_info_ok || !seen_uc_ok) {
        sayn("   signo=", seen_signo);
        return 3;
    }

    /* ---- 4: blocked pends, then fires ----------------------------------- */
    say("4 mask\n");
    {
        sigset_t set, old;
        int before;
        sigemptyset(&set);
        sigaddset(&set, SIGUSR1);
        if (sigprocmask(SIG_BLOCK, &set, &old) != 0)
            return 4;
        before = hits[SIGUSR1];
        raise(SIGUSR1);
        if (hits[SIGUSR1] != before) {
            say("   blocked signal was delivered anyway\n");
            return 4;
        }
        if (sigprocmask(SIG_SETMASK, &old, NULL) != 0)
            return 4;
        /* One syscall to give delivery a return to happen on, for a kernel
         * that only looks at a syscall boundary. Linux delivers on the
         * sigprocmask return itself, so this is free there. */
        (void)getpid();
        if (hits[SIGUSR1] != before + 1) {
            sayn("   after unblock hits=", hits[SIGUSR1]);
            return 4;
        }
    }

    /* ---- 5: nested delivery --------------------------------------------- */
    say("5 nested\n");
    {
        int u2 = hits[SIGUSR2];
        if (install(SIGUSR1, outer, 0) != 0 || install(SIGUSR2, plain, 0) != 0)
            return 5;
        raise(SIGUSR1);
        if (nested_inner != u2 + 1 || hits[SIGUSR2] != u2 + 1) {
            sayn("   inner=", nested_inner);
            return 5;
        }
    }

    /* ---- 6: EINTR out of a blocking read -------------------------------- */
    say("6 eintr\n");
    {
        int fds[2], rdy[2];
        pid_t child;
        int st = 0;
        if (pipe(fds) != 0 || pipe(rdy) != 0)
            return 6;
        child = fork();
        if (child < 0)
            return 6;
        if (child == 0) {
            char c;
            ssize_t n;
            (void)close(fds[1]);
            (void)close(rdy[0]);
            /* No SA_RESTART: Linux reports EINTR rather than restarting. */
            if (install(SIGUSR1, plain, 0) != 0)
                _exit(60 + (errno & 0x7f));
            (void)write(rdy[1], "r", 1);   /* the handler is armed — signal now */
            n = read(fds[0], &c, 1);
            if (n < 0 && errno == EINTR)
                _exit(0);
            _exit(n < 0 ? 61 : 62);
        }
        (void)close(fds[0]);
        (void)close(rdy[1]);
        await_ready(rdy[0]);
        (void)close(rdy[0]);
        st = reap_with_signal(child, SIGUSR1, fds[1]);
        (void)close(fds[1]);
        if (st < 0 || !WIFEXITED(st) || WEXITSTATUS(st) != 0) {
            sayn("   child status=", st);
            return 6;
        }
    }

    /* ---- 7: a fatal default action kills --------------------------------- */
    say("7 fatal\n");
    {
        int fds[2], rdy[2];
        pid_t child;
        int st = 0;
        if (pipe(fds) != 0 || pipe(rdy) != 0)
            return 7;
        child = fork();
        if (child < 0)
            return 7;
        if (child == 0) {
            char c;
            (void)close(fds[1]);
            (void)close(rdy[0]);
            /* Nothing to install — `SIGTERM`'s default action is the point —
             * but the handshake still matters: a signal delivered before the
             * child exists at all is delivered to nobody. */
            (void)write(rdy[1], "r", 1);
            (void)read(fds[0], &c, 1);
            _exit(70);
        }
        (void)close(fds[0]);
        (void)close(rdy[1]);
        await_ready(rdy[0]);
        (void)close(rdy[0]);
        st = reap_with_signal(child, SIGTERM, fds[1]);
        (void)close(fds[1]);
        if (st < 0 || !WIFSIGNALED(st) || WTERMSIG(st) != SIGTERM) {
            sayn("   status=", st);
            return 7;
        }
    }

    /* ---- 8: abort() ------------------------------------------------------ */
    /* musl blocks everything, `tkill`s itself with SIGABRT and then unblocks —
     * so this exercises the pend-while-blocked path from the other end, and it
     * is the one `docs/archive` records as having silently produced a SIGSEGV
     * at address 0 on a kernel that dropped the pending signal. */
    say("8 abort\n");
    {
        pid_t child = fork();
        int st = 0;
        if (child < 0)
            return 8;
        if (child == 0) {
            abort();
            _exit(80);
        }
        if (waitpid(child, &st, 0) != child)
            return 8;
        if (!WIFSIGNALED(st) || WTERMSIG(st) != SIGABRT) {
            sayn("   status=", st);
            return 8;
        }
    }

    say("=== sigprobe OK\n");
    return 0;
}
