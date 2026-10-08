/*
 * capprobe: what a Chromium zygote child does between fork() and its ping
 * (sandbox/linux/services/credentials.cc, ForkAndDropCapabilitiesInChild):
 * open /proc, check it is single-threaded (fstatat "self/task/", nlink 3),
 * capset(V3, all zero) wrapped in a CHECK, then the ping. Also capget's
 * version negotiation (version 0 -> EINVAL, V3 written back), which
 * libcap-ng relies on (version 0 -> V3 written back; 0 with a NULL data,
 * EINVAL with one), and a V3 capget.
 *
 * Akuma amd64 before 2026-10-08 had no row for x86_64 125/126, so capset was
 * ENOSYS, the CHECK killed every zygote child, and the browser logged "Did
 * not receive ping from zygote child" (then "GPU process isn't usable").
 *
 * Linux: capset 0; both capget(v0) forms as above; capget(v3) 0 with
 * effective=0 after the drop; ping received with the child's pid. Akuma
 * accepts capset as a no-op and capget reports root's full set (pinned
 * divergence: the kernel has no capability model), so the post-drop value is
 * printed, not scored.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#define CAP_V3 0x20080522u

struct cap_hdr { uint32_t version; int pid; };
struct cap_data { uint32_t effective, permitted, inheritable; };

static int fails;

static void check(const char *what, int ok) {
    printf("capprobe: %s %s\n", what, ok ? "ok" : "FAIL");
    if (!ok) fails++;
}

/* The child's side, returned as an exit status: bit 0 task nlink, bit 1
 * capset, bit 2 capget(v0, NULL), bit 3 capget v3, bit 4 ping sent,
 * bit 5 capget(v0, data). */
static int child(int ping_fd) {
    int st = 0;
    int procfd = open("/proc", O_DIRECTORY | O_RDONLY | O_CLOEXEC);
    struct stat s;
    if (procfd >= 0 && fstatat(procfd, "self/task/", &s, 0) == 0 && s.st_nlink == 3) st |= 1;

    struct cap_hdr hdr = { CAP_V3, 0 };
    struct cap_data data[2] = { {0, 0, 0}, {0, 0, 0} };
    if (syscall(SYS_capset, &hdr, data) == 0) st |= 2;

    /* Linux's sys_capget: an unknown version always gets V3 written back;
     * the call then answers 0 with a NULL data (the pure "which version?"
     * probe) and EINVAL with one. */
    struct cap_hdr probe = { 0, 0 };
    long r = syscall(SYS_capget, &probe, NULL);
    if (r == 0 && probe.version == CAP_V3) st |= 4;
    struct cap_hdr probe2 = { 0, 0 };
    struct cap_data junk[2];
    r = syscall(SYS_capget, &probe2, junk);
    if (r == -1 && errno == EINVAL && probe2.version == CAP_V3) st |= 32;

    struct cap_hdr hdr3 = { CAP_V3, 0 };
    struct cap_data got[2] = { {0, 0, 0}, {0, 0, 0} };
    if (syscall(SYS_capget, &hdr3, got) == 0) st |= 8;
    /* Linux: 0 after the drop. Akuma: 0xffffffff (no capability model). */
    fprintf(stderr, "capprobe: child caps after drop eff=%#x perm=%#x\n",
            got[0].effective, got[0].permitted);

    char ping[] = "CHILD_PING";
    struct iovec iov = { ping, sizeof ping };
    struct msghdr m = { .msg_iov = &iov, .msg_iovlen = 1 };
    if (sendmsg(ping_fd, &m, MSG_NOSIGNAL) == (ssize_t)sizeof ping) st |= 16;
    return st;
}

int main(void) {
    int sv[2];
    if (socketpair(AF_UNIX, SOCK_SEQPACKET, 0, sv) != 0) { perror("socketpair"); return 1; }
    int on = 1;
    setsockopt(sv[0], SOL_SOCKET, SO_PASSCRED, &on, sizeof on);

    pid_t pid = fork();
    if (pid == 0) _exit(child(sv[1]));
    if (pid < 0) { perror("fork"); return 1; }

    char buf[32];
    struct iovec iov = { buf, sizeof buf };
    char cbuf[CMSG_SPACE(sizeof(struct ucred))];
    struct msghdr m = { .msg_iov = &iov, .msg_iovlen = 1, .msg_control = cbuf, .msg_controllen = sizeof cbuf };
    ssize_t n = recvmsg(sv[0], &m, 0);
    pid_t cred_pid = -1;
    for (struct cmsghdr *c = CMSG_FIRSTHDR(&m); c; c = CMSG_NXTHDR(&m, c)) {
        if (c->cmsg_level == SOL_SOCKET && c->cmsg_type == SCM_CREDENTIALS) {
            struct ucred u;
            memcpy(&u, CMSG_DATA(c), sizeof u);
            cred_pid = u.pid;
        }
    }
    int wst = 0;
    waitpid(pid, &wst, 0);
    int st = WIFEXITED(wst) ? WEXITSTATUS(wst) : -1;
    printf("capprobe: child status %d (exited=%d) n=%zd creds.pid=%d child=%d\n",
           st, WIFEXITED(wst), n, (int)cred_pid, (int)pid);
    check("single-threaded (self/task nlink 3)", st >= 0 && (st & 1));
    check("capset(V3, none) == 0", st >= 0 && (st & 2));
    check("capget(version 0, NULL) -> 0, V3 written", st >= 0 && (st & 4));
    check("capget(version 0, data) -> EINVAL, V3 written", st >= 0 && (st & 32));
    check("capget(V3) == 0", st >= 0 && (st & 8));
    check("ping sent", st >= 0 && (st & 16));
    check("ping received with child's pid", n == 11 && memcmp(buf, "CHILD_PING", 11) == 0 && cred_pid == pid);
    printf("capprobe: %s\n", fails ? "FAIL" : "PASS");
    return fails != 0;
}
