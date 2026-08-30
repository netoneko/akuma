/* futexkill — does exit_group tear down a sibling parked in an untimed
 * FUTEX_WAIT promptly, or does it wait out the kernel's kill grace period?
 *
 * kill_thread_group does not hard-terminate siblings (that leaks the locks they
 * hold). It posts a deferred kill request and WAKES the thread, expecting it to
 * self-terminate at its EL1->EL0 boundary. A thread blocked in an untimed
 * FUTEX_WAIT wakes, sees the futex word unchanged and no signal, and parks
 * again — it never reaches a boundary. Before 2026-08-30 nothing in the wait
 * loop consulted the pending-kill flag, so the only way out was
 * kill_thread_group's 2 s KILL_GRACE_US expiring and hard-killing it.
 *
 * That cost a full 2 s on every such exit. In a guest `cargo build` it fired 60
 * times in a 90 s window (rustc's rayon workers park exactly here), and it was
 * invisible because the console flood buried the "[ktg] grace expired" lines.
 *
 * The probe times exit_group -> reaped. A regressed kernel reports ~2000 ms; a
 * fixed one reports single-digit ms. See
 * docs/archive/KTG_GRACE_EXPIRY_KILL_INTERRUPT.md.
 */
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <pthread.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <linux/futex.h>

/* Grace is 2 s. Anything near it means the sibling was hard-killed on expiry
 * rather than interrupted; a fixed kernel is orders of magnitude under this. */
#define LIMIT_MS 500

static int futex_word = 0;

static void *parker(void *arg)
{
    (void)arg;
    /* Untimed wait on a word that never changes: nothing but an interrupt can
     * end this. Re-park on a spurious wake — that is precisely the loop the
     * deferred kill has to break out of. */
    for (;;)
        syscall(SYS_futex, &futex_word, FUTEX_WAIT, 0, NULL, NULL, 0);
    return NULL;
}

static long ms_since(const struct timespec *t0)
{
    struct timespec t1;
    clock_gettime(CLOCK_MONOTONIC, &t1);
    return (t1.tv_sec - t0->tv_sec) * 1000L + (t1.tv_nsec - t0->tv_nsec) / 1000000L;
}

int main(int argc, char **argv)
{
    int rounds  = argc > 1 ? atoi(argv[1]) : 5;
    int nthread = argc > 2 ? atoi(argv[2]) : 2;
    long worst = -1;
    int slow = 0;

    printf("futexkill: %d rounds, %d parked sibling(s) each, limit %d ms\n",
           rounds, nthread, LIMIT_MS);

    for (int r = 0; r < rounds; r++) {
        int pipefd[2];
        if (pipe(pipefd) != 0) { printf("FAIL: pipe\n"); return 1; }

        pid_t pid = fork();
        if (pid < 0) { printf("FAIL: fork\n"); return 1; }
        if (pid == 0) {
            close(pipefd[0]);
            for (int i = 0; i < nthread; i++) {
                pthread_t th;
                if (pthread_create(&th, NULL, parker, NULL) != 0)
                    _exit(2);
                pthread_detach(th);
            }
            /* Let every sibling actually reach the wait before we exit. */
            usleep(200000);
            /* Start the parent's clock immediately before the call under test. */
            if (write(pipefd[1], "g", 1) != 1)
                _exit(3);
            syscall(SYS_exit_group, 0);
            _exit(4); /* unreachable */
        }

        close(pipefd[1]);
        char c;
        if (read(pipefd[0], &c, 1) != 1) {
            printf("FAIL: child never signalled readiness\n");
            return 1;
        }
        struct timespec t0;
        clock_gettime(CLOCK_MONOTONIC, &t0);

        int st = 0;
        if (waitpid(pid, &st, 0) != pid) { printf("FAIL: waitpid\n"); return 1; }
        long ms = ms_since(&t0);
        close(pipefd[0]);

        if (ms > worst) worst = ms;
        if (ms >= LIMIT_MS) slow++;
        printf("  round %d: exit_group -> reaped in %ld ms%s\n",
               r, ms, ms >= LIMIT_MS ? "   <-- grace-expiry stall" : "");
    }

    if (slow) {
        printf("FAIL: %d/%d rounds took >= %d ms (worst %ld ms) — a parked "
               "sibling is not being interrupted by the deferred kill\n",
               slow, rounds, LIMIT_MS, worst);
        return 1;
    }
    printf("PASS: worst exit_group -> reaped was %ld ms (< %d)\n", worst, LIMIT_MS);
    return 0;
}
