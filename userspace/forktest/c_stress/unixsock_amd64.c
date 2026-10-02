/* unixsock_amd64 — AF_UNIX through socket/bind/listen/accept/connect on amd64.
 *
 * Until 2026-10-03 `socket(AF_UNIX, ..)` answered EAFNOSUPPORT on amd64 (only
 * socketpair was routed to glue's `unixsock`). 14 checks, all must PASS; the
 * probe prints "RESULT: all passed" on success.
 *
 * Build: x86_64-linux-musl-gcc -O1 -static -o unixsock_amd64 unixsock_amd64.c */
#include <stddef.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <errno.h>
#include <fcntl.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/stat.h>
#include <sys/epoll.h>
#include <sys/wait.h>
static int fails;
#define CHECK(c, msg) do { if (c) printf("PASS %s\n", msg); else { printf("FAIL %s (errno=%d)\n", msg, errno); fails++; } } while (0)
int main(void) {
    const char *path = "/tmp/unixprobe.sock";
    unlink(path);
    for (int ty = 0; ty < 2; ty++) {
        int type = ty ? SOCK_DGRAM : SOCK_STREAM;
        int s = socket(AF_UNIX, type, 0);
        CHECK(s >= 0, ty ? "socket(AF_UNIX, DGRAM)" : "socket(AF_UNIX, STREAM)");
        close(s);
    }
    int srv = socket(AF_UNIX, SOCK_STREAM, 0);
    struct sockaddr_un a = { .sun_family = AF_UNIX };
    strcpy(a.sun_path, path);
    CHECK(bind(srv, (struct sockaddr *)&a, sizeof a) == 0, "bind(path)");
    struct stat st;
    CHECK(stat(path, &st) == 0 && S_ISSOCK(st.st_mode), "bound path is S_IFSOCK");
    CHECK(listen(srv, 4) == 0, "listen");
    struct sockaddr_un n; socklen_t nl = sizeof n;
    CHECK(getsockname(srv, (struct sockaddr *)&n, &nl) == 0 && strcmp(n.sun_path, path) == 0, "getsockname");
    pid_t pid = fork();
    if (pid == 0) {
        int c = socket(AF_UNIX, SOCK_STREAM, 0);
        if (connect(c, (struct sockaddr *)&a, sizeof a) != 0) _exit(10);
        char b[16];
        if (write(c, "ping", 4) != 4) _exit(11);
        ssize_t r = read(c, b, sizeof b);
        if (r != 4 || memcmp(b, "pong", 4)) _exit(12);
        _exit(0);
    }
    struct epoll_event ev = { .events = EPOLLIN, .data.fd = srv }, out[1];
    int ep = epoll_create1(0);
    epoll_ctl(ep, EPOLL_CTL_ADD, srv, &ev);
    int n1 = epoll_wait(ep, out, 1, 5000);
    CHECK(n1 == 1 && out[0].data.fd == srv, "epoll reports listener readable");
    int c = accept4(srv, NULL, NULL, SOCK_CLOEXEC);
    CHECK(c >= 0, "accept4");
    char b[16] = {0};
    ssize_t r = read(c, b, sizeof b);
    CHECK(r == 4 && !memcmp(b, "ping", 4), "server read ping");
    CHECK(write(c, "pong", 4) == 4, "server write pong");
    int status = 0;
    waitpid(pid, &status, 0);
    CHECK(WIFEXITED(status) && WEXITSTATUS(status) == 0, "client exited 0 (connect+rw)");
    close(c);
    /* connect to a path nobody listens on */
    int x = socket(AF_UNIX, SOCK_STREAM, 0);
    struct sockaddr_un bad = { .sun_family = AF_UNIX };
    strcpy(bad.sun_path, "/tmp/no-such.sock");
    CHECK(connect(x, (struct sockaddr *)&bad, sizeof bad) < 0 && (errno == ENOENT || errno == ECONNREFUSED), "connect to nothing fails");
    /* abstract namespace */
    int ab = socket(AF_UNIX, SOCK_STREAM, 0);
    struct sockaddr_un aa = { .sun_family = AF_UNIX };
    memcpy(aa.sun_path + 1, "probe-abstract", 14);
    CHECK(bind(ab, (struct sockaddr *)&aa, offsetof(struct sockaddr_un, sun_path) + 15) == 0, "bind(abstract)");
    CHECK(listen(ab, 1) == 0, "listen(abstract)");
    unlink(path);
    printf(fails ? "RESULT: %d FAILED\n" : "RESULT: all passed\n", fails);
    return fails != 0;
}
