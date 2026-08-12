/* segvgroup.c — when a multi-threaded process dies of an unhandled SIGSEGV on
 * its MAIN thread, does the whole thread group actually go away?
 *
 * POSIX says yes: a fatal signal terminates the process, not just the thread
 * that took it (Linux routes it through do_group_exit). Akuma's fault handler
 * only did that for CLONE_VM threads — it gated the group kill on
 * `proc.address_space.is_shared()`, which is FALSE for the main thread, since
 * only pthreads get a shared address space. The main thread instead notified the
 * parent and fell into `return_to_kernel`, whose own `kill_thread_group` call
 * comes AFTER that notify. The notify wakes the parent's wait4; a peer core
 * reaps the crashed process; `return_to_kernel` then resolves no process at all
 * and skips its entire cleanup block. Every worker thread is orphaned: never
 * killed, never reaped, parked in FUTEX_WAIT with a live Process row and a
 * pinned address space for the rest of the boot.
 *
 * Measured on cargo (docs/archive/CARGO_NULL_RC_MEMORY_REFERENCE_AUDIT.md §13.5):
 * pid 151 crashed at T219.58 and four of its threads were still burning CPU in
 * the futex table at T510 — five minutes later. Two crashes, same shape.
 *
 * WHY wait4 CANNOT SEE THIS, and what this probe checks instead.
 * The parent's wait4 returns 139 either way — that is why the leak survived a
 * shell-level build loop unnoticed, and why `segvchild` (which only asks
 * "does wait4 return?") passes on a leaking kernel. What leaks is kernel-side:
 * per crash, T thread slots, T+1 process-table rows, and the child's whole
 * address space. So this probe crashes a process REPEATEDLY and then asks the
 * box to do ordinary work again:
 *
 *   phase 1  R rounds of "T-threaded child, main thread takes a fatal SIGSEGV",
 *            each verifying the parent sees WIFSIGNALED/SIGSEGV
 *   phase 2  fork a fresh T-threaded child that exits cleanly, and re-touch the
 *            same working set in the parent
 *
 * Phase 2 is the detector. Leaked slots make fork/pthread_create fail
 * (MAX_THREADS is 256, MAX_PROCESSES 256, so R*T of each is a hard wall), and a
 * leaked address space is memory the box never gets back. On a kernel that tears
 * the group down correctly nothing accumulates and phase 2 is uneventful.
 *
 * The child's handler is deliberately Rust's std shape, because that is what
 * cargo runs: install SIGSEGV on a sigaltstack with SA_ONSTACK|SA_SIGINFO, and
 * for any address outside the stack guard, reset the disposition to SIG_DFL and
 * RETURN — so the faulting instruction re-executes and dies by default action.
 * That second fault is the one that reaches the kernel's terminal path, and it
 * is the one the old code mishandled; a probe that faults with no handler at all
 * would exercise a different, shorter route.
 *
 * THE SHARPER ORACLE IS THE SERIAL LOG, and it needs only one round. Every group
 * teardown prints `[KTG] my_pid=N ... siblings=T`. So per crashing child:
 *
 *   leaking:  [Fault] Process N (/tmp/segvgroup) SIGSEGV after 0.03s
 *             [TERM] tid=X pid=Some(N) by_tid=Y ...      <- the PARENT reaping us
 *             (no [KTG] my_pid=N line anywhere)
 *   fixed:    [PROC-EXIT] pid=N tgid=N name=/tmp/segvgroup code=-11
 *             [KTG] my_pid=N my_tgid=N ... siblings=T
 *
 * The defaults below are sized to make the leak reachable from userspace alone
 * (40*8 = 320 orphaned workers against a 248-slot ceiling), which is slower and
 * blunter than reading the log but needs nothing but the exit code.
 *
 * Build: aarch64-linux-musl-gcc -static -O2 -Wall -Wextra -o segvgroup segvgroup.c -pthread
 * Calibrate: docker run --rm --platform linux/arm64 -v "$PWD/segvgroup:/segvgroup:ro" \
 *                alpine /segvgroup 40 8 8      (expect PASS)
 * Usage: segvgroup [rounds] [threads] [mb]
 * Exit code 0 = every round crashed correctly and the box still works afterwards.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

#define MAX_WORKERS 32
#define WAIT_SECS 30

/* Workers park here forever. An UNTIMED wait is the point: it is the one state a
 * deferred kill request cannot reach on its own (the thread is woken, re-checks
 * the predicate, finds it unsatisfied and re-parks), so it is what an orphaned
 * worker settles into. Never signalled by anyone. */
static pthread_mutex_t park_lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t park_cond = PTHREAD_COND_INITIALIZER;
static volatile int workers_running;

static void *park_forever(void *arg)
{
    (void)arg;
    pthread_mutex_lock(&park_lock);
    workers_running++;
    for (;;)
        pthread_cond_wait(&park_cond, &park_lock);
    pthread_mutex_unlock(&park_lock); /* not reached; keeps -Wall quiet */
    return NULL;
}

static void on_alarm(int sig)
{
    (void)sig;
    static const char msg[] = "  !! HUNG: waitpid did not return\n";
    ssize_t n = write(1, msg, sizeof(msg) - 1);
    (void)n;
    _exit(2);
}

/* Rust std's stack-overflow handler, reduced to the branch that matters here:
 * the faulting address is not in a guard page, so unregister ourselves and
 * return, letting the instruction re-execute and kill the process by default
 * action. Async-signal-safe: sigaction() only. */
static void segv_handler(int sig, siginfo_t *info, void *uctx)
{
    (void)info;
    (void)uctx;
    struct sigaction dfl;
    memset(&dfl, 0, sizeof(dfl));
    dfl.sa_handler = SIG_DFL;
    sigaction(sig, &dfl, NULL);
}

static int install_guard_handler(void)
{
    static char altstack[SIGSTKSZ < 16384 ? 16384 : SIGSTKSZ];
    stack_t ss;
    struct sigaction sa;

    ss.ss_sp = altstack;
    ss.ss_size = sizeof(altstack);
    ss.ss_flags = 0;
    if (sigaltstack(&ss, NULL) != 0)
        return -1;

    memset(&sa, 0, sizeof(sa));
    sa.sa_sigaction = segv_handler;
    sa.sa_flags = SA_ONSTACK | SA_SIGINFO;
    sigemptyset(&sa.sa_mask);
    return sigaction(SIGSEGV, &sa, NULL);
}

/* Anonymous working set, fully touched, so a leaked address space is real
 * resident memory the box does not get back. Returns 0 on success. */
static int touch_working_set(size_t mb, void **out)
{
    size_t bytes = mb * 1024u * 1024u;
    unsigned char *p;
    size_t i;

    if (bytes == 0) {
        *out = NULL;
        return 0;
    }
    p = mmap(NULL, bytes, PROT_READ | PROT_WRITE,
             MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (p == MAP_FAILED)
        return -1;
    for (i = 0; i < bytes; i += 4096)
        p[i] = (unsigned char)(i >> 12);
    *out = p;
    return 0;
}

static int spawn_workers(int n)
{
    pthread_t t;
    int i;

    for (i = 0; i < n; i++) {
        if (pthread_create(&t, NULL, park_forever, NULL) != 0)
            return -1;
        pthread_detach(t);
    }
    /* Let every worker actually reach the wait — a thread still in
     * pthread_create's setup is not yet the state under test. */
    for (i = 0; i < 500 && workers_running < n; i++)
        usleep(2000);
    return workers_running >= n ? 0 : -1;
}

/* The crashing child. Never returns. */
static void child_crash(int threads, size_t mb)
{
    void *ws;
    volatile int *null_ptr = NULL;

    if (touch_working_set(mb, &ws) != 0)
        _exit(70);
    if (install_guard_handler() != 0)
        _exit(71);
    if (spawn_workers(threads) != 0)
        _exit(72);

    /* Main thread takes the fault. The handler resets to SIG_DFL and returns,
     * this instruction re-executes, and the whole group must die. */
    *null_ptr = 1;

    /* Reached only if the kernel resumed us from an unhandled fatal fault. */
    _exit(73);
}

/* A well-behaved child: same shape, exits cleanly. Phase 2's capacity probe. */
static void child_clean(int threads, size_t mb)
{
    void *ws;

    if (touch_working_set(mb, &ws) != 0)
        _exit(70);
    if (spawn_workers(threads) != 0)
        _exit(72);
    _exit(0);
}

static pid_t fork_child(int threads, size_t mb, int crash)
{
    pid_t pid = fork();

    if (pid == 0) {
        if (crash)
            child_crash(threads, mb);
        else
            child_clean(threads, mb);
        _exit(74); /* not reached */
    }
    return pid;
}

int main(int argc, char **argv)
{
    /* Defaults sized against Akuma's 248-slot thread ceiling: 40*8 orphaned
     * workers overruns it outright, so a leaking kernel cannot finish phase 2. */
    int rounds = argc > 1 ? atoi(argv[1]) : 40;
    int threads = argc > 2 ? atoi(argv[2]) : 8;
    size_t mb = argc > 3 ? (size_t)atoi(argv[3]) : 8;
    int round;
    int failures = 0;

    if (threads < 1)
        threads = 1;
    if (threads > MAX_WORKERS)
        threads = MAX_WORKERS;

    signal(SIGALRM, on_alarm);
    printf("segvgroup: rounds=%d threads=%d mb=%zu\n", rounds, threads, mb);

    /* Phase 1 — crash a T-threaded process on its main thread, R times. */
    for (round = 0; round < rounds; round++) {
        pid_t pid = fork_child(threads, mb, 1);
        int status = 0;

        if (pid < 0) {
            printf("  round %d: fork failed (%s) — the box is out of "
                   "processes, which is the leak\n", round, strerror(errno));
            failures++;
            break;
        }

        alarm(WAIT_SECS);
        if (waitpid(pid, &status, 0) != pid) {
            printf("  round %d: waitpid failed (%s)\n", round, strerror(errno));
            failures++;
            alarm(0);
            continue;
        }
        alarm(0);

        if (!WIFSIGNALED(status) || WTERMSIG(status) != SIGSEGV) {
            /* code 72 here is already the leak: the child could not create its
             * workers because earlier rounds' orphans still hold the slots. */
            printf("  round %d: child did not die of SIGSEGV "
                   "(signaled=%d sig=%d exited=%d code=%d)%s\n",
                   round, WIFSIGNALED(status), WIFSIGNALED(status) ? WTERMSIG(status) : 0,
                   WIFEXITED(status), WIFEXITED(status) ? WEXITSTATUS(status) : 0,
                   WIFEXITED(status) && WEXITSTATUS(status) == 72
                       ? " — pthread_create failed: leaked thread slots" : "");
            failures++;
        }
    }
    printf("  phase 1: %d crash rounds, %d failure(s)\n", round, failures);

    /* Phase 2 — the detector. R*T leaked thread slots, R*(T+1) leaked process
     * rows, or R*mb of pinned address space all surface here as an ordinary
     * request the box can no longer serve. */
    {
        int probe;
        for (probe = 0; probe < 3; probe++) {
            pid_t pid = fork_child(threads, mb, 0);
            int status = 0;

            if (pid < 0) {
                printf("  phase 2 probe %d: fork failed (%s) — leaked process "
                       "rows or thread slots from the crashed groups\n",
                       probe, strerror(errno));
                failures++;
                break;
            }
            alarm(WAIT_SECS);
            if (waitpid(pid, &status, 0) != pid) {
                printf("  phase 2 probe %d: waitpid failed (%s)\n", probe, strerror(errno));
                failures++;
                alarm(0);
                break;
            }
            alarm(0);
            if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
                printf("  phase 2 probe %d: clean child failed "
                       "(signaled=%d sig=%d code=%d) — %s\n",
                       probe, WIFSIGNALED(status),
                       WIFSIGNALED(status) ? WTERMSIG(status) : 0,
                       WIFEXITED(status) ? WEXITSTATUS(status) : -1,
                       WIFEXITED(status) && WEXITSTATUS(status) == 72
                           ? "pthread_create failed: leaked thread slots"
                           : "see exit code (70=mmap, 72=pthread_create)");
                failures++;
            }
        }
    }

    /* And the parent itself must still be able to get memory back. */
    {
        void *ws;
        if (touch_working_set(mb, &ws) != 0) {
            printf("  parent could not map %zu MB after the crashes (%s) — "
                   "leaked address spaces\n", mb, strerror(errno));
            failures++;
        } else if (ws) {
            munmap(ws, mb * 1024u * 1024u);
        }
    }

    if (failures == 0) {
        printf("segvgroup: PASS\n");
        return 0;
    }
    printf("segvgroup: FAIL (%d)\n", failures);
    return 1;
}
