// pidfdprobe — pidfd_open on a child, the way tokio's process reaper uses it.
//
// tokio opens a pidfd for every spawned child, registers it with epoll for
// EPOLLIN, and calls waitpid(WNOHANG) when it becomes readable; without
// pidfd_open it falls back to SIGCHLD. Calibrate on real Linux first:
//   docker run --rm -v "$PWD:/w:ro" alpine /w/pidfdprobe
//
//   1. a live child's pidfd is not readable (poll, 0 timeout)
//   2. it becomes readable when the child exits (epoll, EPOLLIN)
//   3. waitpid then reaps the child with its status
//   4. the pidfd is close-on-exec without asking; O_CLOEXEC (or any flag but
//      PIDFD_NONBLOCK, which is O_NONBLOCK) is EINVAL
//   5. pidfd_send_signal(SIGTERM) kills a second child (WTERMSIG == SIGTERM)

#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <sys/epoll.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static int failures = 0;

static void check(const char *what, int ok) {
    printf("%s %s\n", ok ? "PASS" : "FAIL", what);
    if (!ok) failures++;
}

static int pidfd_open(pid_t pid, unsigned flags) { return (int)syscall(434, pid, flags); }
static int pidfd_send_signal(int fd, int sig) { return (int)syscall(424, fd, sig, 0, 0); }

int main(void) {
    int go[2];
    if (pipe(go)) return 2;
    pid_t c = fork();
    if (c == 0) {
        char b;
        close(go[1]);
        read(go[0], &b, 1); // exit only when the parent says so
        _exit(7);
    }
    close(go[0]);
    int fd = pidfd_open(c, 0);
    check("pidfd_open(child) >= 0", fd >= 0);
    if (fd < 0) {
        printf("errno %d\n", errno);
        return 1;
    }
    check("close-on-exec without asking", (fcntl(fd, F_GETFD) & FD_CLOEXEC) != 0);
    struct pollfd p = {.fd = fd, .events = POLLIN};
    check("live child: not readable", poll(&p, 1, 0) == 0);

    int ep = epoll_create1(EPOLL_CLOEXEC);
    struct epoll_event ev = {.events = EPOLLIN, .data.fd = fd};
    check("epoll_ctl ADD pidfd", epoll_ctl(ep, EPOLL_CTL_ADD, fd, &ev) == 0);
    write(go[1], "x", 1);
    struct epoll_event out;
    int n = epoll_wait(ep, &out, 1, 5000);
    check("exit makes it EPOLLIN", n == 1 && (out.events & EPOLLIN) && out.data.fd == fd);
    int st = 0;
    check("waitpid reaps it", waitpid(c, &st, WNOHANG) == c);
    check("status is exit 7", WIFEXITED(st) && WEXITSTATUS(st) == 7);
    close(ep);
    close(fd);

    check("O_CLOEXEC: EINVAL", pidfd_open(getpid(), O_CLOEXEC) < 0 && errno == EINVAL);
    check("flag 0x1: EINVAL", pidfd_open(getpid(), 0x1) < 0 && errno == EINVAL);

    pid_t d = fork();
    if (d == 0) {
        for (;;) sleep(100);
    }
    int fd2 = pidfd_open(d, O_NONBLOCK);
    check("PIDFD_NONBLOCK sets O_NONBLOCK", fd2 >= 0 && (fcntl(fd2, F_GETFL) & O_NONBLOCK) != 0);
    check("pidfd_send_signal(SIGTERM) == 0", fd2 >= 0 && pidfd_send_signal(fd2, SIGTERM) == 0);
    check("second child dies of SIGTERM", waitpid(d, &st, 0) == d && WIFSIGNALED(st) && WTERMSIG(st) == SIGTERM);
    close(fd2);

    printf("%s pidfdprobe (%d failures)\n", failures ? "FAIL" : "PASS", failures);
    return failures != 0;
}
