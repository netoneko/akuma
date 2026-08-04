/* segvchild.c — does a parent's wait4 return when its child dies via the
 * "SIGSEGV inside clone_thread" path?
 *
 * Motivated by the `-j4` self-host jam (docs/archive/SELFHOST_DEVBOX_SMOLTCP.md):
 * exactly one fault occurred in the jammed boot —
 *
 *   [Fault] Process 79 (/usr/local/bin/rustc) SIGSEGV after 0.02s
 *   [Fault] SIGSEGV in clone_thread, calling exit_group
 *
 * — and cargo never reported it (0 errors), never reaped it, and deadlocked on
 * job/token accounting. That points at the kernel's fault-kill path
 * (src/exceptions.rs: `is_clone_thread` -> sys_exit_group_pub, which never
 * returns, making the notify/vfork_complete calls below it unreachable).
 *
 * Three cases, so a failure is attributable rather than suggestive:
 *   A  child exits normally                 -> control, must reap
 *   B  child's MAIN thread SIGSEGVs         -> the non-clone_thread fault path
 *   C  child spawns a thread that SIGSEGVs  -> the suspect path
 *
 * Each wait is watchdogged with alarm(): a parent that never returns from wait4
 * is the bug, and prints HUNG instead of hanging the harness.
 *
 * Build on the host:
 *   aarch64-linux-musl-gcc -O2 -static -o segvchild segvchild.c
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

#define WAIT_SECS 20

static void on_alarm(int sig)
{
    (void)sig;
    static const char msg[] = "  !! PARENT HUNG: wait4 did not return\n";
    ssize_t n = write(1, msg, sizeof(msg) - 1);
    (void)n;
    _exit(2);
}

static void *boom_thread(void *arg)
{
    (void)arg;
    /* NULL store: EL0 data abort, FAR=0, inside a clone_thread sibling. */
    *(volatile int *)0 = 1;
    return NULL;
}

/* Returns 0 if the parent reaped the child, 1 if it hung. */
static int run_case(const char *name, int mode)
{
    fflush(stdout);
    pid_t pid = fork();
    if (pid < 0) {
        printf("[%s] fork failed: %s\n", name, strerror(errno));
        return 1;
    }
    if (pid == 0) {
        if (mode == 0) {
            _exit(7);
        } else if (mode == 1) {
            *(volatile int *)0 = 1;      /* main-thread SIGSEGV */
            _exit(0);
        } else {
            pthread_t th;
            if (pthread_create(&th, NULL, boom_thread, NULL) != 0)
                _exit(9);
            for (;;) pause();            /* keep the leader alive; the sibling faults */
        }
        _exit(0);
    }

    signal(SIGALRM, on_alarm);
    alarm(WAIT_SECS);
    int status = 0;
    pid_t got = waitpid(pid, &status, 0);
    alarm(0);

    if (got != pid) {
        printf("[%s] waitpid returned %d (want %d): %s\n", name, (int)got, (int)pid, strerror(errno));
        return 1;
    }
    if (WIFEXITED(status))
        printf("[%s] REAPED pid=%d exited=%d\n", name, (int)pid, WEXITSTATUS(status));
    else if (WIFSIGNALED(status))
        printf("[%s] REAPED pid=%d signal=%d\n", name, (int)pid, WTERMSIG(status));
    else
        printf("[%s] REAPED pid=%d status=%#x\n", name, (int)pid, status);
    return 0;
}

int main(void)
{
    setvbuf(stdout, NULL, _IOLBF, 0);
    printf("=== SEGVCHILD start ===\n");
    int bad = 0;
    bad |= run_case("A normal-exit      ", 0);
    bad |= run_case("B main-thread segv ", 1);
    bad |= run_case("C clone_thread segv", 2);
    printf("=== SEGVCHILD DONE — %s ===\n", bad ? "A PARENT HUNG" : "all reaped");
    return bad;
}
