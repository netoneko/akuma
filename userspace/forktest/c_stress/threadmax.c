/* threadmax.c — how many threads can this kernel hold live AT ONCE?
 *
 * Companion to futextest.c. That probe measures whether thread *churn* works
 * (spawn/join in sequence); this one measures the simultaneous ceiling, which is
 * a different number and a different failure mode:
 *
 *   phase A  fan-out: keep spawning threads that all park on a barrier flag and
 *            do NOT exit, until pthread_create refuses. Reports the largest
 *            number alive at one instant, plus the errno that stopped it.
 *   phase B  churn: spawn/join one at a time, 400x, no two ever alive together.
 *            A failure here is NOT a capacity limit — it means terminated slots
 *            are not being collected fast enough to be reused (the
 *            EAGAIN-at-iteration-58 signature from docs/archive/
 *            SELFHOST_DEVBOX_SMOLTCP.md).
 *
 * Reporting both separates "the ceiling is N" from "the ceiling is fine but the
 * collector is starved", which the kernel-side [threads] census logs mirror.
 *
 * Build on the host (in-VM rustc/cc is the chicken-and-egg problem in
 * AKUMA_SELF_HOSTING.md §7g):
 *   aarch64-linux-musl-gcc -O2 -static -o threadmax threadmax.c
 */
#define _GNU_SOURCE
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include <unistd.h>

#define CAP 512

static volatile int hold = 1;
static volatile int started = 0;

static void *parker(void *arg)
{
    (void)arg;
    __atomic_add_fetch(&started, 1, __ATOMIC_SEQ_CST);
    /* Park without using a futex: this probe must measure slot capacity, not
     * interact with the futex paths under investigation. */
    while (__atomic_load_n(&hold, __ATOMIC_SEQ_CST))
        usleep(2000);
    return NULL;
}

static void *noop(void *arg) { (void)arg; return NULL; }

int main(void)
{
    setvbuf(stdout, NULL, _IOLBF, 0);
    printf("=== THREADMAX start ===\n");

    /* ---- phase A: simultaneous ceiling ---- */
    pthread_t t[CAP];
    int n = 0, rc = 0;
    pthread_attr_t attr;
    pthread_attr_init(&attr);
    /* Small stacks: we are probing the kernel's slot ceiling, not its VM. */
    pthread_attr_setstacksize(&attr, 64 * 1024);

    for (n = 0; n < CAP; n++) {
        rc = pthread_create(&t[n], &attr, parker, NULL);
        if (rc != 0)
            break;
    }
    printf("[A] simultaneous live threads reached: %d (+1 main)\n", n);
    printf("[A] stopped by: rc=%d (%s)\n", rc, rc ? strerror(rc) : "hit CAP");
    /* Give them a moment to actually run, so `started` reflects threads that
     * really got scheduled, not just ones the kernel accepted. */
    usleep(200000);
    printf("[A] threads that actually ran: %d\n",
           __atomic_load_n(&started, __ATOMIC_SEQ_CST));

    __atomic_store_n(&hold, 0, __ATOMIC_SEQ_CST);
    for (int i = 0; i < n; i++)
        pthread_join(t[i], NULL);
    printf("[A] all %d joined\n", n);

    /* ---- phase B: churn, never more than 2 alive ---- */
    printf("[B] 400x sequential spawn/join: start\n");
    for (int i = 0; i < 400; i++) {
        pthread_t th;
        rc = pthread_create(&th, &attr, noop, NULL);
        if (rc != 0) {
            printf("[B] FAILED at iter %d: rc=%d (%s) — collector starvation, "
                   "not a capacity limit\n", i, rc, strerror(rc));
            printf("=== THREADMAX DONE — phase B failed ===\n");
            return 1;
        }
        pthread_join(th, NULL);
    }
    printf("[B] ok\n");
    pthread_attr_destroy(&attr);

    printf("=== THREADMAX DONE — ceiling=%d, churn ok ===\n", n);
    return 0;
}
