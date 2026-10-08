/*
 * thrprobe: many threads across many processes at once, the shape of a
 * Chromium session (browser ~20 threads, each GPU/utility/renderer process
 * 5-7, a dozen processes). 8 forked children each create 12 threads that
 * stay alive together (96 threads system-wide beyond the main threads), then
 * every thread is joined. pthread_create must succeed every time.
 *
 * Akuma amd64 before 2026-10-08 held at most 64 non-main threads across all
 * processes (`thread::MAX_THREADS`); the 65th clone answered EAGAIN, which
 * Chromium's GPU process turned into a CHECK and the browser into "GPU
 * process isn't usable". Linux: 8 x 12 created, 0 failed.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

#define PROCS 8
#define THREADS 12

static pthread_mutex_t mu = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t cv = PTHREAD_COND_INITIALIZER;
static int go;

static void *park(void *a) {
    (void)a;
    pthread_mutex_lock(&mu);
    while (!go) pthread_cond_wait(&cv, &mu);
    pthread_mutex_unlock(&mu);
    return 0;
}

/* Child: create THREADS threads, hold them all alive until the parent says
 * so (a byte on the pipe), release and join. Exit status = failed creates. */
static int child(int sync_fd) {
    pthread_t t[THREADS];
    int created = 0, failed = 0, first_errno = 0;
    for (int i = 0; i < THREADS; i++) {
        int rc = pthread_create(&t[i], 0, park, 0);
        if (rc == 0) created++;
        else { failed++; if (!first_errno) first_errno = rc; }
    }
    char b;
    if (read(sync_fd, &b, 1) != 1) failed += 100;
    pthread_mutex_lock(&mu);
    go = 1;
    pthread_cond_broadcast(&cv);
    pthread_mutex_unlock(&mu);
    for (int i = 0; i < created; i++) pthread_join(t[i], 0);
    if (failed) fprintf(stderr, "thrprobe: child %d: %d created, %d failed (%s)\n",
                        (int)getpid(), created, failed, strerror(first_errno));
    return failed > 120 ? 120 : failed;
}

int main(void) {
    int pipes[PROCS][2];
    pid_t pid[PROCS];
    for (int p = 0; p < PROCS; p++) {
        if (pipe(pipes[p]) != 0) { perror("pipe"); return 1; }
        pid[p] = fork();
        if (pid[p] == 0) {
            close(pipes[p][1]);
            _exit(child(pipes[p][0]));
        }
        if (pid[p] < 0) { perror("fork"); return 1; }
        close(pipes[p][0]);
    }
    /* Give every child time to reach its full thread count, then release. */
    sleep(2);
    int failed = 0, total = 0;
    for (int p = 0; p < PROCS; p++) {
        if (write(pipes[p][1], "g", 1) != 1) failed++;
        close(pipes[p][1]);
    }
    for (int p = 0; p < PROCS; p++) {
        int st = 0;
        waitpid(pid[p], &st, 0);
        int f = WIFEXITED(st) ? WEXITSTATUS(st) : 120;
        failed += f;
        total += THREADS;
    }
    printf("thrprobe: %d processes x %d threads: %d created, %d failed %s\n",
           PROCS, THREADS, total - failed, failed, failed ? "FAIL" : "ok");
    printf("thrprobe: %s\n", failed ? "FAIL" : "PASS");
    return failed != 0;
}
