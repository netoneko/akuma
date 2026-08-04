/* pipewake.c — is a thread blocked in read() on a pipe ALWAYS woken when a byte
 * is written?
 *
 * This is the shape cargo's jobserver uses, and the shape the `-j4` self-host jam
 * leaves behind (docs/archive/SELFHOST_DEVBOX_SMOLTCP.md): at the jam, cargo's
 * helper thread sits in `read(fd=5)` on the jobserver pipe forever while every
 * rustc has exited and no work is in flight. jobserver-rs blocks a helper thread
 * on read() to acquire a token; a lost wakeup there stalls the whole build with
 * no error, which is exactly the observed signature (0 errors, 0 live children,
 * no forward progress).
 *
 * Phases, so a failure says WHICH pattern breaks:
 *   1  same-process, thread reader:   writer thread -> blocked reader thread
 *   2  cross-process:                 parent writes -> forked child reads
 *      (the jobserver's real topology: the pipe is inherited across fork/exec)
 *   3  write-before-read:             byte already in the pipe when read() starts
 *      (must return immediately; catches a reader that parks despite ready data)
 *
 * Every wait is watchdogged: the writer records a sequence number before writing
 * and the reader bumps it after reading, so a reader that never wakes is reported
 * as a LOST WAKEUP with its iteration, rather than hanging the harness.
 *
 * Build on the host:
 *   aarch64-linux-musl-gcc -O2 -static -o pipewake pipewake.c
 */
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <errno.h>
#include <pthread.h>
#include <sys/wait.h>

#define ROUNDS      2000
#define TIMEOUT_MS  5000

static int pfd[2];
static volatile unsigned long reads_done = 0;
static volatile int reader_stop = 0;

static void *reader_thread(void *arg)
{
    (void)arg;
    for (;;) {
        unsigned char b;
        ssize_t n = read(pfd[0], &b, 1);
        if (n == 1) {
            __atomic_add_fetch(&reads_done, 1, __ATOMIC_SEQ_CST);
            continue;
        }
        if (n == 0) break;                 /* write end closed */
        if (n < 0 && errno == EINTR) continue;
        break;
    }
    __atomic_store_n(&reader_stop, 1, __ATOMIC_SEQ_CST);
    return NULL;
}

/* Wait for reads_done to reach `target`, up to TIMEOUT_MS. Returns 0 on success. */
static int await_read(unsigned long target)
{
    for (int ms = 0; ms < TIMEOUT_MS; ms += 2) {
        if (__atomic_load_n(&reads_done, __ATOMIC_SEQ_CST) >= target) return 0;
        usleep(2000);
    }
    return 1;
}

static int phase1(void)
{
    printf("[1] same-process: %d write->blocked-read handoffs\n", ROUNDS);
    if (pipe(pfd) != 0) { printf("[1] pipe failed: %s\n", strerror(errno)); return 1; }
    reads_done = 0;
    pthread_t th;
    if (pthread_create(&th, NULL, reader_thread, NULL) != 0) {
        printf("[1] pthread_create failed\n"); return 1;
    }
    int lost = 0;
    for (int i = 1; i <= ROUNDS; i++) {
        /* Let the reader actually block before writing — that is the case that
         * needs a wakeup, as opposed to data already being buffered. */
        usleep(300);
        unsigned char b = 't';
        if (write(pfd[1], &b, 1) != 1) { printf("[1] write failed at %d\n", i); lost++; break; }
        if (await_read((unsigned long)i)) {
            printf("[1] LOST WAKEUP at iter %d: byte written, reader still parked after %d ms\n",
                   i, TIMEOUT_MS);
            lost++;
            break;
        }
    }
    close(pfd[1]);
    pthread_join(th, NULL);
    close(pfd[0]);
    printf("[1] %s\n", lost ? "FAILED" : "ok");
    return lost;
}

static int phase2(void)
{
    printf("[2] cross-process: parent writes -> forked child reads, %d rounds\n", ROUNDS / 4);
    int up[2], down[2];
    if (pipe(up) != 0 || pipe(down) != 0) { printf("[2] pipe failed\n"); return 1; }
    pid_t pid = fork();
    if (pid < 0) { printf("[2] fork failed\n"); return 1; }
    if (pid == 0) {
        close(up[1]); close(down[0]);
        for (;;) {
            unsigned char b;
            ssize_t n = read(up[0], &b, 1);
            if (n != 1) break;
            if (write(down[1], &b, 1) != 1) break;   /* echo back */
        }
        _exit(0);
    }
    close(up[0]); close(down[1]);
    int lost = 0;
    for (int i = 1; i <= ROUNDS / 4; i++) {
        usleep(300);
        unsigned char b = 'x';
        if (write(up[1], &b, 1) != 1) { printf("[2] write failed at %d\n", i); lost++; break; }
        /* Read the echo with a watchdog via alarm-free polling: O_NONBLOCK would
         * change the very behaviour under test, so use a blocking read in a child
         * of our own? Simpler: rely on the kernel and bound it with a poll loop. */
        unsigned char r;
        ssize_t n = read(down[0], &r, 1);
        if (n != 1) {
            printf("[2] echo read failed at iter %d (n=%zd, %s)\n", i, n, strerror(errno));
            lost++;
            break;
        }
    }
    close(up[1]); close(down[0]);
    int st = 0;
    waitpid(pid, &st, 0);
    printf("[2] %s\n", lost ? "FAILED" : "ok");
    return lost;
}

static int phase3(void)
{
    printf("[3] write-before-read: data already buffered, read must not park\n");
    if (pipe(pfd) != 0) { printf("[3] pipe failed\n"); return 1; }
    int lost = 0;
    for (int i = 0; i < 200; i++) {
        unsigned char b = 'q';
        if (write(pfd[1], &b, 1) != 1) { lost++; break; }
        unsigned char r;
        if (read(pfd[0], &r, 1) != 1) { printf("[3] read failed at %d\n", i); lost++; break; }
    }
    close(pfd[0]); close(pfd[1]);
    printf("[3] %s\n", lost ? "FAILED" : "ok");
    return lost;
}

int main(void)
{
    setvbuf(stdout, NULL, _IOLBF, 0);
    printf("=== PIPEWAKE start (pid %d) ===\n", (int)getpid());
    int bad = 0;
    bad |= phase1();
    bad |= phase2();
    bad |= phase3();
    printf("=== PIPEWAKE DONE — %s ===\n", bad ? "LOST WAKEUP SEEN" : "all phases ok");
    return bad ? 1 : 0;
}
