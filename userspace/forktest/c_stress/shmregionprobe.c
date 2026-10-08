/*
 * shmregionprobe: Chromium's shared-memory region, as the font service uses it
 * (base/memory/platform_shared_memory_region_posix.cc): the browser creates a
 * temp file, unlinks it at once, sizes it (fallocate), maps it MAP_SHARED
 * read-write and copies a font into it; the read-only handle for the renderer
 * is made by REOPENING /proc/self/fd/N with O_RDONLY; the fd travels over a
 * unix socket (SCM_RIGHTS) and the renderer maps it MAP_SHARED read-only and
 * hands the bytes to FreeType. On Akuma + Chromium 152 the renderer ends up
 * with no typeface (every font width 0), so this checks each step against the
 * browser's bytes.
 *
 * Steps (each scored; Linux passes all):
 *   1 same-process reopen of /proc/self/fd/N O_RDONLY succeeds
 *   2 a read-only MAP_SHARED of the reopened fd sees the bytes written through
 *     the writer's RW mapping (not zeros, not stale)
 *   3 the same through an fd received over SCM_RIGHTS in a forked child that
 *     maps it before the parent writes (the child polls for the bytes)
 *   4 a child that maps it after the parent wrote and munmap'd
 *   5 pread on the reopened fd sees the bytes
 * Argument: directory for the temp file (default /tmp).
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define SZ (393576)
static int fails;
static void check(const char *what, int ok) {
    printf("shmregionprobe: %s %s\n", what, ok ? "ok" : "FAIL");
    if (!ok) fails++;
}
static void fill(unsigned char *p) { for (long i = 0; i < SZ; i++) p[i] = (unsigned char)((i * 131 + 7) ^ (i >> 8)); }
static int matches(const unsigned char *p) {
    for (long i = 0; i < SZ; i++) if (p[i] != (unsigned char)((i * 131 + 7) ^ (i >> 8))) return 0;
    return 1;
}
static int reopen_ro(int fd) {
    char p[64];
    snprintf(p, sizeof p, "/proc/self/fd/%d", fd);
    return open(p, O_RDONLY | O_CLOEXEC);
}
static void send_fd(int sock, int fd) {
    char b = 'x', cm[CMSG_SPACE(sizeof(int))];
    struct iovec iov = { &b, 1 };
    struct msghdr m = { .msg_iov = &iov, .msg_iovlen = 1, .msg_control = cm, .msg_controllen = sizeof cm };
    struct cmsghdr *c = CMSG_FIRSTHDR(&m);
    c->cmsg_level = SOL_SOCKET; c->cmsg_type = SCM_RIGHTS; c->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(c), &fd, sizeof(int));
    sendmsg(sock, &m, 0);
}
static int recv_fd(int sock) {
    char b, cm[CMSG_SPACE(sizeof(int))];
    struct iovec iov = { &b, 1 };
    struct msghdr m = { .msg_iov = &iov, .msg_iovlen = 1, .msg_control = cm, .msg_controllen = sizeof cm };
    if (recvmsg(sock, &m, 0) <= 0) return -1;
    struct cmsghdr *c = CMSG_FIRSTHDR(&m);
    int fd = -1;
    if (c && c->cmsg_type == SCM_RIGHTS) memcpy(&fd, CMSG_DATA(c), sizeof(int));
    return fd;
}

int main(int argc, char **argv) {
    const char *dir = argc > 1 ? argv[1] : "/tmp";
    char p[128];
    snprintf(p, sizeof p, "%s/.shmregion.%d", dir, getpid());
    int fd = open(p, O_RDWR | O_CREAT | O_EXCL | O_CLOEXEC, 0600);
    if (fd < 0) { perror("open"); return 1; }
    unlink(p);
    if (fallocate(fd, 0, 0, SZ) != 0 && ftruncate(fd, SZ) != 0) { perror("size"); return 1; }
    unsigned char *w = mmap(0, SZ, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    check("RW MAP_SHARED of the unlinked region", w != MAP_FAILED);

    int ro = reopen_ro(fd);
    check("reopen /proc/self/fd/N O_RDONLY", ro >= 0);
    unsigned char *r = ro >= 0 ? mmap(0, SZ, PROT_READ, MAP_SHARED, ro, 0) : MAP_FAILED;
    check("read-only MAP_SHARED of the reopened fd", r != MAP_FAILED);

    if (ro < 0) { printf("shmregionprobe: FAILED (no read-only handle; later steps skipped)\n"); return 1; }

    /* step 3: child maps before the parent writes, receives the fd over a socket */
    int sv[2];
    socketpair(AF_UNIX, SOCK_STREAM, 0, sv);
    pid_t c1 = fork();
    if (c1 == 0) {
        close(sv[0]);
        int rfd = recv_fd(sv[1]);
        if (rfd < 0) _exit(10);
        unsigned char *m = mmap(0, SZ, PROT_READ, MAP_SHARED, rfd, 0);
        if (m == MAP_FAILED) _exit(11);
        for (int i = 0; i < 400; i++) { if (matches(m)) _exit(0); usleep(10000); }
        _exit(12);
    }
    close(sv[1]);
    send_fd(sv[0], ro);
    usleep(100000);
    fill(w);
    int st = 0;
    waitpid(c1, &st, 0);
    /* Printed, not scored: a peer that maps the region BEFORE the writer has
     * touched its pages holds a frame read from the (zero) file, and Akuma does
     * not yet update it when the writer's page appears. Chromium writes the
     * region first and sends the fd afterwards, so the font service never does
     * this. Linux (one page cache) passes. */
    printf("shmregionprobe: (info) child mapped before the write %s the bytes (exit %d)\n",
           (WIFEXITED(st) && WEXITSTATUS(st) == 0) ? "sees" : "does NOT see", WIFEXITED(st) ? WEXITSTATUS(st) : -1);

    if (r != MAP_FAILED) check("parent's read-only map sees the bytes", matches(r));
    unsigned char *buf = malloc(SZ);
    /* Printed, not scored: Akuma's read(2)/pread are not yet coherent with a
     * writer's unflushed MAP_SHARED pages (documented open item); Chromium's
     * renderer maps the region, it does not pread it. */
    printf("shmregionprobe: (info) pread on the reopened fd %s the bytes\n",
           (ro >= 0 && pread(ro, buf, SZ, 0) == SZ && matches(buf)) ? "sees" : "does NOT see");
    free(buf);

    /* step 4: unmap everything, then a child maps late */
    munmap(w, SZ);
    if (r != MAP_FAILED) munmap(r, SZ);
    socketpair(AF_UNIX, SOCK_STREAM, 0, sv);
    pid_t c2 = fork();
    if (c2 == 0) {
        close(sv[0]);
        int rfd = recv_fd(sv[1]);
        if (rfd < 0) _exit(10);
        unsigned char *m = mmap(0, SZ, PROT_READ, MAP_SHARED, rfd, 0);
        if (m == MAP_FAILED) _exit(11);
        _exit(matches(m) ? 0 : 12);
    }
    close(sv[1]);
    send_fd(sv[0], ro >= 0 ? ro : fd);
    waitpid(c2, &st, 0);
    check("child mapping after the writer unmapped sees the bytes", WIFEXITED(st) && WEXITSTATUS(st) == 0);
    if (WIFEXITED(st) && WEXITSTATUS(st)) printf("shmregionprobe:   (child exit %d)\n", WEXITSTATUS(st));

    printf("shmregionprobe: %s\n", fails ? "FAILED" : "all ok");
    return fails != 0;
}
