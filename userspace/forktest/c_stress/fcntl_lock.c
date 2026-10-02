/* fcntl_lock — POSIX record locks across processes (F_SETLK / F_SETLKW / F_GETLK).
 *
 * They were accepted and ignored until 2026-10-03, so SQLite's WAL index (byte-range
 * locks in the -shm file) had no exclusion and goose's sessions.db came up
 * "database disk image is malformed". Table: crates/akuma-reclock.
 *
 * Build: x86_64-linux-musl-gcc -O1 -static -o fcntl_lock fcntl_lock.c
 * Run:   ./fcntl_lock   -> "RESULT: all passed" */
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <errno.h>
#include <time.h>
#include <sys/wait.h>
static int fails;
#define CHECK(c, msg) do { if (c) printf("PASS %s\n", msg); else { printf("FAIL %s (errno=%d)\n", msg, errno); fails++; } } while (0)
static struct flock mk(short type, off_t start, off_t len) { struct flock f; memset(&f, 0, sizeof f); f.l_type = type; f.l_whence = SEEK_SET; f.l_start = start; f.l_len = len; return f; }
/* run `fn` in a child; return its exit status */
static int child(int (*fn)(int), int fd) { pid_t p = fork(); if (p == 0) _exit(fn(fd)); int st; waitpid(p, &st, 0); return WIFEXITED(st) ? WEXITSTATUS(st) : 99; }
static int try_wr(int fd) { struct flock f = mk(F_WRLCK, 0, 10); return fcntl(fd, F_SETLK, &f) == 0 ? 0 : (errno == EAGAIN || errno == EACCES ? 1 : 2); }
static int try_wr_far(int fd) { struct flock f = mk(F_WRLCK, 100, 10); return fcntl(fd, F_SETLK, &f) == 0 ? 0 : 1; }
static int try_rd(int fd) { struct flock f = mk(F_RDLCK, 0, 10); return fcntl(fd, F_SETLK, &f) == 0 ? 0 : 1; }
static int getlk(int fd) { struct flock f = mk(F_WRLCK, 0, 10); if (fcntl(fd, F_GETLK, &f) != 0) return 2; return f.l_type == F_UNLCK ? 3 : (f.l_pid > 0 ? 0 : 4); }
static int getlk_free(int fd) { struct flock f = mk(F_WRLCK, 200, 10); if (fcntl(fd, F_GETLK, &f) != 0) return 2; return f.l_type == F_UNLCK ? 0 : 1; }
static int wait_wr(int fd) { struct flock f = mk(F_WRLCK, 0, 10); struct timespec a, b; clock_gettime(CLOCK_MONOTONIC, &a);
    if (fcntl(fd, F_SETLKW, &f) != 0) return 2; clock_gettime(CLOCK_MONOTONIC, &b);
    long ms = (b.tv_sec - a.tv_sec) * 1000 + (b.tv_nsec - a.tv_nsec) / 1000000; return ms >= 250 ? 0 : 1; }
int main(void) {
    const char *path = "/tmp/fcntl_lock.dat";
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0600);
    write(fd, "0123456789012345678901234567890123456789", 40);
    struct flock w = mk(F_WRLCK, 0, 10);
    CHECK(fcntl(fd, F_SETLK, &w) == 0, "parent takes write lock [0,10)");
    CHECK(child(try_wr, fd) == 1, "child F_SETLK on the same range is refused");
    CHECK(child(try_rd, fd) == 1, "child read lock on a write-locked range is refused");
    CHECK(child(try_wr_far, fd) == 0, "child can lock a disjoint range");
    CHECK(child(getlk, fd) == 0, "F_GETLK reports the holder (l_pid > 0)");
    CHECK(child(getlk_free, fd) == 0, "F_GETLK on a free range says F_UNLCK");
    /* own re-lock / upgrade never conflicts */
    struct flock r = mk(F_RDLCK, 0, 10);
    CHECK(fcntl(fd, F_SETLK, &r) == 0, "parent downgrades its own lock");
    CHECK(child(try_rd, fd) == 0, "child may now share a read lock");
    CHECK(child(try_wr, fd) == 1, "...but still cannot write-lock it");
    /* F_SETLKW blocks until the holder lets go */
    CHECK(fcntl(fd, F_SETLK, &w) == 0, "parent re-takes write lock");
    pid_t p = fork();
    if (p == 0) _exit(wait_wr(fd));
    usleep(300000);
    struct flock u = mk(F_UNLCK, 0, 10);
    fcntl(fd, F_SETLK, &u);
    int st; waitpid(p, &st, 0);
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0, "F_SETLKW blocked >=250ms then acquired after unlock");
    /* closing ANY descriptor for the file drops the process's locks (POSIX) */
    fcntl(fd, F_SETLK, &w);
    int fd2 = open(path, O_RDWR);
    close(fd2);
    CHECK(child(try_wr, fd) == 0, "close() of another fd for the file released the lock");
    /* exit releases */
    pid_t h = fork();
    if (h == 0) { int f3 = open(path, O_RDWR); struct flock x = mk(F_WRLCK, 0, 10); fcntl(f3, F_SETLK, &x); _exit(0); }
    waitpid(h, &st, 0);
    CHECK(try_wr(fd) == 0, "a holder's exit released its lock");
    unlink(path);
    printf(fails ? "RESULT: %d FAILED\n" : "RESULT: all passed\n", fails);
    return fails != 0;
}
