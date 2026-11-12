/* orphanthr: children of a dead process go to init, whichever thread forked them.
 * The process under test forks one child from its leader and one from a worker thread,
 * joins the worker, then exit()s. Each child waits (up to 20 s) for its parent to die and
 * records getppid() in /tmp/orph.<who>. The top process waits for both files and prints
 * them: both must say ppid=1, and the exit status is 0 only then.
 *
 * Linux passes. AArch64 Akuma passes. amd64 Akuma (2026-11-12) fails the thread child: a
 * thread that exits before its process leaves its children parented to its own dead pid
 * (proposals/AKUMA_ONE_EXIT.md, "What step 1 found").
 *
 * Build: aarch64-linux-musl-gcc -static -O2 -o orphanthr orphanthr.c -lpthread */
#define _GNU_SOURCE
#include <fcntl.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static void child(const char *who) {
    struct timespec ts = {0, 20 * 1000 * 1000};
    for (int i = 0; i < 1000 && getppid() != 1; i++) nanosleep(&ts, 0);
    char path[64], b[96];
    snprintf(path, sizeof path, "/tmp/orph.%s.tmp", who);
    int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    int n = snprintf(b, sizeof b, "orphanthr: %s child ppid=%d %s\n", who, (int)getppid(),
                     getppid() == 1 ? "OK" : "FAIL");
    write(fd, b, n);
    close(fd);
    char fin[64];
    snprintf(fin, sizeof fin, "/tmp/orph.%s", who);
    rename(path, fin);
    _exit(0);
}

static void *worker(void *arg) {
    (void)arg;
    if (fork() == 0) child("thread");
    return NULL;
}

static int show(const char *who) {
    char p[64], b[128];
    snprintf(p, sizeof p, "/tmp/orph.%s", who);
    int fd = open(p, O_RDONLY);
    if (fd < 0) return 0;
    int n = read(fd, b, sizeof b);
    close(fd);
    if (n > 0) write(1, b, n);
    unlink(p);
    return n > 0 && memmem(b, n, " OK", 3) ? 1 : 0;
}

int main(void) {
    unlink("/tmp/orph.leader");
    unlink("/tmp/orph.thread");
    pid_t outer = fork();
    if (outer == 0) {
        if (fork() == 0) child("leader");
        pthread_t t;
        pthread_create(&t, NULL, worker, NULL);
        pthread_join(t, NULL);
        exit(0);
    }
    int st;
    waitpid(outer, &st, 0);
    struct timespec ts = {0, 100 * 1000 * 1000};
    for (int i = 0; i < 300; i++) {
        if (access("/tmp/orph.leader", F_OK) == 0 && access("/tmp/orph.thread", F_OK) == 0) break;
        nanosleep(&ts, 0);
    }
    int got = show("leader") + show("thread");
    printf("orphanthr: %d of 2 children reparented to init\n", got);
    return got == 2 ? 0 : 1;
}
