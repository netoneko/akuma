/*
 * credprobe: SCM_CREDENTIALS as Chromium's zygote ping uses them
 * (base::UnixDomainSocket::RecvMsgWithPid): a SOCK_SEQPACKET pair, SO_PASSCRED
 * on the receiving end, a forked child that sends a ping, and a parent that
 * learns the child's pid from the control message. Then the same with an fd
 * riding along (credentials must precede the rights), and a SOCK_STREAM pair.
 * Akuma before 2026-10-08 accepted SO_PASSCRED and never produced
 * credentials, so Chromium logged "Did not receive ping from zygote child".
 * Linux: every case ok.
 */
#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>

static int fails;

static void one(const char *label, int type, int with_fd) {
    int sv[2];
    if (socketpair(AF_UNIX, type, 0, sv) != 0) { perror("socketpair"); fails++; return; }
    int on = 1;
    setsockopt(sv[0], SOL_SOCKET, SO_PASSCRED, &on, sizeof on);
    int got_on = 0;
    socklen_t ol = sizeof got_on;
    getsockopt(sv[0], SOL_SOCKET, SO_PASSCRED, &got_on, &ol);

    pid_t child = fork();
    if (child == 0) {
        char ping[] = "PING";
        struct iovec iov = {ping, 4};
        char cbuf[CMSG_SPACE(sizeof(int))];
        struct msghdr m = {.msg_iov = &iov, .msg_iovlen = 1};
        if (with_fd) {
            int fd = open("/", O_RDONLY);
            m.msg_control = cbuf;
            m.msg_controllen = sizeof cbuf;
            struct cmsghdr *c = CMSG_FIRSTHDR(&m);
            c->cmsg_level = SOL_SOCKET;
            c->cmsg_type = SCM_RIGHTS;
            c->cmsg_len = CMSG_LEN(sizeof(int));
            memcpy(CMSG_DATA(c), &fd, sizeof fd);
        }
        _exit(sendmsg(sv[1], &m, 0) == 4 ? 0 : 1);
    }
    char buf[16];
    struct iovec iov = {buf, sizeof buf};
    char cbuf[CMSG_SPACE(sizeof(struct ucred)) + CMSG_SPACE(sizeof(int))];
    struct msghdr m = {.msg_iov = &iov, .msg_iovlen = 1, .msg_control = cbuf, .msg_controllen = sizeof cbuf};
    ssize_t n = recvmsg(sv[0], &m, 0);
    pid_t cred_pid = -1;
    int first_is_creds = 0, got_fd = -1, idx = 0;
    for (struct cmsghdr *c = CMSG_FIRSTHDR(&m); c; c = CMSG_NXTHDR(&m, c), idx++) {
        if (c->cmsg_level == SOL_SOCKET && c->cmsg_type == SCM_CREDENTIALS) {
            struct ucred u;
            memcpy(&u, CMSG_DATA(c), sizeof u);
            cred_pid = u.pid;
            if (idx == 0) first_is_creds = 1;
        } else if (c->cmsg_level == SOL_SOCKET && c->cmsg_type == SCM_RIGHTS) {
            memcpy(&got_fd, CMSG_DATA(c), sizeof got_fd);
        }
    }
    int st = 0;
    waitpid(child, &st, 0);
    int ok = n == 4 && got_on == 1 && cred_pid == child && first_is_creds
             && (!with_fd || got_fd >= 0) && WIFEXITED(st) && WEXITSTATUS(st) == 0;
    printf("credprobe[%s] n=%zd SO_PASSCRED=%d creds.pid=%d (child %d) creds-first=%d fd=%d %s\n",
           label, n, got_on, (int)cred_pid, (int)child, first_is_creds, got_fd, ok ? "ok" : "FAIL");
    fflush(stdout);
    if (!ok) fails++;
    if (got_fd >= 0) close(got_fd);
    close(sv[0]);
    close(sv[1]);
}

int main(void) {
    one("seqpacket", SOCK_SEQPACKET, 0);
    one("seqpacket+fd", SOCK_SEQPACKET, 1);
    one("stream", SOCK_STREAM, 0);
    printf("credprobe: %s\n", fails ? "FAIL" : "PASS");
    return fails != 0;
}
