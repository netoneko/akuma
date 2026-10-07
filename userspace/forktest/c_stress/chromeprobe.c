/*
 * chromeprobe: the kernel features Chromium's multi-process mode leans on,
 * as measured by `userspace/kami/probe/cdp.py strace` on one page load.
 *
 *   SCM_RIGHTS   628 messages over AF_UNIX SOCK_SEQPACKET + SOCK_STREAM pairs
 *   MAP_SHARED   621 shared mappings of /tmp files passed between processes
 *   PROT_NONE    a 1324 GiB reservation (the V8 sandbox) plus ~17 x 4 GiB
 *
 * Every check prints PASS/FAIL with a name; the last line is the tally.
 * Run it on Linux first: that is the control the expectations come from.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static int fails, passes;

#define CHECK(name, cond, ...)                                  \
    do {                                                        \
        if (cond) {                                             \
            passes++;                                           \
            printf("PASS %s\n", name);                          \
        } else {                                                \
            int e_ = errno;                                     \
            fails++;                                            \
            printf("FAIL %s: ", name);                          \
            printf(__VA_ARGS__);                                \
            printf(" (errno %d)\n", e_);                        \
        }                                                       \
        fflush(stdout);                                         \
    } while (0)

static int send_fds(int s, const void *buf, size_t len, const int *fds, int n)
{
    char cbuf[CMSG_SPACE(sizeof(int) * 8)];
    struct iovec iov = {(void *)buf, len};
    struct msghdr m = {0};
    m.msg_iov = &iov;
    m.msg_iovlen = 1;
    if (n) {
        memset(cbuf, 0, sizeof cbuf);
        m.msg_control = cbuf;
        m.msg_controllen = CMSG_SPACE(sizeof(int) * n);
        struct cmsghdr *c = CMSG_FIRSTHDR(&m);
        c->cmsg_level = SOL_SOCKET;
        c->cmsg_type = SCM_RIGHTS;
        c->cmsg_len = CMSG_LEN(sizeof(int) * n);
        memcpy(CMSG_DATA(c), fds, sizeof(int) * n);
    }
    return sendmsg(s, &m, 0);
}

/* `ctl` is the control buffer size offered (0 = none). */
static ssize_t recv_fds(int s, void *buf, size_t len, int *fds, int *n,
                        size_t ctl, int flags, int *mflags)
{
    char cbuf[CMSG_SPACE(sizeof(int) * 8)];
    struct iovec iov = {buf, len};
    struct msghdr m = {0};
    m.msg_iov = &iov;
    m.msg_iovlen = 1;
    m.msg_control = ctl ? cbuf : NULL;
    m.msg_controllen = ctl;
    ssize_t r = recvmsg(s, &m, flags);
    *n = 0;
    *mflags = m.msg_flags;
    if (r < 0)
        return r;
    for (struct cmsghdr *c = CMSG_FIRSTHDR(&m); c; c = CMSG_NXTHDR(&m, c)) {
        if (c->cmsg_level == SOL_SOCKET && c->cmsg_type == SCM_RIGHTS) {
            int k = (c->cmsg_len - CMSG_LEN(0)) / sizeof(int);
            memcpy(fds + *n, CMSG_DATA(c), sizeof(int) * k);
            *n += k;
        }
    }
    return r;
}

static double now(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec + ts.tv_nsec / 1e9;
}

/* A pipe's read end crosses a SEQPACKET pair and works on the far side, with
 * the sender's copy closed before the receive (Chromium's usual pattern). */
static void seqpacket_pass_pipe(void)
{
    int sv[2], p[2], fds[8], n, fl;
    char buf[16] = {0};
    if (socketpair(AF_UNIX, SOCK_SEQPACKET, 0, sv) || pipe(p)) {
        CHECK("seqpacket_pass_pipe", 0, "setup");
        return;
    }
    int s = send_fds(sv[0], "hello", 5, &p[0], 1);
    close(p[0]);
    ssize_t r = recv_fds(sv[1], buf, sizeof buf, fds, &n, CMSG_SPACE(sizeof(int) * 8), 0, &fl);
    CHECK("seqpacket_send_recv", s == 5 && r == 5 && n == 1 && !memcmp(buf, "hello", 5),
          "send=%d recv=%zd nfds=%d", s, r, n);
    if (n == 1) {
        char got[4] = {0};
        write(p[1], "xyz", 3);
        CHECK("seqpacket_passed_fd_reads", read(fds[0], got, 3) == 3 && !memcmp(got, "xyz", 3),
              "got '%.3s'", got);
        close(fds[0]);
    }
    close(p[1]);
    close(sv[0]);
    close(sv[1]);
}

/* Two descriptor-carrying writes on a stream: one big read must stop after
 * the first, so each message's fds arrive with that message. */
static void stream_stop_rule(void)
{
    int sv[2], a[2], b[2], fds[8], n, fl;
    char buf[64];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv) || pipe(a) || pipe(b)) {
        CHECK("stream_stop_rule", 0, "setup");
        return;
    }
    send_fds(sv[0], "AAAA", 4, &a[0], 1);
    send_fds(sv[0], "BBBBBB", 6, &b[0], 1);
    ssize_t r1 = recv_fds(sv[1], buf, sizeof buf, fds, &n, CMSG_SPACE(sizeof(int) * 8), 0, &fl);
    int n1 = n;
    if (n1)
        close(fds[0]);
    ssize_t r2 = recv_fds(sv[1], buf, sizeof buf, fds, &n, CMSG_SPACE(sizeof(int) * 8), 0, &fl);
    if (n)
        close(fds[0]);
    CHECK("stream_stops_after_fd_message", r1 == 4 && n1 == 1 && r2 == 6 && n == 1,
          "first %zd bytes/%d fds, second %zd/%d", r1, n1, r2, n);
    close(a[0]); close(a[1]); close(b[0]); close(b[1]);
    close(sv[0]);
    close(sv[1]);
}

/* A control buffer that holds one fd of two: one installed, MSG_CTRUNC.
 * No control buffer at all: none installed, MSG_CTRUNC. */
static void ctrunc(void)
{
    int sv[2], p[2], q[2], two[2], fds[8], n, fl;
    char buf[8];
    socketpair(AF_UNIX, SOCK_SEQPACKET, 0, sv);
    pipe(p);
    pipe(q);
    two[0] = p[0];
    two[1] = q[0];
    send_fds(sv[0], "x", 1, two, 2);
    recv_fds(sv[1], buf, sizeof buf, fds, &n, CMSG_LEN(sizeof(int)), 0, &fl);
    CHECK("ctrunc_partial", n == 1 && (fl & MSG_CTRUNC), "nfds=%d flags=0x%x", n, fl);
    if (n)
        close(fds[0]);
    send_fds(sv[0], "y", 1, two, 2);
    recv_fds(sv[1], buf, sizeof buf, fds, &n, 0, 0, &fl);
    CHECK("ctrunc_none", n == 0 && (fl & MSG_CTRUNC), "nfds=%d flags=0x%x", n, fl);
    close(p[0]); close(p[1]); close(q[0]); close(q[1]);
    close(sv[0]);
    close(sv[1]);
}

static void cloexec(void)
{
    int sv[2], p[2], fds[8], n, fl;
    char buf[8];
    socketpair(AF_UNIX, SOCK_SEQPACKET, 0, sv);
    pipe(p);
    send_fds(sv[0], "x", 1, &p[0], 1);
    recv_fds(sv[1], buf, sizeof buf, fds, &n, CMSG_SPACE(sizeof(int) * 8), MSG_CMSG_CLOEXEC, &fl);
    CHECK("cmsg_cloexec_set", n == 1 && (fcntl(fds[0], F_GETFD) & FD_CLOEXEC), "nfds=%d", n);
    if (n)
        close(fds[0]);
    send_fds(sv[0], "x", 1, &p[0], 1);
    recv_fds(sv[1], buf, sizeof buf, fds, &n, CMSG_SPACE(sizeof(int) * 8), 0, &fl);
    CHECK("cmsg_cloexec_clear", n == 1 && !(fcntl(fds[0], F_GETFD) & FD_CLOEXEC), "nfds=%d", n);
    if (n)
        close(fds[0]);
    close(p[0]); close(p[1]);
    close(sv[0]);
    close(sv[1]);
}

/* Chromium's shared memory with --disable-dev-shm-usage: an O_EXCL file in
 * /tmp, sized, mapped MAP_SHARED, its fd passed to another process which maps
 * it too. Writes must be visible both ways. */
static void shared_file_across_processes(void)
{
    int sv[2];
    char path[64];
    socketpair(AF_UNIX, SOCK_SEQPACKET, 0, sv);
    pid_t pid = fork();
    if (pid == 0) {
        int fds[8], n, fl;
        char b;
        close(sv[0]);
        recv_fds(sv[1], &b, 1, fds, &n, CMSG_SPACE(sizeof(int) * 8), 0, &fl);
        if (n != 1)
            _exit(10);
        char *m = mmap(NULL, 65536, PROT_READ | PROT_WRITE, MAP_SHARED, fds[0], 0);
        if (m == MAP_FAILED)
            _exit(11);
        strcpy(m + 4096, "from-child");
        write(sv[1], "c", 1);
        read(sv[1], &b, 1);
        _exit(strcmp(m + 8192, "from-parent") == 0 ? 0 : 12);
    }
    close(sv[1]);
    snprintf(path, sizeof path, "/tmp/.chromeprobe.%d", (int)getpid());
    int fd = open(path, O_RDWR | O_CREAT | O_EXCL, 0600);
    if (fd < 0) {
        snprintf(path, sizeof path, "/.chromeprobe.%d", (int)getpid());
        fd = open(path, O_RDWR | O_CREAT | O_EXCL, 0600);
    }
    int ok = fd >= 0 && ftruncate(fd, 65536) == 0;
    char *m = ok ? mmap(NULL, 65536, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0) : MAP_FAILED;
    CHECK("shm_file_create_map", m != MAP_FAILED, "open/ftruncate/mmap of %s", path);
    if (m == MAP_FAILED) {
        kill(pid, 9);
        waitpid(pid, NULL, 0);
        return;
    }
    send_fds(sv[0], "f", 1, &fd, 1);
    char b = 0;
    read(sv[0], &b, 1);
    CHECK("shm_child_write_visible", strcmp(m + 4096, "from-child") == 0, "parent sees '%.10s'", m + 4096);
    strcpy(m + 8192, "from-parent");
    write(sv[0], "p", 1);
    int st = 0;
    waitpid(pid, &st, 0);
    CHECK("shm_parent_write_visible", WIFEXITED(st) && WEXITSTATUS(st) == 0, "child status 0x%x", st);
    munmap(m, 65536);
    close(fd);
    unlink(path);
    close(sv[0]);
}

/* Chromium's other half of the shm pattern: the file is unlinked the moment it
 * exists, and the process that writes the page exits before the holder of the
 * other mapping has touched it. On Linux the page outlives the writer (the
 * inode is still referenced); Akuma used to drop the write with the writer's
 * last mapping, because write-back went by a path that no longer existed. */
static void shm_unlinked_writer_exits(void)
{
    int sv[2];
    char path[64];
    socketpair(AF_UNIX, SOCK_SEQPACKET, 0, sv);
    pid_t pid = fork();
    if (pid == 0) {
        int fds[8], n, fl;
        char b;
        close(sv[0]);
        recv_fds(sv[1], &b, 1, fds, &n, CMSG_SPACE(sizeof(int) * 8), 0, &fl);
        if (n != 1)
            _exit(10);
        char *m = mmap(NULL, 65536, PROT_READ | PROT_WRITE, MAP_SHARED, fds[0], 0);
        if (m == MAP_FAILED)
            _exit(11);
        strcpy(m + 4096, "child-wrote");
        _exit(0);
    }
    close(sv[1]);
    snprintf(path, sizeof path, "/tmp/.chromeprobe-u.%d", (int)getpid());
    int fd = open(path, O_RDWR | O_CREAT | O_EXCL, 0600);
    if (fd < 0) {
        snprintf(path, sizeof path, "/.chromeprobe-u.%d", (int)getpid());
        fd = open(path, O_RDWR | O_CREAT | O_EXCL, 0600);
    }
    unlink(path);
    ftruncate(fd, 65536);
    char *m = mmap(NULL, 65536, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    send_fds(sv[0], "f", 1, &fd, 1);
    int st = 0;
    waitpid(pid, &st, 0);
    CHECK("shm_unlinked_writer_exits", m != MAP_FAILED && WIFEXITED(st) && WEXITSTATUS(st) == 0 &&
          strcmp(m + 4096, "child-wrote") == 0, "parent sees '%.11s', child status 0x%x",
          m != MAP_FAILED ? m + 4096 : "", st);
    if (m != MAP_FAILED)
        munmap(m, 65536);
    close(fd);
    close(sv[0]);
}

/* V8's sandbox reservation and the pointer-compression cages. */
static void big_reservations(void)
{
    double t0 = now();
    size_t sz = 1324ULL << 30;
    char *p = mmap(NULL, sz, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0);
    CHECK("reserve_1324g", p != MAP_FAILED, "mmap");
    if (p != MAP_FAILED) {
        char *q = p + (512ULL << 30);
        int pr = mprotect(q, 1 << 20, PROT_READ | PROT_WRITE);
        if (pr == 0) {
            memset(q, 0x5a, 1 << 20);
        }
        CHECK("reserve_1324g_commit_middle", pr == 0 && q[12345] == 0x5a, "mprotect=%d", pr);
        CHECK("reserve_1324g_unmap", munmap(p, sz) == 0, "munmap");
    }
    int ok = 0;
    char *cages[17];
    for (int i = 0; i < 17; i++) {
        cages[i] = mmap(NULL, 4ULL << 30, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0);
        ok += cages[i] != MAP_FAILED;
    }
    CHECK("reserve_17x4g", ok == 17, "%d of 17", ok);
    for (int i = 0; i < 17; i++)
        if (cages[i] != MAP_FAILED)
            munmap(cages[i], 4ULL << 30);
    double dt = now() - t0;
    CHECK("reservations_fast", dt < 1.0, "%.3f s", dt);
    printf("INFO reservations took %.1f ms\n", dt * 1e3);
}

int main(void)
{
    setvbuf(stdout, NULL, _IOLBF, 0);
    seqpacket_pass_pipe();
    stream_stop_rule();
    ctrunc();
    cloexec();
    shared_file_across_processes();
    shm_unlinked_writer_exits();
    big_reservations();
    printf("chromeprobe: %d passed, %d failed\n", passes, fails);
    return fails ? 1 : 0;
}
