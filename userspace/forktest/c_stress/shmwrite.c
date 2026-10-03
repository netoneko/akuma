/* shmwrite — every path a writable MAP_SHARED file page can take, across processes.
 *
 * The companion of shmcoh.c (which asks the one question: are two processes' mappings live?).
 * This walks each path the amd64 shared writable page table (crates/akuma-fpcache-rw,
 * amd64/src/shmpages.rs, 2026-10-03) had to get right, one numbered rung each, and prints
 * before every rung so a hang names its rung. Same binary runs on Linux: every rung is
 * Linux behaviour, and `PASS` there is the ground truth.
 *
 *   1  fork-inherited mapping: child's store visible to parent, parent's to child
 *   2  unrelated mapping (child opens + maps itself): live both ways
 *   3  pwrite(2) into the file shows up in another process's mapping
 *   4  a store through the mapping reaches pread(2) after msync
 *   5  ftruncate shrink + regrow: the mapped tail reads zero in another process
 *   6  MADV_DONTNEED in one process: its next read sees the shared bytes, the peer keeps them
 *   7  mprotect(PROT_READ) then (PROT_READ|PROT_WRITE) in a fork child: still shared
 *   8  mremap (MAYMOVE) of a shared mapping: still shared
 *   9  one file mapped twice in one process: the two views agree
 *  10  4 processes x 50000 atomic increments on one shared word: exact total
 *  11  a child that exits WITHOUT munmap/msync: its store reaches the file
 *  12  unlink, recreate the path, then munmap the old mapping: the new file is untouched
 *  13  fork from a WORKER THREAD: the parent stays shared with an unrelated mapper, and the
 *      child sees the mapping (including a page nobody faulted yet). The SQLite-killing case:
 *      amd64's fork read the forking thread's own (empty) region list, CoW-split the parent off
 *      its MAP_SHARED pages, and a write-back of that private copy overwrote everyone's -shm.
 *
 * Build: x86_64-linux-musl-gcc -O1 -static -pthread -o shmwrite shmwrite.c
 * Exit 0 only if every rung passes. */
#define _GNU_SOURCE
#include <fcntl.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/mman.h>
#include <sys/wait.h>

#define PG 4096
#define LEN (8 * PG)
static const char *P = "/tmp/shmwrite.dat";
static int fails;

static void rung(int n, const char *what) { printf("rung %d: %s ... ", n, what); fflush(stdout); }
static void verdict(int ok) { printf("%s\n", ok ? "PASS" : "FAIL"); fflush(stdout); if (!ok) fails++; }

static int fresh(void) {
    unlink(P);
    int fd = open(P, O_RDWR | O_CREAT | O_TRUNC, 0600);
    if (fd < 0 || ftruncate(fd, LEN) != 0) { perror("setup"); exit(2); }
    return fd;
}
static volatile char *map(int fd) {
    void *m = mmap(0, LEN, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (m == MAP_FAILED) { perror("mmap"); exit(2); }
    return m;
}
/* Wait for a byte on `r`; the peer writes one when it has done its part. */
static void await(int r) { char x; if (read(r, &x, 1) != 1) { perror("await"); exit(2); } }
static void signal1(int w) { if (write(w, "x", 1) != 1) { perror("signal"); exit(2); } }
static int child_ok(pid_t c) { int st; waitpid(c, &st, 0); return WIFEXITED(st) && WEXITSTATUS(st) == 0; }

/* Rung 13's worker: fork from a non-leader thread; the child checks the mapping and writes. */
static volatile char *t_map;
static void *fork_from_thread(void *arg) {
    (void)arg;
    pid_t c = fork();
    if (c == 0) {
        int saw = t_map[0] == 70 && t_map[5 * PG] == 0;   /* page 5: never faulted by anyone */
        t_map[6 * PG] = 71;
        _exit(saw ? 0 : 1);
    }
    return (void *)(long)child_ok(c);
}

int main(void) {
    int fd, a2b[2], b2a[2];
    volatile char *a;
    pid_t c;

    /* 1 */
    rung(1, "fork-inherited mapping is shared both ways");
    fd = fresh(); a = map(fd); a[0] = 1;      /* present in the parent before fork */
    pipe(a2b); pipe(b2a);
    if ((c = fork()) == 0) {
        await(a2b[0]);
        int saw = a[0] == 7 && a[PG] == 8;    /* page 1 was never faulted before fork */
        a[2 * PG] = 9;
        signal1(b2a[1]);
        _exit(saw ? 0 : 1);
    }
    a[0] = 7; a[PG] = 8; signal1(a2b[1]); await(b2a[0]);
    verdict(a[2 * PG] == 9 && child_ok(c));
    munmap((void *)a, LEN); close(fd);

    /* 2 */
    rung(2, "an unrelated process's own mapping is live both ways");
    fd = fresh(); a = map(fd); a[0] = 1;
    if ((c = fork()) == 0) {
        int fd2 = open(P, O_RDWR);
        volatile char *b = map(fd2);
        await(a2b[0]);
        int saw = b[0] == 7 && b[3 * PG] == 6;
        b[4 * PG] = 5;
        signal1(b2a[1]);
        _exit(saw ? 0 : 1);
    }
    a[0] = 7; a[3 * PG] = 6; signal1(a2b[1]); await(b2a[0]);
    verdict(a[4 * PG] == 5 && child_ok(c));

    /* 3 (reuses rung 2's mapping) */
    rung(3, "pwrite(2) shows up in another process's mapping");
    if ((c = fork()) == 0) {
        int fd2 = open(P, O_RDWR);
        volatile char *b = map(fd2);
        (void)b[5 * PG];                      /* fault it in before the write */
        signal1(b2a[1]);
        await(a2b[0]);
        _exit(memcmp((const char *)b + 5 * PG + 100, "hello", 5) == 0 ? 0 : 1);
    }
    await(b2a[0]);
    pwrite(fd, "hello", 5, 5 * PG + 100);
    signal1(a2b[1]);
    verdict(child_ok(c) && memcmp((const char *)a + 5 * PG + 100, "hello", 5) == 0);

    /* 4 */
    rung(4, "a mapped store reaches pread(2) after msync");
    memcpy((char *)a + 6 * PG, "world", 5);
    msync((void *)a, LEN, MS_SYNC);
    { char buf[5] = {0}; pread(fd, buf, 5, 6 * PG); verdict(memcmp(buf, "world", 5) == 0); }

    /* 5 */
    rung(5, "ftruncate shrink + regrow zeroes the mapped tail for a peer");
    if ((c = fork()) == 0) {
        int fd2 = open(P, O_RDWR);
        volatile char *b = map(fd2);
        b[7 * PG + 10] = 42;                  /* the peer's page, before the truncate */
        signal1(b2a[1]);
        await(a2b[0]);
        _exit(b[7 * PG + 10] == 0 ? 0 : 1);
    }
    await(b2a[0]);
    ftruncate(fd, 7 * PG); ftruncate(fd, LEN);
    signal1(a2b[1]);
    verdict(child_ok(c) && a[7 * PG + 10] == 0);
    munmap((void *)a, LEN); close(fd);

    /* 6 */
    rung(6, "MADV_DONTNEED drops a mapping, not the shared page");
    fd = fresh(); a = map(fd);
    a[0] = 33;
    if ((c = fork()) == 0) {
        int fd2 = open(P, O_RDWR);
        volatile char *b = map(fd2);
        int saw = b[0] == 33;
        madvise((void *)b, PG, MADV_DONTNEED);
        saw = saw && b[0] == 33;              /* re-faults the shared page, not zeros */
        signal1(b2a[1]);
        _exit(saw ? 0 : 1);
    }
    await(b2a[0]);
    verdict(child_ok(c) && a[0] == 33);

    /* 7 */
    rung(7, "mprotect RO then RW in a fork child keeps the page shared");
    if ((c = fork()) == 0) {
        mprotect((void *)a, PG, PROT_READ);
        mprotect((void *)a, PG, PROT_READ | PROT_WRITE);
        a[1] = 44;
        signal1(b2a[1]);
        _exit(0);
    }
    await(b2a[0]);
    verdict(child_ok(c) && a[1] == 44);

    /* 8 */
    rung(8, "mremap(MAYMOVE) keeps a shared mapping shared");
    {
        volatile char *m = mremap((void *)a, LEN, 2 * LEN, MREMAP_MAYMOVE);
        int ok = m != MAP_FAILED;
        if (ok) {
            a = m;
            if ((c = fork()) == 0) {
                int fd2 = open(P, O_RDWR);
                volatile char *b = map(fd2);
                await(a2b[0]);
                _exit(b[0] == 33 && b[2 * PG] == 55 ? 0 : 1);
            }
            a[2 * PG] = 55;                   /* a page never faulted before the move */
            signal1(a2b[1]);
            ok = child_ok(c);
        }
        verdict(ok);
        munmap((void *)a, ok ? 2 * LEN : LEN);
    }
    close(fd);

    /* 9 */
    rung(9, "one file mapped twice in one process agrees with itself");
    fd = fresh(); a = map(fd);
    {
        volatile char *b = map(fd);
        a[PG + 3] = 66;
        verdict(b[PG + 3] == 66);
        munmap((void *)b, LEN);
    }

    /* 10 */
    rung(10, "4 processes x 50000 atomic increments: exact total");
    {
        volatile long *ctr = (volatile long *)(a + 2 * PG);
        *ctr = 0;
        pid_t kids[4];
        for (int i = 0; i < 4; i++) {
            if ((kids[i] = fork()) == 0) {
                int fd2 = open(P, O_RDWR);
                volatile long *b = (volatile long *)(map(fd2) + 2 * PG);
                for (int k = 0; k < 50000; k++) __sync_fetch_and_add(b, 1);
                _exit(0);
            }
        }
        int ok = 1;
        for (int i = 0; i < 4; i++) ok &= child_ok(kids[i]);
        printf("(total %ld) ", *ctr);
        verdict(ok && *ctr == 200000);
    }
    munmap((void *)a, LEN); close(fd);

    /* 11 */
    rung(11, "a child exiting without munmap/msync: its store reaches the file");
    fd = fresh();
    if ((c = fork()) == 0) {
        int fd2 = open(P, O_RDWR);
        volatile char *b = map(fd2);
        memcpy((char *)b + 3 * PG, "exitflush", 9);
        _exit(0);                             /* no munmap, no msync */
    }
    {
        int ok = child_ok(c);
        char buf[9] = {0};
        pread(fd, buf, 9, 3 * PG);
        verdict(ok && memcmp(buf, "exitflush", 9) == 0);
    }
    close(fd);

    /* 12 */
    rung(12, "unlink + recreate, then munmap the old mapping: the new file is untouched");
    fd = fresh(); a = map(fd);
    memset((char *)a, 'O', LEN);              /* the old file's pages, all dirty */
    unlink(P);
    {
        int nfd = open(P, O_RDWR | O_CREAT | O_TRUNC, 0600);
        pwrite(nfd, "NEWFILE!", 8, 0);
        munmap((void *)a, LEN); close(fd);    /* the old mapping's write-back runs here */
        char buf[8] = {0};
        pread(nfd, buf, 8, 0);
        off_t sz = lseek(nfd, 0, SEEK_END);
        verdict(memcmp(buf, "NEWFILE!", 8) == 0 && sz == 8);
        close(nfd);
    }

    /* 13 */
    rung(13, "fork from a worker thread keeps the parent shared and the child mapped");
    fd = fresh(); a = map(fd); t_map = a;
    a[0] = 70;
    if ((c = fork()) == 0) {                  /* the unrelated peer */
        int fd2 = open(P, O_RDWR);
        volatile char *b = map(fd2);
        (void)b[0];
        signal1(b2a[1]);
        await(a2b[0]);
        _exit(b[0] == 72 && b[6 * PG] == 71 ? 0 : 1);
    }
    await(b2a[0]);
    {
        pthread_t t; void *thread_child_ok = 0;
        pthread_create(&t, 0, fork_from_thread, 0);
        pthread_join(t, &thread_child_ok);
        a[0] = 72;                            /* after the fork: must still reach the peer */
        signal1(a2b[1]);
        verdict(child_ok(c) && thread_child_ok && a[6 * PG] == 71);
    }
    munmap((void *)a, LEN); close(fd);

    unlink(P);
    printf("shmwrite: %s (%d failing)\n", fails ? "FAIL" : "PASS", fails);
    return fails != 0;
}
